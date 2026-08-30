// SPDX-License-Identifier: BUSL-1.1
//! The HTTP surface (RFC 0028 §2.1). P1 routes:
//!
//! | route | callers | auth |
//! |---|---|---|
//! | `GET  /healthz` | probes | none |
//! | `POST /v1/device/start {provider}` | CLI login | none (starts a consent the human completes at the IdP) |
//! | `POST /v1/device/poll {handle}` | CLI login | none (opaque handle) |
//! | `POST /v1/introspect {token}` | gateway, apiserver | admin bearer |
//! | `POST /v1/principals/mint` | operator | admin bearer |
//! | `POST /v1/principals/verify {bearer}` | gateway | admin bearer |
//!
//! Every mutating/validating surface requires the shared control-plane admin
//! bearer; with none configured those surfaces refuse outright (fail closed).
//! No secret value is ever logged; the device handle is an opaque random id
//! so the device_code (a credential) never round-trips through the CLI.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Value};

use crate::oidc::{Federation, OidcError};
use crate::principals::{self, MintRequest};
use crate::store::{DeviceSession, Store};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub struct AppState {
    pub federation: Federation,
    pub store: Arc<dyn Store>,
    pub admin_token: Option<String>,
    /// The AAuth Agent Provider role (RFC 0028 §5). `None` ⇒ those surfaces
    /// answer 404 (the role is opt-in via `IDENTITY_AAUTH_ISSUER`).
    pub aauth: Option<Arc<AauthState>>,
}

/// Provider-role state: the signing key, the issuer URL agents validate
/// `iss` against, and the token lifetime.
pub struct AauthState {
    pub key: crate::aauth::ProviderKey,
    pub issuer: String,
    pub token_ttl: i64,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/providers", get(providers))
        .route("/v1/device/start", post(device_start))
        .route("/v1/device/poll", post(device_poll))
        .route("/v1/introspect", post(introspect))
        .route("/v1/principals/mint", post(principal_mint))
        .route("/v1/principals/verify", post(principal_verify))
        // -- AAuth Agent Provider role (RFC 0028 §5; agentd 1.3.1 wire) -----
        // Agent-facing paths are hard-coded root-relative in the client (no
        // discovery); the well-known + JWKS are the resource servers' trust
        // root and are public by design.
        .route("/.well-known/aauth-agent.json", get(aauth_well_known))
        .route("/aauth-jwks.json", get(aauth_jwks))
        .route("/enroll", post(aauth_enroll))
        .route("/agent-token", post(aauth_agent_token))
        .route("/admin/allowed-keys", post(admin_allowed_keys_post))
        .route(
            "/admin/allowed-keys/{jkt}",
            axum::routing::delete(admin_allowed_keys_delete),
        )
        .route("/admin/agents", get(admin_agents_get))
        .route("/admin/mcpg-token", post(admin_mcpg_token))
        .route("/admin/agents/{local}/revoke", post(admin_agent_revoke))
        .with_state(state)
}

type ApiError = (StatusCode, Json<Value>);

fn err(code: StatusCode, msg: impl std::fmt::Display) -> ApiError {
    (code, Json(json!({ "error": msg.to_string() })))
}

/// Constant-time admin-bearer gate. No token configured ⇒ every gated surface
/// refuses (fail closed) with a message naming the wiring.
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = &state.admin_token else {
        return Err(err(
            StatusCode::FORBIDDEN,
            "identity admin surfaces are disabled: IDENTITY_ADMIN_TOKEN is not configured",
        ));
    };
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    // Constant-time comparison (length leak is fine: token lengths are fixed
    // by our own mint; ring's helper is deprecated for external use).
    let ok = presented.len() == expected.len()
        && presented
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    if ok {
        Ok(())
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "admin bearer required"))
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// -- discovery ---------------------------------------------------------------

/// Names of the configured providers — what `agentctl login` can pick from.
/// Unauthenticated by design: names only, never issuers/clients/secrets.
async fn providers(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "providers": state.federation.provider_names() }))
}

// -- device flow -------------------------------------------------------------

async fn device_start(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "provider is required"))?;
    let start = state
        .federation
        .device_start(provider)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;

    // Park the device_code behind an opaque handle: the CLI polls with the
    // handle; the credential-bearing code never leaves this service.
    let mut raw = [0u8; 24];
    SystemRandom::new().fill(&mut raw).expect("rng");
    let handle = B64.encode(raw);
    state
        .store
        .put_device_session(DeviceSession {
            handle: handle.clone(),
            provider: provider.to_string(),
            device_code: start.device_code.clone(),
            interval_secs: start.interval,
            expires_unix: now_unix() + start.expires_in as i64,
        })
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(json!({
        "handle": handle,
        "user_code": start.user_code,
        "verification_uri": start.verification_uri,
        "expires_in": start.expires_in,
        "interval": start.interval,
    })))
}

