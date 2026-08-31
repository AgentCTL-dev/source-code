// SPDX-License-Identifier: BUSL-1.1
//! The RFC 8693 exchange engine (ADR-0005c, P5-3): (acting user, provider)
//! → a short-lived upstream access token, minted from custody and cached.
//!
//! Two connection kinds:
//! - `static`: the sealed secret IS the upstream credential (API keys, PATs).
//!   Re-issued with a short synthetic `expires_in` so callers observe the
//!   cache/refresh cycle instead of holding one immortal value.
//! - `oauth_refresh`: the sealed secret is an offline refresh token, redeemed
//!   at the provider's `token_endpoint` per mint (RFC 6749 §6). Refresh-token
//!   ROTATION is honored: a `refresh_token` in the response re-seals into
//!   custody before the new access token is returned — losing that race means
//!   the connection dies at the next mint, so persist-first.
//!
//! The cache is process-local by (org, user, provider, scope): identity is a
//! singleton in P1; when it scales horizontally the cache is a per-replica
//! optimization and custody stays the truth. mcpg's host-side credential
//! cache sits in FRONT of this one (keyed caller × plugin × target), so keep
//! `expires_in` short — revocation surfaces only when both lapse.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::seal::Sealer;
use crate::store::{ConnectionRecord, Store, StoreError};

/// How a mint was satisfied — surfaced as the `x-agentctl-exchange` response
/// header so tests (and operators) can see the cache doing its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Served from the in-process cache.
    Cache,
    /// Minted fresh (first sight of this key).
    Mint,
    /// Re-minted over an expired cache entry.
    Refresh,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Cache => "cache",
            Outcome::Mint => "mint",
            Outcome::Refresh => "refresh",
        }
    }
}

/// AAD context binding a connection's sealed SECRET to its row (seal.rs
/// row-swap defense). The admin surface must seal with these same contexts.
pub fn secret_aad(org: &str, user: &str, provider: &str) -> String {
    format!("connection/{org}/{user}/{provider}")
}

/// AAD for the optional sealed OAuth client secret.
pub fn client_secret_aad(org: &str, user: &str, provider: &str) -> String {
    format!("connection-client/{org}/{user}/{provider}")
}

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    /// No connection in custody for (org, user, provider) → `invalid_target`.
    #[error("no connection for user {user:?} to provider {provider:?} in org {org:?}")]
    NoConnection {
        org: String,
        user: String,
        provider: String,
    },
    /// The provider refused the refresh grant (revoked/expired offline token)
    /// → `invalid_grant`.
    #[error("provider refused the refresh grant: {0}")]
    ProviderRefused(String),
    /// Transport/5xx at the provider → `server_error` (fail closed, retry is
    /// the caller's next request).
    #[error("provider unreachable: {0}")]
    ProviderUnreachable(String),
    /// Custody/seal defects (wrong seal key, corrupt row) → `server_error`.
    #[error("custody error: {0}")]
    Custody(String),
}

/// A minted upstream token, as returned to the redeem caller.
#[derive(Debug, Clone)]
pub struct Minted {
    pub access_token: String,
    pub token_type: String,
    pub expires_unix: i64,
    pub scope: Option<String>,
    pub outcome: Outcome,
}

/// Counters for the `/metrics` exposition (P7-2's deferred exchange panel).
#[derive(Debug, Default)]
pub struct ExchangeMetrics {
    pub cache_hits: AtomicU64,
    pub mints: AtomicU64,
    pub refreshes: AtomicU64,
    pub errors: AtomicU64,
    /// Latency accounting in MICROseconds (sum) over mint+refresh calls only
    /// — cache hits are ~0 and would flatter the average into noise.
    pub mint_micros_sum: AtomicU64,
    pub mint_count: AtomicU64,
}

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    token_type: String,
    scope: Option<String>,
    expires_unix: i64,
}

/// Injectable clock: unix seconds. Tests walk it days forward in steps.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct Exchanger {
    store: Arc<dyn Store>,
    sealer: Arc<Sealer>,
    http: reqwest::Client,
    cache: Mutex<HashMap<(String, String, String, String), CachedToken>>,
    /// Synthetic lifetime for `static` connections (seconds).
    pub static_ttl: i64,
    /// Serve from cache only while `expires - margin > now` — the margin is
    /// the "proactive refresh" window (a token is never handed out with less
    /// than `margin` seconds of life left).
    pub refresh_margin: i64,
    pub metrics: ExchangeMetrics,
    clock: Clock,
}

