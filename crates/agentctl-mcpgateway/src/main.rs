// SPDX-License-Identifier: BUSL-1.1
//! # agentctl-mcpgateway
//!
//! The tool-plane broker (RFC 0019): the ModelGateway pattern applied to MCP.
//! A networkless agent holds **no** tool-server credential; it dials this
//! gateway keyless (`--mcp <name>=https://…/s/<name>`), and the gateway:
//!
//! 1. **attests** the calling agent by its (unforgeable) source IP → `(ns,
//!    agent)`, exactly as the ModelGateway does;
//! 2. **scopes** it to only the servers of the `MCPServerSet`s its `Agent` CR
//!    binds (`spec.mcpServerSetRefs`) — the RFC 0019 §5 peer→Agent→allowed-servers
//!    authorization, so one tenant cannot reach another's tool server;
//! 3. **injects** the server's credential (read from a `Secret`, held off-pod)
//!    onto the upstream hop;
//! 4. **forwards** the MCP JSON-RPC transparently (a header-injecting reverse
//!    proxy — the Streamable-HTTP session + SSE flow straight through, so no MCP
//!    state is terminated here).
//!
//! Server-auth-only TLS (the agent trusts our cert via `--tls-ca`; the agent's
//! identity is the source IP, not a client cert). v1 auth is `staticToken` (an
//! off-pod bearer); the OAuth/EMA tiers (RFC 0019 §6/§7) extend `McpAuthMode`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use agent_api::{Agent, MCPServerSet, McpAuthMode, McpServer};
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::ListParams;
use kube::{Api, Client};

/// The upstream MCP request headers we forward (Streamable HTTP). We deliberately
/// do NOT forward `authorization` (the gateway owns it — anti-spoof) or `origin`
/// (agentd 403s cross-origin).
const FORWARD_HEADERS: &[&str] = &[
    "content-type",
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
];

#[derive(Clone)]
struct AppState {
    client: Client,
    http: reqwest::Client,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let client = Client::try_default().await.expect("in-cluster kube client");
    let http = reqwest::Client::builder()
        .build()
        .expect("build upstream http client");
    let state = AppState { client, http };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        // The agent dials `…/s/<server>` for every MCP request (Streamable HTTP
        // POSTs to the one URL). `{*rest}` also accepts a trailing path if a
        // server exposes one.
        .route("/s/{server}", post(proxy))
        .route("/s/{server}/{*rest}", post(proxy))
        .with_state(state);

    // Server-auth-only TLS (agents dial keyless; identity = source IP). Enabled
    // when MCPGATEWAY_TLS_ADDR + _DIR are set; otherwise plaintext :8080 (dev).
    if let (Ok(tls_addr), Ok(tls_dir)) = (
        std::env::var("MCPGATEWAY_TLS_ADDR"),
        std::env::var("MCPGATEWAY_TLS_DIR"),
    ) {
        let tls_addr: SocketAddr = tls_addr.parse().expect("parse MCPGATEWAY_TLS_ADDR");
        let cfg =
            tls_server_config(std::path::Path::new(&tls_dir)).expect("build mcpgateway TLS config");
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(cfg));
        tracing::info!(%tls_addr, dir = %tls_dir, "mcpgateway TLS listener (keyless agent dials)");
        axum_server::bind_rustls(tls_addr, rustls_config)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .expect("serve mcpgateway TLS");
    } else {
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        tracing::info!(%addr, "mcpgateway serving (plaintext — dev)");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("serve");
    }
}

/// The per-request proxy: attest → scope → inject credential → forward.
async fn proxy(
    State(state): State<AppState>,
    Path(params): Path<Vec<(String, String)>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let server_name = params
        .iter()
        .find(|(k, _)| k == "server")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let rest = params
        .iter()
        .find(|(k, _)| k == "rest")
        .map(|(_, v)| v.clone());

    // 1. Attest the caller by source IP → (ns, agent).
    let (ns, agent) = match identity_for_ip(&state.client, peer.ip()).await {
        Some(id) => id,
        None => {
            tracing::warn!(peer = %peer.ip(), "attest: source IP resolves to no agent pod; rejecting");
            return forbidden("cannot attest caller identity from source IP");
        }
    };

    // 2. Scope: the server must belong to an MCPServerSet this agent binds.
    let server = match resolve_bound_server(&state.client, &ns, &agent, &server_name).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(%ns, %agent, server = %server_name, "authz: server not bound to agent");
            return forbidden(&format!(
                "agent {ns}/{agent} is not bound to MCP server '{server_name}'"
            ));
        }
        Err(e) => {
            tracing::error!(%ns, %agent, error = %e, "resolve bound server failed");
            return bad_gateway(&e);
        }
    };

    // 3. Read the credential (off-pod) for the upstream hop.
    let auth_header = match credential_header(&state.client, &ns, &server).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(%ns, server = %server.name, error = %e, "read credential failed");
            return bad_gateway(&e);
        }
    };

    // 4. Forward to the upstream MCP server (transparent + credential-injected).
    let mut url = server.endpoint.clone();
    if let Some(rest) = rest {
        if !url.ends_with('/') {
            url.push('/');
        }
        url.push_str(&rest);
    }
    let mut rb = state.http.post(&url).body(body);
    for name in FORWARD_HEADERS {
        if let Some(v) = headers.get(*name) {
            rb = rb.header(*name, v);
        }
    }
    if let Some((hname, hval)) = auth_header {
        rb = rb.header(hname, hval);
    }

    match rb.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            // Pass back the content-type + the MCP session id (so the agent
            // echoes it on the next call — the session flows end to end).
            let mut out = Response::builder().status(status);
            for name in ["content-type", "mcp-session-id", "mcp-protocol-version"] {
                if let Some(v) = resp.headers().get(name) {
                    out = out.header(name, v);
                }
            }
            tracing::info!(%ns, %agent, server = %server.name, %status, "mcp call proxied");
            out.body(Body::from_stream(resp.bytes_stream()))
                .unwrap_or_else(|_| bad_gateway("build response"))
        }
        Err(e) => {
            tracing::warn!(%ns, server = %server.name, %url, error = %e, "upstream MCP call failed");
            bad_gateway(&format!("upstream {url}: {e}"))
        }
    }
}

