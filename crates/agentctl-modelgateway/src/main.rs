// SPDX-License-Identifier: BUSL-1.1
//! agentctl ModelGateway — the intelligence plane's inference proxy.
//!
//! Conformant agents **hold NO provider secrets** and cannot reach a model
//! provider on their own; instead they dial this gateway keyless and their
//! intelligence request carries only their *identity*. That identity is attested
//! from the connection's source IP (resolved to the caller's pod), or by default
//! taken as-is from the `X-Agent-*` headers. The gateway:
//!   1. selects the agent's `ModelPool` (CRD, `agentctl.dev/v1alpha1`),
//!   2. enforces the pool's token **budget** pre-request,
//!   3. **injects** the pool's provider credential (read from the referenced
//!      `Secret`) — the agent's own credential, if any, is NEVER used,
//!   4. forwards the request to the provider endpoint, and
//!   5. **meters** the tokens consumed into a durable Postgres store.
//!
//! Hand-rolled in Rust (axum); agentctl is Rust-only and depends on the
//! contract/wire, never on a specific agent or provider SDK.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use agent_api::{Agent, AgentFleet, ModelPool, ModelPoolSpec};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use deadpool_postgres::Pool;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::ListParams;
use kube::{Api, Client};
use serde_json::{json, Value};

mod attest;
mod auth;
mod db_tls;
mod metrics;
mod store;

/// Identity header: the requesting agent's namespace (required).
const H_NAMESPACE: &str = "X-Agent-Namespace";
/// Identity header: the requesting agent's name (optional; defaults to `unknown`).
const H_AGENT: &str = "X-Agent-Name";
/// Routing header: which `ModelPool` to use (optional; defaults to the first in ns).
const H_POOL: &str = "X-Model-Pool";
/// Forwarder header: the real caller's pod UID, asserted by the trusted
/// forwarder. Trusted ONLY when the source IP resolves to the forwarder pod (the
/// `agentctl-node-agent` ServiceAccount in the control-plane namespace); ignored
/// from any other (direct) caller.
const H_POD_UID: &str = "X-Agent-Pod-Uid";

#[derive(Clone)]
struct AppState {
    client: Client,
    pool: Pool,
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
    /// When `true`, the caller's identity is **attested** from its source IP
    /// (resolved to the real pod via the kube API) and the spoofable
    /// `X-Agent-Namespace` header is never trusted for the tenant. When `false`
    /// (default), the header carries the identity.
    attest: bool,
    /// The ModelGateway's own (control-plane) namespace, read from `POD_NAMESPACE`
    /// at startup. It anchors the trusted **forwarder**: only a pod in
    /// THIS namespace running the `agentctl-node-agent` ServiceAccount is trusted
    /// to forward another tenant's identity — an anchor a tenant cannot forge.
    /// **Fail closed:** empty (`POD_NAMESPACE` unset/empty) ⇒ NO forwarder is
    /// trusted; every source is attested directly by its own source IP.
    control_plane_ns: String,
    /// TTL cache of `source IP → attested identity`, so a burst from one pod
    /// does not hammer the kube API. Unused when `attest` is `false`.
    ip_cache: Arc<attest::IpIdentityCache>,
    /// Tokens to reserve for a budgeted request that declares no output cap
    /// (`max_tokens` etc.). It stands in as the upper-bound estimate so an uncapped
    /// request still reserves a meaningful amount against the pool budget, keeping
    /// the cap hard under concurrency. From `MODELGATEWAY_DEFAULT_RESERVE_TOKENS`
    /// (default [`DEFAULT_RESERVE_TOKENS`]).
    default_reserve: i64,
}

