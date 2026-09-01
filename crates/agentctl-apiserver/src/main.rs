// SPDX-License-Identifier: BUSL-1.1
//! agentctl aggregated APIServer — the human management access path.
//!
//! Registered via an `APIService` for `management.agentctl.dev`; the
//! kube-aggregator proxies requests here.
//!
//! Serves TLS, discovery, and health so the `APIService` reports
//! `Available=True`, and exposes the
//! `agents/<name>/{drain,lame-duck,cancel,pause,resume}` connect verbs (and the
//! same set on `agentfleets`) under the front-proxy trust model: rustls
//! **requires** a client
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
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use serde_json::{json, Value};

mod metrics;
mod na_client;

const GROUP: &str = "management.agentctl.dev";
const VERSION: &str = "v1alpha1";
const TLS_DIR: &str = "/etc/agentctl-apiserver/tls";

#[derive(Clone)]
struct AppState {
    client: Client,
    /// mTLS client for the control hop to agent pods. Built once.
    na: reqwest::Client,
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
    /// The metering store (P7-4) — read-only aggregation for the export.
    /// `None` when DATABASE_URL is unset (export answers 503).
    metering: Option<deadpool_postgres::Pool>,
    /// The managed state service base URL (P3-5), e.g.
    /// `https://agentctl-state.<ns>.svc:8787`. `None` when the state plane is
    /// off — the state-plane lifecycle verbs (backup/restore/reset) then 503.
    state_url: Option<String>,
}

/// A read-only pool over the shared Postgres for the metering export.
/// sslmode=disable → NoTls; anything else → rustls (encrypt, no CA verify —
/// matching the gateway's default hop; CA pinning is the gateway's writer
/// concern, the reader follows the same DSN).
fn metering_pool() -> Option<deadpool_postgres::Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let cfg: tokio_postgres::Config = url.parse().ok()?;
    let mgr = if cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
        deadpool_postgres::Manager::new(cfg, tokio_postgres::NoTls)
    } else {
        let tls = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(
                agentctl_apiserver_noverify::NoVerify,
            ))
            .with_no_client_auth();
        deadpool_postgres::Manager::new(cfg, tokio_postgres_rustls::MakeRustlsConnect::new(tls))
    };
    deadpool_postgres::Pool::builder(mgr)
        .max_size(4)
        .build()
        .ok()
}

