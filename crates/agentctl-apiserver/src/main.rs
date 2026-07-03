// SPDX-License-Identifier: BUSL-1.1
//! agentctl aggregated APIServer — the human management access path.
//!
//! Registered via an `APIService` for `management.agents.x-k8s.io`; the
//! kube-aggregator proxies requests here.
//!
//! Serves TLS, discovery, and health so the `APIService` reports
//! `Available=True`, and exposes the `agents/<name>/{drain,lame-duck,cancel}`
//! connect verbs under the front-proxy trust model: rustls **requires** a client
//! cert verified against the `requestheader-client-ca` (so only the
//! kube-apiserver can reach the API surface), the handler trusts
//! `X-Remote-User`/`-Group`, and a `SubjectAccessReview` authorizes the verb
//! before forwarding it to the agent.
//!
//! Hand-rolled in Rust (axum + rustls/ring; agentctl is Rust-only). Probes are
//! `tcpSocket` so the kubelet need not present a client cert.

use std::io::BufReader;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use kube::api::{ListParams, PostParams};
use kube::{Api, Client};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use serde_json::{json, Value};

mod metrics;
mod na_client;

const GROUP: &str = "management.agents.x-k8s.io";
const VERSION: &str = "v1alpha1";
const TLS_DIR: &str = "/etc/agentctl-apiserver/tls";

#[derive(Clone)]
struct AppState {
    client: Client,
    /// mTLS client for the control hop to agent pods. Built once.
    na: reqwest::Client,
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set.
    agentctl_telemetry::init("agentctl-apiserver");
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let client = Client::try_default().await.expect("in-cluster kube client");

    // Front-proxy trust anchor: only the kube-apiserver (presenting a cert signed
    // by this CA) may reach the API surface; then we trust its X-Remote-* headers.
    let client_ca = load_requestheader_ca(&client)
        .await
        .expect("load requestheader-client-ca from extension-apiserver-authentication");

    let tls = build_tls_config(client_ca).expect("build TLS server config");

    let app = Router::new()
        .route("/", get(ok))
        .route("/healthz", get(ok))
        .route("/readyz", get(ok))
        .route("/livez", get(ok))
        // `/metrics` rides the EXISTING :6443 HTTPS surface — it does NOT open a
        // separate plaintext port, so it stays behind the front-proxy mTLS gate
        // (only a CA-signed client cert can scrape; never bypasses the apiserver's
        // TLS). The chart's ServiceMonitor scrapes it scheme=https.
        .route("/metrics", get(serve_metrics))
        .route("/apis", get(api_group_list))
        .route("/apis/management.agents.x-k8s.io", get(api_group))
        .route(
            "/apis/management.agents.x-k8s.io/v1alpha1",
            get(api_resources),
        )
        .route(
            "/apis/management.agents.x-k8s.io/v1alpha1/namespaces/{ns}/agents/{name}/{verb}",
            post(handle_verb),
        )
        .route(
            "/apis/management.agents.x-k8s.io/v1alpha1/namespaces/{ns}/agentfleets/{name}/{verb}",
            post(handle_fleet_verb),
        )
        .with_state(AppState {
            client,
            na: na_client::node_agent_client(),
            metrics: Arc::new(metrics::Metrics::new()),
        })
        .fallback(not_found);

    let addr: SocketAddr = "0.0.0.0:6443".parse().unwrap();
    let config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));
    // Graceful shutdown: on SIGTERM/SIGINT, stop accepting and drain in-flight
    // requests (axum-server's `Handle::graceful_shutdown`).
    let handle = axum_server::Handle::new();
    tokio::spawn(shutdown_signal(handle.clone()));
    tracing::info!(%addr, group = GROUP, "agentctl aggregated apiserver serving (stage 2: connect verbs + SAR)");
    axum_server::bind_rustls(addr, config)
        .handle(handle)
        .serve(app.into_make_service())
        .await
        .expect("serve");
}

// --- graceful shutdown -----------------------------------------------------