/// Fallback per-request reservation when a budgeted request declares no output cap.
/// Conservative but not so large it needlessly rejects; overridable via
/// `MODELGATEWAY_DEFAULT_RESERVE_TOKENS`.
const DEFAULT_RESERVE_TOKENS: i64 = 8192;

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set; otherwise byte-identical to before.
    agentctl_telemetry::init("agentctl-modelgateway");

    let client = Client::try_default().await.expect("in-cluster kube client");

    // The durable usage meter. Retry the schema — the DB pod may start after us.
    let pool = build_pool();
    for attempt in 1..=30u32 {
        match store::ensure_schema(&pool).await {
            Ok(()) => break,
            Err(e) if attempt == 30 => panic!("postgres schema after 30 tries: {e}"),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "waiting for postgres…");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    // Shared metrics surface (also feeds the access gate's rejection counter).
    let metrics = Arc::new(metrics::Metrics::new());

    // Identity attestation gate. OFF (default) → the agent's identity is read
    // from the spoofable X-Agent-* headers. ON → the identity is derived from
    // the kernel-set source IP, resolved to the real pod via the kube API; the
    // header can no longer impersonate a tenant.
    let attest = attest::attest_enabled_from_env();
    if attest {
        tracing::info!(
            "IDENTITY_ATTEST set: caller identity ATTESTED from source IP (X-Agent-Namespace is advisory)"
        );
    } else {
        tracing::info!(
            "IDENTITY_ATTEST unset: caller identity taken from X-Agent-* headers (spoofable; back-compat)"
        );
    }

    // The control-plane (own) namespace, from the downward-API POD_NAMESPACE. It
    // anchors the trusted forwarder to a pod in THIS namespace running
    // the agentctl-node-agent ServiceAccount — unforgeable by a tenant. Empty
    // (unset) ⇒ fail closed: NO forwarder is trusted (warn once below); direct
    // source-IP attestation still works.
    let control_plane_ns = std::env::var("POD_NAMESPACE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    if attest {
        if control_plane_ns.is_empty() {
            tracing::warn!(
                "IDENTITY_ATTEST set but POD_NAMESPACE is unset/empty: NO forwarder will be \
                 trusted (fail closed) — better to refuse forwarder trust than anchor it weakly. \
                 Direct source-IP attestation still works; set POD_NAMESPACE (downward API \
                 metadata.namespace) to anchor the node-agent forwarder to the control-plane \
                 namespace + ServiceAccount."
            );
        } else {
            tracing::info!(
                control_plane_ns = %control_plane_ns,
                "attest: node-agent forwarder anchored to the control-plane namespace + \
                 agentctl-node-agent ServiceAccount (unforgeable by a tenant)"
            );
        }
    }
    let ip_cache = Arc::new(attest::IpIdentityCache::new(attest::DEFAULT_TTL));
    // Fallback reservation for budgeted requests that pin no output cap.
    let default_reserve = std::env::var("MODELGATEWAY_DEFAULT_RESERVE_TOKENS")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_RESERVE_TOKENS);
    // Optional bearer-token access gate (AGENTCTL_API_TOKEN). Unset → no-op; set
    // → enforced on the data routes, with /healthz /readyz /metrics exempt. The
    // middleware itself short-circuits the exempt paths, so it can wrap the whole
    // router.
    let gate = auth::Auth::from_env(metrics.clone());

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // `/metrics` rides the EXISTING plaintext :8080 (the chart's `http` port),
        // alongside /healthz — no new port; scraped scheme=http.
        .route("/metrics", get(serve_metrics))
        .route("/v1/infer", post(infer))
        // OpenAI-compatible alias: a conformant agent (e.g. agentd) dialing its
        // `INTELLIGENCE` endpoint as an OpenAI provider POSTs the default
        // path `/v1/chat/completions`. The gateway is provider-neutral on the
        // wire, so this aliases to the SAME identity/pool/budget/credential-inject
        // path as `/v1/infer` — the agent's inference loop reaches the gateway
        // without the agent knowing the gateway's native path.
        .route("/v1/chat/completions", post(infer))
        .route("/v1/usage", get(usage))
        .layer(axum::middleware::from_fn_with_state(gate, auth::gate))
        .with_state(AppState {
            client,
            pool,
            metrics,
            attest,
            control_plane_ns,
            ip_cache,
            default_reserve,
        });

    // Optional TLS listener (contract 1.0): agents dial their rendered
    // `INTELLIGENCE=https://…` keyless — the serving cert (cert-manager,
    // chains to the cluster CA the agent trusts via `--tls-ca`) authenticates
    // US to the agent; the AGENT's identity stays source-IP attestation, so
    // this is server-auth-only TLS (no client certs). Enabled when both
    // `MODELGATEWAY_TLS_ADDR` and `MODELGATEWAY_TLS_DIR` (tls.crt/tls.key) are
    // set; runs alongside the plaintext :8080 (metrics scrape + legacy dials).
    let tls_addr_env = std::env::var("MODELGATEWAY_TLS_ADDR")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let tls_dir_env = std::env::var("MODELGATEWAY_TLS_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty());
    if let (Some(tls_addr), Some(tls_dir)) = (tls_addr_env, tls_dir_env) {
        let tls_addr: SocketAddr = tls_addr
            .parse()
            .unwrap_or_else(|e| panic!("parse MODELGATEWAY_TLS_ADDR {tls_addr}: {e}"));
        let server_config = tls_server_config(std::path::Path::new(&tls_dir))
            .unwrap_or_else(|e| panic!("build modelgateway TLS config from {tls_dir}: {e}"));
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
        let tls_app = app
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        tracing::info!(%tls_addr, dir = %tls_dir, "modelgateway TLS listener (keyless agent dials)");
        tokio::spawn(async move {
            axum_server::bind_rustls(tls_addr, rustls_config)
                .serve(tls_app)
                .await
                .expect("serve modelgateway TLS");
        });
    }

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    tracing::info!(%addr, "agentctl modelgateway serving the intelligence plane");
    // Graceful shutdown: on SIGTERM/SIGINT, stop accepting and drain in-flight
    // requests (hyper's `with_graceful_shutdown`).
    //
    // `into_make_service_with_connect_info::<SocketAddr>()` makes the peer
    // socket address available to handlers via `ConnectInfo<SocketAddr>` — the
    // kernel-set source IP attestation reads from there. This is harmless in
    // header (non-attested) mode; the extractor is simply unused.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("serve");
}

/// Server-auth-only rustls config for the TLS listener: the serving identity
/// from `<dir>/tls.crt` + `<dir>/tls.key` (a mounted cert-manager Secret), NO
/// client-certificate verification — the caller's identity is source-IP
/// attestation, not a certificate. rustls resolves ring as the provider (the
/// only compiled-in crypto feature; no aws-lc-rs in this graph).
fn tls_server_config(dir: &std::path::Path) -> Result<rustls::ServerConfig, String> {
    let load = |name: &str| -> Result<std::io::BufReader<std::fs::File>, String> {
        let p = dir.join(name);
        Ok(std::io::BufReader::new(
            std::fs::File::open(&p).map_err(|e| format!("open {p:?}: {e}"))?,
        ))
    };
    let certs = rustls_pemfile::certs(&mut load("tls.crt")?)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read tls.crt: {e}"))?;
    let key = rustls_pemfile::private_key(&mut load("tls.key")?)
        .map_err(|e| format!("read tls.key: {e}"))?
        .ok_or("no private key in tls.key")?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("server config: {e}"))
}

