// SPDX-License-Identifier: BUSL-1.1
//! The node-agent **INFER-PROXY** for the networkless / Kata tier (RFC 0012/0015).
//!
//! Networkless (and Kata) agents have **no pod IP**, so the ModelGateway's
//! source-IP attestation cannot identify them. Instead each such agent mounts a
//! unix socket the node-agent serves and dials it as its `AGENT_INTELLIGENCE`
//! endpoint. On every accepted connection the node-agent reads the connecting
//! process's **kernel-attested** pid (`SO_PEERCRED`, reused from
//! [`crate::attest::peer_pid_of_fd`] — the node-agent runs `hostPID`, so the pid
//! is host-namespaced), resolves it to a pod UID via `/proc/<pid>/cgroup`
//! ([`pod_uid_for_pid`]), and forwards the request to the ModelGateway carrying
//! that **attested** UID. If the UID cannot be resolved the request is denied
//! (`403`) — the proxy fails closed.
//!
//! The agent **cannot self-assert identity**: any client-supplied `X-Agent-*`
//! identity header is stripped before forwarding, and only the node-agent's
//! attested `X-Agent-Pod-Uid` (plus `X-Agent-Attested-By: node-agent`) is sent
//! upstream. The proxy is otherwise a transparent OpenAI-compatible passthrough:
//! it preserves method, path+query, body, and content-type, and relays the
//! ModelGateway response verbatim (streaming/SSE bodies are piped straight
//! through).
//!
//! Enabled only when `NODE_AGENT_INFER_SOCKET` (the socket path) **and**
//! `MODELGATEWAY_URL` (the upstream) are both set; see [`ProxyConfig::from_env`].

use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::serve::IncomingStream;
use axum::{Json, Router};
use serde_json::json;
use tokio::net::UnixListener;

use crate::attest::{peer_pid_of_fd, pod_uid_for_pid};

/// The header carrying the node-agent-attested pod UID (the ONLY trusted
/// identity the ModelGateway sees from a networkless caller).
pub const HEADER_POD_UID: &str = "x-agent-pod-uid";
/// Marks the request as identity-attested by the node-agent (not self-asserted).
pub const HEADER_ATTESTED_BY: &str = "x-agent-attested-by";
const ATTESTED_BY_VALUE: &str = "node-agent";

/// Inbound identity headers the agent is NOT allowed to assert about itself; we
/// strip every one before forwarding so a malicious agent can never spoof its
/// pod UID, attestation claim, namespace, or name. Only the node-agent's
/// `SO_PEERCRED`-attested UID is trusted.
const STRIPPED_IDENTITY: [&str; 4] = [
    "x-agent-pod-uid",
    "x-agent-attested-by",
    "x-agent-namespace",
    "x-agent-name",
];

/// Cap on a buffered infer request body (16 MiB). Infer requests are JSON
/// (prompts/messages); the proxy buffers the request body once, then streams the
/// response back.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Static config for the infer-proxy, resolved from the environment.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// The unix socket path the proxy listens on (`NODE_AGENT_INFER_SOCKET`).
    pub socket_path: PathBuf,
    /// The ModelGateway base URL to forward to (`MODELGATEWAY_URL`).
    pub target: String,
    /// Optional bearer presented to the ModelGateway (`AGENTCTL_API_TOKEN`).
    pub api_token: Option<String>,
}

impl ProxyConfig {
    /// Resolve the config from the environment. Returns `None` (proxy disabled)
    /// unless BOTH `NODE_AGENT_INFER_SOCKET` and `MODELGATEWAY_URL` are set.
    pub fn from_env() -> Option<Self> {
        let socket_path = std::env::var("NODE_AGENT_INFER_SOCKET").ok()?;
        let target = std::env::var("MODELGATEWAY_URL").ok()?;
        let api_token = std::env::var("AGENTCTL_API_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        Some(Self {
            socket_path: PathBuf::from(socket_path),
            target,
            api_token,
        })
    }
}

/// Shared, cheaply-cloneable proxy state behind an `Arc`.
struct ProxyState {
    client: reqwest::Client,
    target: String,
    api_token: Option<String>,
}

