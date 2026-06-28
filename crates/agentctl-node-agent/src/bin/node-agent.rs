//! The `node-agent` binary (RFC 0008, Tier A — control bridge).
//!
//! One per node (a DaemonSet). It (1) periodically **discovers** local agent
//! management sockets on the stock-unix hostPath and logs what each advertises,
//! and (2) serves a small **HTTP API** the aggregated apiserver (RFC 0009) calls
//! to drive a management verb against a specific local agent:
//!
//! ```text
//! POST /v1/agents/{pod_uid}/{verb}          # verb ∈ drain|lame-duck|cancel|status
//! POST /v1/agents/{pod_uid}/a2a             # bridge a reference a2a.* JSON-RPC request
//! POST /v1/agents/{pod_uid}/a2a/stream      # bridge a streaming a2a.* request → SSE
//! GET  /v1/agents/{pod_uid}/capabilities    # raw capabilities manifest (card projection)
//! GET  /healthz
//! ```
//!
//! The verb is executed by bridging to that pod's socket via the (blocking)
//! [`ManagementClient`], run on a blocking task.
//!
//! ## Two listeners (RFC 0015 — mTLS hardening)
//!
//! The node-agent serves two ports concurrently:
//!
//! * **`:8080` plaintext** — `GET /healthz` and `GET /metrics` only. The kubelet
//!   probes and Prometheus scrape present no client cert, so these stay open.
//! * **`:8443` mTLS** — every control route (`/v1/agents/...`). The TLS server
//!   presents `tls.crt`/`tls.key` and **requires** a client cert chained to the
//!   CA at `/etc/agentctl-node-agent/tls/ca.crt` (rustls `WebPkiClientVerifier`),
//!   so only the control plane (apiserver, gateway) — which holds a CA-signed
//!   client cert — can drive the verbs. No in-cluster pod can reach them.

use std::convert::Infallible;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentctl_node_agent::mgmt::URI_CAPABILITIES;
use agentctl_node_agent::{
    attest_decision, discover, metrics, pod_uid_for_pid, AttestMode, Attestation, DiscoveredAgent,
    Error, ManagementClient,
};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

/// The node-agent's serving cert/key + the CA used to verify control-plane client
/// certs, mounted as a Secret (provisioned separately; we only READ these files).
const TLS_DIR: &str = "/etc/agentctl-node-agent/tls";

#[tokio::main]
async fn main() {
    // mTLS via rustls with the ring provider (no aws-lc-rs → no C toolchain).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let root: PathBuf = std::env::var("AGENTCTL_SOCKET_ROOT")
        .unwrap_or_else(|_| "/run/agentctl/sockets".to_string())
        .into();
    let interval = Duration::from_secs(
        std::env::var("AGENTCTL_DISCOVERY_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30),
    );
    let plain_bind =
        std::env::var("AGENTCTL_NODE_AGENT_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let tls_bind =
        std::env::var("AGENTCTL_NODE_AGENT_TLS_ADDR").unwrap_or_else(|_| "0.0.0.0:8443".into());
    let plain_addr: SocketAddr = plain_bind
        .parse()
        .unwrap_or_else(|e| panic!("parse plaintext addr {plain_bind}: {e}"));
    let tls_addr: SocketAddr = tls_bind
        .parse()
        .unwrap_or_else(|e| panic!("parse TLS addr {tls_bind}: {e}"));

    // Background: periodic discovery + capability logging.
    tokio::spawn(discovery_loop(root.clone(), interval));

    // :8080 plaintext — kubelet probes + Prometheus scrape present no client cert,
    // so health and the metrics scrape-proxy stay open. The shared `root` state is
    // what the metrics handler needs to discover + scrape local agents.
    let plain = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics_handler))
        .with_state(root.clone());

    // :8443 mTLS — the control surface. Same shared `root` state; reached only by a
    // caller presenting a CA-signed client cert (enforced by the TLS layer below).
    let control = Router::new()
        .route("/v1/agents/{pod_uid}/a2a", post(a2a_handler))
        .route("/v1/agents/{pod_uid}/a2a/stream", post(a2a_stream_handler))
        .route(
            "/v1/agents/{pod_uid}/capabilities",
            get(capabilities_handler),
        )
        .route("/v1/agents/{pod_uid}/{verb}", post(verb_handler))
        .with_state(root);

    let tls = build_tls_config().expect("build node-agent mTLS server config");
    let tls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));

    let listener = tokio::net::TcpListener::bind(plain_addr)
        .await
        .unwrap_or_else(|e| panic!("bind {plain_addr}: {e}"));
    eprintln!("node-agent: plaintext (healthz, metrics) on {plain_addr}");
    eprintln!("node-agent: mTLS control API on {tls_addr}");

    // Run both listeners concurrently; if either exits the process should fail.
    let plain_srv = async move {
        axum::serve(listener, plain).await.expect("serve plaintext");
    };
    let tls_srv = async move {
        axum_server::bind_rustls(tls_addr, tls_config)
            .serve(control.into_make_service())
            .await
            .expect("serve mTLS");
    };
    tokio::join!(plain_srv, tls_srv);
}

