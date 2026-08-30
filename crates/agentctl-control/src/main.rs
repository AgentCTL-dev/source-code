// SPDX-License-Identifier: BUSL-1.1
//! The `agentctl-control` binary: HTTPS axum shell around the control MCP
//! dispatcher ([`agentctl_control::handle_rpc`]).
//!
//! Auth model (RFC 0024 rung 1, resource-server side): every `/mcp` POST must
//! be AAuth-signed (`jwt` scheme). Unsigned → the spec challenge (`401` +
//! `AAuth-Requirement: requirement=agent-token`), which is exactly what makes
//! the 1.3.1 client enroll + sign. Signed → the token verifies against the
//! fleet Agent Provider's JWKS (cached, refreshed on unknown `kid`), the HTTP
//! signature proves possession of `cnf.jwk`, and the token's `wl` claim (the
//! operator-registered `<namespace>/<name>`) becomes the caller's scope.
//!
//! Serves HTTPS (cert-manager cert) — agentd's `aauth ⇒ https endpoint` CEL
//! rule refuses plaintext aauth dials, so a plaintext control listener would
//! be unreachable by design.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use agentctl_control::Caller;
use agentctl_identity::aauth::{verify_agent_request, SignatureHeaders};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Clone)]
struct App {
    client: kube::Client,
    jwks: Arc<Jwks>,
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");
    agentctl_telemetry::init("agentctl-control");

    let provider = std::env::var("AGENTCTL_AAUTH_PROVIDER")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .expect("AGENTCTL_AAUTH_PROVIDER is required (the fleet Agent Provider issuer URL)");
    let tls_dir =
        std::env::var("CONTROL_TLS_DIR").unwrap_or_else(|_| "/etc/agentctl/control-tls".into());
    let client = kube::Client::try_default()
        .await
        .expect("in-cluster kube client");

    let jwks = Arc::new(Jwks::new(provider.clone()));
    // Warm the cache; failure is non-fatal (refreshed on first miss).
    if let Err(e) = jwks.refresh().await {
        warn!(error = %e, "initial JWKS fetch failed; will retry on demand");
    }

    let app = App { client, jwks };
    let router = Router::new()
        .route("/mcp", post(mcp))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app);

    let addr: SocketAddr = ([0, 0, 0, 0], 8443).into();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        format!("{tls_dir}/tls.crt"),
        format!("{tls_dir}/tls.key"),
    )
    .await
    .expect("load serving cert (CONTROL_TLS_DIR)");
    info!(%addr, provider, "agentctl-control serving https");
    axum_server::bind_rustls(addr, tls)
        .serve(router.into_make_service())
        .await
        .expect("serve");
}