/// Wait for SIGTERM/SIGINT, then trigger axum-server's graceful drain (a bounded
/// grace period for in-flight requests to finish).
async fn shutdown_signal(handle: axum_server::Handle<SocketAddr>) {
    wait_for_signal().await;
    tracing::info!("shutting down: draining in-flight requests");
    handle.graceful_shutdown(Some(Duration::from_secs(15)));
}

/// Resolve once either SIGINT (Ctrl-C) or SIGTERM arrives.
async fn wait_for_signal() {
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
}

// --- TLS / front-proxy -----------------------------------------------------

/// Read the `requestheader-client-ca-file` PEM from the kube-system
/// `extension-apiserver-authentication` ConfigMap (the CA the kube-apiserver's
/// front-proxy client cert is signed by).
async fn load_requestheader_ca(client: &Client) -> Result<RootCertStore, String> {
    let cm: ConfigMap = Api::namespaced(client.clone(), "kube-system")
        .get("extension-apiserver-authentication")
        .await
        .map_err(|e| format!("get configmap: {e}"))?;
    let pem = cm
        .data
        .as_ref()
        .and_then(|d| d.get("requestheader-client-ca-file"))
        .ok_or("requestheader-client-ca-file missing")?;
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut pem.as_bytes()) {
        roots
            .add(cert.map_err(|e| format!("parse CA: {e}"))?)
            .map_err(|e| format!("add CA: {e}"))?;
    }
    if roots.is_empty() {
        return Err("requestheader CA had no certs".into());
    }
    Ok(roots)
}

/// rustls server config: present the serving cert AND **require** a client cert
/// chained to the front-proxy CA (so unproxied callers can't reach the API).
fn build_tls_config(client_ca: RootCertStore) -> Result<ServerConfig, String> {
    let certs = load_certs(&PathBuf::from(TLS_DIR).join("tls.crt"))?;
    let key = load_key(&PathBuf::from(TLS_DIR).join("tls.key"))?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(client_ca))
        .build()
        .map_err(|e| format!("client verifier: {e}"))?;
    ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("server config: {e}"))
}

fn load_certs(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read certs: {e}"))
}

fn load_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    rustls_pemfile::private_key(&mut r)
        .map_err(|e| format!("read key: {e}"))?
        .ok_or_else(|| "no private key in tls.key".into())
}

// --- connect verbs (drain / lame-duck / cancel) ----------------------------

