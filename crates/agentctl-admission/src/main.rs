// SPDX-License-Identifier: BUSL-1.1
//! agentctl admission plane (RFC 0007) — the validating webhook.
//!
//! The CRDs carry declarative CEL invariants enforced by the apiserver. This
//! webhook covers what CEL **can't** express: cross-object existence
//! (does the named `ModelPool` exist in the namespace?), cluster policy
//! (the image registry allow-list), and the **lethal-trifecta override gate**
//! (exec + egress + secrets together require an explicit opt-in annotation).
//!
//! A `ValidatingWebhookConfiguration` points the kube-apiserver at
//! `POST /validate` over HTTPS; the webhook returns an `AdmissionReview`
//! verdict. Hand-rolled in Rust (axum + rustls/ring; agentctl is Rust-only).
//! The serving cert is mounted at `/etc/agentctl-admission/tls`.

use std::io::BufReader;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use kube::{Api, Client};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use serde_json::{json, Map, Value};
use tracing_subscriber::EnvFilter;

use agent_api::ModelPool;

/// Where the serving cert + key are mounted (a TLS `Secret` volume).
const TLS_DIR: &str = "/etc/agentctl-admission/tls";

/// The override annotation that opts an `Agent` into the lethal trifecta.
const TRIFECTA_ANNOTATION: &str = "agentctl.dev/allow-trifecta";

/// CSV of allowed image-registry prefixes; empty/unset ⇒ allow any registry.
const ALLOWED_REGISTRIES_ENV: &str = "ALLOWED_REGISTRIES";

#[derive(Clone)]
struct AppState {
    /// kube client for cross-object lookups (does the `ModelPool` exist?).
    client: Client,
    /// Allowed image-registry prefixes (empty ⇒ allow all).
    allowed_registries: Vec<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let client = Client::try_default().await.expect("in-cluster kube client");
    let allowed_registries = parse_registries(std::env::var(ALLOWED_REGISTRIES_ENV).ok());

    let tls = build_tls_config().expect("build TLS server config");

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/validate", post(validate))
        .with_state(AppState {
            client,
            allowed_registries: allowed_registries.clone(),
        });

    let addr: SocketAddr = "0.0.0.0:8443".parse().unwrap();
    let config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));
    tracing::info!(
        %addr,
        registries = ?allowed_registries,
        "agentctl admission webhook serving (validating: registry + trifecta + modelPool)"
    );
    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service())
        .await
        .expect("serve");
}

// --- TLS -------------------------------------------------------------------

/// rustls server config presenting the mounted serving cert. The kube-apiserver
/// is the only client (over the cluster network); no client-cert is required.
fn build_tls_config() -> Result<ServerConfig, String> {
    let certs = load_certs(&PathBuf::from(TLS_DIR).join("tls.crt"))?;
    let key = load_key(&PathBuf::from(TLS_DIR).join("tls.key"))?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("server config: {e}"))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read certs: {e}"))
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    rustls_pemfile::private_key(&mut r)
        .map_err(|e| format!("read key: {e}"))?
        .ok_or_else(|| "no private key in tls.key".into())
}

// --- handlers --------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

