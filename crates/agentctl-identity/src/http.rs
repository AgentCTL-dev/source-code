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
    /// The RFC 8693 exchange engine (P5-3) + the sealer the connection admin
    /// surface seals custody rows with (same instance the exchanger unseals).
    pub exchanger: Arc<crate::exchange::Exchanger>,
    pub sealer: Arc<crate::seal::Sealer>,
    /// The audit sink (P7-3): Some with durable custody (Postgres). Evidence,
    /// never flow control — writes are spawned fire-and-forget.
    pub audit: Option<deadpool_postgres::Pool>,
}

/// Fire-and-forget audit write.
fn audit_spawn(state: &AppState, r: agentctl_audit::Record) {
    if let Some(pool) = state.audit.clone() {
        tokio::spawn(async move {
            if let Err(e) = agentctl_audit::pg::record(&pool, &r).await {
                tracing::debug!(error = %e, "audit record failed");
            }
        });
    }
}

/// The inbound trail id (`x-agentctl-trail` / `x-request-id`), when present.
fn trail_of(headers: &HeaderMap) -> String {
    for h in ["x-agentctl-trail", "x-request-id"] {
        if let Some(t) = headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .filter(|t| !t.is_empty() && t.len() <= 128)
        {
            return t.to_string();
        }
    }
    String::new()
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
        .route("/v1/exchange", post(exchange))
        .route("/v1/connections/start", post(connections_start))
        .route("/v1/connections/poll", post(connections_poll))
        .route("/v1/audit/ingest", post(audit_ingest))
        .route("/metrics", get(metrics))
        .route(
            "/admin/connections",
            post(admin_connections_post).get(admin_connections_get),
        )
        .route("/admin/connections/delete", post(admin_connections_delete))
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
            purpose: "login".into(),
            org: None,
            subject: None,
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
    if session.purpose != "login" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "handle belongs to a connect flow; poll /v1/connections/poll",
        ));
    }
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

// -- connections (P5-4 consent flow) -----------------------------------------

/// Resolve the CALLER for the connect flow: the Authorization bearer is
/// either a USER's identity access token (validated against the federated
/// providers — the user connects THEMSELF) or the admin bearer (control
/// plane / tests name the user explicitly).
async fn connect_caller(
    state: &AppState,
    headers: &HeaderMap,
    body: &Value,
) -> Result<String, ApiError> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !bearer.is_empty() {
        if let Ok(identity) = state.federation.validate_any(bearer).await {
            return Ok(identity.subject);
        }
    }
    // Not a user token: the admin channel may connect on a named user's
    // behalf (require_admin refuses anything else, fail closed).
    require_admin(state, headers)?;
    body.get("user")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "admin-channel connect needs an explicit user",
            )
        })
}

/// `POST /v1/connections/start {provider, org}` — begin the CONSENT device
/// flow for a provider connection (RFC 8628 + `offline_access`): the human
/// approves at the IdP once; poll stores the refresh token in custody and
/// the org's agents proceed on the user's behalf from then on (P5-4).
async fn connections_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let subject = connect_caller(&state, &headers, &body).await?;
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "provider is required"))?;
    let org = body
        .get("org")
        .and_then(Value::as_str)
        .filter(|o| !o.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "org is required"))?;
    let start = state
        .federation
        .device_start_scoped(provider, &["offline_access"])
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
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
            purpose: "connect".into(),
            org: Some(org.to_string()),
            subject: Some(subject),
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

