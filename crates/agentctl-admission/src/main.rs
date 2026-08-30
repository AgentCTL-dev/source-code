// SPDX-License-Identifier: BUSL-1.1
//! agentctl admission plane — the admission webhooks.
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
//!    a disallowed image or an ungated trifecta past the gate. It also validates the
//!    per-agent **`spec.access.oidc`** A2A caller-identity policy (issuer is a
//!    non-empty `https://` URL, `audiences` is non-empty, any `jwksUri` override is
//!    `https://`) so a malformed OIDC gate is rejected at admission rather than
//!    failing opaquely at the gateway. And it gates the **`identity.aauth`**
//!    opt-in (RFC 0023): denied on fleet templates (per-pod identities are
//!    phase-gated on assertion enrollment), denied without a resolvable
//!    provider, and spec-level provider/personServer URLs must be `https://`.
//! 2. **Defaulting** (`POST /mutate`): a mutating webhook that returns a base64
//!    JSONPatch of **secure defaults** — the standard `app.kubernetes.io/*` labels,
//!    a conservative `mode`, and a minimal-exposure `surfaces` set. It deliberately
//!    does **not** hard-default `substrate`: absent ⇒ `stock-unix`, the only tier
//!    the operator renders today (`kata-hybrid` / `sidecar-emptydir` are roadmap
//!    tiers, rejected at render until implemented). Leaving it absent keeps the
//!    resolved tier the operator/renderer's decision rather than baking one in.
//!
//! `ValidatingWebhookConfiguration` / `MutatingWebhookConfiguration` point the
//! kube-apiserver at `POST /validate` and `POST /mutate` over HTTPS (mutating runs
//! first — k8s sequences mutating admission before validating). Hand-rolled in Rust
//! (axum + rustls/ring). The serving cert is mounted at
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

mod convert;
mod metrics;

/// Where the serving cert + key are mounted (a TLS `Secret` volume).
const TLS_DIR: &str = "/etc/agentctl-admission/tls";

/// The override annotation that opts an `Agent` into the lethal trifecta.
const TRIFECTA_ANNOTATION: &str = "agentctl.dev/allow-trifecta";

/// CSV of allowed image-registry prefixes; empty/unset ⇒ allow any registry.
const ALLOWED_REGISTRIES_ENV: &str = "ALLOWED_REGISTRIES";

/// The operator's default Agent Provider (RFC 0023). Presence is all admission
/// needs: an `identity.aauth` opt-in without a spec-level `provider` is only
/// admissible when this default exists (the chart passes the operator's value
/// here so the two agree).
const AAUTH_PROVIDER_ENV: &str = "AGENTCTL_AAUTH_PROVIDER";

/// Path to a pinned agentd binary for the ladder's most authoritative rung:
/// composing the SAME config document the operator will mount (the shared
/// `agent-config` builder) and running `agentd --validate-config` over it.
/// Unset ⇒ the rung is skipped (the chart wires it via an initContainer that
/// copies the binary out of the agent image).
const AGENTD_BIN_ENV: &str = "AGENTCTL_AGENTD_BIN";

#[derive(Clone)]
struct AppState {
    /// kube client for cross-object lookups (does the `ModelPool` exist?).
    client: Client,
    /// Allowed image-registry prefixes (empty ⇒ allow all).
    allowed_registries: Vec<String>,
    /// Whether the operator has a default AAuth provider configured
    /// (`AGENTCTL_AAUTH_PROVIDER`) — gates provider-less `identity.aauth`.
    aauth_default_provider: bool,
    /// The pinned agentd binary for ground-truth config validation (rung 4);
    /// `None` ⇒ rung skipped.
    agentd_bin: Option<std::path::PathBuf>,
    /// Prometheus counters surfaced at `/metrics`.
    metrics: Arc<metrics::Metrics>,
}