async fn device_poll(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let handle = body
        .get("handle")
        .and_then(Value::as_str)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "handle is required"))?;
    let session = state
        .store
        .take_device_session(handle)
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, "unknown or expired login handle"))?;
    if session.expires_unix < now_unix() {
        return Err(err(StatusCode::GONE, "login expired; start again"));
    }
    match state
        .federation
        .device_poll(&session.provider, &session.device_code)
        .await
    {
        Ok(tokens) => {
            // The refresh token (custody material) stays here — connection
            // storage lands with the exchange (P5); today we simply never
            // return it, which is already the custody boundary.
            let identity = state
                .federation
                .validate(&session.provider, &tokens.access_token)
                .await
                .ok();
            Ok(Json(json!({
                "status": "ok",
                "access_token": tokens.access_token,
                "id_token": tokens.id_token,
                "expires_in": tokens.expires_in,
                "identity": identity,
            })))
        }
        Err(OidcError::AuthorizationPending) => Ok(Json(json!({ "status": "pending" }))),
        Err(OidcError::SlowDown) => Ok(Json(json!({ "status": "slow_down" }))),
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, e)),
    }
}

// -- validation --------------------------------------------------------------

async fn introspect(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let token = body
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "token is required"))?;
    match state.federation.validate_any(token).await {
        Ok(identity) => Ok(Json(json!({ "active": true, "identity": identity }))),
        Err(_) => Ok(Json(json!({ "active": false }))),
    }
}

// -- principals --------------------------------------------------------------

async fn principal_mint(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<MintRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let resp = principals::mint(state.store.as_ref(), req)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::to_value(resp).expect("serializable")))
}

async fn principal_verify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let bearer = body
        .get("bearer")
        .and_then(Value::as_str)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bearer is required"))?;
    match principals::verify(state.store.as_ref(), bearer).await {
        Ok(p) => Ok(Json(json!({ "active": true, "principal": p }))),
        Err(_) => Ok(Json(json!({ "active": false }))),
    }
}

// -- AAuth Agent Provider role (RFC 0028 §5) --------------------------------

fn aauth_state(state: &AppState) -> Result<&Arc<AauthState>, ApiError> {
    state.aauth.as_ref().ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "the AAuth provider role is not enabled (IDENTITY_AAUTH_ISSUER)",
        )
    })
}

async fn aauth_well_known(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let aauth = aauth_state(&state)?;
    Ok(Json(json!({
        "issuer": aauth.issuer,
        "jwks_uri": format!("{}/aauth-jwks.json", aauth.issuer),
        "enrollment_endpoint": format!("{}/enroll", aauth.issuer),
        "agent_token_endpoint": format!("{}/agent-token", aauth.issuer),
    })))
}

async fn aauth_jwks(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(aauth_state(&state)?.key.jwks()))
}

/// Verify the RFC 9421 `hwk` request signature on an agent-facing call.
/// `@authority` is the received Host header verbatim (lowercased) — the agent
/// covers exactly what it sends. Failures carry the detail in an
/// `aauth-error` header (where the 1.3.1 client looks) as well as the body.
fn verify_agent_call(
    headers: &HeaderMap,
    path: &str,
    body: &[u8],
) -> Result<crate::aauth::SignedCaller, ApiError> {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let authority = get("host").unwrap_or_default().to_ascii_lowercase();
    let (Some(input), Some(sig), Some(key)) = (
        get("signature-input"),
        get("signature"),
        get("signature-key"),
    ) else {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "missing RFC 9421 signature headers (signature-input/signature/signature-key)",
        ));
    };
    crate::aauth::verify_signed_request(
        "POST",
        &authority,
        path,
        &crate::aauth::SignatureHeaders {
            signature_input: input,
            signature: sig,
            signature_key: key,
            content_digest: get("content-digest"),
        },
        body,
    )
    .map_err(|e| err(StatusCode::UNAUTHORIZED, e))
}