/// Encrypt-without-verify rustls verifier for the PG hop (same posture as
/// the gateway's default `db_tls::make_connector`).
mod agentctl_apiserver_noverify {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    #[derive(Debug)]
    pub struct NoVerify;
    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _m: &[u8],
            _c: &rustls::pki_types::CertificateDer<'_>,
            _d: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _m: &[u8],
            _c: &rustls::pki_types::CertificateDer<'_>,
            _d: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
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
        .route("/apis/management.agentctl.dev", get(api_group))
        .route("/apis/management.agentctl.dev/v1alpha1", get(api_resources))
        .route(
            "/apis/management.agentctl.dev/v1alpha1/namespaces/{ns}/agents/{name}/{verb}",
            post(handle_verb),
        )
        .route(
            "/apis/management.agentctl.dev/v1alpha1/namespaces/{ns}/agentfleets/{name}/{verb}",
            post(handle_fleet_verb),
        )
        // Metering export (P7-4): period aggregation of the durable usage
        // events — the invoice pipeline's input. SAR-gated (cluster-scoped
        // `metering` resource, verb get).
        .route(
            "/apis/management.agentctl.dev/v1alpha1/metering/export",
            get(handle_metering_export),
        )
        .route(
            "/apis/management.agentctl.dev/v1alpha1/audit/query",
            get(handle_audit_query),
        )
        .with_state(AppState {
            client,
            na: na_client::node_agent_client(),
            metrics: Arc::new(metrics::Metrics::new()),
            metering: {
                let pool = metering_pool();
                if let Some(p) = &pool {
                    // The audit table (P7-3) rides the same PG; idempotent.
                    if let Err(e) = agentctl_audit::pg::ensure_schema(p).await {
                        tracing::warn!(error = %e, "audit schema init failed");
                    }
                }
                pool
            },
            state_url: std::env::var("AGENTCTL_STATE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
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
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    // Two verb families on an Agent: the RUNTIME verbs forward to the agent
    // pod's admin surface; the STATE-plane lifecycle verbs (P3-5) act on the
    // durable checkpoint / the CR, never the pod.
    let runtime = matches!(
        verb.as_str(),
        "drain" | "lame-duck" | "cancel" | "pause" | "resume"
    );
    let lifecycle = matches!(
        verb.as_str(),
        "backup" | "restore" | "reset" | "stop" | "start" | "migrate"
    );
    if !runtime && !lifecycle {
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
            if lifecycle {
                return handle_lifecycle_verb(&state, &ns, &name, &verb, &user, &body).await;
            }
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
/// `GET /apis/management.agentctl.dev/v1alpha1/metering/export?from=&to=&format=`
/// — the P7-4 export: aggregated usage rows for `[from, to)` (unix seconds;
/// defaults: the last 24h), as JSON or CSV. SAR-gated: the caller needs
/// `get` on the cluster-scoped `metering` resource in the management group.
async fn handle_metering_export(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    state.metrics.inc_request();
    let user = headers
        .get("X-Remote-User")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if user.is_empty() {
        let (c, j) = status(
            StatusCode::UNAUTHORIZED,
            "Failure",
            "no X-Remote-User (not proxied?)",
        );
        return (c, j).into_response();
    }
    let groups: Vec<String> = headers
        .get_all("X-Remote-Group")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    match authorize(&state.client, &user, &groups, "", "", "export", "metering").await {
        Ok(true) => {}
        Ok(false) => {
            let (c, j) = status(StatusCode::FORBIDDEN, "Failure", "metering export denied");
            return (c, j).into_response();
        }
        Err(e) => {
            let (c, j) = status(StatusCode::INTERNAL_SERVER_ERROR, "Failure", &e);
            return (c, j).into_response();
        }
    }
    let Some(pool) = &state.metering else {
        let (c, j) = status(
            StatusCode::SERVICE_UNAVAILABLE,
            "Failure",
            "metering store not configured (DATABASE_URL unset on the apiserver)",
        );
        return (c, j).into_response();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let parse = |k: &str, dflt: i64| q.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(dflt);
    let from = parse("from", now - 86_400);
    let to = parse("to", now);
    match agentctl_metering::pg::export(pool, from, to).await {
        Ok(rows) => {
            if q.get("format").map(String::as_str) == Some("csv") {
                (
                    StatusCode::OK,
                    [("content-type", "text/csv")],
                    agentctl_metering::to_csv(&rows),
                )
                    .into_response()
            } else {
                Json(json!({
                    "schema": agentctl_metering::SCHEMA,
                    "from": from,
                    "to": to,
                    "rows": rows,
                }))
                .into_response()
            }
        }
        Err(e) => {
            let (c, j) = status(StatusCode::BAD_GATEWAY, "Failure", &format!("export: {e}"));
            (c, j).into_response()
        }
    }
}

/// `GET /apis/management.agentctl.dev/v1alpha1/audit/query?from&to&org&user&action&trail&task&limit`
/// — the P7-3 trail read: filtered audit records, newest first. SAR-gated
/// like the metering export (`get` on the cluster-scoped `audit` resource).
async fn handle_audit_query(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    state.metrics.inc_request();
    let user = headers
        .get("X-Remote-User")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if user.is_empty() {
        let (c, j) = status(
            StatusCode::UNAUTHORIZED,
            "Failure",
            "no X-Remote-User (not proxied?)",
        );
        return (c, j).into_response();
    }
    let groups: Vec<String> = headers
        .get_all("X-Remote-Group")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    match authorize(&state.client, &user, &groups, "", "", "query", "audit").await {
        Ok(true) => {}
        Ok(false) => {
            let (c, j) = status(StatusCode::FORBIDDEN, "Failure", "audit query denied");
            return (c, j).into_response();
        }
        Err(e) => {
            let (c, j) = status(StatusCode::INTERNAL_SERVER_ERROR, "Failure", &e);
            return (c, j).into_response();
        }
    }
    let Some(pool) = &state.metering else {
        let (c, j) = status(
            StatusCode::SERVICE_UNAVAILABLE,
            "Failure",
            "audit store not configured (DATABASE_URL unset on the apiserver)",
        );
        return (c, j).into_response();
    };
    let query = agentctl_audit::Query {
        from: q.get("from").and_then(|v| v.parse().ok()),
        to: q.get("to").and_then(|v| v.parse().ok()),
        org: q.get("org").cloned().filter(|v| !v.is_empty()),
        user: q.get("user").cloned().filter(|v| !v.is_empty()),
        action: q.get("action").cloned().filter(|v| !v.is_empty()),
        trail_id: q.get("trail").cloned().filter(|v| !v.is_empty()),
        task_id: q.get("task").cloned().filter(|v| !v.is_empty()),
        limit: q.get("limit").and_then(|v| v.parse().ok()),
    };
    match agentctl_audit::pg::query(pool, &query).await {
        Ok(rows) => Json(json!({
            "schema": agentctl_audit::SCHEMA,
            "rows": rows,
        }))
        .into_response(),
        Err(e) => {
            let (c, j) = status(StatusCode::BAD_GATEWAY, "Failure", &format!("query: {e}"));
            (c, j).into_response()
        }
    }
}

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
/// `a2a.Cancel` on the A2A listener root). The agent serves mTLS-gated HTTPS on :8443
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

/// POST an admin verb to one agent pod's A2A listener root as JSON-RPC (ACC 2:
/// there is no served-MCP `/mcp` any more; the admin verbs ride the A2A
/// endpoint and are accepted case-insensitively in both `a2a.X` and bare
/// spellings). A bounded timeout keeps a single hung replica from stalling a
/// fleet fan-out.
async fn forward_verb_to_ip(
    http: &reqwest::Client,
    pod_ip: &str,
    verb: &str,
) -> Result<String, String> {
    let method = verb_to_method(verb)?;
    let url = format!("https://{pod_ip}:8443/");
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

// --- P3-5 state-plane lifecycle verbs --------------------------------------
// backup/restore/reset act on the durable checkpoint through the state
// service's admin tools; stop/start flip the CR's `lifecycle.paused` (the
// operator parks the managed pod, the central-Postgres state persists);
// migrate replaces the pod and PROVES the checkpoint survived (zero run loss).

/// The subject prefix every managed agent's checkpoint keys sit under — the
/// operator stamps `x-mcpg-subject-id: orgs/<ns>/<name>` on the store binding,
/// so its keys are `orgs/<ns>/<name>/…` and this is the admin backup unit.
fn agent_prefix(ns: &str, name: &str) -> String {
    format!("orgs/{ns}/{name}/")
}

#[tracing::instrument(skip_all, fields(ns = %ns, agent = %name, verb = %verb))]
async fn handle_lifecycle_verb(
    state: &AppState,
    ns: &str,
    name: &str,
    verb: &str,
    user: &str,
    body: &axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    match lifecycle_verb(state, ns, name, verb, body).await {
        Ok((msg, data)) => {
            state.metrics.inc_forwarded();
            tracing::info!(%ns, agent = %name, %verb, %user, "lifecycle verb applied");
            status_data(
                StatusCode::OK,
                "Success",
                &format!("{verb} {ns}/{name} by {user}: {msg}"),
                data,
            )
        }
        Err(e) => {
            state.metrics.inc_error();
            tracing::error!(error = %e, %verb, "lifecycle verb failed");
            let code = if e.starts_with("state plane is off") {
                StatusCode::SERVICE_UNAVAILABLE
            } else if let Some(rest) = e.strip_prefix("precondition: ") {
                return status(StatusCode::CONFLICT, "Failure", rest);
            } else {
                StatusCode::BAD_GATEWAY
            };
            status(code, "Failure", &e)
        }
    }
}

/// The state-plane verb body: returns `(human message, optional data blob)`.
async fn lifecycle_verb(
    state: &AppState,
    ns: &str,
    name: &str,
    verb: &str,
    body: &axum::body::Bytes,
) -> Result<(String, Value), String> {
    match verb {
        "stop" => {
            set_paused(&state.client, ns, name, true).await?;
            Ok((
                "paused — pod parked, managed state preserved".into(),
                Value::Null,
            ))
        }
        "start" => {
            set_paused(&state.client, ns, name, false).await?;
            Ok(("resumed".into(), Value::Null))
        }
        "backup" => {
            let url = state_url(state)?;
            // Page the whole prefix (keyset walk) — a large managed agent's
            // export exceeds one FFI frame (docs/v2/known-limits.md).
            let items = snapshot_all(&state.na, url, &agent_prefix(ns, name)).await?;
            let n = items.len();
            Ok((format!("{n} checkpoint rows"), json!({ "items": items })))
        }
        "restore" => {
            let url = state_url(state)?;
            let items = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| v.get("items").cloned())
                .ok_or("restore needs a JSON body {\"items\": [...]} from `agentctl backup`")?;
            let sc = state_admin_call(
                &state.na,
                url,
                "state.admin.restore",
                json!({ "items": items }),
            )
            .await?;
            let n = sc.get("restored").and_then(Value::as_i64).unwrap_or(0);
            Ok((format!("{n} rows restored"), Value::Null))
        }
        "reset" => {
            let url = state_url(state)?;
            let sc = state_admin_call(
                &state.na,
                url,
                "state.admin.purge",
                json!({ "prefix": agent_prefix(ns, name) }),
            )
            .await?;
            let n = sc.get("rows_affected").and_then(Value::as_i64).unwrap_or(0);
            Ok((format!("{n} rows purged (fresh start)"), Value::Null))
        }
        "migrate" => migrate_agent(state, ns, name).await,
        other => Err(format!("unmapped lifecycle verb: {other}")),
    }
}

fn state_url(state: &AppState) -> Result<&str, String> {
    state
        .state_url
        .as_deref()
        .ok_or_else(|| "state plane is off (AGENTCTL_STATE_URL unset)".to_string())
}

/// Patch `spec.lifecycle.paused` on the Agent CR (dynamic — no typed dep). The
/// operator honours it: a paused daemon renders `replicas: 0` (P7-6).
async fn set_paused(client: &Client, ns: &str, name: &str, paused: bool) -> Result<(), String> {
    let gvk = GroupVersionKind::gvk("agentctl.dev", "v1alpha2", "Agent");
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
    api.patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(json!({ "spec": { "lifecycle": { "paused": paused } } })),
    )
    .await
    .map_err(|e| format!("patch agent {ns}/{name} paused={paused}: {e}"))?;
    Ok(())
}

/// Replace a managed agent's pod and prove the durable checkpoint survived.
async fn migrate_agent(state: &AppState, ns: &str, name: &str) -> Result<(String, Value), String> {
    let url = state_url(state)?;
    let class = agent_store_class(&state.client, ns, name).await?;
    if class != "managed" {
        return Err(format!(
            "precondition: migrate requires store.class=managed; {ns}/{name} is \
             `{class}` — ephemeral/local state is node-local and cannot move"
        ));
    }
    // The checkpoint high-water per key BEFORE the move (the zero-loss witness).
    let before = snapshot_seqs(&state.na, url, &agent_prefix(ns, name)).await?;
    let (pod, from_node) = one_agent_pod(&state.client, ns, name).await?;
    delete_pod(&state.client, ns, &pod).await?;
    let (new_pod, to_node) = wait_running_pod(&state.client, ns, name, &pod).await?;
    // The durable state lives in central Postgres, untouched by the pod
    // swap — assert every key's seq is preserved (never lost, never regressed).
    let after = snapshot_seqs(&state.na, url, &agent_prefix(ns, name)).await?;
    for (k, s0) in &before {
        match after.get(k) {
            Some(s1) if s1 >= s0 => {}
            Some(s1) => return Err(format!("checkpoint regressed for {k}: seq {s0} -> {s1}")),
            None => return Err(format!("checkpoint LOST for {k} (seq {s0}) after migrate")),
        }
    }
    let node_note = if from_node == to_node {
        format!("{from_node} (single-node cluster: same-node reschedule)")
    } else {
        format!("{from_node} -> {to_node}")
    };
    Ok((
        format!(
            "pod {pod} -> {new_pod}, node {node_note}; {} checkpoint keys preserved",
            before.len()
        ),
        json!({ "from_node": from_node, "to_node": to_node, "keys_preserved": before.len() }),
    ))
}

async fn agent_store_class(client: &Client, ns: &str, name: &str) -> Result<String, String> {
    let gvk = GroupVersionKind::gvk("agentctl.dev", "v1alpha2", "Agent");
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
    let obj = api
        .get(name)
        .await
        .map_err(|e| format!("get agent {ns}/{name}: {e}"))?;
    Ok(obj
        .data
        .pointer("/spec/store/class")
        .and_then(Value::as_str)
        .unwrap_or("ephemeral")
        .to_string())
}

/// One Running pod for the agent, with the node it sits on.
async fn one_agent_pod(client: &Client, ns: &str, name: &str) -> Result<(String, String), String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("agentctl.dev/agent={name}"));
    let p = pods
        .list(&lp)
        .await
        .map_err(|e| format!("list pods: {e}"))?
        .items
        .into_iter()
        .find(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .ok_or_else(|| format!("no Running pod for agent {ns}/{name}"))?;
    let pod_name = p.metadata.name.clone().unwrap_or_default();
    let node = p
        .spec
        .and_then(|s| s.node_name)
        .unwrap_or_else(|| "<unscheduled>".into());
    Ok((pod_name, node))
}

async fn delete_pod(client: &Client, ns: &str, pod: &str) -> Result<(), String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    pods.delete(pod, &DeleteParams::default())
        .await
        .map_err(|e| format!("delete pod {pod}: {e}"))?;
    Ok(())
}

/// Poll (bounded, ~40s) for a fresh Running pod whose name differs from the
/// deleted one — the reschedule's completion signal.
async fn wait_running_pod(
    client: &Client,
    ns: &str,
    name: &str,
    old: &str,
) -> Result<(String, String), String> {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
        let lp = ListParams::default().labels(&format!("agentctl.dev/agent={name}"));
        let Ok(list) = pods.list(&lp).await else {
            continue;
        };
        for p in list.items {
            let pn = p.metadata.name.clone().unwrap_or_default();
            if pn == old || pn.is_empty() {
                continue;
            }
            if p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running") {
                let node = p
                    .spec
                    .and_then(|s| s.node_name)
                    .unwrap_or_else(|| "<unscheduled>".into());
                return Ok((pn, node));
            }
        }
    }
    Err(format!(
        "timed out waiting for a replacement pod for {ns}/{name}"
    ))
}

/// Walk `state.admin.list` (keys-only) keyset pages → a `{key: seq}` map for the
/// zero-loss migrate compare. Keys-only stays tiny and paginated, so it never
/// trips the snapshot FFI frame cap regardless of how much state a prefix holds.
async fn snapshot_seqs(
    http: &reqwest::Client,
    url: &str,
    prefix: &str,
) -> Result<std::collections::BTreeMap<String, i64>, String> {
    let mut m = std::collections::BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        let mut args = json!({ "prefix": prefix });
        if let Some(a) = &after {
            args["after"] = json!(a);
        }
        let sc = state_admin_call(http, url, "state.admin.list", args).await?;
        let items = sc
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            break;
        }
        let next = sc.get("next").and_then(Value::as_str).map(str::to_string);
        for it in &items {
            if let (Some(k), Some(s)) = (
                it.get("key").and_then(Value::as_str),
                it.get("seq").and_then(Value::as_i64),
            ) {
                m.insert(k.to_string(), s);
            }
        }
        match next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    Ok(m)
}