// --- mTLS server config (RFC 0015) -----------------------------------------

/// rustls server config for the `:8443` control surface: present the node-agent's
/// serving cert AND **require** a client cert chained to the CA at
/// `{TLS_DIR}/ca.crt` (so only the control plane can reach the control verbs).
/// Mirrors the apiserver's `build_tls_config`.
fn build_tls_config() -> Result<ServerConfig, String> {
    let certs = load_certs(&PathBuf::from(TLS_DIR).join("tls.crt"))?;
    let key = load_key(&PathBuf::from(TLS_DIR).join("tls.key"))?;
    let client_ca = load_client_ca(&PathBuf::from(TLS_DIR).join("ca.crt"))?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(client_ca))
        .build()
        .map_err(|e| format!("client verifier: {e}"))?;
    ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("server config: {e}"))
}

/// Load the CA bundle used to VERIFY control-plane client certs.
fn load_client_ca(path: &std::path::Path) -> Result<RootCertStore, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut r) {
        roots
            .add(cert.map_err(|e| format!("parse CA: {e}"))?)
            .map_err(|e| format!("add CA: {e}"))?;
    }
    if roots.is_empty() {
        return Err(format!("no CA certs in {path:?}"));
    }
    Ok(roots)
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

// --- pod→socket attestation (RFC 0002 §7 / RFC 0015) -----------------------

/// A confirmed attestation mismatch under [`AttestMode::Enforce`]: the socket's
/// server process belongs to a DIFFERENT pod than the one requested. Drives the
/// HTTP 403 response body shared by every control handler.
struct AttestDenial {
    expected: String,
    got: String,
}

impl AttestDenial {
    /// The shared 403 response body.
    fn body(&self) -> Value {
        json!({
            "error": "socket attestation failed",
            "expected": self.expected,
            "got": self.got,
        })
    }
}

/// Read the connected peer's kernel-attested pid (`SO_PEERCRED`), resolve its pod
/// UID, and compare it against `requested_uid`. Returns `Err(AttestDenial)` only
/// on a CONFIRMED mismatch under [`AttestMode::Enforce`]; everything else
/// (attested, warn-mode mismatch, unresolved peer, or `Off`) returns `Ok(())` and
/// the caller proceeds. Fail-open on resolution failure — only a confirmed
/// mismatch denies.
///
/// Called inside the blocking task right after `connect`, before driving the
/// agent, so the 403 path never touches the agent.
fn attest_or_deny(
    client: &ManagementClient,
    requested_uid: &str,
    mode: AttestMode,
) -> Result<(), AttestDenial> {
    if matches!(mode, AttestMode::Off) {
        return Ok(());
    }
    let peer_pid = client.peer_pid();
    let resolved = peer_pid.and_then(pod_uid_for_pid);
    match attest_decision(requested_uid, resolved.as_deref()) {
        Attestation::Attested(uid) => {
            eprintln!(
                "node-agent: attested pod {uid} peer_pid {}",
                peer_pid.unwrap_or(0)
            );
            Ok(())
        }
        Attestation::Mismatch { expected, got } => {
            // Security event: a socket is impersonating another pod.
            eprintln!(
                "node-agent: SECURITY socket attestation failed: expected pod {expected} but peer_pid {} belongs to pod {got}",
                peer_pid.unwrap_or(0)
            );
            if matches!(mode, AttestMode::Enforce) {
                Err(AttestDenial { expected, got })
            } else {
                eprintln!("node-agent: attestation mode=warn — proceeding despite mismatch");
                Ok(())
            }
        }
        Attestation::Unresolved => {
            eprintln!("node-agent: peer pid unresolved; attestation skipped — is hostPID set?");
            Ok(())
        }
    }
}