/// `POST /v1/connections/poll {handle}` — complete the consent: redeem the
/// device code, seal the provider's REFRESH token into custody as the
/// (org, user, provider) connection, and return only facts — no token ever
/// leaves this service on this path.
async fn connections_poll(
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
        .map_err(|_| err(StatusCode::NOT_FOUND, "unknown or expired connect handle"))?;
    if session.purpose != "connect" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "handle belongs to a login flow; poll /v1/device/poll",
        ));
    }
    if session.expires_unix < now_unix() {
        return Err(err(StatusCode::GONE, "consent expired; start again"));
    }
    let (org, user) = match (&session.org, &session.subject) {
        (Some(o), Some(u)) => (o.clone(), u.clone()),
        _ => {
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "connect session lost its binding",
            ))
        }
    };
    match state
        .federation
        .device_poll(&session.provider, &session.device_code)
        .await
    {
        Ok(tokens) => {
            let Some(refresh) = tokens.refresh_token else {
                return Err(err(
                    StatusCode::BAD_GATEWAY,
                    "provider granted no refresh token (offline_access unsupported or refused) — the connection would die with the first access token",
                ));
            };
            let provider_cfg = state
                .federation
                .provider(&session.provider)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?
                .clone();
            let discovery = state
                .federation
                .discovery(&session.provider)
                .await
                .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
            let sealed_secret = state
                .sealer
                .seal(
                    &crate::exchange::secret_aad(&org, &user, &session.provider),
                    refresh.as_bytes(),
                )
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            let sealed_client_secret = match &provider_cfg.client_secret {
                Some(cs) => Some(
                    state
                        .sealer
                        .seal(
                            &crate::exchange::client_secret_aad(&org, &user, &session.provider),
                            cs.as_bytes(),
                        )
                        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?,
                ),
                None => None,
            };
            let now = now_unix();
            state
                .store
                .put_connection(crate::store::ConnectionRecord {
                    org: org.clone(),
                    user: user.clone(),
                    provider: session.provider.clone(),
                    kind: "oauth_refresh".into(),
                    sealed_secret,
                    token_endpoint: Some(discovery.token_endpoint.clone()),
                    client_id: Some(provider_cfg.client_id.clone()),
                    sealed_client_secret,
                    scope: None,
                    created_unix: now,
                    updated_unix: now,
                })
                .await
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            // A re-connect replaces the grant NOW, not at cache expiry.
            state.exchanger.invalidate(&org, &user, &session.provider);
            audit_spawn(
                &state,
                agentctl_audit::Record::new(
                    "identity",
                    org.clone(),
                    format!("org-{org}"),
                    String::new(),
                    agentctl_audit::ACTION_CONSENT,
                    agentctl_audit::OUTCOME_OK,
                )
                .user(user.clone())
                .dim("provider", session.provider.clone()),
            );
            Ok(Json(json!({
                "status": "ok",
                "org": org,
                "user": user,
                "provider": session.provider,
            })))
        }
        Err(OidcError::AuthorizationPending) => Ok(Json(json!({ "status": "pending" }))),
        Err(OidcError::SlowDown) => Ok(Json(json!({ "status": "slow_down" }))),
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, e)),
    }
}

// -- audit ingest (P7-3) ------------------------------------------------------

/// `POST /v1/audit/ingest` — the door for audit records born outside
/// our PG (the tenant mcpg's hash-chained file sink). SELF-AUTHENTICATING:
/// the bearer is one of our own EdDSA workload JWTs with the dedicated
/// `agentctl:audit-ingest` audience (operator-minted into the emitter's
/// Secret) — same trust root as every other verifier. Batch of `audit/v1`
/// records (audit/v1 verbatim, or an mcpg-native event mapped server-side);
/// component is FORCED from the token's subject namespace so an emitter cannot
/// impersonate another org's stream.
async fn audit_ingest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let Some(pool) = state.audit.clone() else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit ingest needs durable custody (postgres)",
        ));
    };
    let aauth = aauth_state(&state)?;
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let claims =
        crate::aauth::verify_identity_jwt(bearer, &aauth.key.public_x_b64url(), &aauth.issuer)
            .map_err(|e| err(StatusCode::UNAUTHORIZED, e))?;
    if claims.get("aud").and_then(Value::as_str) != Some("agentctl:audit-ingest") {
        return Err(err(
            StatusCode::FORBIDDEN,
            "token audience is not agentctl:audit-ingest",
        ));
    }
    // sub = "<namespace>/<emitter>": the org boundary the records must stay in.
    let sub = claims.get("sub").and_then(Value::as_str).unwrap_or("");
    let ns = sub.split('/').next().unwrap_or("");
    if ns.is_empty() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "token subject names no namespace",
        ));
    }
    // The workload the mcpg-native mapping attributes (the token's subject
    // names `<namespace>/<workload>`); org/ns are forced below regardless.
    let workload = sub.split('/').nth(1).unwrap_or("mcpg");
    // Accept BOTH shapes per record: an `audit/v1` Record verbatim, or an
    // mcpg-native AuditEvent (what the `dev.mcpg.audit.http` sink POSTs) mapped
    // server-side. Detection is the record's `schema` tag.
    let raw: Vec<Value> = if body.is_array() {
        serde_json::from_value(body).map_err(|e| err(StatusCode::BAD_REQUEST, e))?
    } else {
        vec![body]
    };
    if raw.len() > 1000 {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "batch over 1000"));
    }
    let mut records: Vec<agentctl_audit::Record> = Vec::with_capacity(raw.len());
    for v in raw {
        if v.get("schema").and_then(Value::as_str) == Some(agentctl_audit::SCHEMA) {
            records.push(serde_json::from_value(v).map_err(|e| err(StatusCode::BAD_REQUEST, e))?);
        } else {
            records.push(agentctl_audit::map_mcpg_native(&v, workload));
        }
    }
    let org = ns.strip_prefix("org-").unwrap_or("").to_string();
    let mut stored = 0usize;
    for r in records.iter_mut() {
        // The token decides attribution, not the payload.
        r.namespace = ns.to_string();
        r.org = org.clone();
        if agentctl_audit::pg::record(&pool, r).await.is_ok() {
            stored += 1;
        }
    }
    Ok(Json(json!({ "stored": stored, "of": records.len() })))
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
    let user = body
        .get("user")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty());
    let token = aauth
        .key
        .mint_gateway_token(&aauth.issuer, workload, audience, ttl, user);
    Ok(Json(json!({ "token": token, "expires_in": ttl })))
}