// --- graceful shutdown -----------------------------------------------------

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

/// `GET /metrics` — the Prometheus text-format exposition.
async fn serve_metrics(
    State(state): State<AppState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

// --- handlers --------------------------------------------------------------

/// `POST /v1/infer` — the inference wire. The agent supplies only its identity in
/// headers and a provider-neutral body; the gateway selects its pool, enforces
/// the budget, injects the pool's credential, forwards to the provider, meters
/// the result, and returns the provider response (tagged with the pool for
/// traceability).
#[tracing::instrument(name = "modelgateway.infer", skip_all)]
async fn infer(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    state.metrics.inc_request();
    // a. identity — attested from the source IP or, by default, from the
    //    X-Agent-* headers.
    let (ns, agent) = match resolve_identity(&state, peer.ip(), &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    // The X-Model-Pool header is an explicit override; otherwise honor the pool the
    // attested agent's CR declares (spec.model.pool). The operator renders no header,
    // so this is how an agent's declared intelligence choice actually takes effect.
    let want_pool = match header_str(&headers, H_POOL) {
        Some(p) => Some(p),
        None => declared_pool(&state.client, &ns, &agent).await,
    };

    // b. select the ModelPool.
    let pools: Api<ModelPool> = Api::namespaced(state.client.clone(), &ns);
    let (pool_name, spec) = match select_pool(&pools, want_pool.as_deref()).await {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(&no_pool_msg(&ns, want_pool.as_deref()), &ns),
        Err(e) => return internal(&format!("select ModelPool: {e}")),
    };
    let budget = spec.budget.as_ref().map(|b| b.max_tokens);
    // Per-fleet cap: when the caller is an AgentFleet with its own budget,
    // enforce it alongside the pool cap so one fleet cannot drain a shared
    // pool. `None` for a plain Agent or a fleet without a budget.
    let fleet_budget = fleet_budget(&state.client, &ns, &agent).await;

    // c. budget — atomic, race-free reservation. When the pool AND/OR the fleet
    //    declares a cap, reserve a conservative upper-bound estimate BEFORE the
    //    provider call: concurrent requests serialize per-pool so every present
    //    cap holds under load. A bare pre-request SUM check would let a whole
    //    fleet overshoot, since concurrent callers each read the same pre-spend
    //    total. No cap ⇒ no reservation. The reservation is reconciled to the
    //    actual spend after the call, or released on any early return below.
    let reservation: Option<(i64, i64)> = if budget.is_some() || fleet_budget.is_some() {
        let est = estimate_reservation(&body, state.default_reserve);
        match store::reserve(
            &state.pool,
            &ns,
            &pool_name,
            &agent,
            est,
            budget,
            fleet_budget,
        )
        .await
        {
            Ok(Some(id)) => Some((id, est)),
            Ok(None) => {
                state.metrics.inc_budget_rejection();
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "error": "budget exceeded",
                        "namespace": ns,
                        "pool": pool_name,
                        "poolBudget": budget,
                        "fleetBudget": fleet_budget,
                        "requestedTokens": est,
                    })),
                )
                    .into_response();
            }
            Err(e) => return internal(&format!("budget reserve: {e}")),
        }
    } else {
        None
    };

    // d. read the credential the gateway will inject (never the agent's own). Any
    //    early return past this point must first release the reservation so a failed
    //    request never permanently consumes budget.
    let secrets: Api<Secret> = Api::namespaced(state.client.clone(), &ns);
    let secret_name = &spec.credential_secret_ref.name;
    let secret = match secrets.get_opt(secret_name).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            release_reservation(&state, &reservation).await;
            return not_found(&format!("Secret {secret_name} not found"), &ns);
        }
        Err(e) => {
            release_reservation(&state, &reservation).await;
            return internal(&format!("get Secret {secret_name}: {e}"));
        }
    };
    let key = match read_secret_key(&secret, &spec.credential_secret_ref.key) {
        Ok(k) => k,
        Err(e) => {
            release_reservation(&state, &reservation).await;
            return internal(&e);
        }
    };

    // e. inject the default model when the body pins none, then forward with the
    //    pool's credential injected as a bearer token.
    inject_model(&mut body, spec.default_model.as_deref());
    let url = format!("{}/v1/infer", spec.endpoint.trim_end_matches('/'));
    let resp = match reqwest::Client::new()
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            state.metrics.inc_error();
            release_reservation(&state, &reservation).await;
            return bad_gateway(&format!("provider POST {url}: {e}"));
        }
    };
    if !resp.status().is_success() {
        state.metrics.inc_error();
        release_reservation(&state, &reservation).await;
        let code = resp.status().as_u16();
        let detail = resp.text().await.unwrap_or_default();
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "provider error", "status": code, "detail": detail })),
        )
            .into_response();
    }
    let mut provider_body = match resp.json::<Value>().await {
        Ok(b) => b,
        Err(e) => {
            state.metrics.inc_error();
            release_reservation(&state, &reservation).await;
            return bad_gateway(&format!("decode provider response: {e}"));
        }
    };

    // f. meter the tokens, then return the provider body tagged with the pool.
    // Normalize across provider shapes (OpenAI total_tokens; OpenAI
    // prompt+completion; Anthropic input+output). Metering is a SPEND CAP input, so
    // never fail open at 0: when a provider returns no parseable usage, charge a
    // conservative size-based estimate so it cannot silently drain the budget.
    let total = match extract_token_count(&provider_body) {
        Some(t) => t,
        None => {
            let est = estimate_tokens_from_size(&provider_body);
            tracing::warn!(
                %ns, pool = %pool_name, est,
                "provider returned no parseable usage; charging a conservative estimate (fail-closed)"
            );
            est
        }
    };
    state.metrics.add_tokens(total);
    // Reconcile: a budgeted request commits its reservation to the ACTUAL spend (so
    // the committed total never exceeds the budget the reservation was admitted
    // under); an uncapped request just appends to the audit ledger.
    match &reservation {
        Some((id, _est)) => {
            if let Err(e) =
                store::commit_reservation(&state.pool, &ns, &pool_name, &agent, *id, total).await
            {
                tracing::warn!(%ns, pool = %pool_name, error = %e, "commit reservation failed");
            }
        }
        None => {
            if let Err(e) = store::record_usage(&state.pool, &ns, &pool_name, &agent, total).await {
                tracing::warn!(%ns, pool = %pool_name, error = %e, "record usage failed");
            }
        }
    }
    if let Some(obj) = provider_body.as_object_mut() {
        obj.insert(
            "x-agentctl-pool".to_string(),
            json!(format!("{ns}/{pool_name}")),
        );
    }
    Json(provider_body).into_response()
}