/// The admission endpoint. Parses an `admission.k8s.io/v1` `AdmissionReview`
/// whose `request.object` is an `Agent`, runs the policy + cross-object checks,
/// and returns an `AdmissionReview` verdict (`allowed` + a denial message).
async fn validate(State(state): State<AppState>, Json(review): Json<Value>) -> Json<Value> {
    let request = &review["request"];
    let uid = request["uid"].as_str().unwrap_or_default().to_string();
    // The namespace of the object under review; fall back to the object's own
    // metadata, then to "default".
    let namespace = request["namespace"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| request["object"]["metadata"]["namespace"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();

    let object = &request["object"];
    let empty = Value::Object(Map::new());
    let spec = object.get("spec").unwrap_or(&empty);

    let empty_map = Map::new();
    let annotations = object["metadata"]["annotations"]
        .as_object()
        .unwrap_or(&empty_map);

    // Cross-object: resolve whether the named ModelPool exists (if one is named).
    let model_pool_exists = resolve_model_pool(&state.client, spec, &namespace).await;

    let verdict = evaluate(
        spec,
        annotations,
        &state.allowed_registries,
        model_pool_exists,
        &namespace,
    );

    match &verdict {
        Ok(()) => tracing::info!(%uid, %namespace, "admit"),
        Err(msg) => tracing::warn!(%uid, %namespace, deny = %msg, "deny"),
    }

    Json(admission_response(&uid, verdict))
}

/// If `spec.modelPool` names a pool, look it up in `namespace`: `Some(true)` if
/// it exists, `Some(false)` if not. `None` when no pool is named — and also when
/// the lookup itself errors (fail-open: a transient apiserver hiccup must not
/// block otherwise-valid admissions; the existence check is simply skipped).
async fn resolve_model_pool(client: &Client, spec: &Value, namespace: &str) -> Option<bool> {
    let name = spec.get("modelPool").and_then(Value::as_str)?;
    let api: Api<ModelPool> = Api::namespaced(client.clone(), namespace);
    match api.get_opt(name).await {
        Ok(found) => Some(found.is_some()),
        Err(e) => {
            tracing::error!(modelPool = name, %namespace, error = %e, "ModelPool lookup failed; skipping cross-object check");
            None
        }
    }
}

/// Build the `AdmissionReview` response carrying the verdict. A denial puts the
/// reason in `status.message` (surfaced to the user by the apiserver).
fn admission_response(uid: &str, verdict: Result<(), String>) -> Value {
    let (allowed, code, message) = match verdict {
        Ok(()) => (true, 200u16, String::new()),
        Err(msg) => (false, 403u16, msg),
    };
    json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "response": {
            "uid": uid,
            "allowed": allowed,
            "status": { "code": code, "message": message }
        }
    })
}

// --- decision logic (pure) -------------------------------------------------