/// A management connect verb on an Agent. The connection is already front-proxy
/// authenticated (rustls required a valid client cert), so we trust the
/// `X-Remote-*` identity; we then `SubjectAccessReview` the verb before acting.
#[tracing::instrument(skip_all, fields(ns = %ns, agent = %name, verb = %verb))]
async fn handle_verb(
    State(state): State<AppState>,
    Path((ns, name, verb)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !matches!(
        verb.as_str(),
        "drain" | "lame-duck" | "cancel" | "pause" | "resume"
    ) {
        return status(
            StatusCode::NOT_FOUND,
            "Failure",
            &format!("unknown verb: {verb}"),
        );
    }
    state.metrics.inc_request();

    let user = headers
        .get("X-Remote-User")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if user.is_empty() {
        return status(
            StatusCode::UNAUTHORIZED,
            "Failure",
            "no X-Remote-User (not proxied?)",
        );
    }
    let groups: Vec<String> = headers
        .get_all("X-Remote-Group")
        .iter()
        .filter_map(|v| v.to_str().ok().map(String::from))
        .collect();

    match authorize(&state.client, &user, &groups, &ns, &name, &verb, "agents").await {
        Ok(true) => {
            state.metrics.inc_authorized();
            tracing::info!(%user, %ns, agent = %name, %verb, "authorized management verb");
            match call_agent_admin(&state.client, &state.na, &ns, &name, &verb).await {
                Ok(result) => {
                    state.metrics.inc_forwarded();
                    tracing::info!(%ns, agent = %name, %verb, "admin verb delivered to agent");
                    status(
                        StatusCode::OK,
                        "Success",
                        &format!("{verb} {ns}/{name} by {user}; agent: {result}"),
                    )
                }
                Err(e) => {
                    state.metrics.inc_error();
                    tracing::error!(error = %e, "agent admin call failed");
                    status(
                        StatusCode::BAD_GATEWAY,
                        "Failure",
                        &format!("forward failed: {e}"),
                    )
                }
            }
        }
        Ok(false) => {
            state.metrics.inc_denied();
            tracing::warn!(%user, %ns, agent = %name, %verb, "denied by SubjectAccessReview");
            status(
                StatusCode::FORBIDDEN,
                "Failure",
                &format!("{user:?} cannot {verb} agents/{name} in {ns}"),
            )
        }
        Err(e) => {
            state.metrics.inc_error();
            tracing::error!(error = %e, "SubjectAccessReview failed");
            status(StatusCode::INTERNAL_SERVER_ERROR, "Failure", &e)
        }
    }
}

/// A management connect verb on an **AgentFleet** — fanned out to ALL Running
/// replicas. Unlike the per-`Agent` path, a fleet drain/pause/cancel must reach
/// every member: hitting one arbitrary pod would leave N−1 replicas running while
/// reporting Success. Returns a partial-success Status (207 when some replicas
/// failed).
#[tracing::instrument(skip_all, fields(ns = %ns, fleet = %name, verb = %verb))]
async fn handle_fleet_verb(
    State(state): State<AppState>,
    Path((ns, name, verb)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !matches!(
        verb.as_str(),
        "drain" | "lame-duck" | "cancel" | "pause" | "resume"
    ) {
        return status(
            StatusCode::NOT_FOUND,
            "Failure",
            &format!("unknown verb: {verb}"),
        );
    }
    state.metrics.inc_request();
    let user = headers
        .get("X-Remote-User")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if user.is_empty() {
        return status(
            StatusCode::UNAUTHORIZED,
            "Failure",
            "no X-Remote-User (not proxied?)",
        );
    }
    let groups: Vec<String> = headers
        .get_all("X-Remote-Group")
        .iter()
        .filter_map(|v| v.to_str().ok().map(String::from))
        .collect();

    match authorize(
        &state.client,
        &user,
        &groups,
        &ns,
        &name,
        &verb,
        "agentfleets",
    )
    .await
    {
        Ok(true) => {
            state.metrics.inc_authorized();
            match call_fleet_admin(&state.client, &state.na, &ns, &name, &verb).await {
                Ok((ok, total, detail)) => {
                    let all_ok = ok == total;
                    if all_ok {
                        state.metrics.inc_forwarded();
                    } else {
                        state.metrics.inc_error();
                    }
                    let code = if all_ok {
                        StatusCode::OK
                    } else {
                        StatusCode::MULTI_STATUS
                    };
                    tracing::info!(%ns, fleet = %name, %verb, ok, total, "fleet verb fanned out");
                    (
                        code,
                        Json(json!({
                            "kind": "Status", "apiVersion": "v1",
                            "status": if all_ok { "Success" } else { "Failure" },
                            "message": format!("{verb} fleet {ns}/{name} by {user}: {ok}/{total} replicas ok"),
                            "code": code.as_u16(),
                            "details": { "ok": ok, "total": total, "replicas": detail },
                        })),
                    )
                }
                Err(e) => {
                    state.metrics.inc_error();
                    status(
                        StatusCode::BAD_GATEWAY,
                        "Failure",
                        &format!("fleet fan-out failed: {e}"),
                    )
                }
            }
        }
        Ok(false) => {
            state.metrics.inc_denied();
            status(
                StatusCode::FORBIDDEN,
                "Failure",
                &format!("{user:?} cannot {verb} agentfleets/{name} in {ns}"),
            )
        }
        Err(e) => {
            state.metrics.inc_error();
            status(StatusCode::INTERNAL_SERVER_ERROR, "Failure", &e)
        }
    }
}

/// SubjectAccessReview: may `user` (with `groups`) `create` the `<resource>/<verb>`
/// subresource on `name` in `ns`?
async fn authorize(
    client: &Client,
    user: &str,
    groups: &[String],
    ns: &str,
    name: &str,
    verb: &str,
    resource: &str,
) -> Result<bool, String> {
    let sar = SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(user.to_string()),
            groups: Some(groups.to_vec()),
            resource_attributes: Some(ResourceAttributes {
                group: Some(GROUP.to_string()),
                resource: Some(resource.to_string()),
                subresource: Some(verb.to_string()),
                verb: Some("create".to_string()),
                namespace: Some(ns.to_string()),
                name: Some(name.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let api: Api<SubjectAccessReview> = Api::all(client.clone());
    let resp = api
        .create(&PostParams::default(), &sar)
        .await
        .map_err(|e| format!("create SAR: {e}"))?;
    Ok(resp.status.map(|s| s.allowed).unwrap_or(false))
}

/// Deliver a management verb directly to the agent pod as a contract-1.0 A2A
/// admin JSON-RPC (`a2a.Drain`/`a2a.LameDuck`/`a2a.Pause`/`a2a.Resume`/
/// `a2a.Cancel` on `POST /mcp`). The agent serves mTLS-gated HTTPS on :8443
/// (rendered by the operator); our client certificate chains to the cluster CA
/// the agent was given as `--serve-client-ca`, which mints the `Management`
/// origin these verbs require. The pod itself is the endpoint, addressed by pod
/// IP (the CA — not DNS — is the trust anchor; see `na_client::CaServerVerifier`).
async fn call_agent_admin(
    client: &Client,
    http: &reqwest::Client,
    ns: &str,
    name: &str,
    verb: &str,
) -> Result<String, String> {
    let ip = running_pod_ips(client, ns, name)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| format!("no running pod for agent {ns}/{name}"))?;
    forward_verb_to_ip(http, &ip, verb).await
}

/// Verb → the agentd extension admin method (Management-gated; the `a2a.` prefix is
/// deliberate — these are operator verbs, not A2A protocol).
fn verb_to_method(verb: &str) -> Result<&'static str, String> {
    Ok(match verb {
        "drain" => "a2a.Drain",
        "lame-duck" => "a2a.LameDuck",
        "cancel" => "a2a.Cancel",
        "pause" => "a2a.Pause",
        "resume" => "a2a.Resume",
        other => return Err(format!("unmapped verb: {other}")),
    })
}

/// Every Running pod IP for a workload labelled `agentctl.dev/agent=<name>` — one
/// for a singleton `Agent`, N for an `AgentFleet` (fleet pods share the label).
async fn running_pod_ips(client: &Client, ns: &str, name: &str) -> Result<Vec<String>, String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("agentctl.dev/agent={name}"));
    Ok(pods
        .list(&lp)
        .await
        .map_err(|e| format!("list pods: {e}"))?
        .items
        .into_iter()
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .filter_map(|p| p.status.and_then(|s| s.pod_ip))
        .collect())
}

/// POST an admin verb to one agent pod's mTLS `/mcp` as a contract-1.0 A2A admin
/// JSON-RPC. A bounded timeout keeps a single hung replica from stalling a fleet
/// fan-out.
async fn forward_verb_to_ip(
    http: &reqwest::Client,
    pod_ip: &str,
    verb: &str,
) -> Result<String, String> {
    let method = verb_to_method(verb)?;
    let url = format!("https://{pod_ip}:8443/mcp");
    // Inject the W3C `traceparent` so the agent's run joins this trace (no-op when
    // OTLP is off). No Origin header is sent (the agent 403s cross-origin).
    let mut trace_headers = reqwest::header::HeaderMap::new();
    agentctl_telemetry::inject_context(&mut trace_headers);
    let resp = http
        .post(&url)
        .headers(trace_headers)
        .timeout(std::time::Duration::from_secs(10))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": {} }))
        .send()
        .await
        .map_err(|e| format!("agent POST {url}: {e}"))?;
    let code = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("agent {code}: unparseable JSON-RPC response: {e}"))?;
    if let Some(err) = body.get("error") {
        return Err(format!("agent JSON-RPC error: {err}"));
    }
    match body.get("result") {
        Some(result) => Ok(result.to_string()),
        None => Err(format!("agent {code}: no result in JSON-RPC response")),
    }
}