/// `GET /v1/usage?namespace=&pool=` — the consumption report for a pool.
async fn usage(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let ns = match params.get("namespace").filter(|s| !s.is_empty()) {
        Some(ns) => ns.clone(),
        None => return bad_request("namespace query parameter required"),
    };
    let want_pool = params.get("pool").filter(|s| !s.is_empty()).cloned();

    let pools: Api<ModelPool> = Api::namespaced(state.client.clone(), &ns);
    let (pool_name, spec) = match select_pool(&pools, want_pool.as_deref()).await {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(&no_pool_msg(&ns, want_pool.as_deref()), &ns),
        Err(e) => return internal(&format!("select ModelPool: {e}")),
    };
    let (used, requests) = match store::usage_report(&state.pool, &ns, &pool_name).await {
        Ok(v) => v,
        Err(e) => return internal(&format!("usage report: {e}")),
    };
    let budget = spec.budget.as_ref().map(|b| b.max_tokens);
    Json(usage_json(&ns, &pool_name, used, requests, budget)).into_response()
}

// --- identity -------------------------------------------------------------

/// Resolve the caller's `(namespace, agent)` for a request.
///
/// In **header mode** (`attest` off, the default) this is purely the
/// `X-Agent-Namespace`/`X-Agent-Name` headers.
///
/// In **attested mode** (`attest` on) the identity is derived from the
/// kernel-set source IP — resolved to the real pod via the kube API — and the
/// pod's namespace is authoritative. If the request also carries an
/// `X-Agent-Namespace` that disagrees, the attested namespace wins and the
/// disagreement is recorded as a spoof attempt. A source IP that resolves to no
/// pod is rejected (`403`) — in attested mode we never fall back to the header.
async fn resolve_identity(
    state: &AppState,
    peer_ip: IpAddr,
    headers: &HeaderMap,
) -> Result<(String, String), Response> {
    let header_ns = header_str(headers, H_NAMESPACE);
    if !state.attest {
        let ns = match header_ns {
            Some(ns) => ns,
            None => return Err(bad_request(&format!("{H_NAMESPACE} header required"))),
        };
        let agent = header_str(headers, H_AGENT).unwrap_or_else(|| "unknown".to_string());
        return Ok((ns, agent));
    }

    // Attested mode: resolve the source IP to a pod, classified for attestation —
    // cache first (direct identities only), then a kube lookup memoized on a short
    // TTL. A kube error is a 500 (we cannot safely attest).
    let source = match state.ip_cache.get(&peer_ip) {
        Some(id) => attest::SourcePod::Direct(id),
        None => match resolve_ip_to_source(&state.client, peer_ip, &state.control_plane_ns).await {
            Ok(src) => {
                // Cache ONLY a direct agent's identity. The forwarder
                // serves many agents from one IP and its caller varies per request
                // (the `X-Agent-Pod-Uid` header), so its IP must never be cached.
                if let attest::SourcePod::Direct(id) = &src {
                    state.ip_cache.put(peer_ip, id.clone());
                }
                src
            }
            Err(e) => return Err(internal(&format!("attest source IP {peer_ip}: {e}"))),
        },
    };

    // The trusted forwarder asserts the real caller's pod UID in
    // `X-Agent-Pod-Uid`. Resolve that UID to the real agent. For ANY other source
    // the header is IGNORED — only the forwarder is trusted to forward identity, so
    // a random pod cannot bill/route as another tenant by setting `X-Agent-Pod-Uid`.
    let is_forwarder = matches!(source, attest::SourcePod::Forwarder);
    let forwarded = if is_forwarder {
        match header_str(headers, H_POD_UID) {
            Some(uid) => match resolve_uid_to_identity(&state.client, &uid).await {
                Ok(id) => id,
                Err(e) => return Err(internal(&format!("attest forwarded uid {uid}: {e}"))),
            },
            None => None,
        }
    } else {
        None
    };

    // Pure policy: attest (direct) / forward (forwarder) / flag-spoof / reject.
    match attest::decide(source, forwarded, header_ns.as_deref()) {
        attest::Decision::Use { identity, spoofed } => {
            state.metrics.inc_identity_attested();
            if spoofed {
                state.metrics.inc_identity_spoof();
                tracing::warn!(
                    %peer_ip,
                    attested_ns = %identity.namespace,
                    header_ns = header_ns.as_deref().unwrap_or(""),
                    agent = %identity.agent,
                    "attest: X-Agent-Namespace disagrees with attested namespace (spoof attempt); using attested",
                );
            }
            Ok((identity.namespace, identity.agent))
        }
        attest::Decision::Forwarded { identity } => {
            state.metrics.inc_identity_forwarded();
            tracing::debug!(
                %peer_ip,
                ns = %identity.namespace,
                agent = %identity.agent,
                "attest: identity forwarded by the trusted node-agent (X-Agent-Pod-Uid)",
            );
            Ok((identity.namespace, identity.agent))
        }
        attest::Decision::Reject => {
            if is_forwarder {
                tracing::warn!(
                    %peer_ip,
                    "attest: node-agent forwarder asserted no resolvable caller (missing/unknown X-Agent-Pod-Uid); rejecting",
                );
            } else {
                tracing::warn!(%peer_ip, "attest: source IP resolves to no pod; rejecting");
            }
            Err(forbidden("cannot attest caller identity from source IP"))
        }
    }
}

