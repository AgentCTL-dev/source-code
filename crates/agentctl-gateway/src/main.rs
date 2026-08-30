// SPDX-License-Identifier: BUSL-1.1
//! agentctl A2A gateway — the public A2A HTTP/JSON-RPC surface.
//!
//! External A2A clients speak the spec slash-form over HTTP; the gateway:
//!   1. projects an **Agent Card** at
//!      `GET /agents/{ns}/{name}/.well-known/agent-card.json` from the agent's
//!      capabilities manifest (fetched directly from the agent's pod over mTLS), and
//!   2. bridges JSON-RPC calls at `POST /agents/{ns}/{name}` — translating the
//!      spec method (`message/send`, …) to the **reference** method
//!      (`a2a.SendMessage`, …) the agent dispatches, then forwarding to the
//!      agent's pod at its mTLS `/mcp`. The `message/stream` method takes the
//!      streaming path: the agent's `/mcp` SSE byte-stream is piped
//!      straight back to the client as `text/event-stream` (transparent pipe;
//!      the gateway never parses the SSE frames), and
//!   3. serves a mesh discovery registry at `GET /agents` — the union of `Agent`
//!      and `AgentFleet` CRs across all namespaces, each with its Agent Card URL.
//!
//! Routing ({ns,name}→pod IP) mirrors the apiserver's `forward_verb_to_ip`:
//! the gateway resolves the target to a Running pod IP and dials it directly at
//! its mTLS `/mcp`, presenting the control-plane client cert. Hand-rolled in Rust
//! (axum); agentctl is Rust-only and depends on the contract wire, never on a
//! specific agent.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use agent_api::{Agent, AgentFleet};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use deadpool_postgres::Pool;
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::{Api, Client};
use serde_json::{json, Value};

mod auth;
mod db_tls;
mod identity;
mod metrics;
mod na_client;
mod oidc;
mod signing;
mod store;
mod trusted_proxy;

#[derive(Clone)]
struct AppState {
    client: Client,
    pool: Pool,
    signer: Arc<signing::Signer>,
    /// mTLS client presenting the control-plane client cert (agentctl-client-tls).
    /// It mints the `Management` origin at an agent's `/mcp`, so the gateway is
    /// the only peer that may drive A2A on the agent (external callers are
    /// authenticated + authorized on the inbound side, then forwarded as
    /// Management). Built once. The `na` field name is historical; it dials the
    /// agent's `/mcp` directly.
    na: reqwest::Client,
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
    /// Per-agent OIDC/JWT verifier for native A2A authn/authz. Holds the
    /// per-issuer JWKS cache; built once.
    oidc: Arc<oidc::Verifier>,
    /// The coarse bearer-token gate, threaded in so the A2A RPC handler can apply
    /// it inline for agents WITHOUT per-agent OIDC (the gate middleware defers the
    /// POST RPC route — see [`auth::gate`]).
    auth: auth::Auth,
    /// Round-robin cursor for load-balancing a fleet endpoint across its worker
    /// replicas. Per-replica (each gateway replica has its own), which
    /// is fine for spreading load; strict global fairness is not required.
    round_robin: Arc<std::sync::atomic::AtomicUsize>,
    /// Identity-service wiring for the org route family (RFC 0029 §3):
    /// inbound bearer introspection + per-(user,agent) principal injection.
    /// Unconfigured ⇒ `/orgs/…` refuses 503 (never open, never operator).
    identity: identity::IdentityConfig,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set.
    agentctl_telemetry::init("agentctl-gateway");
    // ring crypto provider as the process default: no aws-lc-rs → no
    // C toolchain. Required so reqwest's rustls backend (federation/push) and the
    // mTLS client that dials agent pods both resolve a provider.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let client = Client::try_default().await.expect("in-cluster kube client");

    // The Agent Card signing key — required at startup.
    let signer = Arc::new(signing::Signer::from_env().expect("GATEWAY_SIGNING_SEED"));

    // The durable task store. Retry the schema — the DB pod may start
    // after us.
    let pool = build_pool();
    for attempt in 1..=30u32 {
        if let Err(e) = agentctl_metering::pg::ensure_schema(&pool).await {
            tracing::warn!(error = %e, "metering schema init failed (billing rows lost until PG recovers)");
        }
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
    // Cloned for the trusted-proxy mTLS middleware (the original moves into state).
    let mw_metrics = metrics.clone();
    // Optional bearer-token access gate (AGENTCTL_API_TOKEN). Unset → no-op; set
    // → enforced on the A2A surface, with /healthz /readyz /metrics AND the public
    // JWKS (/.well-known/jwks.json) exempt. The middleware short-circuits the
    // exempt paths, so it can wrap the whole router.
    let gate = auth::Auth::from_env(metrics.clone());

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // `/metrics` rides the plaintext :8080 (the chart's `http` port),
        // alongside /healthz — no new port; scraped scheme=http.
        .route("/metrics", get(serve_metrics))
        .route("/.well-known/jwks.json", get(jwks))
        .route("/agents", get(list_agents))
        .route(
            "/agents/{ns}/{name}/.well-known/agent-card.json",
            get(agent_card),
        )
        .route(
            "/fleets/{ns}/{name}/.well-known/agent-card.json",
            get(fleet_card),
        )
        .route("/agents/{ns}/{name}", post(a2a_rpc))
        // The org route family (RFC 0029 §2): the public, tenant-scoped
        // addresses. Resolution: Organization CR → managed namespace; authn:
        // identity-service introspection of the inbound bearer; the caller's
        // per-(user,agent) principal bearer is injected upstream so agentd
        // resolves `user:<subject>`, not operator. (Handle resolution — P2-7 —
        // will overlay name→spec.handle; today <name> is the CR name.)
        .route("/orgs/{org}/agents/{name}", post(org_a2a_rpc))
        .route(
            "/orgs/{org}/agents/{name}/.well-known/agent-card.json",
            get(org_agent_card),
        )
        .route("/orgs/{org}/fleets/{name}", post(org_fleet_rpc))
        // The caller's OWN supervisor (RFC 0027/0029): resolved by the
        // authenticated subject; auto-ensured on first touch when the org
        // policy allows.
        .route("/orgs/{org}/supervisor", post(org_supervisor_rpc))
        // Owner approval for destructive control verbs (P4-5): only the HUMAN
        // whose supervisor asked can approve — verified by THEIR bearer here,
        // a token the supervisor never holds.
        .route("/orgs/{org}/approvals/{nonce}", post(org_approve))
        .route(
            "/orgs/{org}/fleets/{name}/.well-known/agent-card.json",
            get(org_fleet_card),
        )
        // The fleet as a single addressable A2A endpoint: the same
        // RPC surface as an agent, but member selection routes to the coordinator
        // (front door) or load-balances across worker replicas.
        .route("/fleets/{ns}/{name}", post(a2a_fleet_rpc))
        // Workers-only tier (P6-3): the COORDINATOR's own `worker` peer dials
        // here — the front-door rule would loop it back onto itself.
        .route("/fleets/{ns}/{name}/workers", post(a2a_fleet_workers_rpc))
        .layer(axum::middleware::from_fn_with_state(
            gate.clone(),
            auth::gate,
        ))
        .with_state(AppState {
            client,
            pool,
            signer,
            na: na_client::node_agent_client(),
            metrics,
            // Per-agent OIDC verifier (public-CA JWKS HTTP client, ring-backed).
            oidc: Arc::new(oidc::Verifier::new()),
            // Same coarse gate the middleware uses; the RPC handler falls back to
            // it for non-OIDC agents.
            auth: gate,
            round_robin: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            identity: identity::IdentityConfig::from_env(),
        });

    // TRUSTED-PROXY mode (front-proxy trust over mTLS). OFF by default — when off
    // this whole block is skipped and only the plaintext listener path below runs.
    let tp = Arc::new(trusted_proxy::Config::from_env());

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));

    // Graceful shutdown: on SIGTERM/SIGINT, stop accepting and drain in-flight
    // requests (hyper's `with_graceful_shutdown`). In-flight SSE streams
    // (`message/stream`) are short-lived — our agents complete synchronously, so
    // the agent emits its terminal frame and the passthrough body ends,
    // letting the connection close cleanly within the drain.
    if !tp.enabled {
        tracing::info!(%addr, "agentctl gateway serving the A2A HTTP surface");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .expect("serve");
        return;
    }

    // Enabled: serve a SECOND mTLS listener (front-proxy trust) concurrently with
    // the existing plaintext one.
    let tls_addr: SocketAddr = tp
        .tls_addr
        .parse()
        .unwrap_or_else(|e| panic!("parse AGENTCTL_GATEWAY_TLS_ADDR {}: {e}", tp.tls_addr));
    let server_config = trusted_proxy::build_tls_config(&tp.tls_dir, &tp.ca_path)
        .expect("build trusted-proxy mTLS server config");
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
    let acceptor = trusted_proxy::PeerCertAcceptor::new(rustls_config);

    // The mTLS router enforces the allow-list + extracts the asserted identity
    // (a verified TRUSTED caller); the plaintext router STRIPS the asserted
    // identity headers (anti-spoof). Both share the same routes + access gate.
    let mtls_ctx = trusted_proxy::MtlsCtx {
        cfg: tp.clone(),
        metrics: mw_metrics,
    };
    let mtls_app = app
        .clone()
        .layer(axum::middleware::from_fn_with_state(
            mtls_ctx,
            trusted_proxy::mtls_decision,
        ))
        .into_make_service();
    let plaintext_app = app.layer(axum::middleware::from_fn_with_state(
        tp.clone(),
        trusted_proxy::strip_plaintext,
    ));

    tracing::info!(
        %addr, %tls_addr, ca = %tp.ca_path.display(), allowed = ?tp.allowed_names,
        "trusted-proxy ENABLED: plaintext :8080 (identity headers stripped) + mTLS front-proxy listener"
    );
    // The mTLS listener runs as a background task; the plaintext listener keeps the
    // existing graceful-shutdown behaviour in the foreground. On SIGTERM the
    // foreground drains and returns, and the process exits (dropping the task).
    tokio::spawn(async move {
        axum_server::bind(tls_addr)
            .acceptor(acceptor)
            .serve(mtls_app)
            .await
            .expect("serve trusted-proxy mTLS");
    });
    axum::serve(listener, plaintext_app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve");
}

// --- graceful shutdown -----------------------------------------------------

/// Wait for SIGTERM/SIGINT, then resolve so hyper drains in-flight requests
/// (including any in-flight SSE passthroughs).
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
    tracing::info!("shutting down: draining in-flight requests and SSE streams");
}