/// The result of driving a control verb: a bridge/agent failure or a denial.
enum DriveError {
    /// Attestation denied the request under enforce mode → HTTP 403.
    Denied(AttestDenial),
    /// A bridge or agent error → the handler's existing error mapping.
    Bridge(String),
}

/// Execute a management verb against the local agent identified by `pod_uid`.
async fn verb_handler(
    State(root): State<PathBuf>,
    Path((pod_uid, verb)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    if !matches!(verb.as_str(), "drain" | "lame-duck" | "cancel" | "status") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": format!("unknown verb: {verb}") })),
        );
    }
    let socket = root.join(&pod_uid).join("mgmt.sock");
    if !socket.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no socket for pod {pod_uid}") })),
        );
    }

    // ManagementClient is blocking std → run it off the async runtime. Attest the
    // socket's server process against the requested pod UID right after connect
    // and before driving it; an enforced mismatch returns without touching the
    // agent so the 403 path stays clean.
    let mode = AttestMode::from_env();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DriveError> {
        let mut client =
            ManagementClient::connect(&socket).map_err(|e| DriveError::Bridge(e.to_string()))?;
        attest_or_deny(&client, &pod_uid, mode).map_err(DriveError::Denied)?;
        client
            .initialize()
            .map_err(|e| DriveError::Bridge(e.to_string()))?;
        client
            .call_tool(&verb, json!({}))
            .map_err(|e| DriveError::Bridge(e.to_string()))
    })
    .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, Json(json!({ "ok": true, "result": value }))),
        Ok(Err(DriveError::Denied(d))) => (StatusCode::FORBIDDEN, Json(d.body())),
        Ok(Err(DriveError::Bridge(e))) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": e })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

/// Bridge a **reference** A2A JSON-RPC request to the local agent and relay its
/// response (the MVP bridging chain, agentctl RFC 0013). The body is the
/// reference request verbatim (`{jsonrpc,id,method,params}`) — the gateway has
/// already translated the spec slash-form (`message/send`, …) to the reference
/// name (`a2a.SendMessage`, …). We forward `method`/`params` over the socket and
/// wrap the outcome back into a JSON-RPC envelope carrying the original `id`.
///
/// An agent-level JSON-RPC error (e.g. `TASK_NOT_FOUND` −32001) is relayed with
/// its own code; only a bridge-level failure (no socket, connect/handshake/IO)
/// is reported as −32011.
async fn a2a_handler(
    State(root): State<PathBuf>,
    Path(pod_uid): Path<String>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let socket = root.join(&pod_uid).join("mgmt.sock");

    /// An A2A bridge failure variant carrying either a relayed JSON-RPC error or
    /// an enforced attestation denial.
    enum A2aError {
        Denied(AttestDenial),
        Rpc(i64, String),
    }

    // ManagementClient is blocking std → run it off the async runtime. Attest the
    // socket's server process before forwarding the request.
    let mode = AttestMode::from_env();
    let bridged = tokio::task::spawn_blocking(move || -> Result<Value, A2aError> {
        if !socket.exists() {
            return Err(A2aError::Rpc(
                -32011,
                format!("no socket for pod {pod_uid}"),
            ));
        }
        let mut client =
            ManagementClient::connect(&socket).map_err(|e| A2aError::Rpc(-32011, e.to_string()))?;
        attest_or_deny(&client, &pod_uid, mode).map_err(A2aError::Denied)?;
        client
            .initialize()
            .map_err(|e| A2aError::Rpc(-32011, e.to_string()))?;
        client.call(&method, params).map_err(|e| match e {
            // A genuine agent error: relay its code so the gateway can map it.
            Error::Rpc { code, message } => A2aError::Rpc(code, message),
            // Transport/protocol/json: a bridge failure.
            other => A2aError::Rpc(-32011, other.to_string()),
        })
    })
    .await;

    match bridged {
        Ok(Ok(result)) => (
            StatusCode::OK,
            Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
        ),
        // A confirmed impersonation: deny at the HTTP layer, not via JSON-RPC.
        Ok(Err(A2aError::Denied(d))) => (StatusCode::FORBIDDEN, Json(d.body())),
        Ok(Err(A2aError::Rpc(code, message))) => (
            StatusCode::OK,
            Json(
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
            ),
        ),
        Err(e) => (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32011, "message": e.to_string() }
            })),
        ),
    }
}