async fn aauth_enroll(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let aauth = match aauth_state(&state) {
        Ok(a) => a.clone(),
        Err(e) => return e.into_response(),
    };
    let caller = match verify_agent_call(&headers, "/enroll", &body) {
        Ok(c) => c,
        Err(e) => return with_aauth_error(e),
    };

    // Idempotent by thumbprint: the shipped client re-enrolls on every first
    // signed dial after a restart.
    match state.store.find_aauth_agent_by_jkt(&caller.jkt).await {
        Ok(existing) if existing.status == "active" => {
            return Json(json!({ "agent": existing.agent })).into_response();
        }
        Ok(_) => {
            return with_aauth_error(err(StatusCode::FORBIDDEN, "this key has been revoked"));
        }
        Err(_) => {}
    }

    // Gate: the operator-registered key allowlist. (The federated
    // `enrollment_assertion` leg is accepted on the wire but not yet wired —
    // the renderer does not project SA tokens; recorded in the PLAN.)
    let allowed = match state.store.find_allowed_key(&caller.jkt).await {
        Ok(k) if k.expires_unix > now_unix() => k,
        Ok(_) => {
            return with_aauth_error(err(
                StatusCode::FORBIDDEN,
                "key registration expired; re-register the thumbprint",
            ));
        }
        Err(_) => {
            return with_aauth_error(err(
                StatusCode::FORBIDDEN,
                "key is not registered for enrollment (allowlist)",
            ));
        }
    };

    let agent = crate::aauth::agent_id(&aauth.issuer, &caller.jkt);
    let local = crate::aauth::agent_local(&agent)
        .expect("agent_id always carries a local part")
        .to_string();
    let record = crate::store::AauthAgent {
        local,
        agent: agent.clone(),
        jkt: caller.jkt.clone(),
        label: allowed.label,
        status: "active".into(),
        created_unix: now_unix(),
    };
    if let Err(e) = state.store.put_aauth_agent(record).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "agent": agent })).into_response()
}

async fn aauth_agent_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let aauth = match aauth_state(&state) {
        Ok(a) => a.clone(),
        Err(e) => return e.into_response(),
    };
    let caller = match verify_agent_call(&headers, "/agent-token", &body) {
        Ok(c) => c,
        Err(e) => return with_aauth_error(e),
    };
    let agent = match state.store.find_aauth_agent_by_jkt(&caller.jkt).await {
        Ok(a) if a.status == "active" => a,
        Ok(_) => return with_aauth_error(err(StatusCode::FORBIDDEN, "agent is revoked")),
        Err(_) => {
            return with_aauth_error(err(
                StatusCode::UNAUTHORIZED,
                "key is not enrolled; enroll first",
            ));
        }
    };
    let token = aauth.key.mint_agent_token(
        &aauth.issuer,
        &agent.agent,
        &caller.x_b64url,
        aauth.token_ttl,
        // The operator-registered workload label (`<ns>/<name>`) rides the
        // token so in-cluster resource servers can scope by it.
        Some(agent.label.as_str()).filter(|l| !l.is_empty()),
    );
    Json(json!({
        "agent_token": token,
        "expires_in": aauth.token_ttl,
        "agent": agent.agent,
    }))
    .into_response()
}

/// Attach the error detail as the `aauth-error` header the 1.3.1 client
/// surfaces, alongside the JSON body.
fn with_aauth_error(e: ApiError) -> Response {
    let detail = e.1["error"].as_str().unwrap_or("aauth error").to_string();
    let mut resp = e.into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&detail) {
        resp.headers_mut().insert("aauth-error", v);
    }
    resp
}

/// `POST /admin/mcpg-token` (P5-2, operator channel): mint an
/// AUDIENCE-BOUND tenant-gateway access token — a plain EdDSA JWT signed by
/// the provider key, verifiable against the published `/aauth-jwks.json`.
/// `{workload, audience, ttl?}` → `{token, expires_in}`. Admin-gated: the
/// OPERATOR names the workload (the caller cannot mint for someone else).
async fn admin_mcpg_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let aauth = aauth_state(&state)?;
    let workload = body
        .get("workload")
        .and_then(Value::as_str)
        .filter(|w| !w.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "workload is required"))?;
    let audience = body
        .get("audience")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "audience is required"))?;
    let ttl = body
        .get("ttl")
        .and_then(Value::as_i64)
        .unwrap_or(24 * 3600)
        .clamp(60, 30 * 24 * 3600);
    let token = aauth
        .key
        .mint_gateway_token(&aauth.issuer, workload, audience, ttl);
    Ok(Json(json!({ "token": token, "expires_in": ttl })))
}

// -- AAuth admin (operator channel) -----------------------------------------