/// `GET /metrics` — the Prometheus text exposition format.
async fn serve_metrics(
    State(state): State<AppState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

// --- handlers --------------------------------------------------------------

/// Publish the JWKS that verifies signed Agent Cards.
async fn jwks(State(state): State<AppState>) -> Json<Value> {
    Json(state.signer.jwks())
}

/// Project a **signed** A2A Agent Card from the agent's capabilities manifest,
/// fetched directly from the agent's pod over its mTLS `/mcp`. `kind` (when `Some`) is
/// attached as `x-agentctl-kind` — used to mark fleet cards. This is the shared
/// path behind both [`agent_card`] and [`fleet_card`] (a fleet's pods are
/// labelled the same way an agent's are, so [`resolve`] works for both).
async fn build_signed_card(
    state: &AppState,
    ns: &str,
    name: &str,
    base_url: &str,
    kind: Option<&str>,
) -> Result<Value, String> {
    // A fleet card is projected from the COORDINATOR (the front door) when the fleet
    // declares one, else from a worker replica. An agent projects from
    // its own pods.
    let manifest_source = if kind == Some("AgentFleet") {
        let fleets: Api<AgentFleet> = Api::namespaced(state.client.clone(), ns);
        match fleets.get_opt(name).await {
            Ok(Some(f)) if f.spec.coordinator.is_some() => coordinator_agent_name(name),
            _ => name.to_string(),
        }
    } else {
        name.to_string()
    };
    // Best-effort: read the RICH manifest from a live pod; fall back to a STATIC card
    // projected from the CR identity when no replica is Running, so the card is
    // servable at `replicas:0` (a claim fleet idles at zero — its card, discovery, and
    // task-acceptance must NOT depend on a live pod). We never 502 a
    // card merely because the fleet is scaled down.
    let manifest = match resolve(&state.client, ns, &manifest_source).await {
        Ok(pod_ip) => fetch_capabilities(state, &pod_ip).await.ok(),
        Err(e) => {
            tracing::debug!(%ns, %name, error = %e, "no live replica; projecting a static card");
            None
        }
    };
    let mut card = project_card(manifest.as_ref(), ns, name, base_url, kind);
    state.signer.sign_card(&mut card);
    Ok(card)
}

/// Read the `agent://capabilities` manifest from a live pod over its mTLS `/mcp`
/// (contract 1.0; our client cert mints Management). `resources/read` returns
/// `contents[0].text` = the manifest JSON.
async fn fetch_capabilities(state: &AppState, pod_ip: &str) -> Result<Value, String> {
    let url = format!("https://{pod_ip}:8443/");
    let read = json!({
        "jsonrpc": "2.0", "id": 1, "method": "resources/read",
        "params": { "uri": "agent://capabilities" }
    });
    let resp = state
        .na
        .post(&url)
        .json(&read)
        .send()
        .await
        .map_err(|e| format!("agent POST {url}: {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("decode capabilities: {e}"))?;
    if let Some(err) = resp.get("error") {
        return Err(format!("agent resources/read error: {err}"));
    }
    let text = resp
        .pointer("/result/contents/0/text")
        .and_then(Value::as_str)
        .ok_or("capabilities resource has no contents[0].text")?;
    serde_json::from_str(text).map_err(|e| format!("parse capabilities manifest: {e}"))
}

/// Project the signed A2A Agent Card for an `Agent` CR.
async fn agent_card(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    state.metrics.inc_card();
    let base_url = base_url(&headers);
    match build_signed_card(&state, &ns, &name, &base_url, None).await {
        Ok(card) => (StatusCode::OK, Json(card)),
        Err(e) => {
            state.metrics.inc_upstream_error();
            tracing::warn!(%ns, agent = %name, error = %e, "card build failed");
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e })))
        }
    }
}

/// Project the signed A2A Agent Card for an `AgentFleet` CR (marked
/// `x-agentctl-kind: AgentFleet`).
async fn fleet_card(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    state.metrics.inc_card();
    let base_url = base_url(&headers);
    match build_signed_card(&state, &ns, &name, &base_url, Some("AgentFleet")).await {
        Ok(card) => (StatusCode::OK, Json(card)),
        Err(e) => {
            state.metrics.inc_upstream_error();
            tracing::warn!(%ns, fleet = %name, error = %e, "fleet card build failed");
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e })))
        }
    }
}

/// `GET /orgs/{org}/agents/{name}/.well-known/agent-card.json` — the signed
/// card at the tenant-scoped address (the card's URL field advertises the org
/// route via `base_url` + the request path).
async fn org_agent_card(
    State(state): State<AppState>,
    Path((org, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    org_card_common(state, org, name, None, headers).await
}

async fn org_fleet_card(
    State(state): State<AppState>,
    Path((org, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    org_card_common(state, org, name, Some("AgentFleet"), headers).await
}

async fn org_card_common(
    state: AppState,
    org: String,
    name: String,
    kind: Option<&str>,
    headers: HeaderMap,
) -> Response {
    state.metrics.inc_card();
    let ns = match resolve_org(&state, &org).await {
        Ok((_, ns)) => ns,
        Err(resp) => return *resp,
    };
    let name = match resolve_handle(&state, &ns, &name, kind == Some("AgentFleet")).await {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let base_url = base_url(&headers);
    match build_signed_card(&state, &ns, &name, &base_url, kind).await {
        Ok(card) => (StatusCode::OK, Json(card)).into_response(),
        Err(e) => {
            state.metrics.inc_upstream_error();
            tracing::warn!(%org, target = %name, error = %e, "org card build failed");
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response()
        }
    }
}

/// Bridge a spec-form A2A JSON-RPC request to the agent's reference method.
///
/// Non-streaming methods (`message/send`, `tasks/get`, …) forward a single
/// JSON-RPC call and return the agent's response verbatim. `message/stream`
/// takes the streaming path: it forwards to the agent's mTLS `/mcp` and
/// pipes the resulting SSE byte-stream straight back to the client untouched.
#[tracing::instrument(skip_all, fields(ns = %ns, agent = %name))]
async fn a2a_rpc(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    trusted_proxy::TrustedDecision(decision): trusted_proxy::TrustedDecision,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    handle_a2a(state, ns, name, false, decision, headers, req, None).await
}

/// `POST /fleets/{ns}/{name}` — the fleet as a single addressable A2A endpoint.
/// Identical RPC surface + access enforcement to a single agent
/// (`enforce_access` already reads a fleet's `spec.template.access`); only member
/// selection differs (coordinator front door / worker load-balancing).
#[tracing::instrument(name = "gateway.fleet_rpc", skip_all, fields(ns = %ns, fleet = %name))]
async fn a2a_fleet_rpc(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    trusted_proxy::TrustedDecision(decision): trusted_proxy::TrustedDecision,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    handle_a2a(state, ns, name, true, decision, headers, req, None).await
}

/// `POST /fleets/{ns}/{name}/workers` — the dispatcher's DOWNSTREAM tier:
/// selection skips the coordinator front door (task affinity still binds
/// continuations to their owner; fresh sends round-robin the workers). The
/// coordinator's rendered `worker` peer points here, so `a2a.delegate
/// peer=worker` fans out instead of looping back through its own front door.
async fn a2a_fleet_workers_rpc(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    trusted_proxy::TrustedDecision(decision): trusted_proxy::TrustedDecision,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    handle_a2a_routed(
        state,
        ns,
        name,
        FleetTier::WorkersOnly,
        decision,
        headers,
        req,
        None,
    )
    .await
}

/// Fleet member-selection tier for [`handle_a2a_routed`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum FleetTier {
    /// Not a fleet route.
    Agent,
    /// The fleet front door (coordinator when declared, else round-robin).
    Fleet,
    /// The dispatcher's downstream tier: never the coordinator.
    WorkersOnly,
}

/// `POST /orgs/{org}/agents/{name}` — the tenant-scoped route: resolve the
/// org's managed namespace, authenticate the inbound bearer at the identity
/// service, then run the shared handler with the authenticated org user (whose
/// per-agent principal bearer is injected upstream).
#[tracing::instrument(skip_all, fields(org = %org, agent = %name))]
async fn org_a2a_rpc(
    State(state): State<AppState>,
    Path((org, name)): Path<(String, String)>,
    trusted_proxy::TrustedDecision(decision): trusted_proxy::TrustedDecision,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    org_rpc_common(state, org, name, false, decision, headers, req).await
}

/// `POST /orgs/{org}/fleets/{name}` — [`org_a2a_rpc`] for a fleet endpoint.
#[tracing::instrument(skip_all, fields(org = %org, fleet = %name))]
async fn org_fleet_rpc(
    State(state): State<AppState>,
    Path((org, name)): Path<(String, String)>,
    trusted_proxy::TrustedDecision(decision): trusted_proxy::TrustedDecision,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    org_rpc_common(state, org, name, true, decision, headers, req).await
}

/// `POST /orgs/{org}/supervisor` — route to the CALLER's supervisor agent.
/// The Supervisor CR is looked up by the authenticated subject and
/// auto-created on first touch (org policy `supervisors: auto`, the
/// default); while its agent is still coming up the caller gets a retryable
/// "provisioning" answer, never a hang.
#[tracing::instrument(skip_all, fields(org = %org))]
async fn org_supervisor_rpc(
    State(state): State<AppState>,
    Path(org): Path<String>,
    trusted_proxy::TrustedDecision(decision): trusted_proxy::TrustedDecision,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let (org_cr, ns) = match resolve_org(&state, &org).await {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    if !state.identity.ready() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(rpc_error(
                id,
                -32603,
                "org routes require the identity service (identity.service.enabled)",
            )),
        )
            .into_response();
    }
    let Some(token) = bearer_token(&headers).map(str::to_string) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(rpc_error(
                id,
                -32001,
                "a bearer token is required on org routes",
            )),
        )
            .into_response();
    };
    let user = match identity::introspect(&state.na, &state.identity, &token).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            state.metrics.inc_oidc_deny();
            return (
                StatusCode::UNAUTHORIZED,
                Json(rpc_error(id, -32001, "bearer token is not active")),
            )
                .into_response();
        }
        Err(e) => {
            state.metrics.inc_upstream_error();
            tracing::warn!(%org, error = %e, "identity introspection failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(rpc_error(id, -32603, "identity introspection failed")),
            )
                .into_response();
        }
    };
    state.metrics.inc_oidc_allow();

    let name = match ensure_supervisor(&state, &org_cr, &ns, &user).await {
        Ok(SupervisorTarget::Ready(name)) => name,
        Ok(SupervisorTarget::Provisioning) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(rpc_error(
                    id,
                    -32603,
                    "your supervisor is being provisioned — retry in a few seconds",
                )),
            )
                .into_response();
        }
        Err(resp) => return *resp,
    };

    // @mentions become the typed orchestration envelope (P4-7); plain prose
    // stays conversational.
    let req = mentionize_request(&req).unwrap_or(req);
    // Metering (P7-4): a routed supervisor SendMessage is one CONVERSATION
    // unit (the seat-adjacent metric), owner-attributed.
    if req.get("method").and_then(Value::as_str) == Some("SendMessage") {
        let pool = state.pool.clone();
        let ev = agentctl_metering::Event::new(
            org.clone(),
            ns.clone(),
            name.clone(),
            agentctl_metering::KIND_SUPERVISOR_CONVERSATIONS,
            1,
            "conversations",
        )
        .user(user.subject.clone());
        tokio::spawn(async move {
            let _ = agentctl_metering::pg::record(&pool, &ev).await;
        });
    }
    handle_a2a(state, ns, name, false, decision, headers, req, Some(user)).await
}