#[tokio::main]
async fn main() {
    // fmt layer (honoring RUST_LOG, default info) plus OTLP export when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set.
    agentctl_telemetry::init("agentctl-admission");
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let client = Client::try_default().await.expect("in-cluster kube client");
    let allowed_registries = parse_registries(std::env::var(ALLOWED_REGISTRIES_ENV).ok());
    let aauth_default_provider = std::env::var(AAUTH_PROVIDER_ENV)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let agentd_bin = std::env::var(AGENTD_BIN_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|p| {
            let ok = p.is_file();
            if !ok {
                tracing::warn!(path = %p.display(), "AGENTCTL_AGENTD_BIN set but not a file; binary-validation rung disabled");
            }
            ok
        });

    let tls = build_tls_config().expect("build TLS server config");

    let app = Router::new()
        .route("/healthz", get(healthz))
        // `/metrics` rides the same :8443 HTTPS server. Admission's TLS uses
        // `with_no_client_auth`, so Prometheus can scrape it (scheme https,
        // insecureSkipVerify) without a client cert — no new plaintext port.
        .route("/metrics", get(serve_metrics))
        .route("/validate", post(validate))
        .route("/mutate", post(mutate))
        // CRD conversion (P2-1b): the multi-version Agent/AgentFleet CRDs
        // point their spec.conversion webhook here.
        .route("/convert", post(convert_handler))
        .with_state(AppState {
            client,
            allowed_registries: allowed_registries.clone(),
            aauth_default_provider,
            agentd_bin,
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
        "agentctl admission webhook serving (validate: registry + trifecta + model.pool over Agent/AgentFleet; mutate: secure defaults)"
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

/// `GET /metrics` — the Prometheus text exposition format (version 0.0.4).
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
/// `POST /convert` — ConversionReview for the multi-version CRDs. Pure and
/// synchronous; the heavy lifting is `agent_api::v1alpha2::convert`.
async fn convert_handler(Json(review): Json<Value>) -> Json<Value> {
    Json(convert::convert_review(&review))
}

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

    // MCPService UPDATE: only the tag-laundering guard applies.
    if kind == "MCPService" {
        let name = object["metadata"]["name"].as_str().unwrap_or_default();
        let verdict =
            check_tag_laundering(&state, &namespace, name, &request["oldObject"], object).await;
        match &verdict {
            Ok(()) => tracing::info!(%uid, %namespace, %kind, "admit"),
            Err(msg) => tracing::warn!(%uid, %namespace, %kind, deny = %msg, "deny"),
        }
        state.metrics.record(verdict.is_ok());
        return Json(admission_response_with_warnings(&uid, verdict, Vec::new()));
    }

    // The same image/capabilities/model.pool checks apply to an Agent's spec
    // and to an AgentFleet's `spec.template` (itself an AgentSpec), so a fleet
    // template is held to the same policy as a standalone agent.
    let view = agent_spec_view(&kind, spec, &empty);

    let empty_map = Map::new();
    let annotations = object["metadata"]["annotations"]
        .as_object()
        .unwrap_or(&empty_map);

    // An AgentFleet's `spec.coordinator.template` is ALSO an AgentSpec — a normal
    // conformant agent subject to the SAME policy. Check it too, so a coordinator
    // cannot smuggle a disallowed image or ungated trifecta past the gate the worker
    // template is checked at. `None` for a non-fleet or a coordinatorless fleet.
    let coordinator_view = coordinator_spec_view(&kind, spec);

    // Evaluate every AgentSpec view under review (worker template always; the
    // coordinator template when present). The first denial is the verdict — a
    // fleet is admitted only if BOTH its members pass.
    let is_template = kind == "AgentFleet";
    let name = object["metadata"]["name"].as_str().unwrap_or_default();

    // Normalize the view: the webhook receives v1alpha2 (Equivalent match),
    // unit fixtures send v1 — the legacy rungs run on the v1 down-view, the
    // policy ladder on the full v2 spec.
    let normalized = normalize_view(view);
    let (v1_view, v2_spec) = match normalized {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(%uid, %namespace, %kind, deny = %e, "deny (unparseable spec)");
            state.metrics.record(false);
            return Json(admission_response_with_warnings(&uid, Err(e), Vec::new()));
        }
    };

    let mut verdict =
        check_handle_uniqueness(&state, &kind, &namespace, name, spec.get("handle")).await;
    if verdict.is_ok() {
        // The policy ladder (P2-4): scope-chain floors + registry ceilings,
        // BEFORE the per-object rungs (a floor denial should lead).
        verdict = check_policy_ladder(&state, &namespace, &v2_spec).await;
    }
    if verdict.is_ok() {
        verdict = evaluate_view(&state, &v1_view, annotations, &namespace, is_template).await;
    }
    if verdict.is_ok() {
        if let Some(coord) = coordinator_view {
            match normalize_view(coord) {
                Ok((coord_v1, coord_v2)) => {
                    verdict = check_policy_ladder(&state, &namespace, &coord_v2)
                        .await
                        .and(evaluate_view(&state, &coord_v1, annotations, &namespace, true).await)
                        .map_err(|m| format!("coordinator.template: {m}"));
                }
                Err(e) => verdict = Err(format!("coordinator.template: {e}")),
            }
        }
    }

    match &verdict {
        Ok(()) => tracing::info!(%uid, %namespace, %kind, "admit"),
        Err(msg) => tracing::warn!(%uid, %namespace, %kind, deny = %msg, "deny"),
    }
    state.metrics.record(verdict.is_ok());

    // Deprecation notices for v1alpha1 writers (the DoD's "convert with
    // warnings": conversion is lossless, the author still hears about it).
    // Deprecation notices for v1alpha1 writers. The apiserver hands the
    // webhook the v2 view even for v1 writes, so the notices derive from the
    // reconstructed v1 down-view (lossless round-trip). Agent-level only —
    // a fleet's template rides the same reconstruction inside its view.
    let warnings = if request["requestKind"]["version"].as_str() == Some("v1alpha1")
        || object["apiVersion"].as_str() == Some("agentctl.dev/v1alpha1")
    {
        v1_deprecation_warnings("Agent", &v1_view)
    } else {
        Vec::new()
    };
    Json(admission_response_with_warnings(&uid, verdict, warnings))
}

/// Run the full policy (registry + trifecta + model.pool existence) against ONE
/// `AgentSpec`-shaped `view`, resolving its named `ModelPool` cross-object. Shared
/// by the worker template and the coordinator template so both are held to the
/// same bar.
async fn evaluate_view(
    state: &AppState,
    view: &Value,
    annotations: &Map<String, Value>,
    namespace: &str,
    is_template: bool,
) -> Result<(), String> {
    let model_pool_exists = resolve_model_pool(&state.client, view, namespace).await;
    evaluate(
        view,
        annotations,
        &state.allowed_registries,
        model_pool_exists,
        namespace,
        state.aauth_default_provider,
        is_template,
    )?;
    // Referenced-Secret existence (rung 3b): the binary does NOT catch a
    // dangling credential Secret at validate time (upstream-confirmed: only
    // header-map refs are resolution-checked, and a missing secretKeyRef
    // would otherwise surface as a pod that cannot start). NotFound fails
    // CLOSED naming the Secret; transient lookup errors fail OPEN.
    check_referenced_secrets(state, view, namespace).await?;
    // Rung 4 (ground truth): compose the exact document the operator will
    // mount and run the pinned binary's own --validate-config over it. Skipped
    // when no binary is wired; transient cross-object failures fail OPEN (like
    // the pool lookup) — a spec defect the binary names fails CLOSED.
    binary_validate_view(state, view, namespace, is_template).await
}

/// Handle uniqueness (RFC 0033 §2, P2-7): the EFFECTIVE handle
/// (`spec.handle`, else the CR name) must be unique within the namespace
/// across Agents AND AgentFleets — it is the org route segment and the
/// supervisor `@handle` token, so a collision would make routing ambiguous.
/// Syntax is a DNS-1123 label. Duplicates fail CLOSED naming the holder;
/// transient list errors fail OPEN (the cross-object-read posture; CEL cannot
/// express cross-object uniqueness).
async fn check_handle_uniqueness(
    state: &AppState,
    kind: &str,
    namespace: &str,
    name: &str,
    handle: Option<&Value>,
) -> Result<(), String> {
    if kind != "Agent" && kind != "AgentFleet" {
        return Ok(());
    }
    let declared = handle.and_then(Value::as_str);
    if let Some(h) = declared {
        if !agent_api::valid_handle(h) {
            return Err(format!(
                "spec.handle {h:?} is not a DNS-1123 label (lowercase alphanumerics and '-')"
            ));
        }
    }
    let effective = agent_api::effective_handle(declared, name);

    let mut holders: Vec<(String, String, String)> = Vec::new(); // (kind, name, handle)
    let agents: kube::Api<agent_api::Agent> =
        kube::Api::namespaced(state.client.clone(), namespace);
    match agents.list(&Default::default()).await {
        Ok(list) => {
            for a in list.items {
                let n = a.metadata.name.clone().unwrap_or_default();
                let h = agent_api::effective_handle(a.spec.handle.as_deref(), &n).to_string();
                holders.push(("Agent".into(), n, h));
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "handle-uniqueness list Agents failed; failing open");
            return Ok(());
        }
    }
    let fleets: kube::Api<agent_api::AgentFleet> =
        kube::Api::namespaced(state.client.clone(), namespace);
    match fleets.list(&Default::default()).await {
        Ok(list) => {
            for f in list.items {
                let n = f.metadata.name.clone().unwrap_or_default();
                let h = agent_api::effective_handle(f.spec.handle.as_deref(), &n).to_string();
                holders.push(("AgentFleet".into(), n, h));
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "handle-uniqueness list AgentFleets failed; failing open");
            return Ok(());
        }
    }
    for (hk, hn, hh) in holders {
        if hh == effective && !(hk == kind && hn == name) {
            return Err(format!(
                "handle {effective:?} is already held by {hk} {hn:?} in this namespace                  (handles route /orgs/<org>/… and resolve @mentions, so they must be unique)"
            ));
        }
    }
    Ok(())
}

/// Every Secret the spec's credential wiring will mount must exist in the
/// agent's namespace: the bound ModelPool's `credentialSecretRef` and each
/// `mcpServers[].auth.tokenSecretRef`.
async fn check_referenced_secrets(
    state: &AppState,
    view: &Value,
    namespace: &str,
) -> Result<(), String> {
    use k8s_openapi::api::core::v1::Secret;
    let mut refs: Vec<(String, String)> = Vec::new(); // (secret, where)
    if let Some(servers) = view.get("mcpServers").and_then(Value::as_array) {
        for s in servers {
            if let Some(name) = s
                .pointer("/auth/tokenSecretRef/name")
                .and_then(Value::as_str)
            {
                let server = s.get("name").and_then(Value::as_str).unwrap_or("?");
                refs.push((
                    name.to_string(),
                    format!("mcpServers[{server}].auth.tokenSecretRef"),
                ));
            }
        }
    }
    if let Some(pool_name) = view.pointer("/model/pool").and_then(Value::as_str) {
        let api: Api<ModelPool> = Api::namespaced(state.client.clone(), namespace);
        if let Ok(Some(pool)) = api.get_opt(pool_name).await {
            if let Some(r) = pool.spec.credential_secret_ref {
                refs.push((r.name, format!("ModelPool/{pool_name} credentialSecretRef")));
            }
        }
    }
    let secrets: Api<Secret> = Api::namespaced(state.client.clone(), namespace);
    for (secret, site) in refs {
        match secrets.get_opt(&secret).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(format!(
                    "{site}: Secret '{secret}' not found in namespace '{namespace}' — the pod \
                     would fail at start (this is not caught by --validate-config)"
                ))
            }
            Err(e) => {
                tracing::warn!(%secret, error = %e, "Secret lookup failed; skipping existence check");
            }
        }
    }
    Ok(())
}