// --- kube glue (needs a cluster to run, not to compile/test) ---------------

/// Resolve a source IP to its pod, classified for attestation. Lists pods
/// cluster-wide with a `status.podIP` field selector to narrow, then re-verifies
/// the match locally (the selector is advisory) and classifies the pod: a genuine
/// forwarder — a pod in `control_plane_ns` running the `agentctl-node-agent`
/// ServiceAccount (an anchor a tenant cannot forge) → [`attest::SourcePod::Forwarder`]
/// (trusted to forward another agent's identity); any other pod →
/// [`attest::SourcePod::Direct`] with its own namespace + `agentctl.dev/agent`
/// identity. No matching pod (or a pod with no namespace) →
/// [`attest::SourcePod::Unresolved`] (cannot attest). When `control_plane_ns` is
/// empty (`POD_NAMESPACE` unset) the forwarder anchor cannot be verified, so no
/// pod is classified as a forwarder (fail closed) — it is attested directly or not
/// at all.
async fn resolve_ip_to_source(
    client: &Client,
    ip: IpAddr,
    control_plane_ns: &str,
) -> Result<attest::SourcePod, String> {
    let pods: Api<Pod> = Api::all(client.clone());
    let ip_s = ip.to_string();
    let lp = ListParams::default().fields(&format!("status.podIP={ip_s}"));

    // COLD-START RACE: a source IP that reached us over TCP was assigned by the
    // CNI to a real pod — but the kubelet patches `status.podIP` onto the pod
    // AFTER the sandbox is up, so a freshly-started agent that dials on its very
    // first loop iteration can beat its own IP into our (watch-cache-backed)
    // list. "Resolves to no pod" is then a transient propagation lag, not a
    // spoof. Retry a few times over ~1.5s before concluding Unresolved; the
    // cost is paid only on the miss path (rare in steady state) and closes the
    // race so a cold agent's first inference is not a 403 → crash-loop.
    const RESOLVE_RETRIES: usize = 3;
    const RESOLVE_BACKOFF: Duration = Duration::from_millis(500);
    for attempt in 0..=RESOLVE_RETRIES {
        let list = pods.list(&lp).await.map_err(|e| e.to_string())?;
        if let Some(pod) = list.items.iter().find(|p| attest::pod_matches_ip(p, &ip_s)) {
            if attest::is_node_agent_pod(pod, control_plane_ns) {
                return Ok(attest::SourcePod::Forwarder);
            }
            return Ok(match attest::identity_from_pod(pod) {
                Some(id) => attest::SourcePod::Direct(id),
                None => attest::SourcePod::Unresolved,
            });
        }
        if attempt < RESOLVE_RETRIES {
            tokio::time::sleep(RESOLVE_BACKOFF).await;
        }
    }
    Ok(attest::SourcePod::Unresolved)
}

/// Resolve a forwarder-asserted pod UID to the real agent's attested identity.
/// Mirrors [`resolve_ip_to_source`] but matches on `metadata.uid` — which is not
/// a kube field selector, so we list pods cluster-wide and match locally — then
/// derives the identity from the matched pod. `Ok(None)` ⇒ no pod has that UID
/// (the forwarder asserted an unknown caller; reject).
async fn resolve_uid_to_identity(
    client: &Client,
    uid: &str,
) -> Result<Option<attest::Identity>, String> {
    let pods: Api<Pod> = Api::all(client.clone());
    let list = pods
        .list(&ListParams::default())
        .await
        .map_err(|e| e.to_string())?;
    Ok(list
        .items
        .iter()
        .find(|p| attest::pod_matches_uid(p, uid))
        .and_then(attest::identity_from_pod))
}