// -- RFC 8693 exchange + connection custody (P5-3) ---------------------------

const GRANT_TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const TT_ACCESS_TOKEN: &str = "urn:ietf:params:oauth:token-type:access_token";
const TT_JWT: &str = "urn:ietf:params:oauth:token-type:jwt";
const TT_PRINCIPAL: &str = "urn:agentctl:params:oauth:token-type:principal";
const TT_USER: &str = "urn:agentctl:params:oauth:token-type:user";

/// RFC 6749-shaped token-endpoint error. mcpg's issuer plugin surfaces ONLY
/// the `error` code (description is deliberately dropped from its logs), so
/// the code itself carries the diagnosis.
fn oauth_err(status: StatusCode, code: &str, desc: impl std::fmt::Display) -> Response {
    (
        status,
        Json(json!({ "error": code, "error_description": desc.to_string() })),
    )
        .into_response()
}

/// Resolve `aud` (`mcpg:<ns>`) → org: managed namespaces are `org-<org>`.
fn org_from_audience(aud: &str) -> Option<String> {
    let ns = aud.strip_prefix("mcpg:")?;
    Some(ns.strip_prefix("org-").unwrap_or(ns).to_string())
}

/// `POST /v1/exchange` — RFC 8693 token exchange (ADR-0005c): the OBO mint
/// behind mcpg's `oauth_impersonation` federation auth and any control-plane
/// caller. Form-encoded, per the RFC and mcpg's issuer wire.
///
/// The acting user is resolved from the SUBJECT token, by type:
/// - `access_token`/`jwt`: a JWT WE minted (gateway/agent token) — verified
///   against the provider key; the acting user is its `usr` claim (stamped by
///   the operator channel for user-bound workloads; a token without `usr`
///   carries no user authority) and the org comes from its `aud`.
/// - `…:principal`: a per-(user, agent) A2A principal bearer — custody lookup
///   by hash; user = the principal subject, org = the principal org.
/// - `…:user`: the bare user id — ADMIN BEARER ONLY (control-plane callers
///   that authenticated their user upstream), org from the `org` field.
///
/// `audience` names the connection (provider) in custody. Every path is
/// self-authenticating or admin-gated — there is no anonymous mint.
async fn exchange(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> Response {
    if form.get("grant_type").map(String::as_str) != Some(GRANT_TOKEN_EXCHANGE) {
        return oauth_err(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            format!("grant_type must be {GRANT_TOKEN_EXCHANGE}"),
        );
    }
    let Some(subject_token) = form.get("subject_token").filter(|t| !t.is_empty()) else {
        return oauth_err(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "subject_token is required",
        );
    };
    let subject_type = form
        .get("subject_token_type")
        .map(String::as_str)
        .unwrap_or(TT_ACCESS_TOKEN);
    // audience names the custody connection; `resource` is accepted as an
    // alias (mcpg's credential_config may set either).
    let Some(provider) = form
        .get("audience")
        .or_else(|| form.get("resource"))
        .filter(|a| !a.is_empty())
    else {
        return oauth_err(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "audience is required",
        );
    };
    let scope = form
        .get("scope")
        .map(String::as_str)
        .filter(|s| !s.is_empty());

    let (org, user) = match subject_type {
        TT_ACCESS_TOKEN | TT_JWT => {
            let Some(aauth) = &state.aauth else {
                return oauth_err(
                    StatusCode::FORBIDDEN,
                    "access_denied",
                    "token subjects need the AAuth provider role (IDENTITY_AAUTH_ISSUER)",
                );
            };
            let claims = match crate::aauth::verify_identity_jwt(
                subject_token,
                &aauth.key.public_x_b64url(),
                &aauth.issuer,
            ) {
                Ok(c) => c,
                Err(e) => return oauth_err(StatusCode::FORBIDDEN, "access_denied", e),
            };
            let Some(user) = claims.get("usr").and_then(Value::as_str) else {
                return oauth_err(
                    StatusCode::FORBIDDEN,
                    "access_denied",
                    "subject token names no acting user (usr claim) — minted for a non-user-bound workload",
                );
            };
            let org = claims
                .get("aud")
                .and_then(Value::as_str)
                .and_then(org_from_audience)
                .or_else(|| form.get("org").cloned());
            let Some(org) = org else {
                return oauth_err(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "cannot resolve org: subject aud is not mcpg:<ns> and no org field given",
                );
            };
            (org, user.to_string())
        }
        TT_PRINCIPAL => {
            let hash = crate::store::bearer_hash(subject_token);
            match state.store.find_principal_by_hash(&hash).await {
                Ok(p) => (p.org, p.subject),
                Err(_) => {
                    return oauth_err(
                        StatusCode::FORBIDDEN,
                        "access_denied",
                        "unknown principal bearer",
                    )
                }
            }
        }
        TT_USER => {
            if let Err(e) = require_admin(&state, &headers) {
                return e.into_response();
            }
            let Some(org) = form.get("org").filter(|o| !o.is_empty()) else {
                return oauth_err(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "org is required with a user subject",
                );
            };
            (org.clone(), subject_token.clone())
        }
        other => {
            return oauth_err(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("unsupported subject_token_type {other:?}"),
            )
        }
    };

    let outcome = state.exchanger.exchange(&org, &user, provider, scope).await;
    {
        let (out, detail) = match &outcome {
            Ok(m) => (agentctl_audit::OUTCOME_OK, m.outcome.as_str().to_string()),
            Err(e) => (agentctl_audit::OUTCOME_REFUSED, format!("{e:.60}")),
        };
        audit_spawn(
            &state,
            agentctl_audit::Record::new(
                "identity",
                org.clone(),
                format!("org-{org}"),
                String::new(),
                agentctl_audit::ACTION_EXCHANGE,
                out,
            )
            .user(user.clone())
            .trail(trail_of(&headers))
            .dim("provider", provider.clone())
            .dim("detail", detail),
        );
    }
    match outcome {
        Ok(minted) => {
            let expires_in = (minted.expires_unix - state.exchanger.now()).max(1);
            let body = json!({
                "access_token": minted.access_token,
                "issued_token_type": TT_ACCESS_TOKEN,
                "token_type": minted.token_type,
                "expires_in": expires_in,
                "scope": minted.scope,
            });
            (
                [("x-agentctl-exchange", minted.outcome.as_str())],
                Json(body),
            )
                .into_response()
        }
        Err(e) => {
            use crate::exchange::ExchangeError as E;
            match e {
                E::NoConnection {
                    ref org,
                    ref user,
                    ref provider,
                } => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        // The RFC 6749 code mcpg surfaces; agentctl-native
                        // callers additionally get the connect card facts
                        // (P5-4): who must connect what, where.
                        "error": "invalid_target",
                        "error_description": e.to_string(),
                        "connection_required": {
                            "org": org, "user": user, "provider": provider,
                            "connect": format!("agentctl connect {provider} --org {org}"),
                        },
                    })),
                )
                    .into_response(),
                E::ProviderRefused(_) => oauth_err(StatusCode::BAD_REQUEST, "invalid_grant", e),
                E::ProviderUnreachable(_) => oauth_err(StatusCode::BAD_GATEWAY, "server_error", e),
                E::Custody(_) => {
                    tracing::error!(error = %e, "exchange custody failure");
                    oauth_err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "custody failure",
                    )
                }
            }
        }
    }
}