/// Compose + `agentd --validate-config` for one spec view (rung 4).
async fn binary_validate_view(
    state: &AppState,
    view: &Value,
    namespace: &str,
    is_template: bool,
) -> Result<(), String> {
    let Some(bin) = state.agentd_bin.clone() else {
        return Ok(());
    };
    // The view is the CRD's camelCase spec JSON; a fleet template composes the
    // way the operator renders it (coerced Reactive). A view that does not
    // deserialize is left to the CRD schema rung (fail-open here).
    let mut view = view.clone();
    if view.get("mode").is_none() {
        view["mode"] = json!(if is_template { "reactive" } else { "once" });
    }
    let mut spec: agent_api::AgentSpec = match serde_json::from_value(view) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "spec view does not deserialize; skipping binary validation");
            return Ok(());
        }
    };
    if is_template {
        spec.mode = agent_api::Mode::Reactive;
    }

    // Resolve the same facts the operator resolves.
    let intelligence = match spec.model.as_ref().and_then(|m| m.pool.as_deref()) {
        Some(pool_name) => {
            let api: Api<ModelPool> = Api::namespaced(state.client.clone(), namespace);
            match api.get_opt(pool_name).await {
                Ok(Some(pool)) => Some(agent_config::ResolvedIntelligence {
                    endpoint: pool.spec.endpoint,
                    model: spec.model.as_ref().and_then(|m| m.id.clone()),
                    has_token: pool.spec.credential_secret_ref.is_some(),
                }),
                // Absent pool already denied by rung 3; transient error: skip.
                _ => None,
            }
        }
        None => None,
    };
    let workflow_content: Option<String> = match &spec.workflow {
        None => None,
        Some(wf) => {
            if let Some(inline) = &wf.inline {
                Some(inline.clone())
            } else if let Some(r) = &wf.config_map_key_ref {
                use k8s_openapi::api::core::v1::ConfigMap;
                let api: Api<ConfigMap> = Api::namespaced(state.client.clone(), namespace);
                match api.get_opt(&r.name).await {
                    Ok(Some(cm)) => match cm.data.and_then(|mut d| d.remove(&r.key)) {
                        Some(content) => Some(content),
                        None => {
                            return Err(format!(
                                "workflow configMapKeyRef: key '{}' not found in ConfigMap '{}'",
                                r.key, r.name
                            ))
                        }
                    },
                    Ok(None) => {
                        return Err(format!(
                            "workflow configMapKeyRef: ConfigMap '{}' not found in namespace '{namespace}'",
                            r.name
                        ))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "workflow ConfigMap lookup failed; skipping binary validation");
                        return Ok(());
                    }
                }
            } else {
                None
            }
        }
    };

    // The document references the workflow at a path we choose here; the
    // blocking half writes the content beside the document under that name.
    const WORKFLOW_FILE: &str = "workflow.json";
    let doc = agent_config::compose_from_spec(
        &spec,
        intelligence,
        workflow_content.as_ref().map(|_| WORKFLOW_FILE.to_string()),
        None,
    )
    .map_err(|e| format!("config composition: {e}"))?;

    tokio::task::spawn_blocking(move || {
        run_validate_config(&bin, &doc, workflow_content.as_deref(), WORKFLOW_FILE)
    })
    .await
    .map_err(|e| format!("binary validation task failed: {e}"))?
}

