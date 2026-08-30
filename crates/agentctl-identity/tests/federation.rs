// SPDX-License-Identifier: BUSL-1.1
//! Federation integration: an in-process mock OIDC issuer (discovery + JWKS +
//! device + token endpoints) proves the whole login/validation loop without a
//! network: device start → (mock user approves) → poll → access token →
//! introspection resolves subject/groups; issuer-mismatch discovery is
//! refused (the host-poisoning guard); bad-audience tokens are refused.
//!
//! The RSA key under `tests/keys/` is TEST-ONLY material generated for this
//! suite; nothing outside these tests trusts it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::json;

use agentctl_identity::config::Provider;
use agentctl_identity::oidc::{outbound_client, Federation};

const KEY_PEM: &str = include_str!("keys/test-idp.pem");
const KEY_N: &str = include_str!("keys/test-idp-n.txt");
const KEY_E: &str = include_str!("keys/test-idp-e.txt");
const KID: &str = "test-1";

struct Idp {
    issuer: String,
    approved: AtomicBool,
    /// When set, discovery lies about its issuer (poisoning simulation).
    poisoned: bool,
}

fn sign_token(issuer: &str, aud: &str, sub: &str, groups: &[&str]) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 300;
    let claims = json!({
        "iss": issuer,
        "aud": aud,
        "sub": sub,
        "email": format!("{sub}@example.test"),
        "groups": groups,
        "scope": "openid profile",
        "exp": exp,
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_string());
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("test key"),
    )
    .expect("sign")
}

async fn spawn_idp(poisoned: bool) -> (Arc<Idp>, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let issuer = format!("http://127.0.0.1:{}", addr.port());
    let idp = Arc::new(Idp {
        issuer: issuer.clone(),
        approved: AtomicBool::new(false),
        poisoned,
    });

    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(|State(idp): State<Arc<Idp>>| async move {
                let claimed = if idp.poisoned {
                    "https://evil.example".to_string()
                } else {
                    idp.issuer.clone()
                };
                Json(json!({
                    "issuer": claimed,
                    "jwks_uri": format!("{}/jwks.json", idp.issuer),
                    "device_authorization_endpoint": format!("{}/device", idp.issuer),
                    "token_endpoint": format!("{}/token", idp.issuer),
                }))
            }),
        )
        .route(
            "/jwks.json",
            get(|| async {
                Json(json!({ "keys": [{
                    "kty": "RSA", "kid": KID, "alg": "RS256", "use": "sig",
                    "n": KEY_N, "e": KEY_E,
                }]}))
            }),
        )
        .route(
            "/device",
            post(|State(idp): State<Arc<Idp>>| async move {
                Json(json!({
                    "device_code": "dc-1",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": format!("{}/activate", idp.issuer),
                    "expires_in": 600,
                    "interval": 1,
                }))
            }),
        )
        .route(
            "/token",
            post(|State(idp): State<Arc<Idp>>| async move {
                if idp.approved.load(Ordering::SeqCst) {
                    let tok = sign_token(&idp.issuer, "agentctl-cli", "alice", &["eng"]);
                    Json(json!({ "access_token": tok, "expires_in": 300, "refresh_token": "rt-1" }))
                        .into_response()
                } else {
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "authorization_pending" })),
                    )
                        .into_response()
                }
            }),
        )
        .with_state(idp.clone());

    use axum::response::IntoResponse;
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (idp, issuer)
}

fn provider(issuer: &str) -> Provider {
    serde_json::from_value(json!({
        "name": "mock",
        "issuer": issuer,
        "client_id": "agentctl-cli",
    }))
    .unwrap()
}

#[tokio::test]
async fn device_flow_and_validation_end_to_end() {
    let (idp, issuer) = spawn_idp(false).await;
    let fed = Federation::new(outbound_client(), vec![provider(&issuer)]);

    // Start: the mock hands out a user code + verification URI.
    let start = fed.device_start("mock").await.expect("device start");
    assert_eq!(start.user_code, "ABCD-EFGH");
    assert!(start.verification_uri.ends_with("/activate"));

    // Pending until the human approves at the IdP.
    let pending = fed.device_poll("mock", &start.device_code).await;
    assert!(matches!(
        pending,
        Err(agentctl_identity::oidc::OidcError::AuthorizationPending)
    ));

    // The human approves; the poll completes with tokens.
    idp.approved.store(true, Ordering::SeqCst);
    let tokens = fed
        .device_poll("mock", &start.device_code)
        .await
        .expect("tokens");
    assert!(tokens.refresh_token.is_some(), "custody material arrives");

    // The access token validates: subject is provider-prefixed, groups flow.
    let id = fed
        .validate("mock", &tokens.access_token)
        .await
        .expect("validate");
    assert_eq!(id.subject, "mock:alice");
    assert_eq!(id.groups, vec!["eng"]);
    assert_eq!(id.email.as_deref(), Some("alice@example.test"));
    assert!(id.scopes.contains(&"openid".to_string()));

    // validate_any finds the right provider unaided.
    assert_eq!(
        fed.validate_any(&tokens.access_token)
            .await
            .unwrap()
            .subject,
        "mock:alice"
    );
}

#[tokio::test]
async fn wrong_audience_is_refused() {
    let (_idp, issuer) = spawn_idp(false).await;
    let fed = Federation::new(outbound_client(), vec![provider(&issuer)]);
    let bad = sign_token(&issuer, "some-other-app", "alice", &[]);
    let err = fed.validate("mock", &bad).await.unwrap_err();
    assert!(
        format!("{err}").contains("Invalid") || format!("{err}").to_lowercase().contains("aud")
    );
}

#[tokio::test]
async fn poisoned_discovery_is_refused() {
    let (_idp, issuer) = spawn_idp(true).await;
    let fed = Federation::new(outbound_client(), vec![provider(&issuer)]);
    let err = fed.device_start("mock").await.unwrap_err();
    assert!(
        format!("{err}").contains("issuer mismatch"),
        "the host-poisoning guard must name the mismatch: {err}"
    );
}