/// Prometheus exposition: the exchange engine's counters (P7-2's exchange
/// panel). Text format, hand-rolled like the other control-plane surfaces.
async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    use std::sync::atomic::Ordering;
    let m = &state.exchanger.metrics;
    let mut out = String::new();
    out.push_str(
        "# HELP agentctl_identity_exchanges_total /v1/exchange mints by outcome.\n# TYPE agentctl_identity_exchanges_total counter\n",
    );
    for (outcome, v) in [
        ("cache", m.cache_hits.load(Ordering::Relaxed)),
        ("mint", m.mints.load(Ordering::Relaxed)),
        ("refresh", m.refreshes.load(Ordering::Relaxed)),
        ("error", m.errors.load(Ordering::Relaxed)),
    ] {
        out.push_str(&format!(
            "agentctl_identity_exchanges_total{{outcome=\"{outcome}\"}} {v}\n"
        ));
    }
    out.push_str(
        "# HELP agentctl_identity_exchange_mint_seconds_sum Time spent minting (non-cache) upstream tokens.\n# TYPE agentctl_identity_exchange_mint_seconds_sum counter\n",
    );
    out.push_str(&format!(
        "agentctl_identity_exchange_mint_seconds_sum {}\n",
        m.mint_micros_sum.load(Ordering::Relaxed) as f64 / 1e6
    ));
    out.push_str(
        "# HELP agentctl_identity_exchange_mint_seconds_count Mint (non-cache) exchange calls.\n# TYPE agentctl_identity_exchange_mint_seconds_count counter\n",
    );
    out.push_str(&format!(
        "agentctl_identity_exchange_mint_seconds_count {}\n",
        m.mint_count.load(Ordering::Relaxed)
    ));
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        out,
    )
        .into_response()
}