/// Bridge a **streaming** reference A2A request to the local agent and relay the
/// frames it emits as Server-Sent Events (the A2A `message/stream` chain, RFC
/// 0020). The body is the reference request verbatim (`{jsonrpc,id,method,
/// params}`) — the gateway has already translated `message/stream` →
/// `a2a.SendStreamingMessage`. Each agent frame's `result` becomes one SSE
/// `data:` event; the stream closes after the terminal (`final: true`) frame.
///
/// [`ManagementClient`] is blocking, so a [`tokio::task::spawn_blocking`] worker
/// connects, handshakes, and drives [`ManagementClient::stream`], forwarding each
/// frame down an mpsc channel that the SSE response drains. A bridge-level
/// failure (no socket, connect/handshake/IO) is emitted as a single error event
/// (`{"error":{"code":-32011,"message":...}}`) and then the stream closes.
async fn a2a_stream_handler(
    State(root): State<PathBuf>,
    Path(pod_uid): Path<String>,
    Json(req): Json<Value>,
) -> Response {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let socket = root.join(&pod_uid).join("mgmt.sock");

    let bridge_err =
        |code: i64, message: String| json!({ "error": { "code": code, "message": message } });

    /// The outcome of the blocking pre-flight: connect + attest BEFORE we commit
    /// to the SSE response, so an enforced mismatch can return HTTP 403 cleanly
    /// rather than as an in-band SSE event.
    enum Preflight {
        /// Attested (or skipped): hand the already-connected client to the worker.
        Ready(Box<ManagementClient>),
        /// Enforced attestation denial → HTTP 403.
        Denied(AttestDenial),
        /// A connect-time bridge failure → a single SSE error event, as before.
        BridgeErr(i64, String),
    }

    // Pre-flight on a blocking task (connect is blocking std). ManagementClient
    // owns its UnixStream and is Send, so we can carry it back here and into the
    // streaming worker below.
    let mode = AttestMode::from_env();
    let pre = {
        let socket = socket.clone();
        let pod_uid = pod_uid.clone();
        tokio::task::spawn_blocking(move || -> Preflight {
            if !socket.exists() {
                return Preflight::BridgeErr(-32011, format!("no socket for pod {pod_uid}"));
            }
            let client = match ManagementClient::connect(&socket) {
                Ok(c) => c,
                Err(e) => return Preflight::BridgeErr(-32011, e.to_string()),
            };
            match attest_or_deny(&client, &pod_uid, mode) {
                Ok(()) => Preflight::Ready(Box::new(client)),
                Err(d) => Preflight::Denied(d),
            }
        })
        .await
    };

    let client = match pre {
        Ok(Preflight::Ready(client)) => *client,
        // A confirmed impersonation: deny at the HTTP layer, never open the SSE.
        Ok(Preflight::Denied(d)) => return (StatusCode::FORBIDDEN, Json(d.body())).into_response(),
        // Connect-time bridge failure: emit one SSE error event, then close.
        Ok(Preflight::BridgeErr(code, message)) => {
            let event = Event::default().data(bridge_err(code, message).to_string());
            let stream = tokio_stream::once(Ok::<_, Infallible>(event));
            return Sse::new(stream).into_response();
        }
        Err(e) => {
            let event = Event::default().data(bridge_err(-32011, e.to_string()).to_string());
            let stream = tokio_stream::once(Ok::<_, Infallible>(event));
            return Sse::new(stream).into_response();
        }
    };

    // Attested → drive the stream. The blocking worker feeds frames to the SSE
    // response through this channel; when it finishes (or fails), `tx` drops and
    // the stream closes.
    let (tx, rx) = mpsc::channel::<Value>(16);
    tokio::task::spawn_blocking(move || {
        let mut client = client;
        if let Err(e) = client.initialize() {
            let _ = tx.blocking_send(bridge_err(-32011, e.to_string()));
            return;
        }
        if let Err(e) = client.stream(&method, params, |frame| {
            let _ = tx.blocking_send(frame);
        }) {
            // Relay an agent JSON-RPC error with its own code; everything else is
            // a bridge failure (transport/protocol/json).
            let _ = tx.blocking_send(match e {
                Error::Rpc { code, message } => bridge_err(code, message),
                other => bridge_err(-32011, other.to_string()),
            });
        }
    });

    let events = ReceiverStream::new(rx)
        .map(|frame| Ok::<_, Infallible>(Event::default().data(frame.to_string())));
    Sse::new(events).into_response()
}

