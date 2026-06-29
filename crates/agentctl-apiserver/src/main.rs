// SPDX-License-Identifier: BUSL-1.1
//! agentctl aggregated APIServer (RFC 0009) — the human management access path.
//!
//! Registered via an `APIService` for `management.agents.x-k8s.io`; the
//! kube-aggregator proxies requests here.
//!
//! **Stage 1:** TLS + discovery + health → `APIService Available=True`.
//! **Stage 2 (this file):** the `agents/<name>/{drain,lame-duck,cancel}` connect
//! verbs with the front-proxy trust model — rustls **requires** a client cert
//! verified against the `requestheader-client-ca` (so only the kube-apiserver can
//! reach the API surface), the handler trusts `X-Remote-User`/`-Group`, and a
//! `SubjectAccessReview` authorizes the verb before acting. (Forwarding to the
//! node-agent is Stage 2b — here an authorized verb returns Success.)
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
use tracing_subscriber::EnvFilter;

mod metrics;
mod na_client;

const GROUP: &str = "management.agents.x-k8s.io";
const VERSION: &str = "v1alpha1";
const TLS_DIR: &str = "/etc/agentctl-apiserver/tls";

#[derive(Clone)]
struct AppState {
    client: Client,
    /// mTLS client for the node-agent control hop (RFC 0015). Built once.
    na: reqwest::Client,
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
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
async fn handle_verb(
    State(state): State<AppState>,
    Path((ns, name, verb)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !matches!(verb.as_str(), "drain" | "lame-duck" | "cancel") {
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

    match authorize(&state.client, &user, &groups, &ns, &name, &verb).await {
        Ok(true) => {
            state.metrics.inc_authorized();
            tracing::info!(%user, %ns, agent = %name, %verb, "authorized management verb");
            match forward_to_node_agent(&state.client, &state.na, &ns, &name, &verb).await {
                Ok(result) => {
                    state.metrics.inc_forwarded();
                    tracing::info!(%ns, agent = %name, %verb, "forwarded to node-agent");
                    status(
                        StatusCode::OK,
                        "Success",
                        &format!("{verb} {ns}/{name} by {user}; node-agent: {result}"),
                    )
                }
                Err(e) => {
                    state.metrics.inc_error();
                    tracing::error!(error = %e, "node-agent forward failed");
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

/// SubjectAccessReview: may `user` (with `groups`) `create` the `agents/<verb>`
/// subresource on `name` in `ns`?
async fn authorize(
    client: &Client,
    user: &str,
    groups: &[String],
    ns: &str,
    name: &str,
    verb: &str,
) -> Result<bool, String> {
    let sar = SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(user.to_string()),
            groups: Some(groups.to_vec()),
            resource_attributes: Some(ResourceAttributes {
                group: Some(GROUP.to_string()),
                resource: Some("agents".to_string()),
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

/// Resolve the Agent to its pod, find the node-agent on that pod's node, and
/// POST the verb to it. Routing: Agent --(label)--> pod (uid, node) --> the
/// node-agent DaemonSet pod on `node` --> `POST /v1/agents/<pod_uid>/<verb>`.
async fn forward_to_node_agent(
    client: &Client,
    http: &reqwest::Client,
    ns: &str,
    name: &str,
    verb: &str,
) -> Result<String, String> {
    // The agent's pod, labelled by the operator (agentctl.dev/agent=<name>).
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("agentctl.dev/agent={name}"));
    let pod = pods
        .list(&lp)
        .await
        .map_err(|e| format!("list agent pods: {e}"))?
        .items
        .into_iter()
        .find(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .ok_or_else(|| format!("no running pod for agent {ns}/{name}"))?;
    let pod_uid = pod.metadata.uid.ok_or("agent pod has no uid")?;
    let node = pod
        .spec
        .and_then(|s| s.node_name)
        .ok_or("agent pod has no nodeName")?;

    // The node-agent on that node.
    let na: Api<Pod> = Api::namespaced(client.clone(), "agentctl-system");
    let na_lp = ListParams::default()
        .labels("app.kubernetes.io/name=agentctl-node-agent")
        .fields(&format!("spec.nodeName={node}"));
    let na_ip = na
        .list(&na_lp)
        .await
        .map_err(|e| format!("list node-agents: {e}"))?
        .items
        .into_iter()
        // Skip a terminating/old pod during a rollout — only a Running one serves.
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .find_map(|p| p.status.and_then(|s| s.pod_ip))
        .ok_or_else(|| format!("no running node-agent on node {node}"))?;

    // mTLS control hop (RFC 0015): https on :8443, client-cert required.
    let url = format!("https://{na_ip}:8443/v1/agents/{pod_uid}/{verb}");
    let resp = http
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("node-agent POST {url}: {e}"))?;
    let code = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if code.is_success() {
        Ok(body)
    } else {
        Err(format!("node-agent {code}: {body}"))
    }
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

/// `GET /metrics` — the Prometheus exposition (node-agent text format), served on
/// the existing mTLS surface.
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
            { "name": "agents/cancel", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] }
        ],
    }))
}

async fn not_found() -> (StatusCode, Json<Value>) {
    status(StatusCode::NOT_FOUND, "Failure", "not found")
}