/// `POST /orgs/{org}/approvals/{nonce}` — the OWNER says yes to a pending
/// destructive request (P4-5). The bearer must introspect to the SAME subject
/// that asked (recorded in the pending marker); the gateway then writes the
/// approved marker the control server executes on. 404 for an unknown or
/// expired nonce; 403 for anyone but the requester.
#[tracing::instrument(skip_all, fields(org = %org))]
async fn org_approve(
    State(state): State<AppState>,
    Path((org, nonce)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    use agent_api::approval::{
        approval_marker, parse_approval, APPROVAL_TTL_SECS, APPROVED_DELETE_ANNOTATION,
        PENDING_DELETE_ANNOTATION,
    };
    let (_org_cr, ns) = match resolve_org(&state, &org).await {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    if !state.identity.ready() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "approvals require the identity service" })),
        )
            .into_response();
    }
    let Some(token) = bearer_token(&headers).map(str::to_string) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "a bearer token is required" })),
        )
            .into_response();
    };
    let user = match identity::introspect(&state.na, &state.identity, &token).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "bearer token is not active" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::warn!(%org, error = %e, "identity introspection failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "identity introspection failed" })),
            )
                .into_response();
        }
    };

    // Find the target carrying this pending nonce in the org's namespace.
    let agents: Api<agent_api::v1alpha2::Agent> = Api::namespaced(state.client.clone(), &ns);
    let list = match agents.list(&Default::default()).await {
        Ok(l) => l,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("list agents: {e}") })),
            )
                .into_response();
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let pending = list.items.into_iter().find_map(|a| {
        let marker = a
            .metadata
            .annotations
            .as_ref()?
            .get(PENDING_DELETE_ANNOTATION)?
            .clone();
        let (n, requester, exp) = parse_approval(&marker)?;
        (n == nonce && exp > now).then_some((a, requester))
    });
    let Some((target, requester)) = pending else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no live pending approval with that code" })),
        )
            .into_response();
    };
    if requester != user.subject {
        // Addressed to the OWNER alone — not even another admin.
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this approval is addressed to the requesting owner" })),
        )
            .into_response();
    }
    let name = target.metadata.name.clone().unwrap_or_default();
    let patch = json!({ "metadata": { "annotations": {
        APPROVED_DELETE_ANNOTATION: approval_marker(&nonce, &user.subject, now + APPROVAL_TTL_SECS),
        PENDING_DELETE_ANNOTATION: null,
    } } });
    if let Err(e) = agents
        .patch(&name, &Default::default(), &kube::api::Patch::Merge(&patch))
        .await
    {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("record approval: {e}") })),
        )
            .into_response();
    }
    tracing::info!(%org, agent = %name, approved_by = %user.subject, %nonce, "destructive request approved by owner");
    Json(json!({ "approved": name, "by": user.subject })).into_response()
}

enum SupervisorTarget {
    Ready(String),
    Provisioning,
}

/// Extract @handle mentions from prose (P4-7). Lowercase DNS-ish handles
/// after a bare `@`; `@skill:` stays the composer's namespace (upstream
/// hardcodes that prefix), so a match immediately followed by `:` is not a
/// mention. Deduped in order, capped at 8 (the workflow fan-out band).
fn extract_mentions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_lowercase()
                    || bytes[end].is_ascii_digit()
                    || bytes[end] == b'-')
            {
                end += 1;
            }
            let followed_by_colon = end < bytes.len() && bytes[end] == b':';
            if end > start && !followed_by_colon {
                let handle = &text[start..end];
                if !handle.starts_with('-')
                    && !handle.ends_with('-')
                    && !out.iter().any(|h| h == handle)
                    && out.len() < 8
                {
                    out.push(handle.to_string());
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Rewrite a SendMessage whose text carries @mentions into the TYPED
/// `mention` envelope the supervisor's orchestration workflow fires on
/// (`{"data": {"agentd": {"op": "mention", …}}}` — the same DataPart shape
/// agentd's own delegate client sends). Plain prose passes through untouched
/// and rides the ordinary agent loop. `hops` starts the supervisor's OWN
/// ceiling counter — agentd's depth limits never cross pod boundaries.
fn mentionize_request(req: &Value) -> Option<Value> {
    let parts = req.pointer("/params/message/parts")?.as_array()?;
    let text: String = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let mentions = extract_mentions(&text);
    if mentions.is_empty() {
        return None;
    }
    let mut out = req.clone();
    out["params"]["message"]["parts"] = json!([{
        "data": { "agentd": {
            "op": "mention",
            "text": text,
            "mentions": mentions,
            "hops": 2,
        } }
    }]);
    Some(out)
}

/// Seconds-precision RFC3339 UTC (`%Y-%m-%dT%H:%M:%SZ`) — the grammar the
/// operator's idle-park parser reads back.
fn unix_to_rfc3339(mut secs: u64) -> String {
    // Civil-from-days (Howard Hinnant), the inverse of the operator's parse.
    let days = (secs / 86400) as i64;
    secs %= 86400;
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Parse the stamp above back to unix seconds (`None` on foreign formats).
fn httpdate_unix(s: &str) -> Option<u64> {
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let (y, m, d): (i64, i64, i64) = (
        dp.next()?.parse().ok()?,
        dp.next()?.parse().ok()?,
        dp.next()?.parse().ok()?,
    );
    let time = time.split('.').next()?;
    let mut tp = time.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        tp.next()?.parse().ok()?,
        tp.next()?.parse().ok()?,
        tp.next()?.parse().ok()?,
    );
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

/// A DNS-safe supervisor CR name for a subject (`mock:alice` → `sup-mock-alice`).
fn supervisor_name_for(subject: &str) -> String {
    let slug: String = subject
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let mut name = format!("sup-{slug}");
    name.truncate(63);
    name.trim_end_matches('-').to_string()
}

/// Find (or auto-create) the caller's Supervisor and return its agent name
/// once the rendered agent exists.
async fn ensure_supervisor(
    state: &AppState,
    org_cr: &agent_api::Organization,
    ns: &str,
    user: &identity::OrgUser,
) -> Result<SupervisorTarget, Box<Response>> {
    use agent_api::v1alpha2::{Supervisor, SupervisorSpec};
    let subject = user.subject.as_str();
    let mode = org_cr
        .spec
        .supervisors
        .unwrap_or(agent_api::org::SupervisorsMode::Auto);
    let sups: Api<Supervisor> = Api::namespaced(state.client.clone(), ns);
    let existing = sups.list(&Default::default()).await.map_err(|e| {
        Box::new(
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("list supervisors: {e}") })),
            )
                .into_response(),
        )
    })?;
    if let Some(sup) = existing.items.into_iter().find(|s| s.spec.user == subject) {
        let sup_name = sup.metadata.name.clone().unwrap_or_default();
        // Stamp the owner's identity-resolved groups (P4-2): the control MCP
        // evaluates org accessPolicies against THIS — the supervisor cannot
        // assert its owner's groups, and grants refresh on every conversation.
        let stamped = sup
            .status
            .as_ref()
            .map(|st| st.owner_groups.clone())
            .unwrap_or_default();
        // Activity stamp (P7-6): every touch refreshes lastConversation —
        // the idle-park clock AND the wake signal (a parked supervisor's
        // next touch re-stamps, the operator unparks, the caller rides the
        // ordinary 503-provisioning window meanwhile). Rate-limited to one
        // write per 30s so chatty sessions don't hammer the status.
        let now_stamp = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let stale = sup
                .status
                .as_ref()
                .and_then(|st| st.last_conversation.as_deref())
                .and_then(httpdate_unix)
                .map(|t| now.saturating_sub(t) > 30)
                .unwrap_or(true);
            stale.then(|| unix_to_rfc3339(now))
        };
        if stamped != user.groups || now_stamp.is_some() {
            let mut status = json!({ "ownerGroups": user.groups });
            if let Some(ts) = &now_stamp {
                status["lastConversation"] = json!(ts);
            }
            let _ = sups
                .patch_status(
                    &sup_name,
                    &Default::default(),
                    &kube::api::Patch::Merge(&json!({ "status": status })),
                )
                .await
                .map_err(
                    |e| tracing::warn!(error = %e, "supervisor stamp failed (stale grants/clock until next call)"),
                );
        }
        let agent_ref = sup
            .status
            .as_ref()
            .and_then(|st| st.agent_ref.clone())
            .unwrap_or(sup_name);
        // The rendered agent must have a Running pod before we forward.
        return match resolve(&state.client, ns, &agent_ref).await {
            Ok(_) => Ok(SupervisorTarget::Ready(agent_ref)),
            Err(_) => Ok(SupervisorTarget::Provisioning),
        };
    }
    match mode {
        agent_api::org::SupervisorsMode::Disabled => Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "supervisors are disabled for this organization" })),
            )
                .into_response(),
        )),
        agent_api::org::SupervisorsMode::Manual => Err(Box::new(
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no supervisor exists for you (org policy: manual) — ask an admin" })),
            )
                .into_response(),
        )),
        agent_api::org::SupervisorsMode::Auto => {
            let name = supervisor_name_for(subject);
            let mut sup = Supervisor::new(
                &name,
                SupervisorSpec {
                    user: subject.to_string(),
                    paused: false,
                    instruction_override: None,
                    budget_override: None,
                },
            );
            sup.metadata.namespace = Some(ns.to_string());
            let _ = sups
                .create(&Default::default(), &sup)
                .await
                .map_err(|e| tracing::warn!(error = %e, "supervisor auto-create failed (racing?)"));
            Ok(SupervisorTarget::Provisioning)
        }
    }
}

