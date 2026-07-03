// SPDX-License-Identifier: BUSL-1.1
//! agentctl reference coordination MCP server (agentctl RFC 0011 §3.2) — the
//! claim-mode **correctness backbone**: the single serializing point that makes
//! exactly-one-owner hold across replicas.
//!
//! It serves the FROZEN `work.*` contract (agentd RFC 0015 §5.6) over MCP
//! JSON-RPC — the server side of what the conformant agent
//! (`agentd crates/agentd/src/cluster/claim.rs`) calls: an atomic `work.claim`,
//! the lease lifecycle (`renew`/`ack`/`release` + TTL expiry), transactional
//! dedupe on `claim_key`, and the off-pod backlog count (`work.stats` /
//! `work://pending`, contract ask P9) the future KEDA external scaler reads to
//! scale a fleet **from zero**.
//!
//! Surface:
//!   * `POST /` and `POST /mcp` — MCP JSON-RPC 2.0.
//!   * `GET /healthz` — liveness (always 200 while serving).
//!   * `GET /readyz`  — 200 once the lease-sweep loop is up.
//!   * `GET /metrics` — Prometheus exposition (agentctl RFC 0010 text format).
//!
//! Plain HTTP is fine for v1 — it sits behind the cluster network / egress proxy
//! (agentctl RFC 0011 §3.4). Hand-rolled in Rust (axum); agentctl is Rust-only.
//!
//! **Open question (agentctl RFC 0011 §3.2 / §10):** HA, durability, and
//! per-fleet vs cluster-shared sharding of this single replica. The store sits
//! behind the [`store::ClaimStore`] trait so a Redis/Postgres backend slots in
//! without touching the wire layer; v1 ships the in-memory store and documents
//! that a coordination loss collapses the serializing point for dependent fleets.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::{Api, Client};
use serde_json::Value;
use tokio::net::TcpListener;

mod attest;
mod db_tls;
mod mcp;
mod metrics;
mod mtls;
mod pg_store;
mod store;

use attest::CallerIdentity;
use metrics::Metrics;
use pg_store::PgClaimStore;
use store::{ClaimStore, InMemoryStore};

/// FIFO bound on the dedupe (done) set — see `store::DoneSet`. Override with
/// `COORDINATION_DEDUPE_CAP`.
const DEFAULT_DEDUPE_CAP: usize = 100_000;
/// How often the background task sweeps expired leases back to pending. Override
/// with `COORDINATION_SWEEP_INTERVAL_MS`.
const DEFAULT_SWEEP_INTERVAL_MS: u64 = 250;

/// Shared handler state. `store` is the single serializing point (`Arc<dyn …>` so
/// a durable backend can replace it without a wire change).
#[derive(Clone)]
struct AppState {
    store: Arc<dyn ClaimStore>,
    metrics: Arc<Metrics>,
    /// Flipped true once the sweep loop is running (drives `/readyz`).
    ready: Arc<AtomicBool>,
    /// Optional bearer token gating the data endpoints (`POST /`, `POST /mcp`).
    /// `Some` ⇒ enforce `Authorization: Bearer <token>`; `None` (env unset/empty)
    /// ⇒ no auth (back-compat). Read once from `AGENTCTL_API_TOKEN` at startup.
    auth_token: Option<Arc<String>>,
    /// OPT-IN attested-identity gate (RFC 0015), `COORDINATION_ATTEST_IDENTITY`.
    /// When `true`, the claim lifecycle is bound to / verified against the caller's
    /// kernel-attested source-IP identity (the lease HOLDER), not the spoofable
    /// self-asserted `_meta`. When `false` (default), behaviour is unchanged.
    attest: bool,
    /// In-cluster kube client used ONLY in attested mode to resolve a source IP to
    /// its pod. `None` when attestation is off (the server then does NO cluster
    /// reads, exactly as before).
    client: Option<Client>,
    /// TTL cache of `source IP → attested identity`, so a burst of claim calls from
    /// one pod does not hammer the kube API. Unused when `attest` is `false`.
    ip_cache: Arc<attest::IpIdentityCache>,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set — matches every other control-plane bin.
    agentctl_telemetry::init("agentctl-coordination");

    let dedupe_cap = env_usize("COORDINATION_DEDUPE_CAP", DEFAULT_DEDUPE_CAP);
    let sweep_ms = env_u64("COORDINATION_SWEEP_INTERVAL_MS", DEFAULT_SWEEP_INTERVAL_MS);

