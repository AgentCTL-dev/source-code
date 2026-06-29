// SPDX-License-Identifier: BUSL-1.1
//! agentctl admission plane (RFC 0007) — the admission webhooks.
//!
//! The CRDs carry declarative CEL invariants enforced by the apiserver. This
//! server adds the two admission concerns CEL can't own:
//!
//! 1. **Validating** (`POST /validate`): what CEL **can't** express — cross-object
//!    existence (does the named `ModelPool` exist in the namespace?), cluster
//!    policy (the image registry allow-list), and the **lethal-trifecta override
//!    gate** (exec + egress + secrets together require an explicit opt-in
//!    annotation). These checks cover **both** `Agent` (at `spec.*`) and
//!    `AgentFleet` (at `spec.template.*`, an `AgentSpec`) so a fleet cannot smuggle
//!    a disallowed image or an ungated trifecta past the gate (the bypass this
//!    server closes).
//! 2. **Defaulting** (`POST /mutate`): a mutating webhook that returns a base64
//!    JSONPatch of **secure defaults** — the standard `app.kubernetes.io/*` labels,
//!    a conservative `mode`, and a minimal-exposure `surfaces` set. It deliberately
//!    does **not** hard-default `substrate`: the secure default is tenancy-derived
//!    (RFC 0002 §5 — `kata-hybrid` for hostile, `stock-unix` for single) and the
//!    most-isolated tier needs a Kata `RuntimeClass` absent on most stock clusters
//!    (RFC 0002 §9), so forcing one cluster-wide would either break stock clusters
//!    or be insecure. Leaving `substrate` absent lets the operator/renderer resolve
//!    it from the `AgentClass`/tenancy (RFC 0007 B3) — the documented secure path.
//!
//! `ValidatingWebhookConfiguration` / `MutatingWebhookConfiguration` point the
//! kube-apiserver at `POST /validate` and `POST /mutate` over HTTPS (mutating runs
//! first — k8s sequences mutating admission before validating). Hand-rolled in Rust
//! (axum + rustls/ring; agentctl is Rust-only). The serving cert is mounted at
//! `/etc/agentctl-admission/tls`.

use std::io::BufReader;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::header;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use kube::{Api, Client};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use serde_json::{json, Map, Value};

use agent_api::ModelPool;

mod metrics;

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
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) + OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set; otherwise byte-identical to before.
    agentctl_telemetry::init("agentctl-admission");
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let client = Client::try_default().await.expect("in-cluster kube client");
    let allowed_registries = parse_registries(std::env::var(ALLOWED_REGISTRIES_ENV).ok());

    let tls = build_tls_config().expect("build TLS server config");

    let app = Router::new()
        .route("/healthz", get(healthz))
        // `/metrics` rides the EXISTING :8443 HTTPS server. Admission's TLS uses
        // `with_no_client_auth`, so Prometheus can scrape it (scheme https,
        // insecureSkipVerify) without a client cert — no new plaintext port.
        .route("/metrics", get(serve_metrics))
        .route("/validate", post(validate))
        .route("/mutate", post(mutate))
        .with_state(AppState {
            client,
            allowed_registries: allowed_registries.clone(),
            metrics: Arc::new(metrics::Metrics::new()),
        });

    let addr: SocketAddr = "0.0.0.0:8443".parse().unwrap();
    let config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));
    // Graceful shutdown: on SIGTERM/SIGINT, stop accepting and drain in-flight
    // requests (axum-server's `Handle::graceful_shutdown`).
    let handle = axum_server::Handle::new();
    tokio::spawn(shutdown_signal(handle.clone()));
    tracing::info!(
        %addr,
        registries = ?allowed_registries,
        "agentctl admission webhook serving (validate: registry + trifecta + modelPool over Agent/AgentFleet; mutate: secure defaults)"
    );
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