async fn org_rpc_common(
    state: AppState,
    org: String,
    name: String,
    is_fleet: bool,
    decision: trusted_proxy::Decision,
    headers: HeaderMap,
    req: Value,
) -> Response {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let (org_cr, ns) = match resolve_org(&state, &org).await {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    // Org routes REQUIRE identity-service authn — no coarse-token or open
    // fallback, and an unreachable identity service refuses (never fails open).
    if !state.identity.ready() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(rpc_error(
                id,
                -32603,
                "org routes require the identity service (identity.service.enabled)",
            )),
        )
            .into_response();
    }
    let Some(token) = bearer_token(&headers).map(str::to_string) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(rpc_error(
                id,
                -32001,
                "a bearer token is required on org routes",
            )),
        )
            .into_response();
    };
    let user = match identity::introspect(&state.na, &state.identity, &token).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            state.metrics.inc_oidc_deny();
            return (
                StatusCode::UNAUTHORIZED,
                Json(rpc_error(id, -32001, "bearer token is not active")),
            )
                .into_response();
        }
        Err(e) => {
            state.metrics.inc_upstream_error();
            tracing::warn!(%org, error = %e, "identity introspection failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(rpc_error(id, -32603, "identity introspection failed")),
            )
                .into_response();
        }
    };
    state.metrics.inc_oidc_allow();

    // Handle → CR name (P2-7). Resolved only for AUTHENTICATED callers, so
    // an anonymous probe cannot enumerate handles.
    let name = match resolve_handle(&state, &ns, &name, is_fleet).await {
        Ok(n) => n,
        Err(resp) => return *resp,
    };

    // Org access policy (P1-8): roles over label selectors, resolved from the
    // caller's groups, BEFORE any principal/bearer work — "engineering
    // operates engineering, marketing is refused, admins see all".
    let labels = target_labels(&state, &ns, &name).await;
    let role = required_role(
        req.get("method")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if let Err(resp) = enforce_org_policy(&org_cr, &user, &labels, role) {
        state.metrics.inc_oidc_deny();
        return *resp;
    }

    handle_a2a(
        state,
        ns,
        name,
        is_fleet,
        decision,
        headers,
        req,
        Some(user),
    )
    .await
}

/// Resolve an org name to its CR + primary managed namespace
/// (`status.namespaces[0]` when reconciled, else the `org-<name>`
/// convention). An absent CR is a 404 (org routes never guess).
async fn resolve_org(
    state: &AppState,
    org: &str,
) -> Result<(agent_api::Organization, String), Box<Response>> {
    let orgs: Api<agent_api::Organization> = Api::all(state.client.clone());
    match orgs.get_opt(org).await {
        Ok(Some(o)) => {
            let ns = o
                .status
                .as_ref()
                .and_then(|s| s.namespaces.first().cloned())
                .unwrap_or_else(|| agent_api::org::org_namespace(org));
            Ok((o, ns))
        }
        Ok(None) => Err(Box::new(
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown organization {org:?}") })),
            )
                .into_response(),
        )),
        Err(e) => Err(Box::new(
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("read Organization {org}: {e}") })),
            )
                .into_response(),
        )),
    }
}

/// Resolve an org-route path segment to the CR name (P2-7): the segment is
/// the EFFECTIVE handle (`spec.handle`, defaulting to the CR name), so an
/// exact CR-name hit wins only when that CR declares no different handle.
/// Unknown handles are 404s that say "handle", not "agent".
async fn resolve_handle(
    state: &AppState,
    ns: &str,
    token: &str,
    is_fleet: bool,
) -> Result<String, Box<Response>> {
    let found: Option<String> = if is_fleet {
        let fleets: Api<AgentFleet> = Api::namespaced(state.client.clone(), ns);
        match fleets.list(&Default::default()).await {
            Ok(list) => list.items.into_iter().find_map(|f| {
                let n = f.metadata.name.clone().unwrap_or_default();
                (agent_api::effective_handle(f.spec.handle.as_deref(), &n) == token).then_some(n)
            }),
            Err(e) => return Err(Box::new(handle_lookup_error(ns, &e))),
        }
    } else {
        let agents: Api<Agent> = Api::namespaced(state.client.clone(), ns);
        match agents.list(&Default::default()).await {
            Ok(list) => list.items.into_iter().find_map(|a| {
                let n = a.metadata.name.clone().unwrap_or_default();
                (agent_api::effective_handle(a.spec.handle.as_deref(), &n) == token).then_some(n)
            }),
            Err(e) => return Err(Box::new(handle_lookup_error(ns, &e))),
        }
    };
    found.ok_or_else(|| {
        Box::new(
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("no agent with handle {token:?} in this organization") })),
            )
                .into_response(),
        )
    })
}

fn handle_lookup_error(ns: &str, e: &kube::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": format!("resolve handle in {ns}: {e}") })),
    )
        .into_response()
}

/// The org-policy role a gateway-forwarded A2A method needs (RFC 0029 §3):
/// conversing and cancelling mutate (operator); reads/subscriptions are
/// viewer-grade. Unknown methods require operator (fail toward the stronger
/// role, never the weaker).
fn required_role(spec_method: &str) -> agent_api::org::Role {
    use agent_api::org::Role;
    match spec_method {
        "tasks/get" | "tasks/list" | "GetTask" | "ListTasks" | "SubscribeToTask"
        | "SubscribeToEvents" | "tasks/resubscribe" => Role::Viewer,
        _ => Role::Operator,
    }
}

/// Enforce the Organization's accessPolicies for `user` over the target's
/// labels (RFC 0033 §2.1; the pure engine is `agent_api::org::access`). An
/// org with NO policies declared imposes no role scoping (org-membership
/// authn only — policies restrict once stated). Refusals name the role, not
/// the caller's groups.
fn enforce_org_policy(
    org_cr: &agent_api::Organization,
    user: &identity::OrgUser,
    labels: &std::collections::BTreeMap<String, String>,
    role: agent_api::org::Role,
) -> Result<(), Box<Response>> {
    use agent_api::org::access;
    if org_cr.spec.access_policies.is_empty() {
        return Ok(());
    }
    let facts = access::PrincipalFacts {
        groups: user.groups.clone(),
        claims: Default::default(),
    };
    let grants = access::resolve(&facts, &org_cr.spec.access_policies);
    if access::permits(&grants, role, labels) {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": format!(
                    "access policy refuses {role:?}-level access to this agent for your groups"
                ) })),
            )
                .into_response(),
        ))
    }
}

/// The target CR's labels (Agent first, AgentFleet fallback) — what
/// accessPolicies selectors match on. A missing CR yields empty labels (the
/// pod resolve later 404s properly).
async fn target_labels(
    state: &AppState,
    ns: &str,
    name: &str,
) -> std::collections::BTreeMap<String, String> {
    let agents: Api<Agent> = Api::namespaced(state.client.clone(), ns);
    if let Ok(Some(a)) = agents.get_opt(name).await {
        return a.metadata.labels.unwrap_or_default();
    }
    let fleets: Api<AgentFleet> = Api::namespaced(state.client.clone(), ns);
    if let Ok(Some(f)) = fleets.get_opt(name).await {
        return f.metadata.labels.unwrap_or_default();
    }
    Default::default()
}