/// Fan a management verb out to **every** Running replica of an `AgentFleet` and
/// aggregate. A single-replica hit (what the per-`Agent` path does) is dangerous for
/// a fleet — drain/pause/cancel would silently affect one of N pods while reporting
/// Success. Returns `(ok, total, detail)` so the handler can build a partial-success
/// Status.
async fn call_fleet_admin(
    client: &Client,
    http: &reqwest::Client,
    ns: &str,
    name: &str,
    verb: &str,
) -> Result<(usize, usize, Vec<String>), String> {
    let ips = running_pod_ips(client, ns, name).await?;
    let total = ips.len();
    let mut ok = 0usize;
    let mut detail = Vec::with_capacity(total);
    for ip in &ips {
        match forward_verb_to_ip(http, ip, verb).await {
            Ok(_) => {
                ok += 1;
                detail.push(format!("{ip}: ok"));
            }
            Err(e) => detail.push(format!("{ip}: {e}")),
        }
    }
    Ok((ok, total, detail))
}

fn status(code: StatusCode, kind: &str, message: &str) -> (StatusCode, Json<Value>) {
    (
        code,
        Json(json!({
            "kind": "Status", "apiVersion": "v1", "status": kind,
            "message": message, "code": code.as_u16()
        })),
    )
}

// --- discovery / health ----------------------------------------------------