/// Select the `ModelPool` for a request: by name when `want` is given (404 if
/// absent), else the first pool in the namespace. `Ok(None)` ⇒ no matching pool.
async fn select_pool(
    api: &Api<ModelPool>,
    want: Option<&str>,
) -> Result<Option<(String, ModelPoolSpec)>, String> {
    if let Some(name) = want {
        return match api.get_opt(name).await.map_err(|e| e.to_string())? {
            Some(mp) => {
                let resolved = mp.metadata.name.clone().unwrap_or_else(|| name.to_string());
                Ok(Some((resolved, mp.spec)))
            }
            None => Ok(None),
        };
    }
    let list = api
        .list(&ListParams::default())
        .await
        .map_err(|e| e.to_string())?;
    match list.items.len() {
        0 => Ok(None),
        1 => {
            let mp = list.items.into_iter().next().unwrap();
            let name = mp.metadata.name.clone().unwrap_or_default();
            Ok(Some((name, mp.spec)))
        }
        // FAIL CLOSED on ambiguity: silently picking the first pool would bill an
        // arbitrary tenant's pool/budget. The agent must declare spec.model.pool (or
        // send X-Model-Pool) when more than one pool exists.
        n => Err(format!(
            "{n} ModelPools in the namespace and none selected — declare spec.model.pool on the Agent/AgentFleet (or send X-Model-Pool)"
        )),
    }
}

/// The `ModelPool` the attested caller's CR declares (`spec.model.pool`). Tries the
/// `Agent` CR and, on a 404, an `AgentFleet`'s template (fleet pods carry the fleet
/// name). `None` ⇒ the CR declares no pool (or neither exists) — the caller then
/// falls back to the sole namespace pool, or [`select_pool`] fails closed on
/// ambiguity. This is what lets an agent CHOOSE its intelligence source: the
/// operator renders no `X-Model-Pool` header, so without this the gateway used the
/// first pool in the namespace regardless of `spec.model.pool`.
async fn declared_pool(client: &Client, ns: &str, agent: &str) -> Option<String> {
    let agents: Api<Agent> = Api::namespaced(client.clone(), ns);
    match agents.get(agent).await {
        Ok(a) => return a.spec.model.and_then(|m| m.pool),
        Err(kube::Error::Api(ae)) if ae.code == 404 => { /* not an Agent → try Fleet */ }
        Err(_) => return None,
    }
    let fleets: Api<AgentFleet> = Api::namespaced(client.clone(), ns);
    fleets
        .get(agent)
        .await
        .ok()
        .and_then(|f| f.spec.template.model.and_then(|m| m.pool))
}

/// The per-fleet token cap (`AgentFleet.spec.budget.maxTokens`) for the
/// caller, when the attested identity names an `AgentFleet` that declares one. `None`
/// for a plain `Agent`, a fleet without a budget, or an absent CR — in which case
/// only the pool cap (if any) applies.
async fn fleet_budget(client: &Client, ns: &str, agent: &str) -> Option<i64> {
    let fleets: Api<AgentFleet> = Api::namespaced(client.clone(), ns);
    fleets
        .get_opt(agent)
        .await
        .ok()
        .flatten()
        .and_then(|f| f.spec.budget)
        .and_then(|b| b.max_tokens)
}

/// Read and UTF-8-decode the named key from a `Secret`. The typed kube client
/// already base64-decodes `data` into raw `ByteString` bytes, so this only maps
/// bytes → string (trimming trailing whitespace/newlines).
fn read_secret_key(secret: &Secret, key: &str) -> Result<String, String> {
    let bytes = secret
        .data
        .as_ref()
        .and_then(|d| d.get(key))
        .ok_or_else(|| format!("Secret has no key {key:?}"))?;
    let s = String::from_utf8(bytes.0.clone())
        .map_err(|e| format!("Secret key {key:?} is not valid UTF-8: {e}"))?;
    Ok(s.trim().to_string())
}

// --- pure helpers (unit-tested) --------------------------------------------

/// Inject `default_model` into the request body when it pins no (non-empty)
/// model. No-op when there is no default, the body is not a JSON object, or the
/// body already carries a non-empty `model`. The agent stays provider-neutral;
/// the pool supplies the default.
fn inject_model(body: &mut Value, default_model: Option<&str>) {
    let Some(model) = default_model else { return };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let has_model = obj
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !has_model {
        obj.insert("model".to_string(), json!(model));
    }
}

/// Build the `GET /v1/usage` response body. `budget`/`remaining` are JSON `null`
/// when the pool has no budget; `remaining` is floored at 0 (never negative).
fn usage_json(ns: &str, pool: &str, used: i64, requests: i64, budget: Option<i64>) -> Value {
    json!({
        "namespace": ns,
        "pool": pool,
        "usedTokens": used,
        "requests": requests,
        "budget": budget,
        "remaining": budget.map(|b| (b - used).max(0)),
    })
}

/// The "no ModelPool" error message — distinguishes a missing named pool from an
/// empty namespace.
fn no_pool_msg(ns: &str, want: Option<&str>) -> String {
    match want {
        Some(name) => format!("ModelPool {name} not found in {ns}"),
        None => format!("no ModelPool in namespace {ns}"),
    }
}