/// `GET /metrics` — the Prometheus exposition (node-agent text format).
async fn serve_metrics(
    State(state): State<AppState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

/// The validating endpoint. Parses an `admission.k8s.io/v1` `AdmissionReview`
/// whose `request.object` is an `Agent` **or** `AgentFleet`, runs the policy +
/// cross-object checks against the `AgentSpec`-shaped view (`spec` for `Agent`,
/// `spec.template` for `AgentFleet`), and returns an `AdmissionReview` verdict
/// (`allowed` + a denial message).
#[tracing::instrument(name = "admission.validate", skip_all)]
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
    let kind = reviewed_kind(request, object);
    // The same image/exec/egress/secrets/modelPool checks apply to an Agent's spec
    // and to an AgentFleet's `spec.template` (itself an AgentSpec) — the latter is
    // the bypass this closes.
    let view = agent_spec_view(&kind, spec, &empty);

    let empty_map = Map::new();
    let annotations = object["metadata"]["annotations"]
        .as_object()
        .unwrap_or(&empty_map);

    // Cross-object: resolve whether the named ModelPool exists (if one is named).
    let model_pool_exists = resolve_model_pool(&state.client, view, &namespace).await;

    let verdict = evaluate(
        view,
        annotations,
        &state.allowed_registries,
        model_pool_exists,
        &namespace,
    );

    match &verdict {
        Ok(()) => tracing::info!(%uid, %namespace, %kind, "admit"),
        Err(msg) => tracing::warn!(%uid, %namespace, %kind, deny = %msg, "deny"),
    }
    state.metrics.record(verdict.is_ok());

    Json(admission_response(&uid, verdict))
}