/// The shared A2A RPC handler for both an agent (`is_fleet=false`) and a fleet
/// (`is_fleet=true`). The only fleet-specific behaviour is member selection at the
/// forward step (see [`select_member`]); auth, the gateway-owned verbs, and task
/// persistence are keyed by `name` (the agent OR fleet name) identically.
/// `org_user` (the org route family) carries the identity-authenticated caller:
/// their per-(user,agent) principal bearer is fetched and injected upstream so
/// the agent resolves them as `user:<subject>` instead of the operator.
#[allow(clippy::too_many_arguments)]
async fn handle_a2a(
    state: AppState,
    ns: String,
    name: String,
    is_fleet: bool,
    decision: trusted_proxy::Decision,
    headers: HeaderMap,
    req: Value,
    org_user: Option<identity::OrgUser>,
) -> Response {
    let tier = if is_fleet {
        FleetTier::Fleet
    } else {
        FleetTier::Agent
    };
    handle_a2a_routed(state, ns, name, tier, decision, headers, req, org_user).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_a2a_routed(
    state: AppState,
    ns: String,
    name: String,
    tier: FleetTier,
    decision: trusted_proxy::Decision,
    headers: HeaderMap,
    mut req: Value,
    org_user: Option<identity::OrgUser>,
) -> Response {
    let is_fleet = tier != FleetTier::Agent;
    // Metering (P7-4): one durable usage event per handled RPC, at the
    // traffic chokepoint — fire-and-forget, never on the request path.
    {
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let pool = state.pool.clone();
        let mut ev = agentctl_metering::Event::new(
            // Managed namespaces are `org-<name>` by convention; unmanaged
            // ones bill under the empty org (still namespace-attributed).
            ns.strip_prefix("org-").unwrap_or("").to_string(),
            ns.clone(),
            name.clone(),
            agentctl_metering::KIND_A2A_REQUESTS,
            1,
            "requests",
        )
        .dim("method", method);
        if let Some(u) = &org_user {
            ev = ev.user(u.subject.clone());
        }
        tokio::spawn(async move {
            if let Err(e) = agentctl_metering::pg::record(&pool, &ev).await {
                tracing::debug!(error = %e, "metering record failed");
            }
        });
    }
    state.metrics.inc_rpc();
    let id = req.get("id").cloned().unwrap_or(Value::Null);

    // Per-agent access enforcement, BEFORE any method handling. Precedence:
    //   (0) an org-route caller — ALREADY authenticated at the identity
    //       service (org_rpc_common); the legacy rungs don't apply (their
    //       coarse-gate rung would demand the api token from a caller whose
    //       bearer is an IdP token). Identity is forwarded for attribution.
    //   (1) a verified trusted-proxy identity (mTLS listener) — trusted, enforce
    //       any requiredClaims, forward identity;
    //   (2) per-agent OIDC (spec.access.oidc) — validate the JWT;
    //   (3) the coarse bearer gate.
    // On success with identity forwarding, the verified caller identity is sent to
    // the agent as X-Auth-* headers.
    let (identity, forward_identity) = match &org_user {
        Some(user) => (
            Some(oidc::Identity {
                sub: user.subject.clone(),
                email: user.email.clone(),
                groups: user.groups.clone(),
            }),
            true,
        ),
        None => match enforce_access(&state, &ns, &name, &headers, &decision).await {
            Ok(v) => v,
            Err(resp) => return resp,
        },
    };

    // Org-route principal injection (RFC 0028 §6 / 0029 §3): the caller's
    // per-(user,agent) bearer — minted by identity, projected by the operator
    // into `<name>-principals` — is what makes agentd resolve `user:<subject>`
    // rather than the operator the control-plane client cert would mint. An
    // agent that names principals answers ONLY listed subjects (403 for the
    // rest); an agent naming none keeps today's posture (no bearer injected).
    let upstream_bearer = match &org_user {
        Some(user) => match principal_bearer_for(&state, &ns, &name, &user.subject).await {
            Ok(b) => b,
            Err(resp) => return *resp,
        },
        None => None,
    };
    let spec = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // `tasks/list` is served by the GATEWAY from the durable store (the agent
    // serves only live tasks); it is not forwarded.
    if spec == "tasks/list" {
        return match store::list(&state.pool, &ns, &name).await {
            Ok(rows) => {
                let tasks: Vec<Value> = rows.iter().map(store::task_json).collect();
                Json(json!({ "jsonrpc": "2.0", "id": id, "result": { "tasks": tasks } }))
                    .into_response()
            }
            Err(e) => Json(rpc_error(id, -32603, &format!("store list: {e}"))).into_response(),
        };
    }

    // Push-notification config is gateway-owned: the gateway holds the durable task
    // store and performs SSRF-guarded webhook delivery, so it stores the webhook and
    // delivers. Not forwarded.
    if let Some(op) = spec.strip_prefix("tasks/pushNotificationConfig/") {
        return push_config(&state.pool, &ns, &name, op, &req, id).await;
    }

    // `tasks/resubscribe` is served by the GATEWAY: a one-shot SSE resume of the
    // stored task. Live resume of an in-flight stream is not supported — our
    // agents complete synchronously, so the stored task is already terminal and a
    // single replayed frame is the whole stream.
    if spec == "tasks/resubscribe" {
        let tid = req
            .pointer("/params/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return match store::get(&state.pool, &ns, &name, &tid).await {
            Ok(Some(row)) => (
                [(header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {}\n\n", store::task_json(&row)),
            )
                .into_response(),
            Ok(None) => {
                Json(rpc_error(id, -32001, &format!("task not found: {tid}"))).into_response()
            }
            Err(e) => Json(rpc_error(id, -32603, &format!("store get: {e}"))).into_response(),
        };
    }

    // Translate spec → reference; unknown method ⇒ -32601 (METHOD_NOT_FOUND).
    let streaming = spec == "message/stream" || spec == "SendStreamingMessage";
    let reference = match translate_method(&spec) {
        Some(m) => m,
        None => {
            return Json(rpc_error(id, -32601, &format!("method not found: {spec}")))
                .into_response()
        }
    };

    // `tasks/get`: serve from the durable store first (survives the agent),
    // falling back to a live call.
    if spec == "tasks/get" {
        if let Some(tid) = req.pointer("/params/id").and_then(Value::as_str) {
            if let Ok(Some(row)) = store::get(&state.pool, &ns, &name, tid).await {
                return Json(
                    json!({ "jsonrpc": "2.0", "id": id, "result": store::task_json(&row) }),
                )
                .into_response();
            }
        }
    }

    // The input text to persist alongside a message/send result.
    let input = req
        .pointer("/params/message/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Rewrite the request method in place to the reference spelling.
    if let Some(obj) = req.as_object_mut() {
        obj.insert("method".to_string(), json!(reference));
    }

    let pod_ip = match select_member(&state, &ns, &name, tier, &spec, &req).await {
        Ok(ip) => ip,
        Err(e) => {
            state.metrics.inc_upstream_error();
            tracing::warn!(%ns, target = %name, is_fleet, error = %e, "rpc resolve failed");
            return Json(rpc_error(id, -32603, &e)).into_response();
        }
    };
    // The agent serves A2A on its own mTLS `/mcp`; the gateway's client cert
    // mints Management. Non-streaming AND streaming ride the same endpoint (an
    // SSE reply is negotiated by the streaming method + Accept), so a single URL
    // serves both.
    let url = format!("https://{pod_ip}:8443/");

    if streaming {
        // Streaming path: forward and pipe the raw `text/event-stream` body
        // straight through — do NOT parse the SSE frames (transparent byte
        // pipe). v2.1 frames carry no `final`; terminality is the terminal task
        // state + stream close, which the client observes directly.
        state.metrics.inc_stream();
        let forwarded = forward_request(
            &state,
            &url,
            &req,
            &identity,
            forward_identity,
            upstream_bearer.as_deref(),
        );
        return match forwarded.send().await {
            Ok(resp) => (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(resp.bytes_stream()),
            )
                .into_response(),
            Err(e) => {
                state.metrics.inc_upstream_error();
                Json(rpc_error(id, -32603, &format!("agent POST {url}: {e}"))).into_response()
            }
        };
    }

    let forwarded = forward_request(
        &state,
        &url,
        &req,
        &identity,
        forward_identity,
        upstream_bearer.as_deref(),
    );
    let body = match forwarded.send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(b) => b,
            Err(e) => {
                state.metrics.inc_upstream_error();
                return Json(rpc_error(id, -32603, &format!("decode agent: {e}"))).into_response();
            }
        },
        Err(e) => {
            state.metrics.inc_upstream_error();
            return Json(rpc_error(id, -32603, &format!("agent POST {url}: {e}"))).into_response();
        }
    };

    // Persist task state into the durable store. v2.1 `SendMessage` returns the
    // `SendMessageResponse` envelope `{"task": <Task>}`; `GetTask`/`CancelTask`
    // return a bare Task. `task_of` normalizes both so persistence + push read
    // the Task regardless of shape.
    if spec == "message/send" {
        if let Some(task) = body.get("result").and_then(task_of) {
            let tid = task.get("id").and_then(Value::as_str).unwrap_or("task-1");
            let st = task
                .pointer("/status/state")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            let artifact = task
                .pointer("/artifacts/0/parts/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Record which member served the task (owner_pod) so a later live op
            // (cancel/stream/get on a non-terminal task) routes back to it — task
            // affinity across fleet members. Harmless for a single agent.
            if let Err(e) = store::upsert(
                &state.pool,
                &ns,
                &name,
                tid,
                st,
                &input,
                artifact,
                Some(&pod_ip),
            )
            .await
            {
                tracing::warn!(error = %e, "store upsert failed");
            } else {
                state.metrics.inc_task();
            }
            // Deliver a push notification if a webhook is registered.
            if let Ok(Some((url, token))) = store::push_get(&state.pool, &ns, &name, tid).await {
                deliver_push(url, token, task.clone());
            }
        }
    } else if spec == "tasks/cancel" {
        if let Some(tid) = body
            .get("result")
            .and_then(task_of)
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
        {
            let _ = store::set_state(&state.pool, &ns, &name, tid, "canceled").await;
        }
    }

    Json(body).into_response()
}

/// Mesh discovery registry: the union of `Agent` and `AgentFleet` CRs across all
/// namespaces, each carrying its projected Agent Card URL. Contract-shaped — the
/// rows describe CR identity + mode, never any agent's internals.
async fn list_agents(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let base_url = base_url(&headers);
    let mut rows: Vec<Value> = Vec::new();

    let agents: Api<Agent> = Api::all(state.client.clone());
    match agents.list(&ListParams::default()).await {
        Ok(list) => {
            for a in list {
                let ns = a.metadata.namespace.unwrap_or_default();
                let name = a.metadata.name.unwrap_or_default();
                // `spec.mode` is a required enum; project its lowercase wire form.
                let mode = serde_json::to_value(a.spec.mode)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned));
                let mut row = registry_row("Agent", &ns, &name, mode.as_deref(), &base_url);
                row["origin"] = json!("local");
                rows.push(row);
            }
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("list agents: {e}") })),
            )
                .into_response()
        }
    }

    let fleets: Api<AgentFleet> = Api::all(state.client.clone());
    match fleets.list(&ListParams::default()).await {
        Ok(list) => {
            for f in list {
                let ns = f.metadata.namespace.unwrap_or_default();
                let name = f.metadata.name.unwrap_or_default();
                // `AgentFleet` has no top-level `spec.mode` (mode lives on the
                // per-replica template) ⇒ null.
                let mut row = registry_row("AgentFleet", &ns, &name, None, &base_url);
                row["origin"] = json!("local");
                rows.push(row);
            }
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("list fleets: {e}") })),
            )
                .into_response()
        }
    }

    // `?local=…` ⇒ return ONLY local rows. This is the endpoint peers call when
    // federating, so it must NOT fan out again (no infinite recursion).
    if params.contains_key("local") {
        return Json(json!({ "agents": rows })).into_response();
    }

    // Federation: merge each peer gateway's local rows, tagging the peer origin.
    // A peer fetch error is logged and skipped — never fail the whole registry.
    let peers = federation_peers(&std::env::var("FEDERATION_PEERS").unwrap_or_default());
    for peer in peers {
        let url = format!("{peer}/agents?local=1");
        match reqwest::Client::new().get(&url).send().await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(body) => {
                    if let Some(arr) = body.get("agents").and_then(Value::as_array) {
                        for r in arr {
                            let mut r = r.clone();
                            r["origin"] = json!(peer);
                            rows.push(r);
                        }
                    }
                }
                Err(e) => tracing::warn!(%peer, error = %e, "decode peer agents; skipping"),
            },
            Err(e) => tracing::warn!(%peer, error = %e, "fetch peer agents; skipping"),
        }
    }

    Json(json!({ "agents": rows })).into_response()
}