    // BACKEND SELECTION (agentctl RFC 0011 §3.2 / §10): a durable, HA-capable
    // Postgres store when COORDINATION_DATABASE_URL (or DATABASE_URL) is set —
    // the serializing point becomes a shared DB row, so grant-one holds across
    // >1 replica and survives a restart. Absent, the in-memory store is the
    // (single-replica, non-durable) default. Both sit behind the SAME ClaimStore
    // trait, so the MCP wire layer is untouched either way.
    let store: Arc<dyn ClaimStore> = match coordination_database_url() {
        Some(url) => {
            tracing::info!(
                "coordination backend: Postgres (durable, HA-capable across replicas) \
                 via COORDINATION_DATABASE_URL/DATABASE_URL"
            );
            match PgClaimStore::connect(&url) {
                Ok(s) => Arc::new(s),
                Err(e) => panic!("coordination Postgres backend: {e}"),
            }
        }
        None => {
            tracing::info!(
                "coordination backend: in-memory (single-replica, non-durable default) — \
                 set COORDINATION_DATABASE_URL/DATABASE_URL for the durable Postgres backend"
            );
            Arc::new(InMemoryStore::new(dedupe_cap))
        }
    };
    let metrics = Arc::new(Metrics::new());
    let ready = Arc::new(AtomicBool::new(false));

    // Background sweeper: return expired leases to pending so a dead claimer's
    // item is re-offered to the fleet (agentd RFC 0019 §3.2). Marks ready once up.
    {
        let store = store.clone();
        let metrics = metrics.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            ready.store(true, Ordering::Release);
            let mut tick = tokio::time::interval(Duration::from_millis(sweep_ms.max(1)));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let n = store.sweep_expired();
                if n > 0 {
                    metrics.add_expired(n);
                    tracing::debug!(swept = n, "expired leases returned to pending");
                }
            }
        });
    }

    // Bearer-token gate (agentctl RFC 0011 §3.4 hardening): read AGENTCTL_API_TOKEN
    // once. Unset/empty ⇒ no auth (back-compat); set ⇒ enforce on the data routes.
    let auth_token = std::env::var("AGENTCTL_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(Arc::new);
    if auth_token.is_some() {
        tracing::info!(
            "AGENTCTL_API_TOKEN set: bearer-token gate enforced on POST / and POST /mcp"
        );
    } else {
        tracing::info!(
            "AGENTCTL_API_TOKEN unset: data endpoints are unauthenticated (back-compat)"
        );
    }

    // OPT-IN attested-identity gate (RFC 0015), COORDINATION_ATTEST_IDENTITY. OFF
    // (default) ⇒ the lease holder is the self-asserted `_meta` agent, exactly as
    // before, and the server does NO cluster reads. ON ⇒ the holder is derived from
    // the kernel-set source IP (resolved to the real pod via the kube API), so a
    // tenant can neither bill a claim to another identity nor ack/renew/release
    // (settle or steal) another tenant's lease.
    let attest = attest::attest_enabled_from_env();
    // The own (control-plane) namespace, from the downward-API POD_NAMESPACE. Read
    // and logged at startup for parity with the modelgateway; direct source-IP
    // attestation does not otherwise need it.
    let pod_namespace = std::env::var("POD_NAMESPACE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let client = if attest {
        tracing::info!(
            pod_namespace = %pod_namespace,
            "COORDINATION_ATTEST_IDENTITY set: claim lifecycle bound to the source-IP-attested \
             holder (namespace/agent); a tenant cannot settle or steal another tenant's lease"
        );
        Some(Client::try_default().await.expect("in-cluster kube client"))
    } else {
        tracing::info!(
            "COORDINATION_ATTEST_IDENTITY unset: lease holder is the self-asserted _meta agent \
             (token-gated only; back-compat)"
        );
        None
    };
    let ip_cache = Arc::new(attest::IpIdentityCache::new(attest::DEFAULT_TTL));

    let state = AppState {
        store,
        metrics,
        ready,
        auth_token,
        attest,
        client,
        ip_cache,
    };

    // The data routes carry the bearer gate via `route_layer` so it runs ONLY for
    // `POST /` and `POST /mcp`; the probe/metrics routes added afterwards are always
    // exempt (never require the token).
    let app = Router::new()
        .route("/", post(rpc))
        .route("/mcp", post(rpc))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_gate))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(serve_metrics))
        .with_state(state.clone());

    let port = port_from_env();
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));

    // OPTIONAL mTLS listener (COORDINATION_MTLS_ADDR, RFC 0015). UNSET ⇒ OFF: the
    // plaintext path below is byte-identical to before (no second listener, no crypto
    // provider install, no extra cluster work). SET ⇒ a SECOND mTLS listener runs
    // alongside :8080, where a verified + allow-listed client cert authenticates the
    // caller (the scaler) in place of the coarse AGENTCTL_API_TOKEN.
    let Some(mtls_cfg) = mtls::Config::from_env() else {
        tracing::info!(%addr, dedupe_cap, sweep_ms, attest, "agentctl coordination MCP server: serving the work.* claim surface");
        // Graceful shutdown on SIGTERM/SIGINT — drain in-flight requests (matches the
        // gateway). A SIGTERM is the normal pod-stop signal.
        //
        // `into_make_service_with_connect_info::<SocketAddr>()` exposes the peer socket
        // address to handlers via `ConnectInfo<SocketAddr>` — the kernel-set source IP
        // the attested-identity gate reads. Harmless in non-attested mode (the extractor
        // is simply unused for the holder decision).
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve");
        return;
    };

    // mTLS enabled: install the process-default ring provider (ignore an
    // already-installed error — the kube client may install one in attested mode),
    // then build the rustls server config that REQUIRES a CA-signed client cert.
    // Missing/invalid material panics at startup (like the gateway/node-agent).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls_addr: SocketAddr = mtls_cfg
        .addr
        .parse()
        .unwrap_or_else(|e| panic!("parse COORDINATION_MTLS_ADDR {}: {e}", mtls_cfg.addr));
    let server_config = mtls::build_tls_config(&mtls_cfg.tls_dir, &mtls_cfg.ca_path)
        .unwrap_or_else(|e| panic!("build coordination mTLS server config: {e}"));
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
    let acceptor = mtls::PeerCertAcceptor::new(rustls_config);

    // The mTLS router shares the SAME routes; its gate enforces the CN/SAN allow-list
    // (403 on miss) and marks the request authenticated so the bearer gate is skipped.
    let mtls_ctx = mtls::MtlsCtx {
        cfg: Arc::new(mtls_cfg.clone()),
        metrics: state.metrics.clone(),
    };
    let mtls_app = app
        .clone()
        .layer(middleware::from_fn_with_state(mtls_ctx, mtls::mtls_gate))
        .into_make_service_with_connect_info::<SocketAddr>();

    tracing::info!(
        %addr, tls_addr = %mtls_cfg.addr, ca = %mtls_cfg.ca_path.display(),
        allowed = ?mtls_cfg.allowed_names, dedupe_cap, sweep_ms, attest,
        "agentctl coordination MCP server: plaintext :8080 (token-gated) + OPTIONAL mTLS listener (client-cert authenticated, token skipped)"
    );

    // Both listeners run concurrently (tokio::join!, mirroring the node-agent). A
    // shared axum_server Handle wires SIGTERM/SIGINT to the mTLS listener's graceful
    // drain so the join completes and the process exits cleanly on a pod stop.
    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            handle.graceful_shutdown(Some(Duration::from_secs(10)));
        });
    }
    let plain_srv = async {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve plaintext");
    };
    let mtls_srv = async {
        axum_server::bind(tls_addr)
            .handle(handle)
            .acceptor(acceptor)
            .serve(mtls_app)
            .await
            .expect("serve mTLS");
    };
    tokio::join!(plain_srv, mtls_srv);
}