/// The per-connection attestation, derived at accept time from `SO_PEERCRED`.
///
/// `pod_uid` is `Some` only when the connecting process's pid resolved to a pod
/// UID via `/proc`; otherwise the caller could not be attested and the proxy
/// fails closed (`403`).
#[derive(Clone, Debug)]
pub struct PeerCred {
    /// The attested pod UID of the connecting agent, if resolvable.
    pub pod_uid: Option<String>,
    /// The kernel-attested peer pid (host-namespaced under `hostPID`), for logs.
    pub pid: Option<u32>,
}

impl axum::extract::connect_info::Connected<IncomingStream<'_, UnixListener>> for PeerCred {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        // The node-agent is the socket SERVER here; `SO_PEERCRED` on the accepted
        // stream yields the connecting agent's pid. Resolve it to a pod UID via
        // /proc (the only lookup the node-agent needs — no cluster reads).
        let pid = peer_pid_of_fd(stream.io().as_raw_fd());
        let pod_uid = pid.and_then(pod_uid_for_pid);
        PeerCred { pod_uid, pid }
    }
}

/// Bind the proxy socket and serve forever (one accept loop, attested per
/// connection). Removes any stale socket, creates the parent dir, and makes the
/// socket world-connectable (`0o666`) — identity is enforced by `SO_PEERCRED`
/// attestation, not file permissions, and the agent pod may run as a different
/// uid than the (root, hostPID) node-agent.
pub async fn serve(config: ProxyConfig) -> std::io::Result<()> {
    if let Some(parent) = config.socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A stale socket from a prior run would make bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(&config.socket_path);
    let listener = UnixListener::bind(&config.socket_path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.socket_path, std::fs::Permissions::from_mode(0o666))?;
    }

    let state = Arc::new(ProxyState {
        client: reqwest::Client::new(),
        // Make the upstream host an ABSOLUTE (fully-qualified) name so the forward
        // is resolved directly and never re-expanded through the resolver's
        // `search` list — see [`absolutize_host`].
        target: absolutize_host(&config.target),
        api_token: config.api_token,
    });
    let app = Router::new().fallback(proxy_handler).with_state(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<PeerCred>(),
    )
    .await
}

/// The catch-all proxy handler: attest, then forward. Every method + path lands
/// here (it is the router fallback), so `/v1/infer` and any other `/v1/*` path
/// pass through transparently.
async fn proxy_handler(
    ConnectInfo(peer): ConnectInfo<PeerCred>,
    State(state): State<Arc<ProxyState>>,
    req: Request,
) -> Response {
    handle(&state, peer.pod_uid.as_deref(), peer.pid, req).await
}

/// The attest-or-forward decision, factored out for direct unit testing.
async fn handle(
    state: &ProxyState,
    attested_uid: Option<&str>,
    pid: Option<u32>,
    req: Request,
) -> Response {
    match attested_uid {
        Some(uid) => forward(state, uid, req).await,
        // Could not attest the caller → fail closed.
        None => {
            eprintln!(
                "node-agent infer-proxy: DENY — cannot attest caller (peer_pid {pid:?}); failing closed"
            );
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "cannot attest caller pod; infer denied" })),
            )
                .into_response()
        }
    }
}

/// Forward an attested request to the ModelGateway and relay its response. The
/// request body is buffered once (infer bodies are JSON); the response body is
/// streamed straight back so SSE/streaming completions pipe through untouched.
async fn forward(state: &ProxyState, attested_uid: &str, req: Request) -> Response {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let inbound_headers = req.headers().clone();

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("read request body: {e}")).into_response()
        }
    };

    let url = format!("{}{}", state.target.trim_end_matches('/'), path_and_query);
    tracing::debug!(
        %url,
        uid = %attested_uid,
        "infer-proxy: forwarding attested request to the ModelGateway"
    );
    let headers = build_forward_headers(&inbound_headers, attested_uid);
    let mut rb = state
        .client
        .request(method, &url)
        .headers(headers)
        .body(body_bytes);
    // Present the control-plane bearer to the ModelGateway, if configured.
    if let Some(token) = &state.api_token {
        rb = rb.bearer_auth(token);
    }

    match rb.send().await {
        Ok(resp) => relay_response(resp),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("modelgateway {url}: {e}") })),
        )
            .into_response(),
    }
}