/// `POST /admin/connections` — upsert a custody connection (P5-4's consent
/// flow lands on top of this same row; admin seeding is the P5-3 primitive).
/// `{org, user, provider, kind, secret, token_endpoint?, client_id?,
/// client_secret?, scope?}`. Secrets are sealed HERE — plaintext never
/// reaches the store, and no read surface returns it.
async fn admin_connections_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let field = |k: &str| -> Result<String, ApiError> {
        body.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("{k} is required")))
    };
    let (org, user, provider) = (field("org")?, field("user")?, field("provider")?);
    let kind = body
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("static")
        .to_string();
    if kind != "static" && kind != "oauth_refresh" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "kind must be static or oauth_refresh",
        ));
    }
    let secret = field("secret")?;
    let token_endpoint = body
        .get("token_endpoint")
        .and_then(Value::as_str)
        .map(str::to_string);
    if kind == "oauth_refresh" && token_endpoint.is_none() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "oauth_refresh needs token_endpoint",
        ));
    }
    let sealed_secret = state
        .sealer
        .seal(
            &crate::exchange::secret_aad(&org, &user, &provider),
            secret.as_bytes(),
        )
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let sealed_client_secret = match body.get("client_secret").and_then(Value::as_str) {
        Some(cs) if !cs.is_empty() => Some(
            state
                .sealer
                .seal(
                    &crate::exchange::client_secret_aad(&org, &user, &provider),
                    cs.as_bytes(),
                )
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?,
        ),
        _ => None,
    };
    let now = now_unix();
    state
        .store
        .put_connection(crate::store::ConnectionRecord {
            org: org.clone(),
            user: user.clone(),
            provider: provider.clone(),
            kind,
            sealed_secret,
            token_endpoint,
            client_id: body
                .get("client_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            sealed_client_secret,
            scope: body
                .get("scope")
                .and_then(Value::as_str)
                .map(str::to_string),
            created_unix: now,
            updated_unix: now,
        })
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    // A rotation must take effect NOW, not at cache expiry.
    state.exchanger.invalidate(&org, &user, &provider);
    Ok(Json(json!({ "ok": true })))
}

/// `GET /admin/connections?org=&user=` — list custody rows, SECRET-FREE.
async fn admin_connections_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let Some(org) = q.get("org").filter(|o| !o.is_empty()) else {
        return Err(err(StatusCode::BAD_REQUEST, "org is required"));
    };
    let rows = state
        .store
        .list_connections(org, q.get("user").map(String::as_str))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|c| {
            json!({
                "org": c.org, "user": c.user, "provider": c.provider, "kind": c.kind,
                "token_endpoint": c.token_endpoint, "client_id": c.client_id,
                "scope": c.scope, "created_unix": c.created_unix, "updated_unix": c.updated_unix,
            })
        })
        .collect();
    Ok(Json(json!({ "connections": rows })))
}