/// Serve the A2A `tasks/pushNotificationConfig/*` methods (set/get/list/delete)
/// from the gateway-owned store. The gateway holds the durable task store and
/// performs webhook delivery, so it owns the webhook config — these are never
/// forwarded.
async fn push_config(
    pool: &Pool,
    ns: &str,
    name: &str,
    op: &str,
    req: &Value,
    id: Value,
) -> Response {
    let task_id = req
        .pointer("/params/taskId")
        .or_else(|| req.pointer("/params/id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url_param = req
        .pointer("/params/pushNotificationConfig/url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let token_param = req
        .pointer("/params/pushNotificationConfig/token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let outcome: Result<Value, String> = match op {
        "set" if task_id.is_empty() || url_param.is_empty() => {
            Err("set requires params.taskId and params.pushNotificationConfig.url".into())
        }
        // Reject an SSRF/exfil webhook at registration time (re-validated on delivery).
        "set" => match webhook::validate(&url_param).await {
            Err(e) => Err(e),
            Ok(()) => store::push_set(pool, ns, name, &task_id, &url_param, &token_param)
                .await
                .map(|_| {
                    json!({ "taskId": task_id, "pushNotificationConfig": push_cfg(&url_param, &token_param) })
                }),
        },
        "get" => store::push_get(pool, ns, name, &task_id)
            .await
            .map(|u| match u {
                Some((url, token)) => {
                    json!({ "taskId": task_id, "pushNotificationConfig": push_cfg(&url, &token) })
                }
                None => Value::Null,
            }),
        "list" => store::push_list(pool, ns, name).await.map(|rows| {
            Value::Array(
                rows.into_iter()
                    .map(|(t, u)| json!({ "taskId": t, "pushNotificationConfig": { "url": u } }))
                    .collect(),
            )
        }),
        "delete" => store::push_delete(pool, ns, name, &task_id)
            .await
            .map(|_| Value::Null),
        other => Err(format!("unknown pushNotificationConfig op: {other}")),
    };

    match outcome {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err(e) => Json(rpc_error(id, -32602, &e)).into_response(),
    }
}

/// SSRF/exfil guard for push webhooks. The gateway POSTs a client-supplied
/// URL from INSIDE the cluster, so an unvalidated webhook is a server-side request
/// forgery + data-exfiltration primitive: an attacker registers a URL pointing at
/// cloud metadata (169.254.169.254), an in-cluster Service, or an RFC1918 host and the
/// gateway both probes it and ships the task payload there. Enforce https-only, and
/// that EVERY resolved address is public — validated on `set` AND again at delivery
/// (re-resolve + pin the connection to the validated IP, defeating DNS rebinding).
mod webhook {
    use std::net::{IpAddr, SocketAddr};

    /// Parse an `https://host[:port]` webhook, returning `(host, port)`. Rejects any
    /// non-https scheme.
    pub fn parse_https(url: &str) -> Result<(String, u16), String> {
        let rest = url
            .strip_prefix("https://")
            .ok_or_else(|| "webhook url must be https://".to_string())?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        // Drop any userinfo (`user:pass@host`).
        let hostport = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = if let Some(after) = hostport.strip_prefix('[') {
            // Bracketed IPv6 literal: [::1]:443
            let (h6, tail) = after.split_once(']').ok_or("malformed IPv6 host")?;
            let port = tail
                .strip_prefix(':')
                .map(|p| p.parse::<u16>().map_err(|_| "bad port".to_string()))
                .transpose()?
                .unwrap_or(443);
            (h6.to_string(), port)
        } else if let Some((h, p)) = hostport.rsplit_once(':') {
            (h.to_string(), p.parse::<u16>().map_err(|_| "bad port")?)
        } else {
            (hostport.to_string(), 443u16)
        };
        if host.is_empty() {
            return Err("webhook url has no host".into());
        }
        Ok((host, port))
    }

    /// A globally-routable address only: rejects loopback, private (RFC1918 + CGNAT
    /// 100.64/10), link-local (incl. 169.254.169.254 metadata), unspecified,
    /// broadcast, documentation, multicast, and the IPv6 ULA/link-local ranges.
    pub fn is_public(ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                let cgnat = o[0] == 100 && (o[1] & 0xc0) == 64; // 100.64.0.0/10
                !(v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    || v4.is_documentation()
                    || v4.is_multicast()
                    || cgnat)
            }
            IpAddr::V6(v6) => {
                if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                    return false;
                }
                if let Some(v4) = v6.to_ipv4_mapped() {
                    return is_public(&IpAddr::V4(v4));
                }
                let seg0 = v6.segments()[0];
                let ula = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
                let link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
                !(ula || link_local)
            }
        }
    }

    /// Resolve `host:port` and return a validated public `SocketAddr` to pin the
    /// connection to. Rejects if ANY resolved address is non-public.
    pub async fn resolve_public(host: &str, port: u16) -> Result<SocketAddr, String> {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("resolve webhook host {host}: {e}"))?
            .collect();
        if addrs.is_empty() {
            return Err(format!("webhook host {host} did not resolve"));
        }
        for a in &addrs {
            if !is_public(&a.ip()) {
                return Err(format!(
                    "webhook host {host} resolves to non-public address {} (SSRF blocked)",
                    a.ip()
                ));
            }
        }
        Ok(addrs[0])
    }

    /// Full validation used on `set`: scheme + all-resolved-addresses-public.
    pub async fn validate(url: &str) -> Result<(), String> {
        let (host, port) = parse_https(url)?;
        resolve_public(&host, port).await.map(|_| ())
    }
}

/// Fire-and-forget delivery of a task to a registered push webhook.
/// Retries up to 3 attempts (200ms backoff) until a 2xx; a non-empty `token` is
/// sent as `Authorization: Bearer <token>`. The webhook is RE-validated + the
/// connection PINNED to a validated public IP at delivery time (anti-DNS-rebinding).
fn deliver_push(url: String, token: String, task: Value) {
    tokio::spawn(async move {
        let (host, port) = match webhook::parse_https(&url) {
            Ok(hp) => hp,
            Err(e) => {
                tracing::warn!(%url, error = %e, "push webhook rejected (scheme)");
                return;
            }
        };
        let pinned = match webhook::resolve_public(&host, port).await {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!(%url, error = %e, "push webhook rejected at delivery (SSRF guard)");
                return;
            }
        };
        // Pin DNS to the validated address so a rebind between resolve and connect
        // cannot redirect us to an internal host.
        let client = match reqwest::Client::builder()
            .resolve(&host, pinned)
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%url, error = %e, "build push client");
                return;
            }
        };
        let mut last = String::from("no attempt");
        for attempt in 1..=3u32 {
            let mut rb = client.post(&url).json(&task);
            if !token.is_empty() {
                rb = rb.bearer_auth(&token);
            }
            match rb.send().await {
                Ok(r) if r.status().is_success() => {
                    let status = r.status().as_u16();
                    tracing::info!(%url, status, attempt, "push delivered");
                    return;
                }
                Ok(r) => last = format!("status {}", r.status().as_u16()),
                Err(e) => last = e.to_string(),
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        tracing::warn!(%url, error = %last, "push delivery failed after 3 attempts");
    });
}

// --- pure helpers (unit-tested) --------------------------------------------

/// Translate an A2A spec slash-form method to the **bare PascalCase** method the
/// agent serves (the A2A spec's JSON-RPC binding; agentd v2.1). agentd still accepts
/// the legacy `a2a.`-prefixed spellings, but bare is the conformant wire, so we
/// emit it. `None` ⇒ unsupported (→ JSON-RPC -32601).
fn translate_method(spec: &str) -> Option<&'static str> {
    match spec {
        // The public slash binding, translated to the wire dialect…
        "message/send" => Some("SendMessage"),
        "message/stream" => Some("SendStreamingMessage"),
        "tasks/get" => Some("GetTask"),
        "tasks/cancel" => Some("CancelTask"),
        // …and the native PascalCase wire dialect passed through as-is
        // (ACC 2: the gateway is a PEP over A2A, not a dialect gate — a
        // conformant A2A client speaks these directly).
        "SendMessage" => Some("SendMessage"),
        "SendStreamingMessage" => Some("SendStreamingMessage"),
        "GetTask" => Some("GetTask"),
        "CancelTask" => Some("CancelTask"),
        "ListTasks" => Some("ListTasks"),
        _ => None,
    }
}

/// Normalize an A2A method `result` to its Task object. v2.1 `SendMessage`
/// returns the `SendMessageResponse` oneof `{"task": <Task>}`; `GetTask` /
/// `CancelTask` return a bare `<Task>`. A Task is identified by carrying an
/// `id`, so a `result.task` is unwrapped and a bare Task is returned as-is.
fn task_of(result: &Value) -> Option<&Value> {
    if let Some(task) = result.get("task") {
        return Some(task);
    }
    result.get("id").map(|_| result)
}

/// Build the `pushNotificationConfig` object echoed back to clients: always the
/// `url`, plus `token` only when one is set (don't leak an empty token field).
fn push_cfg(url: &str, token: &str) -> Value {
    let mut cfg = json!({ "url": url });
    if !token.is_empty() {
        cfg["token"] = json!(token);
    }
    cfg
}