/// One MCP `tools/call` against the state service (initialize → call), over
/// the shared chart-CA-trusting mTLS client. Returns the tool's
/// `structuredContent`.
async fn state_admin_call(
    http: &reqwest::Client,
    base_url: &str,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    let mcp = format!("{}/mcp", base_url.trim_end_matches('/'));
    let (_, session) = state_post(
        http,
        &mcp,
        None,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "agentctl-apiserver", "version": "0" } } }),
    )
    .await?;
    let _ = state_post(
        http,
        &mcp,
        session.clone(),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    let (body, _) = state_post(
        http,
        &mcp,
        session,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": tool, "arguments": args } }),
    )
    .await?;
    if let Some(err) = body.pointer("/error") {
        return Err(format!("state {tool}: {err}"));
    }
    // A tool-execution error is a SUCCESSFUL JSON-RPC response with
    // result.isError — surface it (e.g. the 256 KiB FFI snapshot payload cap)
    // instead of reading the null structuredContent as an empty result.
    if body.pointer("/result/isError").and_then(Value::as_bool) == Some(true) {
        let msg = body
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("tool execution error");
        return Err(format!("state {tool}: {msg}"));
    }
    body.pointer("/result/structuredContent")
        .cloned()
        .ok_or_else(|| format!("state {tool}: no structuredContent in response"))
}