/// Fetch the local agent's capabilities manifest as **raw JSON** (RFC 0005
/// §3.6). The contract `Manifest` is deserialize-only, so we return the wire
/// text parsed straight to a [`Value`] — the lossless passthrough the gateway
/// projects into an Agent Card (RFC 0013), with no re-serialization round-trip.
async fn capabilities_handler(
    State(root): State<PathBuf>,
    Path(pod_uid): Path<String>,
) -> (StatusCode, Json<Value>) {
    let socket = root.join(&pod_uid).join("mgmt.sock");
    if !socket.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no socket for pod {pod_uid}") })),
        );
    }

    let mode = AttestMode::from_env();
    let fetched = tokio::task::spawn_blocking(move || -> Result<Value, DriveError> {
        let mut client =
            ManagementClient::connect(&socket).map_err(|e| DriveError::Bridge(e.to_string()))?;
        attest_or_deny(&client, &pod_uid, mode).map_err(DriveError::Denied)?;
        client
            .initialize()
            .map_err(|e| DriveError::Bridge(e.to_string()))?;
        let text = client
            .read_resource_text(URI_CAPABILITIES)
            .map_err(|e| DriveError::Bridge(e.to_string()))?;
        serde_json::from_str::<Value>(&text).map_err(|e| DriveError::Bridge(e.to_string()))
    })
    .await;

    match fetched {
        Ok(Ok(manifest)) => (StatusCode::OK, Json(manifest)),
        Ok(Err(DriveError::Denied(d))) => (StatusCode::FORBIDDEN, Json(d.body())),
        Ok(Err(DriveError::Bridge(e))) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": e })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

/// Scrape-proxy: read every local agent's metrics over the socket and re-expose
/// them as one Prometheus exposition (RFC 0010). Networkless agents stay
/// observable; Prometheus scrapes this node-agent endpoint.
async fn metrics_handler(
    State(root): State<PathBuf>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    let agents = discover(&root).unwrap_or_default();
    let mut collected: Vec<(String, String)> = Vec::new();
    for agent in agents {
        let socket = agent.socket.clone();
        let scraped = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut client = ManagementClient::connect(&socket).map_err(|e| e.to_string())?;
            client.initialize().map_err(|e| e.to_string())?;
            client
                .read_resource_text("agentd://metrics")
                .map_err(|e| e.to_string())
        })
        .await;
        match scraped {
            Ok(Ok(text)) => collected.push((agent.pod_uid, text)),
            Ok(Err(e)) => eprintln!("node-agent: scrape {} failed: {e}", agent.pod_uid),
            Err(e) => eprintln!("node-agent: scrape task panicked: {e}"),
        }
    }
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        metrics::merge(&collected),
    )
}

async fn discovery_loop(root: PathBuf, interval: Duration) {
    eprintln!(
        "node-agent: discovering management sockets under {}",
        root.display()
    );
    loop {
        match discover(&root) {
            Ok(agents) => {
                eprintln!("node-agent: discovered {} agent socket(s)", agents.len());
                for agent in &agents {
                    match probe(agent) {
                        Ok(line) => eprintln!("  + {line}"),
                        Err(e) => eprintln!("  ! pod {} probe failed: {e}", agent.pod_uid),
                    }
                }
            }
            Err(e) => eprintln!("node-agent: discovery error: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}

fn probe(agent: &DiscoveredAgent) -> Result<String, Box<dyn std::error::Error>> {
    let mut client = ManagementClient::connect(&agent.socket)?;
    // Best-effort attestation for observability only (RFC 0015): confirm the
    // socket's server process belongs to the pod whose subdir it was found in.
    let peer_pid = client.peer_pid();
    let resolved = peer_pid.and_then(pod_uid_for_pid);
    match attest_decision(&agent.pod_uid, resolved.as_deref()) {
        Attestation::Attested(uid) => {
            eprintln!("    attested pod {uid} peer_pid {}", peer_pid.unwrap_or(0))
        }
        Attestation::Mismatch { expected, got } => eprintln!(
            "    SECURITY discovery attestation MISMATCH: subdir pod {expected} but peer belongs to pod {got}"
        ),
        // Unresolved is expected without hostPID/host /proc — stay quiet.
        Attestation::Unresolved => {}
    }
    client.initialize()?;
    let manifest = client.read_capabilities()?;
    Ok(format!(
        "pod {} -> server={} contract={} mode={:?}",
        agent.pod_uid,
        client.server_name.as_deref().unwrap_or("?"),
        manifest.contract_version,
        manifest.mode.as_deref().unwrap_or("?"),
    ))
}