/// `POST /admin/connections/delete {org, user, provider}` — revocation:
/// custody row gone + cache dropped ⇒ all downstream minting stops.
async fn admin_connections_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let field = |k: &str| -> Result<String, ApiError> {
        body.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("{k} is required")))
    };
    let (org, user, provider) = (field("org")?, field("user")?, field("provider")?);
    let deleted = state
        .store
        .delete_connection(&org, &user, &provider)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    state.exchanger.invalidate(&org, &user, &provider);
    if deleted {
        audit_spawn(
            &state,
            agentctl_audit::Record::new(
                "identity",
                org.clone(),
                format!("org-{org}"),
                String::new(),
                agentctl_audit::ACTION_CONNECTION_REVOKED,
                agentctl_audit::OUTCOME_OK,
            )
            .user(user.clone())
            .dim("provider", provider.clone()),
        );
    }
    Ok(Json(json!({ "deleted": deleted })))
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
        let store: Arc<MemoryStore> = Arc::new(MemoryStore::default());
        let sealer = Arc::new(crate::seal::Sealer::new([9u8; 32]));
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
            store: store.clone(),
            admin_token: admin.map(str::to_string),
            aauth: Some(Arc::new(AauthState {
                key: crate::aauth::ProviderKey::from_seed(&[3u8; 32]).unwrap(),
                issuer: "http://identity.test".into(),
                token_ttl: 300,
            })),
            exchanger: Arc::new(crate::exchange::Exchanger::new(
                store,
                sealer.clone(),
                crate::oidc::outbound_client(),
                300,
                60,
            )),
            sealer,
            audit: None,
        })
    }

    /// Form-encoded POST (the RFC 8693 wire) with optional bearer.
    async fn call_form(
        app: Router,
        path: &str,
        bearer: Option<&str>,
        form: &[(&str, &str)],
    ) -> (StatusCode, Option<String>, Value) {
        let body: String = form
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded");
        if let Some(b) = bearer {
            req = req.header("authorization", format!("Bearer {b}"));
        }
        let resp = app
            .oneshot(req.body(axum::body::Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let outcome = resp
            .headers()
            .get("x-agentctl-exchange")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            outcome,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Minimal percent-encoding for test form bodies (covers ':' '/' '+').
    fn urlencode(v: &str) -> String {
        let mut out = String::new();
        for b in v.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    const GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";

    /// Seed a static custody connection through the ADMIN surface (the same
    /// sealing path production uses), then exchange with each subject type.
    #[tokio::test]
    async fn exchange_full_ladder_over_admin_seeded_connection() {
        let st = state(Some("adm"));
        // Seed: acme/okta:andrii ↔ zendesk.
        let (code, _) = call(
            router(st.clone()),
            "POST",
            "/admin/connections",
            Some("adm"),
            json!({"org": "acme", "user": "okta:andrii", "provider": "zendesk", "secret": "sk-live-fake"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);

        // Leg 1: subject = OUR gateway JWT with usr claim, org from aud.
        let aauth = st.aauth.as_ref().unwrap();
        let jwt = aauth.key.mint_gateway_token(
            &aauth.issuer,
            "org-acme/sup-andrii",
            "mcpg:org-acme",
            300,
            Some("okta:andrii"),
        );
        let (code, outcome, body) = call_form(
            router(st.clone()),
            "/v1/exchange",
            None,
            &[
                ("grant_type", GRANT),
                ("subject_token", &jwt),
                ("audience", "zendesk"),
                ("client_id", "mcpg-tenant"),
            ],
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(outcome.as_deref(), Some("mint"));
        assert_eq!(body["access_token"], "sk-live-fake");
        assert_eq!(body["token_type"], "Bearer");
        assert!(body["expires_in"].as_i64().unwrap() > 0);

        // Same subject again: served from cache.
        let (_, outcome, _) = call_form(
            router(st.clone()),
            "/v1/exchange",
            None,
            &[
                ("grant_type", GRANT),
                ("subject_token", &jwt),
                ("audience", "zendesk"),
            ],
        )
        .await;
        assert_eq!(outcome.as_deref(), Some("cache"));

        // Leg 1 refusal: a token WITHOUT usr carries no user authority.
        let no_usr = aauth.key.mint_gateway_token(
            &aauth.issuer,
            "org-acme/some-agent",
            "mcpg:org-acme",
            300,
            None,
        );
        let (code, _, body) = call_form(
            router(st.clone()),
            "/v1/exchange",
            None,
            &[
                ("grant_type", GRANT),
                ("subject_token", &no_usr),
                ("audience", "zendesk"),
            ],
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "access_denied");

        // Leg 2: principal bearer resolves (user, org) from custody.
        st.store
            .put_principal(crate::store::PrincipalRecord {
                org: "acme".into(),
                namespace: "org-acme".into(),
                agent: "sup-andrii".into(),
                subject: "okta:andrii".into(),
                bearer_hash: crate::store::bearer_hash("principal-bearer-1"),
                created_unix: 0,
            })
            .await
            .unwrap();
        let (code, _, body) = call_form(
            router(st.clone()),
            "/v1/exchange",
            None,
            &[
                ("grant_type", GRANT),
                ("subject_token", "principal-bearer-1"),
                (
                    "subject_token_type",
                    "urn:agentctl:params:oauth:token-type:principal",
                ),
                ("audience", "zendesk"),
            ],
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["access_token"], "sk-live-fake");

        // Leg 3: bare user subject is ADMIN ONLY.
        let leg3 = [
            ("grant_type", GRANT),
            ("subject_token", "okta:andrii"),
            (
                "subject_token_type",
                "urn:agentctl:params:oauth:token-type:user",
            ),
            ("audience", "zendesk"),
            ("org", "acme"),
        ];
        let (code, _, _) = call_form(router(st.clone()), "/v1/exchange", None, &leg3).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, _, body) =
            call_form(router(st.clone()), "/v1/exchange", Some("adm"), &leg3).await;
        assert_eq!(code, StatusCode::OK, "{body}");

        // Unknown provider → invalid_target (the code mcpg surfaces).
        let (code, _, body) = call_form(
            router(st.clone()),
            "/v1/exchange",
            None,
            &[
                ("grant_type", GRANT),
                ("subject_token", &jwt),
                ("audience", "github"),
            ],
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_target");

        // Wrong grant_type refused.
        let (code, _, body) = call_form(
            router(st.clone()),
            "/v1/exchange",
            None,
            &[
                ("grant_type", "client_credentials"),
                ("subject_token", &jwt),
                ("audience", "zendesk"),
            ],
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "unsupported_grant_type");

        // Deletion is revocation: row + cache go together.
        let (code, _) = call(
            router(st.clone()),
            "POST",
            "/admin/connections/delete",
            Some("adm"),
            json!({"org": "acme", "user": "okta:andrii", "provider": "zendesk"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let (code, _, body) = call_form(
            router(st.clone()),
            "/v1/exchange",
            None,
            &[
                ("grant_type", GRANT),
                ("subject_token", &jwt),
                ("audience", "zendesk"),
            ],
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_target");
    }

    /// P5-4: the whole consent flow against an in-process IdP — start (with
    /// offline_access appended), pending poll, approval, then the refresh
    /// token lands SEALED in custody (never in the poll response) and the
    /// exchange mints from it immediately.
    #[tokio::test]
    async fn connect_flow_stores_refresh_token_and_feeds_the_exchange() {
        use axum::extract::Form;
        use axum::routing::{get, post};
        use std::collections::HashMap as Map;
        use std::sync::atomic::{AtomicU64, Ordering};

        // In-process IdP: discovery + device + token endpoints. First token
        // poll is authorization_pending; the second issues tokens WITH a
        // refresh token, and refresh_token grants mint at-<n>.
        struct Idp {
            polls: AtomicU64,
            scope_seen: std::sync::Mutex<String>,
        }
        let idp = Arc::new(Idp {
            polls: AtomicU64::new(0),
            scope_seen: std::sync::Mutex::new(String::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let (b1, b2) = (base.clone(), base.clone());
        let (i1, i2) = (idp.clone(), idp.clone());
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let b = b1.clone();
                    async move {
                        Json(json!({
                            "issuer": b,
                            "token_endpoint": format!("{b}/token"),
                            "device_authorization_endpoint": format!("{b}/device"),
                            "jwks_uri": format!("{b}/jwks.json"),
                        }))
                    }
                }),
            )
            .route(
                "/device",
                post(move |Form(f): Form<Map<String, String>>| {
                    let idp = i1.clone();
                    let b = b2.clone();
                    async move {
                        *idp.scope_seen.lock().unwrap() =
                            f.get("scope").cloned().unwrap_or_default();
                        Json(json!({
                            "device_code": "dc-1",
                            "user_code": "ABCD-EFGH",
                            "verification_uri": format!("{b}/activate"),
                            "expires_in": 300,
                            "interval": 1,
                        }))
                    }
                }),
            )
            .route(
                "/token",
                post(move |Form(f): Form<Map<String, String>>| {
                    let idp = i2.clone();
                    async move {
                        match f.get("grant_type").map(String::as_str) {
                            Some("urn:ietf:params:oauth:grant-type:device_code") => {
                                if idp.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                                    return (
                                        StatusCode::BAD_REQUEST,
                                        Json(json!({"error": "authorization_pending"})),
                                    )
                                        .into_response();
                                }
                                Json(json!({
                                    "access_token": "at-consent",
                                    "refresh_token": "rt-consent-0",
                                    "expires_in": 3600,
                                }))
                                .into_response()
                            }
                            Some("refresh_token") => Json(json!({
                                "access_token": format!(
                                    "at-{}",
                                    f.get("refresh_token").cloned().unwrap_or_default()
                                ),
                                "expires_in": 3600,
                            }))
                            .into_response(),
                            _ => (
                                StatusCode::BAD_REQUEST,
                                Json(json!({"error": "unsupported_grant_type"})),
                            )
                                .into_response(),
                        }
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // State whose "dev" provider points at the in-process IdP.
        let store: Arc<MemoryStore> = Arc::new(MemoryStore::default());
        let sealer = Arc::new(crate::seal::Sealer::new([9u8; 32]));
        let st = Arc::new(AppState {
            federation: Federation::new(
                outbound_client(),
                vec![Provider {
                    name: "zendesk".into(),
                    issuer: base.clone(),
                    client_id: "agentctl-cli".into(),
                    client_secret: None,
                    audiences: vec![],
                    scopes: vec!["openid".into()],
                    groups_claim: "groups".into(),
                }],
            ),
            store: store.clone(),
            admin_token: Some("adm".into()),
            aauth: None,
            exchanger: Arc::new(crate::exchange::Exchanger::new(
                store,
                sealer.clone(),
                crate::oidc::outbound_client(),
                300,
                60,
            )),
            sealer,
            audit: None,
        });

        // Start (admin channel names the user; a real CLI presents the
        // user's own identity token instead).
        let (code, body) = call(
            router(st.clone()),
            "POST",
            "/v1/connections/start",
            Some("adm"),
            json!({"provider": "zendesk", "org": "acme", "user": "okta:andrii"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let handle = body["handle"].as_str().unwrap().to_string();
        assert!(body["verification_uri"]
            .as_str()
            .unwrap()
            .contains("/activate"));
        // The consent flow asked for offline power; login never does.
        assert!(idp.scope_seen.lock().unwrap().contains("offline_access"));

        // First poll: pending. Second: consent granted, connection stored.
        let (_, body) = call(
            router(st.clone()),
            "POST",
            "/v1/connections/poll",
            None,
            json!({"handle": handle}),
        )
        .await;
        assert_eq!(body["status"], "pending");
        let (code, body) = call(
            router(st.clone()),
            "POST",
            "/v1/connections/poll",
            None,
            json!({"handle": handle}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "ok");
        assert!(
            body.get("access_token").is_none() && body.get("refresh_token").is_none(),
            "no token may leave the connect path: {body}"
        );

        // Custody now feeds the exchange: the refresh token redeems at the
        // IdP and the minted access token flows out.
        let (code, _, body) = call_form(
            router(st.clone()),
            "/v1/exchange",
            Some("adm"),
            &[
                ("grant_type", GRANT),
                ("subject_token", "okta:andrii"),
                (
                    "subject_token_type",
                    "urn:agentctl:params:oauth:token-type:user",
                ),
                ("audience", "zendesk"),
                ("org", "acme"),
            ],
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["access_token"], "at-rt-consent-0");

        // Wrong-poller guards both ways.
        let (code, body) = call(
            router(st.clone()),
            "POST",
            "/v1/device/poll",
            None,
            json!({"handle": handle}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    }

    /// A missing connection's exchange refusal carries the CONNECT CARD facts
    /// (P5-4): org, user, provider, and the CLI one-liner.
    #[tokio::test]
    async fn exchange_refusal_names_the_required_connection() {
        let st = state(Some("adm"));
        let (code, _, body) = call_form(
            router(st.clone()),
            "/v1/exchange",
            Some("adm"),
            &[
                ("grant_type", GRANT),
                ("subject_token", "okta:andrii"),
                (
                    "subject_token_type",
                    "urn:agentctl:params:oauth:token-type:user",
                ),
                ("audience", "github"),
                ("org", "acme"),
            ],
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_target");
        let card = &body["connection_required"];
        assert_eq!(card["provider"], "github");
        assert_eq!(card["org"], "acme");
        assert_eq!(card["user"], "okta:andrii");
        assert_eq!(card["connect"], "agentctl connect github --org acme");
    }

    #[tokio::test]
    async fn connections_admin_list_is_secret_free_and_gated() {
        let st = state(Some("adm"));
        let (code, _) = call(
            router(st.clone()),
            "POST",
            "/admin/connections",
            Some("adm"),
            json!({"org": "acme", "user": "u1", "provider": "gh", "kind": "oauth_refresh",
                   "secret": "rt-secret", "token_endpoint": "https://gh.test/token",
                   "client_id": "cid", "client_secret": "cs-secret"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let (code, body) = call(
            router(st.clone()),
            "GET",
            "/admin/connections?org=acme",
            Some("adm"),
            Value::Null,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let listed = body["connections"].to_string();
        assert!(listed.contains("\"gh\""));
        assert!(!listed.contains("rt-secret"));
        assert!(!listed.contains("cs-secret"));
        assert!(!listed.contains("sealed"));
        // Unauthenticated list refused.
        let (code, _) = call(
            router(st.clone()),
            "GET",
            "/admin/connections?org=acme",
            None,
            Value::Null,
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
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