/// Blocking half: materialize the document (+ optional workflow file) into a
/// tempdir and run `agentd -c agentd.json --validate-config` with a
/// placeholder env for every `{{secret:…}}` the document references (the
/// binary requires them to RESOLVE; validity is not checked at this stage).
fn run_validate_config(
    bin: &std::path::Path,
    projection: &agent_config::Projection,
    workflow_content: Option<&str>,
    workflow_file: &str,
) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| format!("binary validation tempdir: {e}"))?;
    // The EXACT invocation shape the pod uses: catalog layer first, instance
    // last (RFC 7396 layering; folders adopt beside the last file).
    let svc_path = dir.path().join(agent_config::paths::SERVICES_FILE);
    let cfg_path = dir.path().join(agent_config::paths::CONFIG_FILE);
    std::fs::write(&svc_path, projection.services.to_json())
        .map_err(|e| format!("binary validation write services: {e}"))?;
    std::fs::write(&cfg_path, projection.instance.to_json())
        .map_err(|e| format!("binary validation write: {e}"))?;
    if let Some(content) = workflow_content {
        std::fs::write(dir.path().join(workflow_file), content)
            .map_err(|e| format!("binary validation write workflow: {e}"))?;
    }
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("-c")
        .arg(&svc_path)
        .arg("-c")
        .arg(&cfg_path)
        .arg("--validate-config")
        .current_dir(dir.path());
    for name in projection.secret_refs() {
        cmd.env(name, "admission-placeholder");
    }
    let out = cmd
        .output()
        .map_err(|e| format!("binary validation exec: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // stderr is NDJSON; surface the config.invalid messages verbatim — the
    // binary's diagnosis is the most precise text a user will get.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let msgs: Vec<String> = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v.get("msg").and_then(Value::as_str).map(str::to_string))
        .collect();
    let detail = if msgs.is_empty() {
        stderr.trim().to_string()
    } else {
        msgs.join("; ")
    };
    Err(format!(
        "agentd --validate-config refused the spec: {detail}"
    ))
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
/// Normalize an AgentSpec-shaped view to BOTH versions: the webhook now
/// receives the v1alpha2 view (matchPolicy Equivalent up-converts v1 writes),
/// but the render/binary rungs still speak v1, and unit fixtures are
/// v1-shaped. Detection: a v1 view carries `mode`; a v2 view carries `shape`.
fn normalize_view(view: &Value) -> Result<(Value, agent_api::v1alpha2::AgentSpec), String> {
    if view.get("mode").is_some() {
        let v1: agent_api::AgentSpec = serde_json::from_value(view.clone())
            .map_err(|e| format!("parse v1alpha1 spec view: {e}"))?;
        let (v2, _) = agent_api::v1alpha2::convert::agent_v1_to_v2(&v1);
        Ok((view.clone(), v2))
    } else {
        let v2: agent_api::v1alpha2::AgentSpec = serde_json::from_value(view.clone())
            .map_err(|e| format!("parse v1alpha2 spec view: {e}"))?;
        let (v1, _) = agent_api::v1alpha2::convert::agent_v2_to_v1(&v2);
        let v1_view = serde_json::to_value(v1).map_err(|e| e.to_string())?;
        Ok((v1_view, v2))
    }
}

/// The policy ladder (RFC 0032, P2-4): resolve the agent's scope chain
/// (AgentClass parents, depth-capped) + the namespace registry slice
/// (MCPServices) and run the pure resolver — every floor violation denies,
/// naming the floor's holder. Transient cluster reads fail OPEN (the
/// cross-object posture); a NAMED class that does not exist fails CLOSED.
async fn check_policy_ladder(
    state: &AppState,
    namespace: &str,
    v2: &agent_api::v1alpha2::AgentSpec,
) -> Result<(), String> {
    use agent_api::v1alpha2::{AgentClass, MCPService};

    let classes: kube::Api<AgentClass> = kube::Api::namespaced(state.client.clone(), namespace);
    // Build the chain root-first by walking parents from the named class.
    let mut chain_specs: Vec<(String, agent_api::v1alpha2::AgentClassSpec)> = Vec::new();
    let explicit = v2.class.is_some();
    let mut cursor = Some(v2.class.clone().unwrap_or_else(|| "default".to_string()));
    let mut hops = 0;
    while let Some(name) = cursor.take() {
        hops += 1;
        if hops > 5 {
            return Err("AgentClass parent chain exceeds 5 links (cycle?)".into());
        }
        match classes.get_opt(&name).await {
            Ok(Some(c)) => {
                cursor = c.spec.parent.clone();
                chain_specs.push((format!("class {name:?}"), c.spec));
            }
            Ok(None) if hops == 1 && !explicit => break, // no implicit default class — no floors
            Ok(None) => {
                return Err(format!("AgentClass {name:?} not found in {namespace}"));
            }
            Err(e) => {
                tracing::warn!(error = %e, "policy ladder class read failed; failing open");
                return Ok(());
            }
        }
    }
    chain_specs.reverse(); // walked child→parent; the resolver wants root FIRST

    let services_api: kube::Api<MCPService> =
        kube::Api::namespaced(state.client.clone(), namespace);
    let registry: std::collections::BTreeMap<String, agent_api::v1alpha2::MCPServiceSpec> =
        match services_api.list(&Default::default()).await {
            Ok(list) => list
                .items
                .into_iter()
                .filter_map(|m| m.metadata.name.clone().map(|n| (n, m.spec)))
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "policy ladder registry read failed; failing open");
                return Ok(());
            }
        };

    let chain: Vec<agent_api::registry::ScopedClass<'_>> = chain_specs
        .iter()
        .map(|(scope, spec)| agent_api::registry::ScopedClass {
            scope: scope.clone(),
            class: spec,
        })
        .collect();
    match agent_api::registry::resolve(&chain, v2, &registry) {
        Ok(_) => Ok(()),
        Err(violations) => {
            let msgs: Vec<String> = violations.iter().map(|v| v.to_string()).collect();
            Err(msgs.join("; "))
        }
    }
}

/// Tag-laundering guard (RFC 0032 §5.3): while LIVE consumers exist, an
/// MCPService edit may not widen what those consumers effectively hold —
/// dropping a capability tag, widening the allow ceiling, shrinking exclude,
/// or repointing the endpoint are all refused naming the consumers.
async fn check_tag_laundering(
    state: &AppState,
    namespace: &str,
    service_name: &str,
    old: &Value,
    new: &Value,
) -> Result<(), String> {
    let old: agent_api::v1alpha2::MCPServiceSpec = match serde_json::from_value(
        old.get("spec").cloned().unwrap_or_default(),
    ) {
        Ok(s) => s,
        Err(e) => {
            // No old spec = a create (nothing to launder). Log it so a
            // malformed old object can never silently bypass the guard.
            tracing::info!(service = %service_name, error = %e, "laundering guard: no parseable old spec (create?)");
            return Ok(());
        }
    };
    let new: agent_api::v1alpha2::MCPServiceSpec =
        serde_json::from_value(new.get("spec").cloned().unwrap_or_default())
            .map_err(|e| format!("parse MCPService spec: {e}"))?;

    let mut widenings = Vec::new();
    for t in &old.tags {
        if !new.tags.contains(t) {
            widenings.push(format!("drops the {t:?} capability tag"));
        }
    }
    if !old.allow.is_empty() {
        if new.allow.is_empty() {
            widenings.push("removes the allow ceiling entirely".to_string());
        } else {
            for pat in &new.allow {
                let within = old.allow.iter().any(|c| {
                    c == pat || (!pat.contains('*') && agent_api::org::access::glob_match(c, pat))
                });
                if !within {
                    widenings.push(format!("widens the allow ceiling with {pat:?}"));
                }
            }
        }
    }
    for e in &old.exclude {
        if !new.exclude.contains(e) {
            widenings.push(format!("removes the {e:?} exclusion"));
        }
    }
    if old.endpoint != new.endpoint {
        widenings.push(format!(
            "repoints the endpoint ({:?} → {:?})",
            old.endpoint, new.endpoint
        ));
    }
    tracing::info!(service = %service_name, widenings = ?widenings, "tag-laundering check");
    if widenings.is_empty() {
        return Ok(());
    }

    // Widening with NO consumers is a plain registry edit — allowed.
    let agents: kube::Api<agent_api::v1alpha2::Agent> =
        kube::Api::namespaced(state.client.clone(), namespace);
    let consumers: Vec<String> = match agents.list(&Default::default()).await {
        Ok(list) => list
            .items
            .into_iter()
            .filter(|a| a.spec.services.iter().any(|g| g.name == service_name))
            .filter_map(|a| a.metadata.name)
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "laundering-guard consumer list failed; failing open");
            return Ok(());
        }
    };
    tracing::info!(service = %service_name, consumers = ?consumers, "tag-laundering consumers");
    if consumers.is_empty() {
        return Ok(());
    }
    Err(format!(
        "tag-laundering guard: this edit would widen live consumers ({}): {}",
        consumers.join(", "),
        widenings.join("; ")
    ))
}

fn agent_spec_view<'a>(kind: &str, spec: &'a Value, empty: &'a Value) -> &'a Value {
    if kind == "AgentFleet" {
        spec.get("template").unwrap_or(empty)
    } else {
        spec
    }
}