/// Walk `state.admin.snapshot` keyset pages and concatenate every {key, seq,
/// state} row under `prefix`. The whole prefix cannot ride one FFI frame
/// (256 KiB host cap), so a backup pages it — `next` is the resume cursor, an
/// empty page ends the walk (docs/v2/known-limits.md).
async fn snapshot_all(
    http: &reqwest::Client,
    url: &str,
    prefix: &str,
) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut args = json!({ "prefix": prefix });
        if let Some(a) = &after {
            args["after"] = json!(a);
        }
        let sc = state_admin_call(http, url, "state.admin.snapshot", args).await?;
        let items = sc
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            break;
        }
        let next = sc.get("next").and_then(Value::as_str).map(str::to_string);
        out.extend(items);
        match next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    Ok(out)
}

/// One streamable-HTTP POST to the state gateway, returning the last
/// result/error frame and any session id. mcpg interleaves log notifications,
/// so `.rfind` the frame that actually carries a result/error.
async fn state_post(
    http: &reqwest::Client,
    mcp: &str,
    session: Option<String>,
    body: Value,
) -> Result<(Value, Option<String>), String> {
    let mut req = http
        .post(mcp)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25")
        .header("x-mcpg-subject-id", "control-plane/apiserver")
        .timeout(Duration::from_secs(30))
        .json(&body);
    if let Some(s) = &session {
        req = req.header("mcp-session-id", s.clone());
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("state POST {mcp}: {e}"))?;
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = resp
        .text()
        .await
        .map_err(|e| format!("state read body: {e}"))?;
    let v = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .rfind(|v| v.get("result").is_some() || v.get("error").is_some())
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    Ok((v, sid))
}

fn status_data(
    code: StatusCode,
    kind: &str,
    message: &str,
    data: Value,
) -> (StatusCode, Json<Value>) {
    let mut obj = json!({
        "kind": "Status", "apiVersion": "v1", "status": kind,
        "message": message, "code": code.as_u16()
    });
    if !data.is_null() {
        if let Some(map) = obj.as_object_mut() {
            map.insert("data".into(), data);
        }
    }
    (code, Json(obj))
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
            { "name": "agentfleets/resume", "singularName": "", "namespaced": true, "kind": "AgentFleet", "verbs": ["create"] },
            { "name": "metering", "singularName": "", "namespaced": false, "kind": "Metering", "verbs": ["get"] },
            { "name": "metering/export", "singularName": "", "namespaced": false, "kind": "Metering", "verbs": ["get", "create"] },
            { "name": "audit", "singularName": "", "namespaced": false, "kind": "Audit", "verbs": ["get"] },
            { "name": "audit/query", "singularName": "", "namespaced": false, "kind": "Audit", "verbs": ["get", "create"] },
        ],
    }))
}

async fn not_found() -> (StatusCode, Json<Value>) {
    status(StatusCode::NOT_FOUND, "Failure", "not found")
}