/// The reviewed object's kind, preferring `request.object.kind`, falling back to
/// the request GVK (`request.kind.kind`), defaulting to `"Agent"`.
fn reviewed_kind(request: &Value, object: &Value) -> String {
    object
        .get("kind")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| request["kind"]["kind"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Agent")
        .to_string()
}

/// Select the `AgentSpec`-shaped sub-object to check from a reviewed object's
/// `spec`, given the object `kind`. For `AgentFleet` the `AgentSpec` lives at
/// `spec.template`; for `Agent` (and anything else) it is `spec` itself. An
/// `AgentFleet` missing its (required) `template` falls back to `empty` so the
/// checks simply find nothing to deny rather than panicking.
fn agent_spec_view<'a>(kind: &str, spec: &'a Value, empty: &'a Value) -> &'a Value {
    if kind == "AgentFleet" {
        spec.get("template").unwrap_or(empty)
    } else {
        spec
    }
}

/// The mutating endpoint. Parses an `admission.k8s.io/v1` `AdmissionReview` for an
/// `Agent`/`AgentFleet` and returns an `AdmissionReview` carrying a base64
/// JSONPatch of secure defaults (labels + `mode` + `surfaces`); see
/// [`build_default_patch`] for the field-by-field rationale, and the module docs
/// for why `substrate` is deliberately **not** defaulted here.
async fn mutate(State(state): State<AppState>, Json(review): Json<Value>) -> Json<Value> {
    let request = &review["request"];
    let uid = request["uid"].as_str().unwrap_or_default().to_string();
    let object = &request["object"];
    let kind = reviewed_kind(request, object);

    let patch = build_default_patch(&kind, object);

    tracing::info!(%uid, %kind, ops = patch.len(), "mutate");
    state.metrics.record_mutation(!patch.is_empty());

    Json(mutation_response(&uid, &patch))
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

// --- defaulting (pure) -----------------------------------------------------

/// The `app.kubernetes.io/name` value for a reviewed kind.
fn kind_app_name(kind: &str) -> &'static str {
    match kind {
        "AgentFleet" => "agentfleet",
        _ => "agent",
    }
}

/// Escape a string for use as a single JSON Pointer (RFC 6901) reference token:
/// `~` ⇒ `~0`, `/` ⇒ `~1` (order matters — `~` first).
fn escape_pointer_token(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// Build the RFC 6902 JSONPatch of **secure defaults** for an `Agent`/`AgentFleet`.
/// Every op is conditional on the field being **absent** — defaulting never
/// clobbers an author's explicit value (and is auditable in the patch). Defaults:
///
/// 1. **Standard `app.kubernetes.io/*` labels** (`managed-by`/`part-of`/`name`) —
///    pure metadata, always safe. Adds the whole `metadata.labels` object if none
///    exists, else only the missing keys.
/// 2. **`mode`** ⇒ `"once"` — the conservative run-once shape and the documented
///    enum default; only added when absent (it is a required field, so defaulting
///    it pre-empts a structural rejection with the safest run shape).
/// 3. **`surfaces`** ⇒ all-`false` — minimal control-plane exposure; an author opts
///    a surface on explicitly. `a2a` in particular is a network/contract-unsupported
///    (RFC 0007 B6 / P2) surface that must never default on.
///
/// Deliberately **not** defaulted: `substrate`. Its secure default is
/// tenancy-derived (RFC 0002 §5) and the most-isolated tier (`kata-hybrid`) needs a
/// Kata `RuntimeClass` absent on most stock clusters (RFC 0002 §9); hard-defaulting
/// here would either break stock clusters or be insecure, so the field is left
/// absent for the operator/renderer to resolve from `AgentClass`/tenancy
/// (RFC 0007 B3).
///
/// The `AgentSpec`-shaped defaults target `spec.*` for an `Agent` and
/// `spec.template.*` for an `AgentFleet`, and are emitted only when the parent
/// (`spec` / `spec.template`) is present — an "add" into a missing parent would
/// fail to apply.
fn build_default_patch(kind: &str, object: &Value) -> Vec<Value> {
    let mut ops = Vec::new();

    // 1. Standard recommended labels (only the absent keys).
    let desired_labels = [
        ("app.kubernetes.io/managed-by", "agentctl"),
        ("app.kubernetes.io/part-of", "agentctl"),
        ("app.kubernetes.io/name", kind_app_name(kind)),
    ];
    match object["metadata"]["labels"].as_object() {
        None => {
            // No labels map at all — add the whole object in one op.
            let mut m = Map::new();
            for (k, v) in desired_labels {
                m.insert(k.to_string(), json!(v));
            }
            ops.push(json!({ "op": "add", "path": "/metadata/labels", "value": m }));
        }
        Some(existing) => {
            for (k, v) in desired_labels {
                if !existing.contains_key(k) {
                    ops.push(json!({
                        "op": "add",
                        "path": format!("/metadata/labels/{}", escape_pointer_token(k)),
                        "value": v,
                    }));
                }
            }
        }
    }

    // 2/3. Safe AgentSpec-shaped defaults: /spec/* (Agent) or /spec/template/*
    // (AgentFleet). Only when the parent container is present.
    let (view, base) = if kind == "AgentFleet" {
        (
            object.get("spec").and_then(|s| s.get("template")),
            "/spec/template",
        )
    } else {
        (object.get("spec"), "/spec")
    };
    if let Some(view) = view {
        if view.get("mode").is_none() {
            ops.push(json!({ "op": "add", "path": format!("{base}/mode"), "value": "once" }));
        }
        if view.get("surfaces").is_none() {
            ops.push(json!({
                "op": "add",
                "path": format!("{base}/surfaces"),
                "value": { "management": false, "metrics": false, "a2a": false },
            }));
        }
    }

    ops
}

/// Build the mutating `AdmissionReview` response. An empty patch yields a bare
/// `allowed: true` (no `patch`/`patchType`); a non-empty patch is serialized,
/// base64-encoded, and tagged `patchType: JSONPatch`.
fn mutation_response(uid: &str, patch: &[Value]) -> Value {
    let mut response = json!({ "uid": uid, "allowed": true });
    if !patch.is_empty() {
        let bytes = serde_json::to_vec(patch).unwrap_or_default();
        response["patchType"] = json!("JSONPatch");
        response["patch"] = json!(BASE64.encode(bytes));
    }
    json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "response": response,
    })
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

    // --- AgentFleet coverage (the closed bypass) ---------------------------

    #[test]
    fn agent_view_is_the_spec_itself() {
        let spec = json!({ "image": "x", "exec": true });
        let empty = Value::Object(Map::new());
        assert_eq!(agent_spec_view("Agent", &spec, &empty), &spec);
        // Anything that is not an AgentFleet is treated like an Agent.
        assert_eq!(agent_spec_view("Whatever", &spec, &empty), &spec);
    }

    #[test]
    fn agentfleet_view_is_spec_template() {
        let template = json!({ "image": "x", "exec": true });
        let spec = json!({ "template": template.clone(), "scaling": { "mode": "claim" } });
        let empty = Value::Object(Map::new());
        assert_eq!(agent_spec_view("AgentFleet", &spec, &empty), &template);
    }

    #[test]
    fn agentfleet_missing_template_falls_back_to_empty() {
        let spec = json!({ "scaling": { "mode": "claim" } });
        let empty = Value::Object(Map::new());
        assert_eq!(agent_spec_view("AgentFleet", &spec, &empty), &empty);
    }

    #[test]
    fn agentfleet_trifecta_denied_via_template() {
        // The same lethal trifecta in a fleet's template must be gated — this is
        // the bypass the fix closes (the webhook used to only see `agents`).
        let spec = json!({
            "template": { "mode": "loop", "exec": true, "egress": true, "secrets": ["db"] },
            "scaling": { "mode": "claim" }
        });
        let empty = Value::Object(Map::new());
        let view = agent_spec_view("AgentFleet", &spec, &empty);
        let err = evaluate(view, &Map::new(), &[], None, "default").unwrap_err();
        assert!(err.contains("lethal trifecta"));
    }

    #[test]
    fn agentfleet_trifecta_allowed_with_annotation() {
        let spec = json!({
            "template": { "exec": true, "egress": true, "secrets": ["db"] },
            "scaling": { "mode": "claim" }
        });
        let empty = Value::Object(Map::new());
        let view = agent_spec_view("AgentFleet", &spec, &empty);
        // The override annotation rides on the AgentFleet object's metadata.
        let anns = annotations(json!({ "agentctl.dev/allow-trifecta": "true" }));
        assert!(evaluate(view, &anns, &[], None, "default").is_ok());
    }

    #[test]
    fn agentfleet_registry_denied_via_template() {
        let spec = json!({
            "template": { "image": "docker.io/library/evil:latest" },
            "scaling": { "mode": "claim" }
        });
        let empty = Value::Object(Map::new());
        let view = agent_spec_view("AgentFleet", &spec, &empty);
        let registries = vec!["ghcr.io/acme/".to_string()];
        let err = evaluate(view, &Map::new(), &registries, None, "default").unwrap_err();
        assert!(err.contains("not from an allowed registry"));
    }

    #[test]
    fn reviewed_kind_prefers_object_then_request_then_default() {
        let req = json!({ "kind": { "kind": "AgentFleet" } });
        let obj = json!({ "kind": "Agent" });
        assert_eq!(reviewed_kind(&req, &obj), "Agent");
        // Object kind missing ⇒ fall back to the request GVK.
        let obj_no_kind = json!({ "metadata": {} });
        assert_eq!(reviewed_kind(&req, &obj_no_kind), "AgentFleet");
        // Both missing ⇒ default to Agent.
        assert_eq!(reviewed_kind(&json!({}), &json!({})), "Agent");
    }

    // --- defaulting / mutate ----------------------------------------------

    #[test]
    fn mutate_defaults_agent_mode_surfaces_and_labels() {
        let object = json!({
            "kind": "Agent",
            "metadata": { "name": "demo" },
            "spec": { "image": "ghcr.io/acme/a:v1" }
        });
        let ops = build_default_patch("Agent", &object);
        // No labels map existed ⇒ one op adds the whole labels object.
        let labels_op = ops
            .iter()
            .find(|o| o["path"] == "/metadata/labels")
            .expect("labels object op");
        assert_eq!(
            labels_op["value"]["app.kubernetes.io/managed-by"],
            "agentctl"
        );
        assert_eq!(labels_op["value"]["app.kubernetes.io/name"], "agent");
        // mode + surfaces defaulted on /spec.
        assert!(ops
            .iter()
            .any(|o| o["path"] == "/spec/mode" && o["value"] == "once"));
        let surfaces = ops
            .iter()
            .find(|o| o["path"] == "/spec/surfaces")
            .expect("surfaces op");
        assert_eq!(surfaces["value"]["management"], false);
        assert_eq!(surfaces["value"]["metrics"], false);
        assert_eq!(surfaces["value"]["a2a"], false);
    }

    #[test]
    fn mutate_targets_template_for_agentfleet() {
        let object = json!({
            "kind": "AgentFleet",
            "metadata": { "name": "f", "labels": { "team": "acme" } },
            "spec": { "template": { "image": "ghcr.io/acme/a:v1" }, "scaling": { "mode": "claim" } }
        });
        let ops = build_default_patch("AgentFleet", &object);
        // AgentSpec defaults land on /spec/template/*.
        assert!(ops
            .iter()
            .any(|o| o["path"] == "/spec/template/mode" && o["value"] == "once"));
        assert!(ops.iter().any(|o| o["path"] == "/spec/template/surfaces"));
        // labels already present ⇒ per-key adds (escaped), never a whole-object add.
        assert!(!ops.iter().any(|o| o["path"] == "/metadata/labels"));
        assert!(ops
            .iter()
            .any(|o| o["path"] == "/metadata/labels/app.kubernetes.io~1managed-by"));
        // app.kubernetes.io/name resolves to the fleet kind.
        let name_op = ops
            .iter()
            .find(|o| o["path"] == "/metadata/labels/app.kubernetes.io~1name")
            .expect("name label op");
        assert_eq!(name_op["value"], "agentfleet");
    }

    #[test]
    fn mutate_does_not_clobber_present_fields() {
        let object = json!({
            "kind": "Agent",
            "metadata": { "name": "demo", "labels": { "app.kubernetes.io/managed-by": "me" } },
            "spec": { "mode": "loop", "surfaces": { "management": true } }
        });
        let ops = build_default_patch("Agent", &object);
        // mode + surfaces already set ⇒ no ops for them.
        assert!(!ops.iter().any(|o| o["path"] == "/spec/mode"));
        assert!(!ops.iter().any(|o| o["path"] == "/spec/surfaces"));
        // managed-by already set ⇒ not re-added; part-of + name still added.
        assert!(!ops
            .iter()
            .any(|o| o["path"] == "/metadata/labels/app.kubernetes.io~1managed-by"));
        assert!(ops
            .iter()
            .any(|o| o["path"] == "/metadata/labels/app.kubernetes.io~1part-of"));
    }

    #[test]
    fn mutate_never_defaults_substrate() {
        let object = json!({
            "kind": "Agent",
            "metadata": { "name": "demo" },
            "spec": { "image": "ghcr.io/acme/a:v1" }
        });
        let ops = build_default_patch("Agent", &object);
        assert!(
            !ops.iter()
                .any(|o| o["path"].as_str().is_some_and(|p| p.contains("substrate"))),
            "substrate must be left to the operator/renderer (RFC 0002 §5 / RFC 0007 B3)"
        );
    }

    #[test]
    fn mutate_skips_spec_defaults_when_spec_absent() {
        // No spec ⇒ no /spec/* ops (an "add" into a missing parent would fail).
        let object = json!({ "kind": "Agent", "metadata": { "name": "x" } });
        let ops = build_default_patch("Agent", &object);
        assert!(!ops
            .iter()
            .any(|o| o["path"].as_str().is_some_and(|p| p.starts_with("/spec"))));
        // Labels are still defaulted (metadata always patchable).
        assert!(ops.iter().any(|o| o["path"] == "/metadata/labels"));
    }

    #[test]
    fn mutation_response_encodes_base64_jsonpatch() {
        let ops = build_default_patch(
            "Agent",
            &json!({ "kind": "Agent", "metadata": {}, "spec": {} }),
        );
        let resp = mutation_response("uid-1", &ops);
        assert_eq!(resp["response"]["uid"], "uid-1");
        assert_eq!(resp["response"]["allowed"], true);
        assert_eq!(resp["response"]["patchType"], "JSONPatch");
        let encoded = resp["response"]["patch"].as_str().unwrap();
        let decoded = BASE64.decode(encoded).unwrap();
        let back: Value = serde_json::from_slice(&decoded).unwrap();
        assert!(back
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["path"] == "/spec/mode"));
    }

    #[test]
    fn mutation_response_empty_patch_omits_patch_fields() {
        let resp = mutation_response("uid-2", &[]);
        assert_eq!(resp["response"]["allowed"], true);
        assert_eq!(resp["apiVersion"], "admission.k8s.io/v1");
        assert!(resp["response"].get("patch").is_none());
        assert!(resp["response"].get("patchType").is_none());
    }

    #[test]
    fn escape_pointer_token_escapes_slash_and_tilde() {
        assert_eq!(
            escape_pointer_token("app.kubernetes.io/name"),
            "app.kubernetes.io~1name"
        );
        assert_eq!(escape_pointer_token("a~b/c"), "a~0b~1c");
    }
}