// --- small response helpers ------------------------------------------------

/// Read a header value as an owned `String`, treating empty as absent.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

fn not_found(msg: &str, ns: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": msg, "namespace": ns })),
    )
        .into_response()
}

fn internal(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg })),
    )
        .into_response()
}

fn bad_gateway(msg: &str) -> Response {
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": msg }))).into_response()
}

/// `403 Forbidden` — used when attested mode cannot derive a caller identity
/// from the source IP (the IP resolves to no pod). We never fall back to the
/// spoofable header in attested mode.
fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": msg }))).into_response()
}

/// Build the Postgres connection pool for the usage meter from `DATABASE_URL`
/// (e.g. `postgres://user:pw@host:5432/db?sslmode=disable`).
///
/// `sslmode=disable` (the default path) → [`tokio_postgres::NoTls`]: a plain
/// in-cluster hop, kept NetworkPolicy-scoped. `sslmode=require`/`prefer` (e.g.
/// bundled `postgres.tls.enabled` or an external managed DSN) → a rustls/ring
/// connector ([`db_tls::make_connector`]) that encrypts the hop without verifying
/// the cert. `sslmode=verify-full` (or `DB_TLS_VERIFY=full`) with a mounted CA
/// bundle → a CA-pinning connector ([`db_tls::make_verifying_connector`]) that
/// verifies the chain and server name. All paths stay pure-Rust (no C toolchain).
fn build_pool() -> Pool {
    let raw = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let (url, verify_full) = db_tls::resolve_tls(&raw);
    let cfg: tokio_postgres::Config = url.parse().expect("parse DATABASE_URL");
    let mgr = if cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
        deadpool_postgres::Manager::new(cfg, tokio_postgres::NoTls)
    } else if verify_full {
        let ca = db_tls::ca_file_path();
        match db_tls::make_verifying_connector(&ca) {
            Ok(connector) => {
                tracing::info!(ca = %ca.display(), "postgres TLS: verify-full (CA pinning)");
                deadpool_postgres::Manager::new(cfg, connector)
            }
            Err(err) => {
                tracing::warn!(
                    ca = %ca.display(),
                    error = %err,
                    "postgres TLS: verify-full requested but CA load failed; \
                     falling back to encrypt-without-verify"
                );
                deadpool_postgres::Manager::new(cfg, db_tls::make_connector())
            }
        }
    } else {
        deadpool_postgres::Manager::new(cfg, db_tls::make_connector())
    };
    Pool::builder(mgr)
        .max_size(8)
        .build()
        .expect("build postgres pool")
}

/// Extract a token count from a provider response's `usage` block, tolerant of the
/// common shapes: OpenAI `total_tokens`; OpenAI `prompt_tokens`+`completion_tokens`;
/// Anthropic `input_tokens`+`output_tokens`. Returns `None` only when there is no
/// recognizable usage at all (the caller then charges a conservative estimate — a
/// spend cap must never fail open at 0).
fn extract_token_count(body: &Value) -> Option<i64> {
    let usage = body.get("usage")?;
    if let Some(t) = usage.get("total_tokens").and_then(Value::as_i64) {
        return Some(t);
    }
    // Sum a request/response token pair; a present-but-partial pair still counts.
    let pair = |a: &str, b: &str| -> Option<i64> {
        let x = usage.get(a).and_then(Value::as_i64);
        let y = usage.get(b).and_then(Value::as_i64);
        match (x, y) {
            (None, None) => None,
            (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
        }
    };
    pair("input_tokens", "output_tokens") // Anthropic
        .or_else(|| pair("prompt_tokens", "completion_tokens")) // OpenAI (no total)
}

/// A conservative token estimate from the serialized response size (~4 chars/token),
/// floored at 1, used when a provider hides usage — so the budget still advances and
/// a usage-omitting provider cannot silently drain it.
fn estimate_tokens_from_size(body: &Value) -> i64 {
    let chars = serde_json::to_string(body).map(|s| s.len()).unwrap_or(0);
    ((chars / 4) as i64).max(1)
}

/// A conservative UPPER-BOUND estimate of what an infer REQUEST may cost, used to
/// reserve budget BEFORE the provider call (the race-free cap; see [`store::reserve`]).
/// Sums an input estimate from the request body size (~4 chars/token) with the
/// request's declared output cap — `max_tokens` / `max_output_tokens` /
/// `max_completion_tokens`, whichever is present and positive. With no declared cap
/// the configured `default_reserve` stands in, so an uncapped request still reserves
/// a meaningful amount. Floored at 1.
fn estimate_reservation(body: &Value, default_reserve: i64) -> i64 {
    let input_est = estimate_tokens_from_size(body);
    let output_cap = ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|k| body.get(*k).and_then(Value::as_i64))
        .filter(|n| *n > 0)
        .unwrap_or(default_reserve);
    input_est.saturating_add(output_cap).max(1)
}