async fn admin_allowed_keys_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    aauth_state(&state)?;
    let jkt = body
        .get("jkt")
        .and_then(Value::as_str)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "jkt is required"))?;
    let label = body.get("label").and_then(Value::as_str).unwrap_or("");
    let ttl = body.get("ttl").and_then(Value::as_i64).unwrap_or(86_400);
    state
        .store
        .put_allowed_key(crate::store::AllowedKey {
            jkt: jkt.to_string(),
            label: label.to_string(),
            expires_unix: now_unix() + ttl,
        })
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "ok": true })))
}

async fn admin_allowed_keys_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(jkt): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    require_admin(&state, &headers)?;
    aauth_state(&state)?;
    let existed = state
        .store
        .delete_allowed_key(&jkt)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(if existed {
        Json(json!({ "ok": true })).into_response()
    } else {
        err(StatusCode::NOT_FOUND, "unknown thumbprint").into_response()
    })
}

async fn admin_agents_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    aauth_state(&state)?;
    let agents = state
        .store
        .list_aauth_agents()
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let rows: Vec<Value> = agents
        .iter()
        .map(|a| {
            json!({
                "label": a.label,
                "local": a.local,
                "agent": a.agent,
                "status": a.status,
            })
        })
        .collect();
    Ok(Json(json!({ "agents": rows })))
}

async fn admin_agent_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(local): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    require_admin(&state, &headers)?;
    aauth_state(&state)?;
    let existed = state
        .store
        .revoke_aauth_agent(&local)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(if existed {
        Json(json!({ "ok": true })).into_response()
    } else {
        err(StatusCode::NOT_FOUND, "unknown agent").into_response()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Provider;
    use crate::oidc::outbound_client;
    use crate::store::MemoryStore;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    fn state(admin: Option<&str>) -> Arc<AppState> {
        Arc::new(AppState {
            federation: Federation::new(
                outbound_client(),
                vec![Provider {
                    name: "dev".into(),
                    issuer: "http://127.0.0.1:1".into(), // never dialed in these tests
                    client_id: "cli".into(),
                    client_secret: None,
                    audiences: vec![],
                    scopes: vec!["openid".into()],
                    groups_claim: "groups".into(),
                }],
            ),
            store: Arc::new(MemoryStore::default()),
            admin_token: admin.map(str::to_string),
            aauth: Some(Arc::new(AauthState {
                key: crate::aauth::ProviderKey::from_seed(&[3u8; 32]).unwrap(),
                issuer: "http://identity.test".into(),
                token_ttl: 300,
            })),
        })
    }

    async fn call(
        app: Router,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(b) = bearer {
            req = req.header("authorization", format!("Bearer {b}"));
        }
        let resp = app
            .oneshot(req.body(axum::body::Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn admin_surfaces_fail_closed_without_a_token() {
        let app = router(state(None));
        let (code, body) = call(
            app,
            "POST",
            "/v1/principals/mint",
            None,
            json!({ "org": "a", "namespace": "org-a", "agent": "x", "subject": "dev:u" }),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(body["error"].as_str().unwrap().contains("disabled"));
    }

    #[tokio::test]
    async fn admin_bearer_gates_and_mint_verify_roundtrip() {
        let app = router(state(Some("s3cret")));
        // Wrong bearer → 401.
        let (code, _) = call(
            app.clone(),
            "POST",
            "/v1/principals/mint",
            Some("nope"),
            json!({ "org": "a", "namespace": "org-a", "agent": "x", "subject": "dev:u" }),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        // Right bearer → mint, then verify round-trips.
        let (code, minted) = call(
            app.clone(),
            "POST",
            "/v1/principals/mint",
            Some("s3cret"),
            json!({ "org": "a", "namespace": "org-a", "agent": "x", "subject": "dev:u" }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let bearer = minted["bearer"].as_str().unwrap();
        assert!(bearer.starts_with("pat-"));
        assert_eq!(minted["secret_name"], "x-principals");
        let (code, verified) = call(
            app,
            "POST",
            "/v1/principals/verify",
            Some("s3cret"),
            json!({ "bearer": bearer }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(verified["active"], true);
        assert_eq!(verified["principal"]["subject"], "dev:u");
    }

    #[tokio::test]
    async fn providers_lists_names_only() {
        let app = router(state(None));
        let (code, body) = call(app, "GET", "/v1/providers", None, Value::Null).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, json!({ "providers": ["dev"] }));
    }

    #[tokio::test]
    async fn device_poll_refuses_unknown_handles() {
        let app = router(state(None)); // device flow is unauthenticated by design
        let (code, _) = call(
            app,
            "POST",
            "/v1/device/poll",
            None,
            json!({ "handle": "x" }),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }
}