/// The coordinator's `AgentSpec` view (`spec.coordinator.template`), if the
/// reviewed object is an `AgentFleet` that declares a coordinator.
/// `None` for a plain `Agent`, or a fleet without a coordinator — nothing extra to
/// check. When `Some`, `validate` runs the SAME policy against it as the worker
/// template, so a coordinator cannot smuggle a disallowed image or ungated trifecta.
fn coordinator_spec_view<'a>(kind: &str, spec: &'a Value) -> Option<&'a Value> {
    if kind == "AgentFleet" {
        spec.get("coordinator").and_then(|c| c.get("template"))
    } else {
        None
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

/// Whether the view carries an inline `mcpServers[]` entry with
/// `auth.mode: aauth` (a direct signed dial). Pure — the servers are inline on
/// the Agent now, so no cross-object lookup.
fn bound_aauth_server(spec: &Value) -> bool {
    spec.get("mcpServers")
        .and_then(Value::as_array)
        .is_some_and(|servers| {
            servers
                .iter()
                .any(|s| s.pointer("/auth/mode").and_then(Value::as_str) == Some("aauth"))
        })
}

/// If `spec.model.pool` names a pool, look it up in `namespace`: `Some(true)` if
/// it exists, `Some(false)` if not. `None` when no pool is named — and also when
/// the lookup itself errors (fail-open: a transient apiserver hiccup must not
/// block otherwise-valid admissions; the existence check is simply skipped).
async fn resolve_model_pool(client: &Client, spec: &Value, namespace: &str) -> Option<bool> {
    let name = spec.pointer("/model/pool").and_then(Value::as_str)?;
    let api: Api<ModelPool> = Api::namespaced(client.clone(), namespace);
    match api.get_opt(name).await {
        Ok(found) => Some(found.is_some()),
        Err(e) => {
            tracing::error!(model_pool = name, %namespace, error = %e, "ModelPool lookup failed; skipping cross-object check");
            None
        }
    }
}

/// Build the `AdmissionReview` response carrying the verdict. A denial puts the
/// reason in `status.message` (surfaced to the user by the apiserver).
#[cfg(test)]
fn admission_response(uid: &str, verdict: Result<(), String>) -> Value {
    admission_response_with_warnings(uid, verdict, Vec::new())
}

/// AdmissionReview response with optional client-visible warnings — the
/// channel the v1alpha1 deprecation notices ride (ConversionReview has none).
fn admission_response_with_warnings(
    uid: &str,
    verdict: Result<(), String>,
    warnings: Vec<String>,
) -> Value {
    let (allowed, code, message) = match verdict {
        Ok(()) => (true, 200u16, String::new()),
        Err(msg) => (false, 403u16, msg),
    };
    let mut resp = json!({
        "uid": uid,
        "allowed": allowed,
        "status": { "code": code, "message": message }
    });
    if !warnings.is_empty() {
        resp["warnings"] = json!(warnings);
    }
    json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "response": resp
    })
}

/// The v1alpha1→v1alpha2 migration notices for a spec written at v1alpha1
/// (the conversion itself is lossless; these tell the author what to update).
fn v1_deprecation_warnings(kind: &str, spec: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match kind {
        "Agent" => {
            if let Ok(v1) = serde_json::from_value::<agent_api::AgentSpec>(spec.clone()) {
                let (_, warnings) = agent_api::v1alpha2::convert::agent_v1_to_v2(&v1);
                out.extend(
                    warnings
                        .into_iter()
                        .map(|w| format!("agentctl.dev/v1alpha1 is deprecated: {w}")),
                );
            }
        }
        "AgentFleet" => {
            if let Ok(v1) = serde_json::from_value::<agent_api::AgentFleetSpec>(spec.clone()) {
                let (_, warnings) = agent_api::v1alpha2::convert::fleet_v1_to_v2(&v1);
                out.extend(
                    warnings
                        .into_iter()
                        .map(|w| format!("agentctl.dev/v1alpha1 is deprecated: {w}")),
                );
            }
        }
        _ => {}
    }
    out
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
/// 2. The lethal trifecta (`capabilities.exec` && `capabilities.egress` && a
///    non-empty `capabilities.secrets`) is requested without the
///    `agentctl.dev/allow-trifecta: "true"` annotation.
/// 3. `spec.model.pool` is named but `model_pool_exists == Some(false)`.
/// 4. `spec.access.oidc` is present but malformed (non-https/empty `issuer`,
///    empty `audiences`, or a non-https `jwksUri`) — see [`validate_oidc`].
/// 5. `identity.aauth` on a FLEET view (worker or coordinator template) —
///    phase-gated: fleet identities land with per-pod (assertion) enrollment
///    (RFC 0023 §5.3/§10.1); a shared-template identity today would silently
///    alias every replica.
/// 6. `identity.aauth` without a resolvable provider (no spec `provider` and
///    no operator default) — catch the config error here, not as a
///    crash-looping pod.
/// 7. `identity.aauth.provider` / `.personServer`, when set, are not
///    `https://` URLs (the operator's own default is its config, not re-checked
///    here; a spec-level override travels the cluster and must be verifiable).
/// 8. An inline `auth.mode: aauth` MCP server (`bound_aauth_server`) without
///    `identity.aauth` + `capabilities.egress: true` — the direct signed dial
///    (RFC 0024) needs both: an identity to sign with, and the declared-intent
///    egress that will carry it.
#[allow(clippy::too_many_arguments)] // a pure verdict fn over pre-resolved facts
fn evaluate(
    spec: &Value,
    annotations: &Map<String, Value>,
    allowed_registries: &[String],
    model_pool_exists: Option<bool>,
    namespace: &str,
    aauth_default_provider: bool,
    is_template: bool,
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

    // 2. Lethal-trifecta override gate. The grants are grouped under
    // `spec.capabilities{}` (evaluated as a union).
    let exec = spec.pointer("/capabilities/exec").and_then(Value::as_bool) == Some(true);
    let egress = spec
        .pointer("/capabilities/egress")
        .and_then(Value::as_bool)
        == Some(true);
    let secrets = spec
        .pointer("/capabilities/secrets")
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
    if let Some(name) = spec.pointer("/model/pool").and_then(Value::as_str) {
        if model_pool_exists == Some(false) {
            return Err(format!(
                "ModelPool '{name}' not found in namespace {namespace}"
            ));
        }
    }

    // 4. Per-agent OIDC access policy (`spec.access.oidc`) must be well-formed:
    // the gateway can only enforce a verifiable issuer + a bounded audience.
    if let Some(oidc) = spec
        .get("access")
        .and_then(|a| a.get("oidc"))
        .filter(|v| !v.is_null())
    {
        validate_oidc(oidc)?;
    }

    // 5–7. AAuth identity opt-in (`identity.aauth`, RFC 0023).
    if let Some(aauth) = spec.pointer("/identity/aauth").filter(|v| !v.is_null()) {
        if is_template {
            return Err(
                "identity.aauth is not yet supported on fleet templates (replicas would \
                 share one identity); per-pod fleet identities land with assertion \
                 enrollment (RFC 0023)"
                    .to_string(),
            );
        }
        let provider = aauth.get("provider").and_then(Value::as_str);
        match provider {
            None if !aauth_default_provider => {
                return Err(
                    "identity.aauth requires a provider: set spec.identity.aauth.provider \
                     or configure the operator default (AGENTCTL_AAUTH_PROVIDER / chart \
                     identity.aauth.provider)"
                        .to_string(),
                );
            }
            Some(p) if !is_https_url(p) => {
                return Err(format!(
                    "spec.identity.aauth.provider must be an https:// URL (got '{p}')"
                ));
            }
            _ => {}
        }
        if let Some(ps) = aauth.get("personServer").and_then(Value::as_str) {
            if !is_https_url(ps) {
                return Err(format!(
                    "spec.identity.aauth.personServer must be an https:// URL (got '{ps}')"
                ));
            }
        }
    }

    // 8. Direct-dial (aauth) MCP servers need an identity to sign with and the
    // declared egress that will carry the dial (RFC 0024).
    if bound_aauth_server(spec) {
        let has_identity = spec
            .pointer("/identity/aauth")
            .filter(|v| !v.is_null())
            .is_some();
        if !has_identity {
            return Err(
                "binding an aauth-mode MCP server requires spec.identity.aauth — the agent \
                 itself signs the direct dial (RFC 0024)"
                    .to_string(),
            );
        }
        let egress = spec
            .pointer("/capabilities/egress")
            .and_then(Value::as_bool)
            == Some(true);
        if !egress {
            return Err(
                "binding an aauth-mode MCP server requires capabilities.egress: true — the \
                 dial leaves the cluster directly (declared-intent egress, RFC 0024)"
                    .to_string(),
            );
        }
    }

    Ok(())
}

/// Validate a `spec.access.oidc` block (the per-agent A2A caller-identity policy,
/// also reached at `spec.template.access.oidc` for an `AgentFleet`). The gateway
/// turns this into a JWKS-verified JWT gate, so the fields it needs must be
/// well-formed at admission time rather than failing opaquely at request time:
///
/// 1. `issuer` is a non-empty `https://` URL — JWKS discovery and the `iss` check
///    both key off it, and a plaintext issuer would let JWKS be MITM'd.
/// 2. `audiences` is non-empty — an empty audience set accepts *any* `aud`, which
///    silently widens the gate to tokens minted for other services.
/// 3. `jwksUri`, when set (the discovery override), is itself `https://`.
fn validate_oidc(oidc: &Value) -> Result<(), String> {
    let issuer = oidc.get("issuer").and_then(Value::as_str).unwrap_or("");
    if issuer.is_empty() {
        return Err(
            "spec.access.oidc.issuer is required and must be a non-empty https:// URL".to_string(),
        );
    }
    if !is_https_url(issuer) {
        return Err(format!(
            "spec.access.oidc.issuer must be an https:// URL (got '{issuer}')"
        ));
    }

    let audiences_non_empty = oidc
        .get("audiences")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|v| v.as_str().is_some_and(|s| !s.is_empty())));
    if !audiences_non_empty {
        return Err(
            "spec.access.oidc.audiences must list at least one non-empty audience".to_string(),
        );
    }

    if let Some(jwks) = oidc.get("jwksUri").and_then(Value::as_str) {
        if !is_https_url(jwks) {
            return Err(format!(
                "spec.access.oidc.jwksUri must be an https:// URL (got '{jwks}')"
            ));
        }
    }

    Ok(())
}