/// Parse the `ALLOWED_REGISTRIES` CSV: trim each entry, drop empties. An absent
/// or all-blank value yields an empty list, which means "allow any registry".
fn parse_registries(csv: Option<String>) -> Vec<String> {
    csv.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// The pure verdict function — no cluster, no I/O. All cross-object state is
/// pre-resolved into `model_pool_exists` by the caller.
///
/// Denies when:
/// 1. `allowed_registries` is non-empty, `spec.image` is set, and the image is
///    not prefixed by an allowed registry.
/// 2. The lethal trifecta (`exec` && `egress` && a non-empty `secrets`) is
///    requested without the `agentctl.dev/allow-trifecta: "true"` annotation.
/// 3. `spec.modelPool` is named but `model_pool_exists == Some(false)`.
fn evaluate(
    spec: &Value,
    annotations: &Map<String, Value>,
    allowed_registries: &[String],
    model_pool_exists: Option<bool>,
    namespace: &str,
) -> Result<(), String> {
    // 1. Image registry allow-list.
    if !allowed_registries.is_empty() {
        if let Some(image) = spec.get("image").and_then(Value::as_str) {
            let ok = allowed_registries.iter().any(|p| image.starts_with(p));
            if !ok {
                return Err(format!(
                    "image '{image}' is not from an allowed registry ({})",
                    allowed_registries.join(", ")
                ));
            }
        }
    }

    // 2. Lethal-trifecta override gate.
    let exec = spec.get("exec").and_then(Value::as_bool) == Some(true);
    let egress = spec.get("egress").and_then(Value::as_bool) == Some(true);
    let secrets = spec
        .get("secrets")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if exec && egress && secrets {
        let allowed = annotations.get(TRIFECTA_ANNOTATION).and_then(Value::as_str) == Some("true");
        if !allowed {
            return Err(format!(
                "agent enables the lethal trifecta (exec + egress + secrets); \
                 set annotation {TRIFECTA_ANNOTATION}=\"true\" to allow"
            ));
        }
    }

    // 3. Cross-object: the named ModelPool must exist.
    if let Some(name) = spec.get("modelPool").and_then(Value::as_str) {
        if model_pool_exists == Some(false) {
            return Err(format!(
                "modelPool '{name}' not found in namespace {namespace}"
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn annotations(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn clean_agent_is_allowed() {
        let spec = json!({ "mode": "once", "image": "ghcr.io/acme/agent:v1" });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default").is_ok());
    }

    #[test]
    fn trifecta_without_annotation_is_denied() {
        let spec = json!({
            "mode": "loop",
            "exec": true,
            "egress": true,
            "secrets": ["db-password"]
        });
        let err = evaluate(&spec, &Map::new(), &[], None, "default").unwrap_err();
        assert!(err.contains("lethal trifecta"));
        assert!(!err.is_empty());
    }

    #[test]
    fn trifecta_with_annotation_is_allowed() {
        let spec = json!({
            "mode": "loop",
            "exec": true,
            "egress": true,
            "secrets": ["db-password"]
        });
        let anns = annotations(json!({ "agentctl.dev/allow-trifecta": "true" }));
        assert!(evaluate(&spec, &anns, &[], None, "default").is_ok());
    }

    #[test]
    fn trifecta_annotation_must_be_literal_true() {
        let spec = json!({
            "exec": true,
            "egress": true,
            "secrets": ["s"]
        });
        // Any value other than "true" does not open the gate.
        let anns = annotations(json!({ "agentctl.dev/allow-trifecta": "yes" }));
        assert!(evaluate(&spec, &anns, &[], None, "default").is_err());
    }

    #[test]
    fn two_of_three_trifecta_legs_is_allowed() {
        // exec + egress but no secrets ⇒ not the full trifecta ⇒ no gate.
        let spec = json!({ "exec": true, "egress": true });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default").is_ok());
        // exec + egress with an empty secrets array is still only two legs.
        let spec = json!({ "exec": true, "egress": true, "secrets": [] });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default").is_ok());
    }

    #[test]
    fn disallowed_registry_is_denied() {
        let spec = json!({ "image": "docker.io/library/evil:latest" });
        let registries = vec!["ghcr.io/acme/".to_string()];
        let err = evaluate(&spec, &Map::new(), &registries, None, "default").unwrap_err();
        assert!(err.contains("not from an allowed registry"));
        assert!(err.contains("ghcr.io/acme/"));
        assert!(!err.is_empty());
    }

    #[test]
    fn allowed_registry_is_allowed() {
        let spec = json!({ "image": "ghcr.io/acme/agent@sha256:abc" });
        let registries = vec!["ghcr.io/acme/".to_string()];
        assert!(evaluate(&spec, &Map::new(), &registries, None, "default").is_ok());
    }

    #[test]
    fn empty_registry_list_allows_any_image() {
        let spec = json!({ "image": "quay.io/whatever:1" });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default").is_ok());
    }

    #[test]
    fn missing_model_pool_is_denied() {
        let spec = json!({ "modelPool": "shared" });
        let err = evaluate(&spec, &Map::new(), &[], Some(false), "team-a").unwrap_err();
        assert!(err.contains("shared"));
        assert!(err.contains("team-a"));
        assert!(!err.is_empty());
    }

    #[test]
    fn present_model_pool_is_allowed() {
        let spec = json!({ "modelPool": "shared" });
        assert!(evaluate(&spec, &Map::new(), &[], Some(true), "team-a").is_ok());
    }

    #[test]
    fn no_model_pool_named_skips_cross_object_check() {
        let spec = json!({ "mode": "once" });
        // Even if the resolver reported a negative, no pool is named ⇒ no deny.
        assert!(evaluate(&spec, &Map::new(), &[], Some(false), "default").is_ok());
    }

    #[test]
    fn deny_message_is_non_empty() {
        let spec = json!({ "exec": true, "egress": true, "secrets": ["x"] });
        let err = evaluate(&spec, &Map::new(), &[], None, "default").unwrap_err();
        assert!(!err.trim().is_empty());
    }

    #[test]
    fn parse_registries_trims_and_drops_empties() {
        let got = parse_registries(Some(" ghcr.io/acme/ , ,docker.io/lib/ ".to_string()));
        assert_eq!(got, vec!["ghcr.io/acme/", "docker.io/lib/"]);
        assert!(parse_registries(None).is_empty());
        assert!(parse_registries(Some("   ".to_string())).is_empty());
    }

    #[test]
    fn admission_response_carries_uid_and_denial() {
        let resp = admission_response("uid-123", Err("nope".to_string()));
        assert_eq!(resp["response"]["uid"], "uid-123");
        assert_eq!(resp["response"]["allowed"], false);
        assert_eq!(resp["response"]["status"]["message"], "nope");
        assert_eq!(resp["apiVersion"], "admission.k8s.io/v1");

        let ok = admission_response("uid-9", Ok(()));
        assert_eq!(ok["response"]["allowed"], true);
    }
}