/// Build the forwarded header set: copy the inbound headers EXCEPT the
/// client-asserted identity headers (stripped — the agent cannot self-identify)
/// and hop-by-hop/framing headers (reqwest sets its own), then inject the
/// node-agent-attested identity.
fn build_forward_headers(inbound: &HeaderMap, attested_uid: &str) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        if is_stripped_request_header(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    // Inject the attested identity (overwrites any survivor by construction).
    if let Ok(v) = HeaderValue::from_str(attested_uid) {
        out.insert(HeaderName::from_static(HEADER_POD_UID), v);
    }
    out.insert(
        HeaderName::from_static(HEADER_ATTESTED_BY),
        HeaderValue::from_static(ATTESTED_BY_VALUE),
    );
    out
}

/// Relay the ModelGateway response: status + headers (minus hop-by-hop/framing)
/// with the body streamed straight through.
fn relay_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let upstream_headers = resp.headers().clone();
    let mut builder = Response::builder().status(status);
    if let Some(h) = builder.headers_mut() {
        for (name, value) in &upstream_headers {
            if is_hop_by_hop(name.as_str()) {
                continue;
            }
            h.append(name.clone(), value.clone());
        }
    }
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("relay modelgateway response: {e}"),
            )
                .into_response()
        })
}

/// A header that must not be forwarded on the request hop: a client-asserted
/// identity header (security — stripped) or a hop-by-hop/framing header.
fn is_stripped_request_header(name: &HeaderName) -> bool {
    let n = name.as_str();
    STRIPPED_IDENTITY.contains(&n) || is_hop_by_hop(n)
}

/// Hop-by-hop / framing headers the HTTP client (reqwest) and server (hyper)
/// manage themselves; copying them through corrupts framing.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "connection"
            | "transfer-encoding"
            | "keep-alive"
            | "proxy-connection"
            | "upgrade"
            | "te"
            | "trailer"
    )
}