impl Exchanger {
    pub fn new(
        store: Arc<dyn Store>,
        sealer: Arc<Sealer>,
        http: reqwest::Client,
        static_ttl: i64,
        refresh_margin: i64,
    ) -> Exchanger {
        Exchanger {
            store,
            sealer,
            http,
            cache: Mutex::new(HashMap::new()),
            static_ttl,
            refresh_margin,
            metrics: ExchangeMetrics::default(),
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            }),
        }
    }

    /// Replace the clock (tests: simulated days).
    pub fn with_clock(mut self, clock: Clock) -> Exchanger {
        self.clock = clock;
        self
    }

    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Drop any cached token for the tuple — called on connection
    /// upsert/delete so a rotation takes effect immediately, not at expiry.
    pub fn invalidate(&self, org: &str, user: &str, provider: &str) {
        self.cache
            .lock()
            .unwrap()
            .retain(|(o, u, p, _), _| !(o == org && u == user && p == provider));
    }

    /// The exchange: cache → custody → adapter. `scope` narrows the cache key
    /// and rides through to the provider on `oauth_refresh`.
    pub async fn exchange(
        &self,
        org: &str,
        user: &str,
        provider: &str,
        scope: Option<&str>,
    ) -> Result<Minted, ExchangeError> {
        let now = self.now();
        let key = (
            org.to_string(),
            user.to_string(),
            provider.to_string(),
            scope.unwrap_or_default().to_string(),
        );
        let had_entry = {
            let cache = self.cache.lock().unwrap();
            match cache.get(&key) {
                Some(c) if c.expires_unix - self.refresh_margin > now => {
                    self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(Minted {
                        access_token: c.access_token.clone(),
                        token_type: c.token_type.clone(),
                        expires_unix: c.expires_unix,
                        scope: c.scope.clone(),
                        outcome: Outcome::Cache,
                    });
                }
                Some(_) => true,
                None => false,
            }
        };

        let conn = match self.store.find_connection(org, user, provider).await {
            Ok(c) => c,
            Err(StoreError::NotFound) => {
                self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                return Err(ExchangeError::NoConnection {
                    org: org.to_string(),
                    user: user.to_string(),
                    provider: provider.to_string(),
                });
            }
            Err(e) => {
                self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                return Err(ExchangeError::Custody(e.to_string()));
            }
        };

        let started = std::time::Instant::now();
        let minted = match conn.kind.as_str() {
            "static" => self.mint_static(&conn, now),
            "oauth_refresh" => self.mint_oauth_refresh(&conn, scope, now).await,
            other => Err(ExchangeError::Custody(format!(
                "connection kind {other:?} is not supported"
            ))),
        };
        match minted {
            Ok(mut m) => {
                self.metrics
                    .mint_micros_sum
                    .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                self.metrics.mint_count.fetch_add(1, Ordering::Relaxed);
                m.outcome = if had_entry {
                    Outcome::Refresh
                } else {
                    Outcome::Mint
                };
                if had_entry {
                    self.metrics.refreshes.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.metrics.mints.fetch_add(1, Ordering::Relaxed);
                }
                self.cache.lock().unwrap().insert(
                    key,
                    CachedToken {
                        access_token: m.access_token.clone(),
                        token_type: m.token_type.clone(),
                        scope: m.scope.clone(),
                        expires_unix: m.expires_unix,
                    },
                );
                Ok(m)
            }
            Err(e) => {
                self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    fn mint_static(&self, conn: &ConnectionRecord, now: i64) -> Result<Minted, ExchangeError> {
        let aad = secret_aad(&conn.org, &conn.user, &conn.provider);
        let secret = self
            .sealer
            .unseal(&aad, &conn.sealed_secret)
            .map_err(|e| ExchangeError::Custody(e.to_string()))
            .and_then(|b| {
                String::from_utf8(b)
                    .map_err(|_| ExchangeError::Custody("secret is not UTF-8".into()))
            })?;
        Ok(Minted {
            access_token: secret,
            token_type: "Bearer".into(),
            expires_unix: now + self.static_ttl,
            scope: conn.scope.clone(),
            outcome: Outcome::Mint,
        })
    }

    async fn mint_oauth_refresh(
        &self,
        conn: &ConnectionRecord,
        scope: Option<&str>,
        now: i64,
    ) -> Result<Minted, ExchangeError> {
        let endpoint = conn.token_endpoint.as_deref().ok_or_else(|| {
            ExchangeError::Custody("oauth_refresh connection has no token_endpoint".into())
        })?;
        let aad = secret_aad(&conn.org, &conn.user, &conn.provider);
        let refresh_token = self
            .sealer
            .unseal(&aad, &conn.sealed_secret)
            .map_err(|e| ExchangeError::Custody(e.to_string()))
            .and_then(|b| {
                String::from_utf8(b)
                    .map_err(|_| ExchangeError::Custody("secret is not UTF-8".into()))
            })?;
        let mut form: Vec<(&str, String)> = vec![
            ("grant_type", "refresh_token".into()),
            ("refresh_token", refresh_token),
        ];
        if let Some(cid) = &conn.client_id {
            form.push(("client_id", cid.clone()));
        }
        if let Some(sealed) = &conn.sealed_client_secret {
            let cs = self
                .sealer
                .unseal(
                    &client_secret_aad(&conn.org, &conn.user, &conn.provider),
                    sealed,
                )
                .map_err(|e| ExchangeError::Custody(e.to_string()))
                .and_then(|b| {
                    String::from_utf8(b)
                        .map_err(|_| ExchangeError::Custody("client secret is not UTF-8".into()))
                })?;
            form.push(("client_secret", cs));
        }
        if let Some(s) = scope.or(conn.scope.as_deref()) {
            form.push(("scope", s.to_string()));
        }
        let resp = self
            .http
            .post(endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| ExchangeError::ProviderUnreachable(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ExchangeError::ProviderUnreachable(format!("bad token response: {e}")))?;
        if !status.is_success() {
            let code = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return if status.is_server_error() {
                Err(ExchangeError::ProviderUnreachable(format!(
                    "provider {status}: {code}"
                )))
            } else {
                Err(ExchangeError::ProviderRefused(code.to_string()))
            };
        }
        let access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExchangeError::ProviderUnreachable("token response has no access_token".into())
            })?
            .to_string();
        // Rotation: persist the replacement refresh token BEFORE returning —
        // a crash after return would otherwise strand custody on a token the
        // provider just invalidated.
        if let Some(rotated) = body.get("refresh_token").and_then(Value::as_str) {
            let sealed = self
                .sealer
                .seal(&aad, rotated.as_bytes())
                .map_err(|e| ExchangeError::Custody(e.to_string()))?;
            let mut updated = conn.clone();
            updated.sealed_secret = sealed;
            updated.updated_unix = now;
            self.store
                .put_connection(updated)
                .await
                .map_err(|e| ExchangeError::Custody(e.to_string()))?;
        }
        let expires_in = body
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(3600)
            .max(1);
        Ok(Minted {
            access_token,
            token_type: body
                .get("token_type")
                .and_then(Value::as_str)
                .unwrap_or("Bearer")
                .to_string(),
            expires_unix: now + expires_in,
            scope: body
                .get("scope")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| scope.map(str::to_string)),
            outcome: Outcome::Mint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use std::sync::atomic::AtomicI64;

    fn sealer() -> Sealer {
        Sealer::new([7u8; 32])
    }

    fn conn_static(sealer: &Sealer) -> ConnectionRecord {
        ConnectionRecord {
            org: "acme".into(),
            user: "andrii".into(),
            provider: "zendesk".into(),
            kind: "static".into(),
            sealed_secret: sealer
                .seal(&secret_aad("acme", "andrii", "zendesk"), b"sk-live-fake")
                .unwrap(),
            token_endpoint: None,
            client_id: None,
            sealed_client_secret: None,
            scope: None,
            created_unix: 0,
            updated_unix: 0,
        }
    }

    /// A fixed-step clock: every call returns the value of the shared atomic.
    fn test_clock(now: Arc<AtomicI64>) -> Clock {
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn static_connection_caches_and_refreshes_over_simulated_days() {
        let store = Arc::new(MemoryStore::default());
        let s = sealer();
        store.put_connection(conn_static(&s)).await.unwrap();
        let now = Arc::new(AtomicI64::new(1_000_000));
        let ex = Exchanger::new(store, Arc::new(s), crate::oidc::outbound_client(), 300, 60)
            .with_clock(test_clock(now.clone()));

        // First mint, then a burst inside the ttl: exactly one mint.
        let first = ex
            .exchange("acme", "andrii", "zendesk", None)
            .await
            .unwrap();
        assert_eq!(first.outcome, Outcome::Mint);
        assert_eq!(first.access_token, "sk-live-fake");
        for _ in 0..5 {
            let m = ex
                .exchange("acme", "andrii", "zendesk", None)
                .await
                .unwrap();
            assert_eq!(m.outcome, Outcome::Cache);
        }

        // Simulate two days at one exchange per hour: each hour crosses the
        // 300s ttl, so every call after the first in an hour slot re-mints.
        let mut refreshes = 0;
        for hour in 1..=48 {
            now.store(1_000_000 + hour * 3600, Ordering::SeqCst);
            let m = ex
                .exchange("acme", "andrii", "zendesk", None)
                .await
                .unwrap();
            assert_eq!(m.outcome, Outcome::Refresh, "hour {hour}");
            refreshes += 1;
            // Within the same hour the cache serves.
            let again = ex
                .exchange("acme", "andrii", "zendesk", None)
                .await
                .unwrap();
            assert_eq!(again.outcome, Outcome::Cache);
        }
        assert_eq!(refreshes, 48);
        assert_eq!(ex.metrics.refreshes.load(Ordering::Relaxed), 48);
        assert_eq!(ex.metrics.mints.load(Ordering::Relaxed), 1);
        assert_eq!(ex.metrics.cache_hits.load(Ordering::Relaxed), 5 + 48);
    }

    #[tokio::test]
    async fn refresh_margin_re_mints_before_expiry() {
        let store = Arc::new(MemoryStore::default());
        let s = sealer();
        store.put_connection(conn_static(&s)).await.unwrap();
        let now = Arc::new(AtomicI64::new(0));
        let ex = Exchanger::new(store, Arc::new(s), crate::oidc::outbound_client(), 300, 60)
            .with_clock(test_clock(now.clone()));
        ex.exchange("acme", "andrii", "zendesk", None)
            .await
            .unwrap();
        // 250s in: 50s of life left < 60s margin ⇒ proactive re-mint.
        now.store(250, Ordering::SeqCst);
        let m = ex
            .exchange("acme", "andrii", "zendesk", None)
            .await
            .unwrap();
        assert_eq!(m.outcome, Outcome::Refresh);
    }

    #[tokio::test]
    async fn unknown_connection_is_invalid_target() {
        let store = Arc::new(MemoryStore::default());
        let ex = Exchanger::new(
            store,
            Arc::new(sealer()),
            crate::oidc::outbound_client(),
            300,
            60,
        );
        let e = ex
            .exchange("acme", "andrii", "github", None)
            .await
            .unwrap_err();
        assert!(matches!(e, ExchangeError::NoConnection { .. }));
    }

    #[tokio::test]
    async fn invalidate_drops_all_scopes_for_the_tuple() {
        let store = Arc::new(MemoryStore::default());
        let s = sealer();
        store.put_connection(conn_static(&s)).await.unwrap();
        let ex = Exchanger::new(store, Arc::new(s), crate::oidc::outbound_client(), 300, 60);
        ex.exchange("acme", "andrii", "zendesk", Some("read"))
            .await
            .unwrap();
        ex.exchange("acme", "andrii", "zendesk", None)
            .await
            .unwrap();
        ex.invalidate("acme", "andrii", "zendesk");
        let m = ex
            .exchange("acme", "andrii", "zendesk", None)
            .await
            .unwrap();
        assert_eq!(m.outcome, Outcome::Mint, "cache was dropped, not refreshed");
    }

    #[tokio::test]
    async fn oauth_refresh_redeems_rotates_and_caches() {
        use axum::extract::Form;
        use axum::routing::post;
        use std::collections::HashMap as Map;

        // In-process provider: counts redemptions, rotates the refresh token,
        // refuses a re-used (pre-rotation) one — the strictest IdP behavior.
        #[derive(Default)]
        struct Provider {
            hits: AtomicU64,
            expect: Mutex<String>,
        }
        let provider = Arc::new(Provider {
            hits: AtomicU64::new(0),
            expect: Mutex::new("rt-0".to_string()),
        });
        let p = provider.clone();
        let app = axum::Router::new().route(
            "/token",
            post(move |Form(f): Form<Map<String, String>>| {
                let p = p.clone();
                async move {
                    let n = p.hits.fetch_add(1, Ordering::SeqCst);
                    let mut expect = p.expect.lock().unwrap();
                    if f.get("grant_type").map(String::as_str) != Some("refresh_token")
                        || f.get("refresh_token") != Some(&*expect)
                    {
                        return axum::Json(serde_json::json!({"error": "invalid_grant"}))
                            .into_response();
                    }
                    *expect = format!("rt-{}", n + 1);
                    axum::Json(serde_json::json!({
                        "access_token": format!("at-{n}"),
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "refresh_token": *expect,
                    }))
                    .into_response()
                }
            }),
        );
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let store = Arc::new(MemoryStore::default());
        let s = sealer();
        store
            .put_connection(ConnectionRecord {
                org: "acme".into(),
                user: "andrii".into(),
                provider: "idp".into(),
                kind: "oauth_refresh".into(),
                sealed_secret: s
                    .seal(&secret_aad("acme", "andrii", "idp"), b"rt-0")
                    .unwrap(),
                token_endpoint: Some(format!("http://{addr}/token")),
                client_id: Some("agentctl".into()),
                sealed_client_secret: None,
                scope: None,
                created_unix: 0,
                updated_unix: 0,
            })
            .await
            .unwrap();
        let now = Arc::new(AtomicI64::new(0));
        let ex = Exchanger::new(
            store.clone(),
            Arc::new(s),
            crate::oidc::outbound_client(),
            300,
            60,
        )
        .with_clock(test_clock(now.clone()));

        // Simulate a week at 2 exchanges per hour: tokens live 3600s, so each
        // hour's first call redeems (with the ROTATED refresh token — a replay
        // of an old one would get invalid_grant) and the second hits cache.
        for hour in 0..(7 * 24) {
            now.store(hour * 3600, Ordering::SeqCst);
            let m = ex.exchange("acme", "andrii", "idp", None).await.unwrap();
            assert_eq!(m.access_token, format!("at-{hour}"));
            assert_ne!(m.outcome, Outcome::Cache, "hour {hour}");
            let again = ex.exchange("acme", "andrii", "idp", None).await.unwrap();
            assert_eq!(again.outcome, Outcome::Cache);
        }
        assert_eq!(provider.hits.load(Ordering::SeqCst), 7 * 24);
        // Custody now holds the latest rotation, sealed.
        let held = store
            .find_connection("acme", "andrii", "idp")
            .await
            .unwrap();
        assert_eq!(
            ex.sealer
                .unseal(&secret_aad("acme", "andrii", "idp"), &held.sealed_secret)
                .unwrap(),
            format!("rt-{}", 7 * 24).into_bytes()
        );
    }

    #[tokio::test]
    async fn provider_refusal_maps_to_invalid_grant_not_unreachable() {
        use axum::response::IntoResponse;
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/token",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"error": "invalid_grant"})),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let store = Arc::new(MemoryStore::default());
        let s = sealer();
        store
            .put_connection(ConnectionRecord {
                org: "acme".into(),
                user: "andrii".into(),
                provider: "idp".into(),
                kind: "oauth_refresh".into(),
                sealed_secret: s
                    .seal(&secret_aad("acme", "andrii", "idp"), b"rt-revoked")
                    .unwrap(),
                token_endpoint: Some(format!("http://{addr}/token")),
                client_id: None,
                sealed_client_secret: None,
                scope: None,
                created_unix: 0,
                updated_unix: 0,
            })
            .await
            .unwrap();
        let ex = Exchanger::new(store, Arc::new(s), crate::oidc::outbound_client(), 300, 60);
        let e = ex
            .exchange("acme", "andrii", "idp", None)
            .await
            .unwrap_err();
        match e {
            ExchangeError::ProviderRefused(code) => assert_eq!(code, "invalid_grant"),
            other => panic!("wrong error: {other}"),
        }
    }
}