/// Parse the comma-separated `FEDERATION_PEERS` env value into clean gateway
/// base URLs (trimmed; empties dropped). Pure — unit-tested.
fn federation_peers(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One mesh-registry row for a discovered CR (`Agent` / `AgentFleet`): identity,
/// the projected Agent Card URL, and the optional run mode (`None` ⇒ JSON null).
fn registry_row(kind: &str, ns: &str, name: &str, mode: Option<&str>, base_url: &str) -> Value {
    // A fleet's card + RPC live under /fleets/...; an agent's under /agents/... —
    // discovery must point consumers at the surface matching the kind, or the
    // x-agentctl-kind fleet marker is stripped by the /agents route.
    let seg = if kind == "AgentFleet" {
        "fleets"
    } else {
        "agents"
    };
    json!({
        "kind": kind,
        "namespace": ns,
        "name": name,
        "cardUrl": format!("{base_url}/{seg}/{ns}/{name}/.well-known/agent-card.json"),
        "mode": mode,
    })
}

/// Project a minimal A2A Agent Card. With a live capabilities `manifest` the card
/// carries the real version + advertised streaming; WITHOUT one (a fleet idling at
/// `replicas:0`) it projects a valid STATIC card from the CR identity — the card
/// must be servable at rest. `kind` selects the endpoint path
/// (`/fleets/...` for a fleet, `/agents/...` otherwise) and is echoed as
/// `x-agentctl-kind` so a consumer routes follow-up RPC to the right surface.
fn project_card(
    manifest: Option<&Value>,
    ns: &str,
    name: &str,
    base_url: &str,
    kind: Option<&str>,
) -> Value {
    let version = manifest
        .and_then(|m| m.get("agent_version"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    // Advertise streaming from the live manifest when present; at rest the gateway
    // still proxies message/stream, so default TRUE — never under-advertise a
    // capability the endpoint actually offers.
    let streaming = manifest
        .and_then(|m| m.pointer("/surfaces/a2a/streaming"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let seg = if kind == Some("AgentFleet") {
        "fleets"
    } else {
        "agents"
    };
    let mut card = json!({
        "protocolVersion": "1.0",
        "name": format!("{ns}/{name}"),
        "url": format!("{base_url}/{seg}/{ns}/{name}"),
        "version": version,
        "capabilities": { "streaming": streaming },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": []
    });
    if let Some(k) = kind {
        card["x-agentctl-kind"] = json!(k);
    }
    card
}

/// A JSON-RPC 2.0 error envelope, preserving the request id.
fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The externally reachable base URL, from the request `Host` header.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080");
    format!("http://{host}")
}

/// Build the Postgres connection pool for the durable task store from
/// `DATABASE_URL` (e.g. `postgres://user:pw@host:5432/db?sslmode=disable`).
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

// --- per-agent access enforcement (OIDC) -----------------------------------

/// Enforce the per-agent access policy for an inbound A2A RPC, BEFORE method
/// handling. Returns `(identity, forward_identity)` on success — `identity` is
/// `Some` for a trusted-proxy or OIDC caller (so the caller can forward it). On any
/// failure it returns the terminal [`Response`] to send (401 authN / 403 authZ /
/// 502 lookup).
///
/// Precedence:
///   1. a verified trusted-proxy identity (`decision == Trusted`, mTLS listener):
///      authN is satisfied; if the agent declares `spec.access.oidc.requiredClaims`
///      they are enforced against the asserted identity (403 on miss); the identity
///      is forwarded to the agent.
///   2. `spec.access.oidc` set: a bearer JWT is required + validated for THIS agent.
///   3. otherwise the coarse bearer gate the middleware enforces is applied inline.
// The Err IS the terminal HTTP response (the axum idiom); boxing it would
// churn every `?`/`return Err` site for a cold path taken once per request.
#[allow(clippy::result_large_err)]
async fn enforce_access(
    state: &AppState,
    ns: &str,
    name: &str,
    headers: &HeaderMap,
    decision: &trusted_proxy::Decision,
) -> Result<(Option<oidc::Identity>, bool), Response> {
    let access = match read_access(&state.client, ns, name).await {
        Ok(a) => a,
        Err(e) => {
            // A hard error reading the CR (not a clean NotFound) → fail closed.
            state.metrics.inc_upstream_error();
            tracing::warn!(%ns, agent = %name, error = %e, "read access policy failed");
            return Err((StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response());
        }
    };

    // (1) Verified trusted-proxy identity (mTLS listener). The front proxy already
    // performed edge authN; we only apply authZ (requiredClaims) and forward the
    // asserted identity to the agent.
    if let trusted_proxy::Decision::Trusted(identity) = decision {
        if let Some(rules) = access
            .as_ref()
            .and_then(|a| a.oidc.as_ref())
            .and_then(|o| o.required_claims.as_deref())
        {
            let claims = trusted_proxy::identity_claims(identity);
            if oidc::enforce_claims(&claims, Some(rules)).is_err() {
                state.metrics.inc_trusted_proxy_rejected();
                tracing::warn!(%ns, agent = %name, sub = %identity.sub, "trusted-proxy authZ denied: requiredClaims unsatisfied");
                return Err(StatusCode::FORBIDDEN.into_response());
            }
        }
        state.metrics.inc_trusted_proxy_accepted();
        return Ok((Some(identity.clone()), true));
    }

    let Some(oidc_cfg) = access.as_ref().and_then(|a| a.oidc.as_ref()) else {
        // No per-agent OIDC → fall back to the coarse bearer gate.
        if state.auth.authorize(headers) {
            return Ok((None, false));
        }
        state.metrics.inc_auth_rejected();
        return Err(StatusCode::UNAUTHORIZED.into_response());
    };

    // OIDC agent: require + validate a bearer JWT scoped to THIS agent.
    let Some(token) = bearer_token(headers) else {
        state.metrics.inc_oidc_deny();
        return Err(StatusCode::UNAUTHORIZED.into_response());
    };
    match state.oidc.verify(oidc_cfg, token).await {
        Ok(identity) => {
            state.metrics.inc_oidc_allow();
            Ok((Some(identity), oidc_cfg.forward_identity.unwrap_or(false)))
        }
        // No token detail leaks to the client (body is the bare status); the
        // reason is logged server-side only.
        Err(oidc::AuthError::Unauthorized(reason)) => {
            state.metrics.inc_oidc_deny();
            tracing::warn!(%ns, agent = %name, reason = %reason, "oidc authN denied");
            Err(StatusCode::UNAUTHORIZED.into_response())
        }
        Err(oidc::AuthError::Forbidden(reason)) => {
            state.metrics.inc_oidc_deny();
            tracing::warn!(%ns, agent = %name, reason = %reason, "oidc authZ denied");
            Err(StatusCode::FORBIDDEN.into_response())
        }
    }
}

/// Read `spec.access` for an `Agent`, falling back to an `AgentFleet`'s
/// `spec.template.access`. A clean 404 on both kinds ⇒ `Ok(None)` (no policy; the
/// later [`resolve`] surfaces "no running pod"); a transport/permission error ⇒
/// `Err` so the caller fails closed.
async fn read_access(
    client: &Client,
    ns: &str,
    name: &str,
) -> Result<Option<agent_api::Access>, String> {
    let agents: Api<Agent> = Api::namespaced(client.clone(), ns);
    match agents.get_opt(name).await {
        Ok(Some(a)) => return Ok(a.spec.access),
        Ok(None) => {}
        Err(e) => return Err(format!("get Agent {ns}/{name}: {e}")),
    }
    let fleets: Api<AgentFleet> = Api::namespaced(client.clone(), ns);
    match fleets.get_opt(name).await {
        Ok(Some(f)) => Ok(f.spec.template.access),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("get AgentFleet {ns}/{name}: {e}")),
    }
}

/// Extract `<JWT>` from an `Authorization: Bearer <JWT>` header (non-empty).
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
}

/// Build the forwarded request to the agent's pod, injecting the verified caller
/// identity as `X-Auth-*` headers when `forward_identity` is enabled for an OIDC
/// agent.
fn forward_request(
    state: &AppState,
    url: &str,
    req: &Value,
    identity: &Option<oidc::Identity>,
    forward_identity: bool,
    upstream_bearer: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut rb = state.na.post(url).json(req);
    // The per-(user,agent) principal bearer: agentd's principal rules are
    // first-match-wins with the user bearer rules listed before the
    // control-plane operator san-rule, so this header — not the client cert —
    // decides the caller's principal.
    if let Some(b) = upstream_bearer {
        rb = rb.bearer_auth(b);
    }
    match (forward_identity, identity) {
        (true, Some(id)) => id.inject(rb),
        _ => rb,
    }
}

/// Fetch the caller's per-agent principal bearer from the projected
/// `<name>-principals` Secret. Outcomes: agent names the subject → its bearer;
/// agent names principals but NOT this subject → 403 (the addressed gate);
/// agent names none → `None` (no injection, today's posture). The Secret is
/// operator-owned; a listed subject whose key is missing is a provisioning
/// race → 503 (retryable), never silent operator promotion.
async fn principal_bearer_for(
    state: &AppState,
    ns: &str,
    name: &str,
    subject: &str,
) -> Result<Option<String>, Box<Response>> {
    let declared = match read_access(&state.client, ns, name).await {
        Ok(a) => a.map(|a| a.principals).unwrap_or_default(),
        Err(e) => {
            state.metrics.inc_upstream_error();
            return Err(Box::new(
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("read access policy: {e}") })),
                )
                    .into_response(),
            ));
        }
    };
    if declared.is_empty() {
        return Ok(None);
    }
    if !declared.iter().any(|s| s == subject) {
        return Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": format!("{subject:?} is not a named principal on this agent") })),
            )
                .into_response(),
        ));
    }
    let secrets: Api<k8s_openapi::api::core::v1::Secret> =
        Api::namespaced(state.client.clone(), ns);
    let key = agent_config::principal_secret_key(subject);
    let secret_name = format!("{name}-principals");
    let found = secrets
        .get_opt(&secret_name)
        .await
        .map_err(|e| {
            state.metrics.inc_upstream_error();
            Box::new(
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("read {secret_name}: {e}") })),
                )
                    .into_response(),
            )
        })?
        .and_then(|s| s.data)
        .and_then(|d| d.get(&key).map(|v| v.0.clone()));
    match found {
        Some(raw) => String::from_utf8(raw).map(Some).map_err(|_| {
            Box::new(
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "projected principal bearer is not UTF-8" })),
                )
                    .into_response(),
            )
        }),
        None => Err(Box::new(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": format!(
                    "principal for {subject:?} is declared but not yet projected (key {key} in {secret_name}); retry shortly"
                ) })),
            )
                .into_response(),
        )),
    }
}

// --- routing (kube; needs a cluster to run, not to compile/test) -----------

/// Resolve `{ns,name}` → the agent's **Running pod IP** (contract 1.0). The agent
/// serves its A2A surface mTLS-gated on its own `:8443/` (ACC 2: the A2A
/// listener root — the served-MCP `/mcp` path no longer exists); the
/// gateway holds the control-plane client cert that mints the `Management` origin
/// those methods require, so it reaches the pod directly.
/// (A fleet's pods are labelled the same way, so this resolves a fleet member
/// too; picking the first Running replica is the current fan-out policy.)
async fn resolve(client: &Client, ns: &str, name: &str) -> Result<String, String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("agentctl.dev/agent={name}"));
    pods.list(&lp)
        .await
        .map_err(|e| format!("list agent pods: {e}"))?
        .items
        .into_iter()
        .find(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .ok_or_else(|| format!("no running pod for agent {ns}/{name}"))?
        .status
        .and_then(|s| s.pod_ip)
        .ok_or_else(|| format!("agent pod for {ns}/{name} has no podIP"))
}

/// The coordinator workload name for a fleet (mirrors the operator's
/// `render::coordinator_name`): `<fleet>-coordinator`. Its own
/// `agentctl.dev/agent=<fleet>-coordinator` label keeps it distinct from the worker
/// pool's `agentctl.dev/agent=<fleet>`.
fn coordinator_agent_name(fleet: &str) -> String {
    format!("{fleet}-coordinator")
}

/// Every Running pod IP for `agentctl.dev/agent={name}` (the worker pool of a
/// fleet, or the replicas of an agent). Ordered as the apiserver returns them.
async fn resolve_all(client: &Client, ns: &str, name: &str) -> Result<Vec<String>, String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("agentctl.dev/agent={name}"));
    let ips: Vec<String> = pods
        .list(&lp)
        .await
        .map_err(|e| format!("list pods: {e}"))?
        .items
        .into_iter()
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .filter_map(|p| p.status.and_then(|s| s.pod_ip))
        .collect();
    Ok(ips)
}

/// The task id an A2A request targets, for fleet task affinity. Checks
/// `params.id` (tasks/get|cancel|resubscribe — `TaskIdParams`), then
/// `params.taskId` (push-config), then `params.message.taskId` (the
/// SendMessage/SendStreamingMessage gate-reply continuation, which resumes a run
/// paused at INPUT_REQUIRED). Returns `None` for a fresh request with no existing
/// task to stick to.
fn affinity_task_id(req: &Value) -> Option<&str> {
    req.pointer("/params/id")
        .and_then(Value::as_str)
        .or_else(|| req.pointer("/params/taskId").and_then(Value::as_str))
        .or_else(|| {
            req.pointer("/params/message/taskId")
                .and_then(Value::as_str)
        })
}