/// Resolve a source IP → `(namespace, agent)` by the pod's `agentctl.dev/agent`
/// label. Retries briefly (the cold-start `status.podIP` propagation race — a
/// source IP that reached us over TCP is a real pod; see the ModelGateway).
async fn identity_for_ip(client: &Client, ip: IpAddr) -> Option<(String, String)> {
    let pods: Api<Pod> = Api::all(client.clone());
    let ip_s = ip.to_string();
    let lp = ListParams::default().fields(&format!("status.podIP={ip_s}"));
    for attempt in 0..=3 {
        if let Ok(list) = pods.list(&lp).await {
            if let Some(pod) = list.items.into_iter().find(|p| {
                p.status
                    .as_ref()
                    .and_then(|s| s.pod_ip.as_deref())
                    .map(|pip| pip == ip_s)
                    .unwrap_or(false)
            }) {
                let ns = pod.metadata.namespace?;
                let agent = pod
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("agentctl.dev/agent"))
                    .cloned()?;
                return Some((ns, agent));
            }
        }
        if attempt < 3 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    None
}

/// Find the server named `server_name` among the `MCPServerSet`s the agent's CR
/// binds (`spec.mcpServerSetRefs`) — the authorization scope. `Ok(None)` ⇒ the
/// agent is not bound to a server of that name (reject).
async fn resolve_bound_server(
    client: &Client,
    ns: &str,
    agent: &str,
    server_name: &str,
) -> Result<Option<McpServer>, String> {
    let agents: Api<Agent> = Api::namespaced(client.clone(), ns);
    let cr = agents
        .get(agent)
        .await
        .map_err(|e| format!("get Agent {ns}/{agent}: {e}"))?;
    let sets: Api<MCPServerSet> = Api::namespaced(client.clone(), ns);
    for r in &cr.spec.mcp_server_set_refs {
        let set = match sets.get(&r.name).await {
            Ok(s) => s,
            // A dangling ref is a config error, not this agent's fault — skip it
            // (another ref may still carry the server) but log.
            Err(e) => {
                tracing::warn!(%ns, set = %r.name, error = %e, "bound MCPServerSet not found");
                continue;
            }
        };
        if let Some(s) = set.spec.servers.into_iter().find(|s| s.name == server_name) {
            return Ok(Some(s));
        }
    }
    Ok(None)
}

/// Build the upstream auth header for a server from its `Secret`-backed
/// credential (off-pod). `None` for `mode: none`. `staticToken` reads the Secret
/// and returns `(header, value)` — `Authorization: Bearer <v>` by default, or a
/// custom header carrying the raw value.
async fn credential_header(
    client: &Client,
    ns: &str,
    server: &McpServer,
) -> Result<Option<(String, String)>, String> {
    let Some(auth) = &server.auth else {
        return Ok(None);
    };
    match auth.mode {
        McpAuthMode::None => Ok(None),
        McpAuthMode::StaticToken => {
            let secret_ref = auth
                .token_secret_ref
                .as_ref()
                .ok_or("staticToken auth needs tokenSecretRef")?;
            let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
            let secret = secrets
                .get(&secret_ref.name)
                .await
                .map_err(|e| format!("get Secret {ns}/{}: {e}", secret_ref.name))?;
            let raw = secret
                .data
                .as_ref()
                .and_then(|d| d.get(&secret_ref.key))
                .ok_or_else(|| {
                    format!(
                        "Secret {}/{} has no key {}",
                        ns, secret_ref.name, secret_ref.key
                    )
                })?;
            let value = String::from_utf8(raw.0.clone())
                .map_err(|_| "credential is not valid UTF-8".to_string())?;
            match &auth.header {
                Some(h) => Ok(Some((h.clone(), value))),
                None => Ok(Some((
                    "authorization".to_string(),
                    format!("Bearer {value}"),
                ))),
            }
        }
    }
}

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

fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, error_json(msg)).into_response()
}
fn bad_gateway(msg: &str) -> Response {
    (StatusCode::BAD_GATEWAY, error_json(msg)).into_response()
}
fn error_json(msg: &str) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "error": msg }))
}