async fn mcp(State(app): State<App>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let h = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (Some(sig_input), Some(sig), Some(sig_key)) =
        (h("signature-input"), h("signature"), h("signature-key"))
    else {
        return (
            StatusCode::UNAUTHORIZED,
            [("aauth-requirement", "requirement=agent-token")],
            "agent token required",
        )
            .into_response();
    };

    // Resolve the token's kid to a provider key BEFORE the sync verify (the
    // JWKS refresh is async). An unknown kid refreshes once.
    let kid = token_kid(&sig_key);
    let provider_key = match &kid {
        Some(kid) => app.jwks.key_for(kid).await,
        None => None,
    };
    let content_digest = h("content-digest");
    let authority = h("host").unwrap_or_default().to_ascii_lowercase();
    let authority = authority.trim_end_matches(":443").to_string();
    let sig_headers = SignatureHeaders {
        signature_input: &sig_input,
        signature: &sig,
        signature_key: &sig_key,
        content_digest: content_digest.as_deref(),
    };
    let verified = verify_agent_request(
        "POST",
        &authority,
        "/mcp",
        &sig_headers,
        &body,
        |kid: &str| {
            provider_key
                .clone()
                .ok_or_else(|| format!("kid {kid} not in the provider JWKS"))
        },
    );
    let agent = match verified {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "signed control call rejected");
            return (
                StatusCode::UNAUTHORIZED,
                [("signature-error", "error=invalid_signature")],
                format!("invalid signature: {e}"),
            )
                .into_response();
        }
    };
    let Some((namespace, name)) = agent.workload else {
        // A token without the workload label cannot be scoped — refuse rather
        // than guess (pre-upgrade tokens age out within their TTL).
        return (
            StatusCode::FORBIDDEN,
            "token carries no workload label; re-obtain the agent token",
        )
            .into_response();
    };
    let caller = Caller {
        agent: agent.agent,
        namespace,
        name,
    };
    info!(agent = %caller.agent, ns = %caller.namespace, workload = %caller.name, "control call");

    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "body is not JSON").into_response(),
    };
    match agentctl_control::handle_rpc(&req, &app.client, &caller).await {
        Some(resp) => (
            StatusCode::OK,
            [("mcp-session-id", "control-1")],
            Json(resp),
        )
            .into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// The `kid` from the `Signature-Key: sig=jwt;jwt="…"` token header — parsed
/// cheaply (no verification) just to drive the JWKS cache lookup.
fn token_kid(sig_key: &str) -> Option<String> {
    use base64::Engine as _;
    let token = sig_key.strip_prefix("sig=jwt;jwt=\"")?.strip_suffix('"')?;
    let header_b64 = token.split('.').next()?;
    let header: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header_b64)
            .ok()?,
    )
    .ok()?;
    header
        .get("kid")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Provider JWKS cache: `kid → Ed25519 public key bytes`. Refreshes on an
/// unknown kid (rate-limited by the singleflight write lock) — key rotation
/// at the provider propagates without a restart.
struct Jwks {
    provider: String,
    http: reqwest::Client,
    keys: RwLock<HashMap<String, Vec<u8>>>,
}

impl Jwks {
    fn new(provider: String) -> Self {
        let mut b = reqwest::Client::builder();
        // Private-CA providers: trust the platform bundle when mounted.
        if let Ok(ca) = std::env::var("AGENTCTL_CA_FILE") {
            if let Ok(pem) = std::fs::read(&ca) {
                if let Ok(cert) = reqwest::Certificate::from_pem(&pem) {
                    b = b.add_root_certificate(cert);
                }
            }
        }
        Jwks {
            provider: provider.trim_end_matches('/').to_string(),
            http: b.build().expect("reqwest client"),
            keys: RwLock::new(HashMap::new()),
        }
    }

    async fn key_for(&self, kid: &str) -> Option<Vec<u8>> {
        if let Some(k) = self.keys.read().await.get(kid) {
            return Some(k.clone());
        }
        if let Err(e) = self.refresh().await {
            warn!(error = %e, "JWKS refresh failed");
        }
        self.keys.read().await.get(kid).cloned()
    }

    /// Fetch via the discovery document (the published contract), falling
    /// back to the conventional `/aauth-jwks.json` path.
    async fn refresh(&self) -> Result<(), String> {
        use base64::Engine as _;
        let meta_url = format!("{}/.well-known/aauth-agent.json", self.provider);
        let jwks_url = match self.fetch_json(&meta_url).await {
            Ok(meta) => meta
                .get("jwks_uri")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}/aauth-jwks.json", self.provider)),
            Err(_) => format!("{}/aauth-jwks.json", self.provider),
        };
        let jwks = self.fetch_json(&jwks_url).await?;
        let mut map = HashMap::new();
        for k in jwks
            .get("keys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(kid), Some(x)) = (
                k.get("kid").and_then(Value::as_str),
                k.get("x").and_then(Value::as_str),
            ) else {
                continue;
            };
            if k.get("crv").and_then(Value::as_str) != Some("Ed25519") {
                continue;
            }
            if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(x) {
                map.insert(kid.to_string(), bytes);
            }
        }
        if map.is_empty() {
            return Err(format!("{jwks_url} yielded no Ed25519 keys"));
        }
        *self.keys.write().await = map;
        Ok(())
    }

    async fn fetch_json(&self, url: &str) -> Result<Value, String> {
        self.http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("GET {url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("{url}: not JSON: {e}"))
    }
}