/// Select the target pod IP for an A2A request. An agent resolves to its single
/// pod. A **fleet** routes to a member:
///   1. **task affinity** — a live op (cancel/stream/get) on an existing task goes
///      back to the member that served it (`owner_pod`), if still reachable;
///   2. else the **coordinator** (front door), when the fleet declares one;
///   3. else **load-balance** round-robin across the worker replicas.
async fn select_member(
    state: &AppState,
    ns: &str,
    name: &str,
    tier: FleetTier,
    spec: &str,
    req: &Value,
) -> Result<String, String> {
    if tier == FleetTier::Agent {
        return resolve(&state.client, ns, name).await;
    }

    // (1) Task affinity for live ops on an existing task. `message/send` is
    // included for the WORKFLOW GATE-REPLY continuation: a SendMessage that
    // carries `message.taskId` resumes a run paused at INPUT_REQUIRED and MUST
    // land on the member that owns that task — otherwise a fresh worker answers
    // -32004 (no gate waiting). A fresh message/send has no taskId ⇒ no owner ⇒
    // falls through to coordinator/round-robin.
    if matches!(
        spec,
        "message/send" | "message/stream" | "tasks/cancel" | "tasks/get"
    ) {
        if let Some(tid) = affinity_task_id(req) {
            if let Ok(Some(owner)) = store::owner_pod(&state.pool, ns, name, tid).await {
                // Only honour the affinity if that pod is still Running (else fall
                // through to fresh selection — the member is gone).
                if resolve_all(&state.client, ns, name)
                    .await
                    .map(|ips| ips.contains(&owner))
                    .unwrap_or(false)
                {
                    return Ok(owner);
                }
            }
        }
    }

    // (2) The coordinator front door, if the fleet declares one — SKIPPED on
    // the workers tier (the coordinator's own downstream dial; front-dooring
    // it would loop the dispatcher onto itself).
    if tier == FleetTier::Fleet {
        let fleets: Api<AgentFleet> = Api::namespaced(state.client.clone(), ns);
        let has_coordinator = matches!(
            fleets.get_opt(name).await,
            Ok(Some(f)) if f.spec.coordinator.is_some()
        );
        if has_coordinator {
            return resolve(&state.client, ns, &coordinator_agent_name(name)).await;
        }
    }

    // (3) Load-balance across the worker replicas (round-robin).
    let ips = resolve_all(&state.client, ns, name).await?;
    if ips.is_empty() {
        return Err(format!("no running worker for fleet {ns}/{name}"));
    }
    let idx = state
        .round_robin
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        % ips.len();
    Ok(ips[idx].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_method_maps_the_mvp_set() {
        // Bare PascalCase (A2A spec JSON-RPC binding / agentd v2.1), not the legacy a2a.* prefix.
        assert_eq!(translate_method("message/send"), Some("SendMessage"));
        assert_eq!(
            translate_method("message/stream"),
            Some("SendStreamingMessage")
        );
        assert_eq!(translate_method("tasks/get"), Some("GetTask"));
        assert_eq!(translate_method("tasks/cancel"), Some("CancelTask"));
    }

    #[test]
    fn task_of_normalizes_both_a2a_result_shapes() {
        // v2.1 SendMessage: the SendMessageResponse envelope {"task": <Task>}.
        let envelope = json!({ "task": { "id": "t1", "status": { "state": "completed" } } });
        assert_eq!(task_of(&envelope).unwrap()["id"], "t1");
        // GetTask/CancelTask: a bare Task (identified by carrying `id`).
        let bare = json!({ "id": "t2", "status": { "state": "working" } });
        assert_eq!(task_of(&bare).unwrap()["id"], "t2");
        // Neither shape ⇒ None (nothing to persist).
        assert!(task_of(&json!({ "unrelated": true })).is_none());
    }

    #[test]
    fn translate_method_rejects_unknown() {
        assert_eq!(translate_method("tasks/list"), None);
        assert_eq!(translate_method(""), None);
        assert_eq!(translate_method("a2a.SendMessage"), None);
    }

    #[test]
    fn affinity_task_id_finds_the_gate_reply_continuation() {
        // A FRESH message/send (no taskId) ⇒ no affinity ⇒ round-robins.
        let fresh = json!({ "params": { "message": { "parts": [{ "text": "hi" }] } } });
        assert_eq!(affinity_task_id(&fresh), None);

        // A GATE-REPLY message/send carries message.taskId ⇒ sticks to the owner.
        let reply = json!({
            "params": { "message": { "taskId": "served.7", "parts": [{ "text": "yes" }] } }
        });
        assert_eq!(affinity_task_id(&reply), Some("served.7"));

        // tasks/get|cancel|resubscribe use params.id (TaskIdParams).
        let get = json!({ "params": { "id": "served.3" } });
        assert_eq!(affinity_task_id(&get), Some("served.3"));

        // params.taskId (push-config shape) is also honoured.
        let push = json!({ "params": { "taskId": "served.9" } });
        assert_eq!(affinity_task_id(&push), Some("served.9"));
    }

    #[test]
    fn coordinator_agent_name_matches_the_operator_convention() {
        // MUST equal the operator's render::coordinator_name so the gateway resolves
        // the very pods the operator labels (agentctl.dev/agent=<fleet>-coordinator).
        assert_eq!(coordinator_agent_name("research"), "research-coordinator");
        // Distinct from the worker label value, so neither selector captures the other.
        assert_ne!(coordinator_agent_name("research"), "research");
    }

    #[test]
    fn project_card_reads_neutral_version_and_streaming() {
        let manifest = json!({
            "agent_version": "1.0.0",
            "surfaces": { "a2a": { "streaming": true } }
        });
        let card = project_card(
            Some(&manifest),
            "team-a",
            "echo",
            "https://gw.example",
            None,
        );

        assert_eq!(card["protocolVersion"], "1.0");
        assert_eq!(card["name"], "team-a/echo");
        assert_eq!(card["url"], "https://gw.example/agents/team-a/echo");
        assert_eq!(card["version"], "1.0.0");
        // Streaming is read from the manifest.
        assert_eq!(card["capabilities"]["streaming"], true);
        assert_eq!(card["defaultInputModes"], json!(["text/plain"]));
        assert_eq!(card["skills"], json!([]));
        assert!(card.get("x-agentctl-kind").is_none());
    }

    #[test]
    fn project_card_defaults_version_when_absent() {
        let card = project_card(Some(&json!({})), "ns", "a", "http://h", None);
        assert_eq!(card["version"], "unknown");
    }

    #[test]
    fn fleet_card_is_servable_at_rest_from_static_facts() {
        // No live manifest (replicas:0) → a VALID static fleet card: /fleets url,
        // streaming advertised (the gateway proxies it), and the kind marker set.
        let card = project_card(
            None,
            "team-a",
            "crawlers",
            "https://gw.example",
            Some("AgentFleet"),
        );
        assert_eq!(card["name"], "team-a/crawlers");
        assert_eq!(card["url"], "https://gw.example/fleets/team-a/crawlers");
        assert_eq!(card["version"], "unknown");
        assert_eq!(card["capabilities"]["streaming"], true);
        assert_eq!(card["x-agentctl-kind"], "AgentFleet");
    }

    #[test]
    fn registry_row_builds_card_url_and_carries_mode() {
        let row = registry_row(
            "Agent",
            "team-a",
            "echo",
            Some("loop"),
            "https://gw.example",
        );
        assert_eq!(row["kind"], "Agent");
        assert_eq!(row["namespace"], "team-a");
        assert_eq!(row["name"], "echo");
        assert_eq!(
            row["cardUrl"],
            "https://gw.example/agents/team-a/echo/.well-known/agent-card.json"
        );
        assert_eq!(row["mode"], "loop");
    }

    #[test]
    fn registry_row_null_mode_serializes_to_json_null() {
        let row = registry_row("AgentFleet", "ns", "fleet-a", None, "http://h:8080");
        assert_eq!(row["kind"], "AgentFleet");
        assert_eq!(row["namespace"], "ns");
        assert_eq!(row["name"], "fleet-a");
        // A fleet row points at the /fleets/... surface (not /agents/...).
        assert_eq!(
            row["cardUrl"],
            "http://h:8080/fleets/ns/fleet-a/.well-known/agent-card.json"
        );
        assert_eq!(row["mode"], Value::Null);
    }

    #[test]
    fn push_cfg_includes_token_only_when_set() {
        let with = push_cfg("https://hook", "s3cr3t");
        assert_eq!(with["url"], "https://hook");
        assert_eq!(with["token"], "s3cr3t");

        let without = push_cfg("https://hook", "");
        assert_eq!(without["url"], "https://hook");
        assert_eq!(without.get("token"), None);
    }

    #[test]
    fn federation_peers_splits_trims_and_drops_empties() {
        assert_eq!(federation_peers(""), Vec::<String>::new());
        assert_eq!(federation_peers("   "), Vec::<String>::new());
        assert_eq!(federation_peers(",,"), Vec::<String>::new());
        assert_eq!(
            federation_peers("http://a , http://b ,, http://c "),
            vec![
                "http://a".to_string(),
                "http://b".to_string(),
                "http://c".to_string()
            ]
        );
        assert_eq!(
            federation_peers("http://only"),
            vec!["http://only".to_string()]
        );
    }

    #[test]
    fn rpc_error_preserves_id_and_shape() {
        let e = rpc_error(json!(7), -32601, "method not found: foo/bar");
        assert_eq!(e["jsonrpc"], "2.0");
        assert_eq!(e["id"], 7);
        assert_eq!(e["error"]["code"], -32601);
        assert_eq!(e["error"]["message"], "method not found: foo/bar");
    }

    #[test]
    fn webhook_parse_requires_https_and_extracts_hostport() {
        assert!(webhook::parse_https("http://evil.example/hook").is_err());
        assert!(webhook::parse_https("ftp://x").is_err());
        assert_eq!(
            webhook::parse_https("https://hooks.acme.io/p?x=1").unwrap(),
            ("hooks.acme.io".to_string(), 443)
        );
        assert_eq!(
            webhook::parse_https("https://h.example:8443/p").unwrap(),
            ("h.example".to_string(), 8443)
        );
        assert_eq!(
            webhook::parse_https("https://[2606:4700:4700::1111]:443/p").unwrap(),
            ("2606:4700:4700::1111".to_string(), 443)
        );
    }

    #[test]
    fn is_public_blocks_ssrf_targets() {
        use std::net::IpAddr;
        let pub_v4: IpAddr = "1.1.1.1".parse().unwrap();
        assert!(webhook::is_public(&pub_v4));
        // The classic SSRF targets are all rejected.
        for bad in [
            "127.0.0.1",        // loopback
            "169.254.169.254",  // cloud metadata (link-local)
            "10.1.2.3",         // RFC1918
            "192.168.0.5",      // RFC1918
            "172.16.9.9",       // RFC1918
            "100.64.0.1",       // CGNAT
            "0.0.0.0",          // unspecified
            "::1",              // IPv6 loopback
            "fd00::1",          // IPv6 ULA
            "fe80::1",          // IPv6 link-local
            "::ffff:127.0.0.1", // IPv4-mapped loopback
        ] {
            let ip: IpAddr = bad.parse().unwrap();
            assert!(!webhook::is_public(&ip), "{bad} must be blocked");
        }
    }
}
