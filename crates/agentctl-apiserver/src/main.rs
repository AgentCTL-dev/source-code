//! agentctl aggregated APIServer (RFC 0009) — the human management access path.
//!
//! Registered via an `APIService` for `management.agents.x-k8s.io`, the
//! kube-aggregator proxies requests here. **Stage 1 (this file):** serve TLS +
//! the discovery + health surface so the aggregator marks the APIService
//! `Available=True`. **Stage 2 (next):** the `agents/<name>/drain` connect verb —
//! verify the front-proxy client cert, trust `X-Remote-User`, `SubjectAccessReview`
//! the verb, then forward to the node-agent → agent.
//!
//! Hand-rolled in Rust (axum + rustls, ring provider) — agentctl is Rust-only.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;

const GROUP: &str = "management.agents.x-k8s.io";
const VERSION: &str = "v1alpha1";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    // Install the ring crypto provider (the no-provider axum-server feature).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let tls_dir = std::env::var("TLS_DIR").unwrap_or_else(|_| "/etc/agentctl-apiserver/tls".into());
    let cert = PathBuf::from(&tls_dir).join("tls.crt");
    let key = PathBuf::from(&tls_dir).join("tls.key");

    let app = Router::new()
        // Health/availability surface the aggregator + kubelet probe.
        .route("/", get(ok))
        .route("/healthz", get(ok))
        .route("/readyz", get(ok))
        .route("/livez", get(ok))
        // Discovery the aggregator + kubectl consume.
        .route("/apis", get(api_group_list))
        .route("/apis/management.agents.x-k8s.io", get(api_group))
        .route("/apis/management.agents.x-k8s.io/v1alpha1", get(api_resources))
        .fallback(not_found);

    let addr: SocketAddr = "0.0.0.0:6443".parse().unwrap();
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
        .await
        .unwrap_or_else(|e| panic!("load TLS from {tls_dir}: {e}"));

    tracing::info!(%addr, group = GROUP, version = VERSION, "agentctl aggregated apiserver serving (stage 1: discovery + health)");
    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service())
        .await
        .expect("serve");
}

async fn ok() -> &'static str {
    "ok"
}

/// `GET /apis` — the aggregated group list for this server.
async fn api_group_list() -> Json<Value> {
    Json(json!({
        "kind": "APIGroupList",
        "apiVersion": "v1",
        "groups": [group_obj()],
    }))
}

/// `GET /apis/management.agents.x-k8s.io` — the group.
async fn api_group() -> Json<Value> {
    Json(group_obj())
}

fn group_obj() -> Value {
    let gv = format!("{GROUP}/{VERSION}");
    json!({
        "kind": "APIGroup",
        "apiVersion": "v1",
        "name": GROUP,
        "versions": [{ "groupVersion": gv, "version": VERSION }],
        "preferredVersion": { "groupVersion": gv, "version": VERSION },
    })
}

/// `GET /apis/management.agents.x-k8s.io/v1alpha1` — the resource list. The
/// management verbs are modeled as connect subresources on `agents` (Stage 2
/// implements their handlers).
async fn api_resources() -> Json<Value> {
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": format!("{GROUP}/{VERSION}"),
        "resources": [
            { "name": "agents/drain", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] },
            { "name": "agents/lame-duck", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] },
            { "name": "agents/cancel", "singularName": "", "namespaced": true, "kind": "Agent", "verbs": ["create"] }
        ],
    }))
}

async fn not_found() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": "NotFound", "code": 404
        })),
    )
}