/// Release a held budget reservation (if any) on an early-return / error path, so a
/// request that never reaches the provider frees the headroom it reserved. Best
/// effort: a failed release self-heals via the reservation TTL, so we only warn.
async fn release_reservation(state: &AppState, reservation: &Option<(i64, i64)>) {
    if let Some((id, _est)) = reservation {
        if let Err(e) = store::release_reservation(&state.pool, *id).await {
            tracing::warn!(reservation_id = id, error = %e, "release reservation failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_count_handles_provider_shapes() {
        // OpenAI total_tokens.
        assert_eq!(
            extract_token_count(&serde_json::json!({ "usage": { "total_tokens": 150 } })),
            Some(150)
        );
        // Anthropic input+output (the shape the old `total_tokens`-only path metered as 0).
        assert_eq!(
            extract_token_count(
                &serde_json::json!({ "usage": { "input_tokens": 40, "output_tokens": 60 } })
            ),
            Some(100)
        );
        // OpenAI prompt+completion without a total.
        assert_eq!(
            extract_token_count(
                &serde_json::json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 5 } })
            ),
            Some(15)
        );
        // No usage at all → None (caller charges an estimate, never 0).
        assert_eq!(
            extract_token_count(&serde_json::json!({ "choices": [] })),
            None
        );
    }

    #[test]
    fn estimate_tokens_never_zero() {
        assert!(estimate_tokens_from_size(&serde_json::json!({})) >= 1);
        assert!(estimate_tokens_from_size(&serde_json::json!({ "a": "x".repeat(400) })) >= 100);
    }

    #[test]
    fn reservation_uses_declared_output_cap_plus_input() {
        // A declared cap (max_tokens/max_output_tokens/max_completion_tokens) is a
        // real upper bound → reserve it plus the input-size estimate, NOT the default.
        for cap_key in ["max_tokens", "max_output_tokens", "max_completion_tokens"] {
            let body = json!({ cap_key: 500, "messages": [] });
            let est = estimate_reservation(&body, 8192);
            assert!(
                (500..8192).contains(&est),
                "{cap_key}: est {est} should be ~cap+input, not the default"
            );
        }
    }

    #[test]
    fn reservation_falls_back_to_default_without_cap() {
        // No declared cap → the default reservation stands in (plus input estimate),
        // so an uncapped request still reserves a meaningful amount.
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let est = estimate_reservation(&body, 8192);
        assert!(est > 8192, "no cap ⇒ default + input estimate: got {est}");
    }

    #[test]
    fn reservation_ignores_nonpositive_cap() {
        // A zero/negative cap is not a real bound → fall back to the default.
        let body = json!({ "max_tokens": 0 });
        assert!(estimate_reservation(&body, 8192) > 8192);
    }

    #[test]
    fn inject_model_fills_when_absent() {
        let mut body = json!({ "messages": [] });
        inject_model(&mut body, Some("claude-x"));
        assert_eq!(body["model"], "claude-x");
    }

    #[test]
    fn inject_model_fills_when_empty_string() {
        let mut body = json!({ "model": "", "messages": [] });
        inject_model(&mut body, Some("claude-x"));
        assert_eq!(body["model"], "claude-x");
    }

    #[test]
    fn inject_model_preserves_pinned_model() {
        let mut body = json!({ "model": "pinned", "messages": [] });
        inject_model(&mut body, Some("claude-x"));
        assert_eq!(body["model"], "pinned");
    }

    #[test]
    fn inject_model_noop_without_default() {
        let mut body = json!({ "messages": [] });
        inject_model(&mut body, None);
        assert_eq!(body.get("model"), None);
    }

    #[test]
    fn inject_model_noop_on_non_object() {
        let mut body = json!("not-an-object");
        inject_model(&mut body, Some("claude-x"));
        assert_eq!(body, json!("not-an-object"));
    }

    #[test]
    fn usage_json_with_budget_reports_remaining() {
        let v = usage_json("team-a", "default", 30, 3, Some(100));
        assert_eq!(v["namespace"], "team-a");
        assert_eq!(v["pool"], "default");
        assert_eq!(v["usedTokens"], 30);
        assert_eq!(v["requests"], 3);
        assert_eq!(v["budget"], 100);
        assert_eq!(v["remaining"], 70);
    }

    #[test]
    fn usage_json_remaining_floors_at_zero() {
        let v = usage_json("ns", "p", 150, 5, Some(100));
        assert_eq!(v["remaining"], 0);
    }

    #[test]
    fn usage_json_without_budget_is_null() {
        let v = usage_json("ns", "p", 42, 7, None);
        assert_eq!(v["budget"], Value::Null);
        assert_eq!(v["remaining"], Value::Null);
        assert_eq!(v["usedTokens"], 42);
        assert_eq!(v["requests"], 7);
    }

    #[test]
    fn no_pool_msg_distinguishes_named_from_empty() {
        assert_eq!(
            no_pool_msg("team-a", Some("gpt")),
            "ModelPool gpt not found in team-a"
        );
        assert_eq!(
            no_pool_msg("team-a", None),
            "no ModelPool in namespace team-a"
        );
    }

    #[test]
    fn read_secret_key_decodes_and_trims() {
        use k8s_openapi::ByteString;
        use std::collections::BTreeMap;
        let mut data = BTreeMap::new();
        data.insert("apiKey".to_string(), ByteString(b"sk-secret\n".to_vec()));
        let secret = Secret {
            data: Some(data),
            ..Default::default()
        };
        assert_eq!(read_secret_key(&secret, "apiKey").unwrap(), "sk-secret");
    }

    #[test]
    fn read_secret_key_missing_key_errors() {
        let secret = Secret::default();
        assert!(read_secret_key(&secret, "apiKey").is_err());
    }
}