/// A pragmatic `https://` URL check for admission: an `https://` scheme (ASCII,
/// case-insensitive) with a non-empty authority after it. The gateway does the
/// full RFC parse at runtime; admission only rejects the obvious foot-guns
/// (plaintext `http://`, a bare host, an empty URL).
fn is_https_url(s: &str) -> bool {
    s.len() > "https://".len()
        && s.get(.."https://".len())
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

// --- defaulting (pure) -----------------------------------------------------

/// The `app.kubernetes.io/name` value for a reviewed kind.
fn kind_app_name(kind: &str) -> &'static str {
    match kind {
        "AgentFleet" => "agentfleet",
        _ => "agent",
    }
}

/// Escape a string for use as a single JSON Pointer reference token:
/// `~` ⇒ `~0`, `/` ⇒ `~1` (order matters — `~` first).
fn escape_pointer_token(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// Build the JSON Patch of **secure defaults** for an `Agent`/`AgentFleet`.
/// Every op is conditional on the field being **absent** — defaulting never
/// clobbers an author's explicit value (and is auditable in the patch). Defaults:
///
/// 1. **Standard `app.kubernetes.io/*` labels** (`managed-by`/`part-of`/`name`) —
///    pure metadata, always safe. Adds the whole `metadata.labels` object if none
///    exists, else only the missing keys.
/// 2. **`mode`** ⇒ `"once"` — the conservative run-once shape and the enum default;
///    only added when absent (it is a required field, so defaulting it pre-empts a
///    structural rejection with the safest run shape).
/// 3. **`surfaces`** ⇒ all-`false` — minimal control-plane exposure; an author opts
///    a surface on explicitly. `a2a` in particular is a network-exposed surface that
///    must never default on.
///
/// Deliberately **not** defaulted: `substrate`. Its secure default is
/// tenancy-derived and the most-isolated tier (`kata-hybrid`) needs a Kata
/// `RuntimeClass` absent on most stock clusters; hard-defaulting here would either
/// break stock clusters or be insecure, so the field is left absent for the
/// operator/renderer to resolve from `AgentClass`/tenancy.
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

    // The v1-era mode/surfaces defaulting is GONE: the webhook receives the
    // v1alpha2 view (Equivalent match up-converts v1 writes BEFORE mutation),
    // where `shape` is schema-defaulted and expose is opt-in. Injecting v1
    // fields into a v2 view corrupted the validate stage's version detection
    // (observed live: a `mode` key made the ladder see a bare v1 spec and
    // skip the class floors entirely).

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
    /// Ground truth (rung 4), against the REAL pinned binary. Gated on
    /// AGENTD_BIN so `cargo test` stays hermetic; CI wires the release binary.
    #[test]
    fn binary_validation_denies_what_the_binary_refuses_and_admits_the_rest() {
        let Some(bin) = std::env::var_os("AGENTD_BIN") else {
            eprintln!("skipping: AGENTD_BIN not set");
            return;
        };
        let bin = std::path::PathBuf::from(bin);

        // A valid once-shaped spec composes + validates clean.
        let good = agent_api::AgentSpec {
            mode: agent_api::Mode::Once,
            instruction: Some("Do the thing.".into()),
            ..Default::default()
        };
        let doc = agent_config::compose_from_spec(
            &good,
            Some(agent_config::ResolvedIntelligence {
                endpoint: "http://127.0.0.1:9999/v1".into(),
                model: Some("t".into()),
                has_token: true,
            }),
            None,
            None,
        )
        .unwrap();
        super::run_validate_config(&bin, &doc, None, "workflow.json")
            .expect("a clean spec must validate");

        // A pre-rewrite (dialect 1/2) workflow document is refused BY THE
        // BINARY, and its diagnosis travels into the deny message verbatim.
        let bad = agent_api::AgentSpec {
            mode: agent_api::Mode::Workflow,
            workflow: Some(agent_api::WorkflowSource {
                inline: Some(r#"{"start":"a","nodes":{"a":{"kind":"halt"}}}"#.into()),
                config_map_key_ref: None,
            }),
            ..Default::default()
        };
        let doc = agent_config::compose_from_spec(&bad, None, Some("workflow.json".into()), None)
            .unwrap();
        let err = super::run_validate_config(
            &bin,
            &doc,
            Some(r#"{"start":"a","nodes":{"a":{"kind":"halt"}}}"#),
            "workflow.json",
        )
        .expect_err("an old-dialect workflow must be refused");
        assert!(
            err.contains("--validate-config refused"),
            "deny message names the rung: {err}"
        );
    }

    use super::*;
    use serde_json::json;

    fn annotations(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn clean_agent_is_allowed() {
        let spec = json!({ "mode": "once", "image": "ghcr.io/acme/agent:v1" });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
    }

    #[test]
    fn trifecta_without_annotation_is_denied() {
        let spec = json!({
            "mode": "loop",
            "capabilities": { "exec": true, "egress": true, "secrets": ["db-password"] }
        });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("lethal trifecta"));
        assert!(!err.is_empty());
    }

    #[test]
    fn trifecta_with_annotation_is_allowed() {
        let spec = json!({
            "mode": "loop",
            "capabilities": { "exec": true, "egress": true, "secrets": ["db-password"] }
        });
        let anns = annotations(json!({ "agentctl.dev/allow-trifecta": "true" }));
        assert!(evaluate(&spec, &anns, &[], None, "default", false, false).is_ok());
    }

    #[test]
    fn coordinator_view_selected_only_for_fleet_with_coordinator() {
        // A plain Agent has no coordinator view.
        let agent_spec = json!({ "mode": "once", "image": "ghcr.io/acme/agent:v1" });
        assert!(coordinator_spec_view("Agent", &agent_spec).is_none());
        // A fleet without a coordinator: nothing extra to check.
        let bare_fleet = json!({ "template": { "mode": "reactive" } });
        assert!(coordinator_spec_view("AgentFleet", &bare_fleet).is_none());
        // A fleet WITH a coordinator: its template is returned for policy checks.
        let fleet = json!({
            "template": { "mode": "reactive" },
            "coordinator": { "template": { "mode": "reactive", "image": "ghcr.io/acme/main:v1" } }
        });
        let view = coordinator_spec_view("AgentFleet", &fleet).expect("coordinator view");
        assert_eq!(view["image"], "ghcr.io/acme/main:v1");
    }

    #[test]
    fn coordinator_template_is_held_to_the_same_policy() {
        // The worker template is clean, but the coordinator smuggles an ungated
        // lethal trifecta — evaluate (the shared policy) denies it, and validate
        // runs it against the coordinator view. Assert the policy itself rejects
        // the coordinator spec (the view validate feeds it).
        let fleet = json!({
            "template": { "mode": "reactive", "image": "ghcr.io/acme/agent:v1" },
            "coordinator": { "template": {
                "mode": "reactive",
                "capabilities": { "exec": true, "egress": true, "secrets": ["db-password"] }
            }}
        });
        let coord = coordinator_spec_view("AgentFleet", &fleet).unwrap();
        let err = evaluate(coord, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(
            err.contains("lethal trifecta"),
            "coordinator trifecta must be denied: {err}"
        );
    }

    #[test]
    fn trifecta_annotation_must_be_literal_true() {
        let spec = json!({
            "capabilities": { "exec": true, "egress": true, "secrets": ["s"] }
        });
        // Any value other than "true" does not open the gate.
        let anns = annotations(json!({ "agentctl.dev/allow-trifecta": "yes" }));
        assert!(evaluate(&spec, &anns, &[], None, "default", false, false).is_err());
    }

    #[test]
    fn two_of_three_trifecta_legs_is_allowed() {
        // exec + egress but no secrets ⇒ not the full trifecta ⇒ no gate.
        let spec = json!({ "capabilities": { "exec": true, "egress": true } });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
        // exec + egress with an empty secrets array is still only two legs.
        let spec = json!({ "capabilities": { "exec": true, "egress": true, "secrets": [] } });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
    }

    #[test]
    fn disallowed_registry_is_denied() {
        let spec = json!({ "image": "docker.io/library/evil:latest" });
        let registries = vec!["ghcr.io/acme/".to_string()];
        let err = evaluate(
            &spec,
            &Map::new(),
            &registries,
            None,
            "default",
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("not from an allowed registry"));
        assert!(err.contains("ghcr.io/acme/"));
        assert!(!err.is_empty());
    }

    #[test]
    fn allowed_registry_is_allowed() {
        let spec = json!({ "image": "ghcr.io/acme/agent@sha256:abc" });
        let registries = vec!["ghcr.io/acme/".to_string()];
        assert!(evaluate(
            &spec,
            &Map::new(),
            &registries,
            None,
            "default",
            false,
            false,
        )
        .is_ok());
    }

    #[test]
    fn empty_registry_list_allows_any_image() {
        let spec = json!({ "image": "quay.io/whatever:1" });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
    }

    #[test]
    fn missing_model_pool_is_denied() {
        let spec = json!({ "model": { "pool": "shared" } });
        let err =
            evaluate(&spec, &Map::new(), &[], Some(false), "team-a", false, false).unwrap_err();
        assert!(err.contains("shared"));
        assert!(err.contains("team-a"));
        assert!(!err.is_empty());
    }

    #[test]
    fn present_model_pool_is_allowed() {
        let spec = json!({ "model": { "pool": "shared" } });
        assert!(evaluate(&spec, &Map::new(), &[], Some(true), "team-a", false, false,).is_ok());
    }

    #[test]
    fn no_model_pool_named_skips_cross_object_check() {
        let spec = json!({ "mode": "once" });
        // Even if the resolver reported a negative, no pool is named ⇒ no deny.
        assert!(evaluate(
            &spec,
            &Map::new(),
            &[],
            Some(false),
            "default",
            false,
            false,
        )
        .is_ok());
    }

    // --- per-agent OIDC access policy (spec.access.oidc) -------------------

    #[test]
    fn oidc_well_formed_is_allowed() {
        let spec = json!({
            "access": { "oidc": {
                "issuer": "https://idp.example.com",
                "audiences": ["agentctl-a2a"],
                "jwksUri": "https://idp.example.com/keys",
                "requiredClaims": [{ "claim": "groups", "anyOf": ["support"] }],
                "forwardIdentity": true
            }}
        });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
    }

    #[test]
    fn oidc_without_jwks_uri_is_allowed() {
        // jwksUri is optional (auto-discovery from the issuer).
        let spec = json!({
            "access": { "oidc": {
                "issuer": "https://idp.example.com",
                "audiences": ["agentctl-a2a"]
            }}
        });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
    }

    #[test]
    fn oidc_absent_or_public_only_access_is_allowed() {
        // No access block at all.
        let spec = json!({ "mode": "once" });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
        // access present but only the doc-only `public` flag, no oidc.
        let spec = json!({ "access": { "public": true } });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
        // explicit null oidc is treated as absent.
        let spec = json!({ "access": { "oidc": null } });
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", false, false).is_ok());
    }

    #[test]
    fn oidc_missing_issuer_is_denied() {
        let spec = json!({ "access": { "oidc": { "audiences": ["a"] } } });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("issuer"));
        assert!(!err.is_empty());
    }

    #[test]
    fn oidc_empty_issuer_is_denied() {
        let spec = json!({ "access": { "oidc": { "issuer": "", "audiences": ["a"] } } });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("issuer"));
    }

    #[test]
    fn oidc_non_https_issuer_is_denied() {
        let spec = json!({
            "access": { "oidc": { "issuer": "http://idp.example.com", "audiences": ["a"] } }
        });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("issuer"));
        assert!(err.contains("https://"));
    }

    #[test]
    fn oidc_empty_audiences_is_denied() {
        // Missing audiences.
        let spec = json!({ "access": { "oidc": { "issuer": "https://idp.example.com" } } });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("audiences"));
        // Present but empty array.
        let spec = json!({
            "access": { "oidc": { "issuer": "https://idp.example.com", "audiences": [] } }
        });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("audiences"));
        // Present but only a blank string ⇒ still effectively empty.
        let spec = json!({
            "access": { "oidc": { "issuer": "https://idp.example.com", "audiences": [""] } }
        });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("audiences"));
    }

    #[test]
    fn oidc_non_https_jwks_uri_is_denied() {
        let spec = json!({
            "access": { "oidc": {
                "issuer": "https://idp.example.com",
                "audiences": ["a"],
                "jwksUri": "http://idp.example.com/keys"
            }}
        });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("jwksUri"));
        assert!(err.contains("https://"));
    }

    #[test]
    fn oidc_validated_for_agentfleet_template() {
        // The same OIDC policy under an AgentFleet's template must be validated —
        // the fleet view is `spec.template`, an AgentSpec.
        let spec = json!({
            "template": { "access": { "oidc": {
                "issuer": "ftp://nope",
                "audiences": ["a"]
            }}},
            "scaling": { "mode": "claim" }
        });
        let empty = Value::Object(Map::new());
        let view = agent_spec_view("AgentFleet", &spec, &empty);
        let err = evaluate(view, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("issuer"));
    }

    #[test]
    fn is_https_url_accepts_only_https() {
        assert!(is_https_url("https://idp.example.com"));
        assert!(is_https_url("HTTPS://idp.example.com")); // scheme is case-insensitive
        assert!(!is_https_url("http://idp.example.com"));
        assert!(!is_https_url("https://")); // scheme only, no authority
        assert!(!is_https_url("idp.example.com"));
        assert!(!is_https_url(""));
    }

    #[test]
    fn deny_message_is_non_empty() {
        let spec = json!({ "capabilities": { "exec": true, "egress": true, "secrets": ["x"] } });
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(!err.trim().is_empty());
    }

    // --- identity.aauth (RFC 0023) ------------------------------------------

    #[test]
    fn aauth_on_a_fleet_template_is_denied_phase_gated() {
        let spec = json!({ "identity": { "aauth": {} } });
        // The SAME spec passes as an Agent (with a default provider) but is
        // denied as a template view — replicas would alias one identity.
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", true, false).is_ok());
        let err = evaluate(&spec, &Map::new(), &[], None, "default", true, true).unwrap_err();
        assert!(err.contains("fleet templates"), "got: {err}");
    }

    #[test]
    fn aauth_without_any_provider_is_denied() {
        let spec = json!({ "identity": { "aauth": {} } });
        // No spec provider + no operator default ⇒ deny with a pointed message.
        let err = evaluate(&spec, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("requires a provider"), "got: {err}");
        // Operator default configured ⇒ the empty opt-in is fine.
        assert!(evaluate(&spec, &Map::new(), &[], None, "default", true, false).is_ok());
        // A spec-level provider also satisfies it (no default needed).
        let with = json!({ "identity": { "aauth": { "provider": "https://ap.example" } } });
        assert!(evaluate(&with, &Map::new(), &[], None, "default", false, false).is_ok());
    }

    #[test]
    fn aauth_urls_must_be_https() {
        let plaintext = json!({ "identity": { "aauth": { "provider": "http://ap.example" } } });
        let err =
            evaluate(&plaintext, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("https://"), "got: {err}");

        let bad_ps = json!({ "identity": { "aauth": {
            "provider": "https://ap.example",
            "personServer": "http://ps.example"
        } } });
        let err = evaluate(&bad_ps, &Map::new(), &[], None, "default", false, false).unwrap_err();
        assert!(err.contains("personServer"), "got: {err}");
    }

    #[test]
    fn aauth_mode_server_binding_requires_identity_and_egress() {
        // An inline aauth-mode MCP server + no identity ⇒ deny.
        let aauth_srv = json!([{ "name": "secure",
            "endpoint": "https://mcp.secure/mcp", "auth": { "mode": "aauth" } }]);
        let bare = json!({ "mcpServers": aauth_srv });
        let err = evaluate(&bare, &Map::new(), &[], None, "default", true, false).unwrap_err();
        assert!(err.contains("identity.aauth"), "got: {err}");

        // Identity but no declared egress ⇒ deny on the egress leg.
        let no_egress = json!({ "mcpServers": aauth_srv, "identity": { "aauth": {} } });
        let err = evaluate(&no_egress, &Map::new(), &[], None, "default", true, false).unwrap_err();
        assert!(err.contains("capabilities.egress"), "got: {err}");

        // Identity + egress ⇒ admitted.
        let full = json!({
            "mcpServers": aauth_srv,
            "identity": { "aauth": {} },
            "capabilities": { "egress": true }
        });
        assert!(evaluate(&full, &Map::new(), &[], None, "default", true, false).is_ok());

        // A non-aauth server (none/staticToken) ⇒ rule 8 is inert.
        let none_srv = json!({ "mcpServers": [{ "name": "x", "endpoint": "https://x/mcp" }] });
        assert!(evaluate(&none_srv, &Map::new(), &[], None, "default", true, false).is_ok());
    }

    #[test]
    fn absent_or_null_identity_is_ignored() {
        let absent = json!({ "mode": "once" });
        assert!(evaluate(&absent, &Map::new(), &[], None, "default", false, false,).is_ok());
        let null = json!({ "identity": { "aauth": null } });
        assert!(evaluate(&null, &Map::new(), &[], None, "default", false, true).is_ok());
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

    // --- AgentFleet coverage -----------------------------------------------

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
        // The same lethal trifecta in a fleet's template must be gated, since the
        // fleet view is `spec.template`, an AgentSpec.
        let spec = json!({
            "template": {
                "mode": "loop",
                "capabilities": { "exec": true, "egress": true, "secrets": ["db"] }
            },
            "scaling": { "mode": "claim" }
        });
        let empty = Value::Object(Map::new());
        let view = agent_spec_view("AgentFleet", &spec, &empty);
        let err = evaluate(view, &Map::new(), &[], None, "default", false, false).unwrap_err();
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
        assert!(evaluate(view, &anns, &[], None, "default", false, false).is_ok());
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
        let err = evaluate(
            view,
            &Map::new(),
            &registries,
            None,
            "default",
            false,
            false,
        )
        .unwrap_err();
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
    fn mutate_defaults_labels_only() {
        let object = json!({
            "kind": "Agent",
            "metadata": { "name": "demo" },
            "spec": { "image": "ghcr.io/acme/a:v1" }
        });
        let ops = build_default_patch("Agent", &object);
        // No spec-level ops at all any more — labels only.
        assert!(ops.iter().all(|o| o["path"]
            .as_str()
            .unwrap_or("")
            .starts_with("/metadata/labels")));
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
    }

    #[test]
    fn mutate_targets_template_for_agentfleet() {
        let object = json!({
            "kind": "AgentFleet",
            "metadata": { "name": "f", "labels": { "team": "acme" } },
            "spec": { "template": { "image": "ghcr.io/acme/a:v1" }, "scaling": { "mode": "claim" } }
        });
        let ops = build_default_patch("AgentFleet", &object);
        // The v1-era mode/surfaces defaults are GONE (the webhook sees the
        // v1alpha2 view where shape is schema-defaulted).
        assert!(!ops
            .iter()
            .any(|o| o["path"].as_str().unwrap_or("").ends_with("/mode")));
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
            "substrate must be left to the operator/renderer"
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
        // Labels-only defaulting: the decoded patch round-trips as JSON ops.
        assert!(back
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["path"].as_str().unwrap_or("").starts_with("/metadata/labels")));
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