/// Rewrite a base URL so its **host is fully qualified** (absolute — a trailing
/// dot), so the resolver looks it up directly instead of treating it as a
/// relative name and walking its `search` list first.
///
/// The in-cluster upstream is a Service name like
/// `agentctl-modelgateway.agentctl-system.svc.cluster.local` — **4 dots**. Under
/// the Kubernetes default `options ndots:5`, a name with fewer than 5 dots is
/// treated as RELATIVE: the stub resolver tries every `search` domain BEFORE the
/// name as given. When the node's `/etc/resolv.conf` contributes an extra search
/// domain backed by a **wildcard** record (e.g. a corporate `*.example.org`), the
/// expansion `…svc.cluster.local.example.org` resolves to a FOREIGN host first —
/// so the attested infer forward is silently delivered to the wrong server (it
/// `404`s, and the ModelGateway is never reached). Appending a trailing dot marks
/// the name ABSOLUTE, so the `search` list is skipped and the Service name wins.
///
/// Only a DNS-name host is rewritten: an IP literal (v4 or bracketed `[v6]`), an
/// empty host, or a host that is already absolute (`host.`) is returned verbatim.
/// Any optional `userinfo@`, `:port`, path, query and fragment are preserved.
fn absolutize_host(target: &str) -> String {
    // Need a `scheme://` to locate the authority; otherwise leave it untouched.
    let Some(scheme_end) = target.find("://") else {
        return target.to_string();
    };
    let (scheme, rest) = target.split_at(scheme_end + 3); // keep the "://"
                                                          // The authority runs to the first '/', '?' or '#'.
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    // Strip optional `userinfo@` (kept verbatim, re-emitted unchanged).
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(at) => (&authority[..=at], &authority[at + 1..]),
        None => ("", authority),
    };
    // An IPv6 literal is bracketed (`[::1]`, possibly `:port` after) — never an
    // absolutizable DNS name; leave it alone.
    if hostport.starts_with('[') {
        return target.to_string();
    }
    // Split a trailing `:port` (only a non-empty all-digits suffix counts as one).
    let (host, port) = match hostport.rfind(':') {
        Some(c)
            if !hostport[c + 1..].is_empty()
                && hostport[c + 1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            (&hostport[..c], &hostport[c..]) // `port` retains its ':'
        }
        _ => (hostport, ""),
    };
    // Leave an IP literal, an empty host, or an already-absolute host untouched.
    if host.is_empty() || host.ends_with('.') || host.parse::<std::net::IpAddr>().is_ok() {
        return target.to_string();
    }
    format!("{scheme}{userinfo}{host}.{port}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::http::{Method, Uri};

    fn val(map: &HeaderMap, name: &str) -> Option<String> {
        map.get(name).map(|v| v.to_str().unwrap().to_string())
    }

    #[test]
    fn strips_client_identity_and_injects_attested() {
        let mut inbound = HeaderMap::new();
        inbound.insert("content-type", HeaderValue::from_static("application/json"));
        // Every client-asserted identity header must be dropped...
        inbound.insert("x-agent-pod-uid", HeaderValue::from_static("spoofed-uid"));
        inbound.insert("x-agent-attested-by", HeaderValue::from_static("self"));
        inbound.insert("x-agent-namespace", HeaderValue::from_static("evil-ns"));
        inbound.insert("x-agent-name", HeaderValue::from_static("evil-name"));
        // ...along with hop-by-hop/framing headers.
        inbound.insert("host", HeaderValue::from_static("attacker"));
        inbound.insert("content-length", HeaderValue::from_static("999"));

        let out = build_forward_headers(&inbound, "real-uid-1234");

        // The attested identity is injected, exactly once, with the real UID.
        assert_eq!(
            val(&out, "x-agent-pod-uid").as_deref(),
            Some("real-uid-1234")
        );
        assert_eq!(
            val(&out, "x-agent-attested-by").as_deref(),
            Some("node-agent")
        );
        assert_eq!(out.get_all("x-agent-pod-uid").iter().count(), 1);
        assert_eq!(out.get_all("x-agent-attested-by").iter().count(), 1);
        // The spoofable client identity headers are gone.
        assert!(out.get("x-agent-namespace").is_none());
        assert!(out.get("x-agent-name").is_none());
        // Hop-by-hop/framing stripped; a normal passthrough header preserved.
        assert!(out.get("host").is_none());
        assert!(out.get("content-length").is_none());
        assert_eq!(
            val(&out, "content-type").as_deref(),
            Some("application/json")
        );
    }

    #[test]
    fn absolutize_host_qualifies_in_cluster_service_name() {
        // The real bug: a 4-dot Service name is relative under ndots:5 and leaks
        // through the search list; the trailing dot pins it absolute.
        assert_eq!(
            absolutize_host("http://agentctl-modelgateway.agentctl-system.svc.cluster.local"),
            "http://agentctl-modelgateway.agentctl-system.svc.cluster.local."
        );
    }

    #[test]
    fn absolutize_host_preserves_port_and_path() {
        assert_eq!(
            absolutize_host("http://svc.ns.svc.cluster.local:8080"),
            "http://svc.ns.svc.cluster.local.:8080"
        );
        assert_eq!(
            absolutize_host("http://svc.ns.svc.cluster.local/base/path"),
            "http://svc.ns.svc.cluster.local./base/path"
        );
        assert_eq!(
            absolutize_host("https://svc.ns.svc.cluster.local:8443/v1/infer?x=1"),
            "https://svc.ns.svc.cluster.local.:8443/v1/infer?x=1"
        );
    }

    #[test]
    fn absolutize_host_preserves_userinfo() {
        assert_eq!(
            absolutize_host("http://user:pw@svc.ns.svc.cluster.local:80/p"),
            "http://user:pw@svc.ns.svc.cluster.local.:80/p"
        );
    }

    #[test]
    fn absolutize_host_is_idempotent_when_already_absolute() {
        assert_eq!(
            absolutize_host("http://svc.ns.svc.cluster.local."),
            "http://svc.ns.svc.cluster.local."
        );
        assert_eq!(
            absolutize_host("http://svc.ns.svc.cluster.local.:8080/p"),
            "http://svc.ns.svc.cluster.local.:8080/p"
        );
    }

    #[test]
    fn absolutize_host_leaves_ip_literals_untouched() {
        assert_eq!(
            absolutize_host("http://10.96.193.113"),
            "http://10.96.193.113"
        );
        assert_eq!(
            absolutize_host("http://10.96.193.113:80/v1/infer"),
            "http://10.96.193.113:80/v1/infer"
        );
        assert_eq!(
            absolutize_host("http://[fd00::1]:8080"),
            "http://[fd00::1]:8080"
        );
        assert_eq!(absolutize_host("http://[::1]"), "http://[::1]");
    }

    #[test]
    fn absolutize_host_handles_single_label_and_missing_scheme() {
        // A single-label host is still made absolute (correct + harmless).
        assert_eq!(
            absolutize_host("http://modelgateway"),
            "http://modelgateway."
        );
        // No scheme → cannot locate the authority safely → returned verbatim.
        assert_eq!(absolutize_host("modelgateway.svc"), "modelgateway.svc");
    }

    #[tokio::test]
    async fn unattested_caller_is_denied_403() {
        // attested_uid = None short-circuits BEFORE any network call, so a bogus
        // target is never dialed.
        let state = ProxyState {
            client: reqwest::Client::new(),
            target: "http://127.0.0.1:1".into(),
            api_token: None,
        };
        let req = Request::builder()
            .method("POST")
            .uri("/v1/infer")
            .body(Body::from("{}"))
            .unwrap();
        let resp = handle(&state, None, Some(4321), req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Capture of what the loopback ModelGateway received.
    #[derive(Default)]
    struct Captured {
        method: Option<Method>,
        uri: Option<Uri>,
        headers: Option<HeaderMap>,
        body: Option<String>,
    }

    #[tokio::test]
    async fn forwards_attested_identity_and_relays_response() {
        use axum::routing::any;

        let captured: Arc<Mutex<Captured>> = Arc::new(Mutex::new(Captured::default()));
        let cap = captured.clone();
        let app = Router::new().fallback(any(
            move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
                let cap = cap.clone();
                async move {
                    let mut c = cap.lock().unwrap();
                    c.method = Some(method);
                    c.uri = Some(uri);
                    c.headers = Some(headers);
                    c.body = Some(body.clone());
                    (StatusCode::OK, format!("echoed:{body}"))
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let state = ProxyState {
            client: reqwest::Client::new(),
            target: format!("http://{addr}"),
            api_token: None,
        };
        let req = Request::builder()
            .method("POST")
            .uri("/v1/infer?stream=true")
            .header("content-type", "application/json")
            // The agent tries to spoof its identity — must be ignored.
            .header("x-agent-pod-uid", "spoofed")
            .header("x-agent-namespace", "evil")
            .body(Body::from(r#"{"model":"m"}"#))
            .unwrap();

        let resp = handle(&state, Some("attested-uid-9"), Some(42), req).await;

        // Response relayed verbatim (status + body).
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], br#"echoed:{"model":"m"}"#);

        let c = captured.lock().unwrap();
        // Method, path+query, and body preserved (transparent passthrough).
        assert_eq!(c.method.as_ref().unwrap(), Method::POST);
        let uri = c.uri.as_ref().unwrap();
        assert_eq!(uri.path(), "/v1/infer");
        assert_eq!(uri.query(), Some("stream=true"));
        assert_eq!(c.body.as_deref(), Some(r#"{"model":"m"}"#));
        // Only the node-agent-attested identity reached the upstream.
        let h = c.headers.as_ref().unwrap();
        assert_eq!(val(h, "x-agent-pod-uid").as_deref(), Some("attested-uid-9"));
        assert_eq!(val(h, "x-agent-attested-by").as_deref(), Some("node-agent"));
        assert!(h.get("x-agent-namespace").is_none());
        assert_eq!(val(h, "content-type").as_deref(), Some("application/json"));
    }
}