/// Bearer-token gate for the data endpoints. When `state.auth_token` is `None`
/// (env unset/empty) every request passes through unchanged (back-compat). When
/// `Some`, the request MUST carry `Authorization: Bearer <token>` with a token
/// that matches in constant time; otherwise it is rejected with a bare 401 (no
/// body, no detail leak) and counted in `agentctl_coordination_auth_rejected_total`.
/// This layer wraps only `POST /` and `POST /mcp` — the probes/metrics are exempt.
async fn auth_gate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // mTLS listener: a verified + allow-listed client cert IS the authentication
    // (the `mtls::mtls_gate` layer set this marker upstream), so skip the coarse
    // bearer-token gate for those connections. The plaintext listener never sets it.
    if req.extensions().get::<mtls::MtlsVerified>().is_some() {
        return next.run(req).await;
    }
    let Some(expected) = &state.auth_token else {
        // No token configured ⇒ auth disabled.
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let ok = presented.is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        state.metrics.inc_auth_rejected();
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Constant-time byte-slice equality — avoids the early-exit timing side-channel of
/// `==` when comparing the secret token. Unequal lengths are not equal (length is
/// not itself the secret here); equal-length inputs are compared in full every time.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `POST /` and `POST /mcp` — one MCP JSON-RPC message (or a batch array). A
/// notification yields no body (202 Accepted).
///
/// The caller's attested identity is resolved ONCE from the kernel-set source IP
/// (per-connection, so it applies to every message of a batch) and threaded into
/// the pure wire layer. In non-attested mode it is [`CallerIdentity::Disabled`].
async fn rpc(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<Value>,
) -> Response {
    let caller = resolve_caller(&state, peer.ip()).await;
    if let Some(batch) = req.as_array() {
        let mut out = Vec::new();
        for item in batch {
            if let Some(resp) = mcp::handle_rpc(item, state.store.as_ref(), &state.metrics, &caller)
            {
                out.push(resp);
            }
        }
        if out.is_empty() {
            StatusCode::ACCEPTED.into_response()
        } else {
            Json(Value::Array(out)).into_response()
        }
    } else {
        match mcp::handle_rpc(&req, state.store.as_ref(), &state.metrics, &caller) {
            Some(resp) => Json(resp).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        }
    }
}

/// Resolve the caller's attested identity from its source IP (RFC 0015).
///
/// Non-attested mode (the default) ⇒ [`CallerIdentity::Disabled`] (no cluster
/// read). Attested mode ⇒ a cache-first kube lookup: the source IP is resolved to
/// its pod and the pod's `namespace/agent` is the attested holder
/// ([`CallerIdentity::Attested`]). A source IP that owns no attestable pod — or a
/// kube error — yields [`CallerIdentity::Unresolved`], which **fails closed** on the
/// claim lifecycle (we never trust the spoofable self-asserted holder in attested
/// mode).
async fn resolve_caller(state: &AppState, peer_ip: IpAddr) -> CallerIdentity {
    if !state.attest {
        return CallerIdentity::Disabled;
    }
    if let Some(id) = state.ip_cache.get(&peer_ip) {
        return CallerIdentity::Attested(id.holder());
    }
    let Some(client) = state.client.as_ref() else {
        // Unreachable: attest ⇒ Some(client). Fail closed if it ever is not.
        return CallerIdentity::Unresolved;
    };
    match resolve_ip_to_identity(client, peer_ip).await {
        Ok(Some(id)) => {
            state.ip_cache.put(peer_ip, id.clone());
            CallerIdentity::Attested(id.holder())
        }
        Ok(None) => CallerIdentity::Unresolved,
        Err(e) => {
            // A kube lookup failure cannot safely attest ⇒ fail closed (Unresolved).
            tracing::warn!(%peer_ip, error = %e, "attest: source IP lookup failed; failing closed");
            CallerIdentity::Unresolved
        }
    }
}

/// Resolve a source IP to its pod's attested identity (kube glue — needs a cluster
/// to run, not to compile/test). Lists pods cluster-wide with a `status.podIP`
/// field selector to narrow, re-verifies the match locally (the selector is
/// advisory), and derives `{namespace, agent}`. `Ok(None)` ⇒ no pod owns the IP (or
/// it has no namespace) — cannot attest.
async fn resolve_ip_to_identity(
    client: &Client,
    ip: IpAddr,
) -> Result<Option<attest::Identity>, String> {
    let pods: Api<Pod> = Api::all(client.clone());
    let ip_s = ip.to_string();
    let lp = ListParams::default().fields(&format!("status.podIP={ip_s}"));
    let list = pods.list(&lp).await.map_err(|e| e.to_string())?;
    Ok(list
        .items
        .iter()
        .find(|p| attest::pod_matches_ip(p, &ip_s))
        .and_then(attest::identity_from_pod))
}

/// `GET /readyz` — 200 once the sweep loop is running, else 503.
async fn readyz(State(state): State<AppState>) -> StatusCode {
    if state.ready.load(Ordering::Acquire) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// `GET /metrics` — Prometheus exposition. The `pending`/`claimed` gauges are read
/// live from the store at scrape time.
async fn serve_metrics(
    State(state): State<AppState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    let s = state.store.stats();
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(s.pending, s.claimed, s.deadletter),
    )
}

/// Wait for SIGTERM/SIGINT, then resolve so hyper drains in-flight requests.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down: draining in-flight requests");
}