async fn ok() -> &'static str {
    "ok"
}

/// `GET /metrics` — the Prometheus exposition (`text/plain; version=0.0.4`),
/// served on the existing front-proxy mTLS surface.
async fn serve_metrics(
    State(state): State<AppState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

async fn api_group_list() -> Json<Value> {
    Json(json!({ "kind": "APIGroupList", "apiVersion": "v1", "groups": [group_obj()] }))
}

async fn api_group() -> Json<Value> {
    Json(group_obj())
}

fn group_obj() -> Value {
    let gv = format!("{GROUP}/{VERSION}");
    json!({
        "kind": "APIGroup", "apiVersion": "v1", "name": GROUP,
        "versions": [{ "groupVersion": gv, "version": VERSION }],
        "preferredVersion": { "groupVersion": gv, "version": VERSION },
    })
}

async fn api_resources() -> Json<Value> {
    Json(json!({
        "kind": "APIResourceList", "apiVersion": "v1",
        "groupVersion": format!("{GROUP}/{VERSION}"),
        "resources": [
            { "name": "agents/drain", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] },
            { "name": "agents/lame-duck", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] },
            { "name": "agents/cancel", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] },
            { "name": "agents/pause", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] },
            { "name": "agents/resume", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] },
            { "name": "agentfleets/drain", "singularName": "", "namespaced": true, "kind": "AgentFleet", "verbs": ["create"] },
            { "name": "agentfleets/lame-duck", "singularName": "", "namespaced": true, "kind": "AgentFleet", "verbs": ["create"] },
            { "name": "agentfleets/cancel", "singularName": "", "namespaced": true, "kind": "AgentFleet", "verbs": ["create"] },
            { "name": "agentfleets/pause", "singularName": "", "namespaced": true, "kind": "AgentFleet", "verbs": ["create"] },
            { "name": "agentfleets/resume", "singularName": "", "namespaced": true, "kind": "AgentFleet", "verbs": ["create"] }
        ],
    }))
}

async fn not_found() -> (StatusCode, Json<Value>) {
    status(StatusCode::NOT_FOUND, "Failure", "not found")
}