/// The HTTP port: `PORT`, then `HTTP_PORT`, else 8080.
fn port_from_env() -> u16 {
    for key in ["PORT", "HTTP_PORT"] {
        if let Ok(v) = std::env::var(key) {
            if let Ok(p) = v.parse::<u16>() {
                return p;
            }
        }
    }
    8080
}

/// The coordination Postgres DSN, if configured. Prefers the coordination-specific
/// `COORDINATION_DATABASE_URL`, then the shared `DATABASE_URL`. An unset OR empty
/// value selects the in-memory backend (back-compat default).
fn coordination_database_url() -> Option<String> {
    for key in ["COORDINATION_DATABASE_URL", "DATABASE_URL"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Parse a `usize` env var, falling back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Parse a `u64` env var, falling back to `default`.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_defaults_to_8080() {
        // No env override in the test harness ⇒ default.
        assert_eq!(port_from_env(), 8080);
    }

    #[test]
    fn env_helpers_fall_back_on_absent_or_garbage() {
        assert_eq!(env_usize("COORDINATION_NOPE_USIZE", 7), 7);
        assert_eq!(env_u64("COORDINATION_NOPE_U64", 9), 9);
    }

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"s3cret-token", b"s3cret-token"));
        assert!(!constant_time_eq(b"s3cret-token", b"s3cret-toker"));
        assert!(!constant_time_eq(b"s3cret-token", b"s3cret")); // length mismatch
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
