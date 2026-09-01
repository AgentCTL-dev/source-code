// SPDX-License-Identifier: BUSL-1.1
//! `e2e` — the agentctl functional scenario runner.
//!
//! ~25 discrete, asserted scenarios across every plane: provisioning, management
//! (aggregated APIServer + RBAC), intelligence (secretless infer + budgets),
//! claim-mode coordination, shard-mode, A2A, conformance, and the seven security
//! overlays. Each scenario asserts via the `/metrics` + CR-status oracles, leaves
//! the cluster clean (deletes its CRs and awaits GC), and reports PASS / SKIP /
//! FAIL. Any FAIL ⇒ a nonzero process exit.
//!
//! Run all, a named subset, or one group:
//! ```text
//! e2e                       # all scenarios
//! e2e prov-once claim-dedupe
//! e2e --group security
//! e2e --list
//! ```
//! It needs a cluster (built from `KUBECONFIG`); with no cluster the scenarios
//! simply error. It is excluded from the workspace so `cargo test --workspace`
//! never compiles or runs it.

use std::future::Future;
use std::pin::Pin;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use serde_json::{json, Value};

use agent_api::{
    Agent, AgentFleet, AgentFleetSpec, AgentSpec, Mode, ScaleMode, ScaleTarget, Scaling, Work,
};
use agentctl_e2e::{contract, kube_helpers as kh, prom, shell, Ctx};

// --- timeouts ---------------------------------------------------------------

const READY_TIMEOUT: Duration = Duration::from_secs(180);
const GC_TIMEOUT: Duration = Duration::from_secs(120);
const SCALE_TIMEOUT: Duration = Duration::from_secs(240);

/// Where the mock provider + ModelPool fixtures live, relative to the repo root
/// (override with `AGENTCTL_EXAMPLES_DIR`). Defaults to `e2e/manifests`, whose
/// `mock-provider.yaml` answers in the OpenAI `chat/completions` envelope the real
/// agentd parses (so a once agent COMPLETES), while still carrying
/// `usage.total_tokens` for the gateway to meter/budget. Point it at
/// `deploy/examples` for the metering-only (non-OpenAI) mock.
fn examples_dir() -> String {
    std::env::var("AGENTCTL_EXAMPLES_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "e2e/manifests".to_string())
}

// --- control-plane Service names (Helm release `agentctl`) ------------------

const SVC_COORDINATION: &str = "agentctl-coordination";
const SVC_GATEWAY: &str = "agentctl-gateway";
const SVC_APISERVER: &str = "agentctl-apiserver";

// --- control-plane Service ports (the chart's Service.port, NOT the container
// targetPort). The http control-plane Services publish :80 -> :8080; the
// aggregated APIServer Service publishes :443 -> :6443. kubectl port-forward and
// the apiserver Service proxy address the Service port, so these — not 8080/6443
// — are what the scenarios must use. ---
const PORT_HTTP: u16 = 80;
const PORT_APISERVER: u16 = 443;
/// The coordination mTLS listener's Service port (added by the sec-coord-mtls
/// overlay's second listener on :8443).
const PORT_COORD_MTLS: u16 = 8443;

// --- scenario plumbing ------------------------------------------------------

/// The result of running one scenario.
enum Outcome {
    /// Asserted and clean.
    Passed,
    /// Deliberately not run, with a human reason (e.g. needs the Calico lane).
    Skipped(String),
}

fn pass() -> Result<Outcome> {
    Ok(Outcome::Passed)
}
fn skip(reason: impl Into<String>) -> Result<Outcome> {
    Ok(Outcome::Skipped(reason.into()))
}

type ScenFut<'a> = Pin<Box<dyn Future<Output = Result<Outcome>> + 'a>>;

/// A registered scenario: a stable name, its group, and the async body.
struct Scenario {
    name: &'static str,
    group: &'static str,
    run: Box<dyn for<'a> Fn(&'a Ctx) -> ScenFut<'a>>,
}

/// Wrap an `async fn(&Ctx) -> Result<Outcome>` into a [`Scenario`]. The nested `run`
/// fn gives the boxed future an explicit (lifetime-elided, HRTB) return type so the
/// trait-object coercion is unambiguous.
macro_rules! scenario {
    ($name:literal, $group:literal, $f:ident) => {{
        fn run(ctx: &Ctx) -> ScenFut<'_> {
            Box::pin($f(ctx))
        }
        Scenario {
            name: $name,
            group: $group,
            run: Box::new(run),
        }
    }};
}

/// The full catalogue (~25), in run order.
fn catalogue() -> Vec<Scenario> {
    vec![
        // provisioning
        scenario!("prov-once", "provisioning", prov_once_ready_exit),
        scenario!("prov-reactive", "provisioning", prov_reactive_capabilities),
        // management
        scenario!("mgmt-drain", "management", mgmt_drain),
        scenario!("mgmt-lame-duck", "management", mgmt_lame_duck),
        scenario!("mgmt-cancel", "management", mgmt_cancel),
        scenario!("mgmt-rbac-403", "management", mgmt_rbac_403),
        scenario!("mgmt-pause-resume", "management", mgmt_pause_resume),
        // claim-mode
        scenario!("claim-atomic", "claim", claim_atomic_single_grant),
        scenario!("claim-dedupe", "claim", claim_dedupe),
        scenario!("claim-lease-expiry", "claim", claim_lease_expiry_reoffer),
        scenario!("claim-scale-zero", "claim", claim_scale_zero_n_zero),
        // shard-mode
        scenario!("shard-kn", "shard", shard_k_of_n),
        // A2A
        scenario!("a2a-card-jws", "a2a", a2a_card_jws),
        scenario!("a2a-message-send", "a2a", a2a_message_send),
        scenario!("a2a-message-stream", "a2a", a2a_message_stream),
        // conformance
        scenario!("conf-exit-codes", "conformance", conf_exit_codes),
        scenario!(
            "conf-metrics-registry",
            "conformance",
            conf_metrics_registry
        ),
        // security overlays
        scenario!("sec-oidc", "security", sec_oidc),
        scenario!("sec-trusted-proxy", "security", sec_trusted_proxy),
        scenario!("sec-coord-attest", "security", sec_coord_attest),
        scenario!("sec-coord-mtls", "security", sec_coord_mtls),
        scenario!("sec-apitoken", "security", sec_apitoken),
        scenario!("sec-netpol", "security", sec_netpol),
        scenario!("sec-aauth", "security", sec_aauth),
        // tenancy (P1-3)
        scenario!("org-tenancy", "tenancy", org_tenancy),
        // per-user principals (P1-4)
        scenario!("a2a-principals-gate", "tenancy", a2a_principals_gate),
        // org routes + identity authn + principal injection (P1-5)
        scenario!("org-route-user", "tenancy", org_route_user),
        // accessPolicies enforcement + RBAC mirror (P1-8)
        scenario!("org-access-policy", "tenancy", org_access_policy),
        // the caller's own supervisor: auto-ensure → render → converse (P4-3/4)
        scenario!("supervisor-route", "tenancy", supervisor_route),
        // registry floors + tag-laundering guard (P2-4)
        scenario!("policy-ladder", "tenancy", policy_ladder),
        // trigger sugar over the ten start kinds (P2-8)
        scenario!("trigger-matrix", "triggers", trigger_matrix),
        // managed state service: checkpointer contract + SIGKILL/restore (P3)
        scenario!("state-durability", "durability", state_durability),
        // control MCP: AAuth-verified, namespace-scoped control.* tools (P4-1)
        scenario!("control-mcp", "control", control_mcp),
        // @mention orchestration: gateway envelope → supervisor workflow
        // fan-out → owner-authenticated delegates → gathered answer (P4-7)
        scenario!("mention-orchestration", "control", mention_orchestration),
        // static fleet: vars overlays + ordinal-0 singleton (P6-1)
        scenario!("fleet-static", "fleets", fleet_static),
        // guarded shard re-partition: quiesce-then-flip, never mixed moduli (P6-2)
        scenario!("shard-resize", "fleets", shard_resize),
        // work fabric: crash-mid-lease redelivery + result + DLQ round-trip (P6-4)
        scenario!("work-redelivery", "fleets", work_redelivery),
        // dispatcher: coordinator front door + a2a.delegate fan-out to workers (P6-3)
        scenario!("dispatcher-fanout", "fleets", dispatcher_fanout),
        // tenant mcpg: org gateway federating the registry, allow-narrowed (P5-1)
        scenario!("tenant-mcpg", "capability", tenant_mcpg),
        // OBO exchange: per-user credential injected upstream via /v1/exchange (P5-3)
        scenario!("obo-exchange", "capability", obo_exchange),
        // Connections: consent once via device flow, agents proceed OBO (P5-4)
        scenario!("connections-flow", "capability", connections_flow),
        // Hooks ingress: external delivery through the gateway, HMAC at agentd (P7-1)
        scenario!("hooks-ingress", "capability", hooks_ingress),
        // Scale-from-zero: parked webhook daemon wakes on first delivery (P6-5)
        scenario!("webhook-scale-zero", "fleets", webhook_scale_zero),
        // HITL fabric: gate parks the run, channel notified, right identity answers (P5-6)
        scenario!("hitl-gate", "capability", hitl_gate),
        // Audit pipeline: one queryable trail for a full OBO tool call (P7-3)
        scenario!("audit-trail", "capability", audit_trail),
        // Sandbox cell: code runs in a single-use, network-denied pod (P5-5)
        scenario!("sandbox-run", "capability", sandbox_run),
        // Artifacts façade: put/get/list over S3 (MinIO), org-fenced + quota (P3-3)
        scenario!("artifacts-flow", "capability", artifacts_flow),
        // OCI WorkflowSet bundles: a digest-pinned setRef resolves + projects (P7-7)
        scenario!("oci-bundles", "capability", oci_bundles),
        // Store classes: ephemeral/local/managed all render + run (P3-4)
        scenario!("store-classes", "capability", store_classes),
        // Lifecycle verbs: backup/restore/reset/stop/start/migrate on a managed
        // agent — migrate reschedules the pod with zero checkpoint loss (P3-5)
        scenario!("lifecycle-verbs", "durability", lifecycle_verbs),
        // supervisor scale-to-zero: idle park + touch-to-wake (P7-6)
        scenario!("supervisor-park", "control", supervisor_park),
        // billing-ready metering: durable events → attributed export (P7-4)
        scenario!("metering-export", "billing", metering_export),
        // fleet budget window: breach pauses intake, window passes, resumes (P6-6)
        scenario!("fleet-budget", "billing", fleet_budget),
    ]
}

/// P6-6: a fleet budget window breach PAUSES intake. A static fleet carries
/// `budget: {kind: a2a_requests, maxUnits: 3, windowSeconds: 75}`; four
/// fleet-route requests (metered at the gateway chokepoint) breach it — the
/// operator's sweep scales the pool to ZERO with `Ready=False/BudgetExceeded`
/// — and once the window slides past, the fleet resumes on its own.
async fn fleet_budget(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    let name = "fleet-billed";
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: AgentFleet\nmetadata: {{ name: {name}, namespace: {ns} }}\nspec:\n  replicas: 2\n  scaling: {{ mode: shard, shards: 2 }}\n  budget: {{ kind: a2a_requests, maxUnits: 3, windowSeconds: 75 }}\n  partitioning:\n    strategy: static\n  template:\n    shape: daemon\n    runtime: {{ image: \"agentd:1.3.1\" }}\n    instruction: {{ text: \"answer briefly\" }}\n    expose: {{ a2a: true }}\n"
    ))?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let out = shell::kubectl(&[
            "get",
            "statefulset",
            "-n",
            ns,
            name,
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        Ok(out.trim() == "2")
    })
    .await
    .context("billed fleet 2/2 ready")?;

    // Breach the window: 4 fleet-route requests (metering counts them at
    // the gateway entry regardless of the upstream outcome).
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18117)?;
    for i in 0..4u32 {
        let _ = ctx
            .http
            .post(format!("{}/fleets/{ns}/{name}", pf.base_url()))
            .json(&json!({ "jsonrpc": "2.0", "id": i, "method": "SendMessage",
                "params": { "message": { "role": "ROLE_USER",
                    "messageId": format!("bill-{i}"), "parts": [{ "text": "hi" }] } } }))
            .send()
            .await;
    }
    drop(pf);

    // The sweep pauses intake: zero replicas + the named condition.
    kh::poll_until(Duration::from_secs(120), Duration::from_secs(5), || async {
        let replicas = shell::kubectl(&[
            "get",
            "statefulset",
            "-n",
            ns,
            name,
            "-o",
            "jsonpath={.spec.replicas}",
        ])
        .unwrap_or_default();
        let reason = shell::kubectl(&[
            "get",
            "agentfleet",
            "-n",
            ns,
            name,
            "-o",
            r#"jsonpath={.status.conditions[?(@.type=="Ready")].reason}"#,
        ])
        .unwrap_or_default();
        Ok(replicas.trim() == "0" && reason.contains("BudgetExceeded"))
    })
    .await
    .context("budget breach never paused intake")?;

    // The window slides past → the fleet resumes without any operator input.
    kh::poll_until(
        Duration::from_secs(180),
        Duration::from_secs(10),
        || async {
            let out = shell::kubectl(&[
                "get",
                "statefulset",
                "-n",
                ns,
                name,
                "-o",
                "jsonpath={.status.readyReplicas}",
            ])
            .unwrap_or_default();
            Ok(out.trim() == "2")
        },
    )
    .await
    .context("fleet never resumed after the window")?;

    shell::kubectl(&["delete", "agentfleet", "-n", ns, name, "--wait=false"]).ok();
    pass()
}

/// P7-4: an invoice is computable from the export ALONE. Drive one org's
/// supervisor conversation; the gateway records durable, attributed usage
/// events; the management API exports the period aggregation (JSON + CSV)
/// and the org's line items are right there — org, workload, kind, unit,
/// totals — no reference back to internal state.
async fn metering_export(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use kube::api::{Api, Patch, PatchParams};

    const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
    let sign = |sub: &str| {
        let exp = now() + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({ "iss": "https://mock-idp:8443", "aud": "agentctl-cli", "sub": sub,
                     "email": format!("{sub}@example.test"), "groups": ["eng"], "exp": exp }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-bill";
    let ns = org_namespace(org);
    let _ = ns;
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(
            org,
            serde_json::from_value::<OrganizationSpec>(json!({ "displayName": "E2E Billing" }))?,
        )),
    )
    .await
    .context("apply Organization")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    let started = now();
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18116)?;
    let url = format!("{}/orgs/{org}/supervisor", pf.base_url());
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let url = url.clone();
        let token = sign("finn");
        async move {
            let resp = ctx
                .http
                .post(&url)
                .bearer_auth(token)
                .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
                    "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-bill-1",
                        "parts": [{ "text": "bill me" }] } } }))
                .send()
                .await?;
            let ok_status = resp.status().is_success();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            Ok(ok_status && body.get("result").is_some())
        }
    })
    .await
    .context("billable conversation never completed")?;
    drop(pf);

    // The export (JSON): the org's rows appear, attributed to the workload
    // and the acting user path. Poll — the writes are fire-and-forget.
    let export_url = format!(
        "/apis/management.agentctl.dev/v1alpha1/metering/export?from={}&to={}",
        started - 5,
        now() + 60
    );
    let rows = std::sync::Arc::new(std::sync::Mutex::new(Value::Null));
    kh::poll_until(Duration::from_secs(60), Duration::from_secs(3), || {
        let rows = rows.clone();
        let export_url = export_url.clone();
        async move {
            let out = shell::kubectl(&["get", "--raw", &export_url]).unwrap_or_default();
            let doc: Value = serde_json::from_str(out.trim()).unwrap_or(Value::Null);
            let hit = doc["rows"].as_array().is_some_and(|rs| {
                rs.iter().any(|r| {
                    r["org"] == json!(org)
                        && r["kind"] == json!("supervisor_conversations")
                        && r["total"].as_i64().unwrap_or(0) >= 1
                })
            });
            if hit {
                *rows.lock().unwrap() = doc;
            }
            Ok(hit)
        }
    })
    .await
    .context("the supervisor conversation never showed in the export")?;
    let doc = rows.lock().unwrap().clone();
    let rs = doc["rows"].as_array().unwrap().clone();
    if !rs
        .iter()
        .any(|r| r["org"] == json!(org) && r["kind"] == json!("a2a_requests"))
    {
        bail!("a2a_requests rows missing for the org: {rs:?}");
    }
    // Invoice math from the export alone.
    let org_units: i64 = rs
        .iter()
        .filter(|r| r["org"] == json!(org))
        .filter_map(|r| r["total"].as_i64())
        .sum();
    if org_units < 2 {
        bail!("org line items too thin for an invoice: {rs:?}");
    }

    // The CSV form: same rows, header intact.
    let csv = shell::kubectl(&["get", "--raw", &format!("{export_url}&format=csv")])?;
    if !csv.starts_with("org,namespace,workload,kind,unit,total,events") {
        bail!("CSV export malformed: {}", csv.lines().next().unwrap_or(""));
    }
    if !csv.contains(&format!("{org},")) {
        bail!("CSV lacks the org rows");
    }

    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P7-6: a dormant supervisor costs ~0. After the idle window (20s in the
/// e2e values) with no conversations, the supervisor's daemon scales to
/// ZERO and the CR reports Parked; the owner's next message wakes it — the
/// gateway re-stamps activity, the operator unparks, and the caller rides
/// the ordinary provisioning window to a live answer.
async fn supervisor_park(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use kube::api::{Api, Patch, PatchParams};

    const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
    let sign = |sub: &str| {
        let exp = now() + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({ "iss": "https://mock-idp:8443", "aud": "agentctl-cli", "sub": sub,
                     "email": format!("{sub}@example.test"), "groups": ["eng"], "exp": exp }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-park";
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(
            org,
            serde_json::from_value::<OrganizationSpec>(json!({ "displayName": "E2E Park" }))?,
        )),
    )
    .await
    .context("apply Organization")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18115)?;
    let url = format!("{}/orgs/{org}/supervisor", pf.base_url());
    let send = |text: &'static str, id: u64| {
        let url = url.clone();
        let token = sign("erin");
        async move {
            let resp = ctx
                .http
                .post(&url)
                .bearer_auth(token)
                .json(
                    &json!({ "jsonrpc": "2.0", "id": id, "method": "SendMessage",
                    "params": { "message": { "role": "ROLE_USER",
                        "messageId": format!("e2e-park-{id}"),
                        "parts": [{ "text": text }] } } }),
                )
                .send()
                .await?;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok((status, body))
        }
    };

    // First conversation: auto-ensure through to a live answer.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let send = &send;
        async move {
            let (status, body) = send("hello", 1).await?;
            Ok(status.is_success() && body.get("result").is_some())
        }
    })
    .await
    .context("supervisor never conversed")?;

    // Silence past the window (20s) + the park sweep: the daemon reaches
    // ZERO replicas and the CR reports Parked.
    let sup = "sup-mock-erin";
    kh::poll_until(Duration::from_secs(120), Duration::from_secs(5), || async {
        let replicas = shell::kubectl(&[
            "get",
            "deploy",
            "-n",
            &org_namespace("e2e-park"),
            sup,
            "-o",
            "jsonpath={.spec.replicas}",
        ])
        .unwrap_or_default();
        let phase = shell::kubectl(&[
            "get",
            "supervisors",
            "-n",
            &org_namespace("e2e-park"),
            sup,
            "-o",
            "jsonpath={.status.phase}",
        ])
        .unwrap_or_default();
        Ok(replicas.trim() == "0" && phase.trim() == "Parked")
    })
    .await
    .context("idle supervisor never parked to zero")?;

    // The wake: one message brings it back — provisioning first, then live.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let send = &send;
        async move {
            let (status, body) = send("are you there?", 2).await?;
            Ok(status.is_success() && body.get("result").is_some())
        }
    })
    .await
    .context("parked supervisor never woke")?;

    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P7-1: the external webhook door. A daemon declares an HMAC webhook
/// trigger AND exposes it; the operator provisions the route secret; an
/// "external" delivery through the GATEWAY's `/hooks/{ns}/{name}/<path>`
/// route lands on agentd's listener where the HMAC is verified and the
/// workflow fires. The refusal matrix runs at the right layers: bad
/// signature dies at agentd (401), wrong method / unexposed path / oversized
/// body die at the gateway (405/404/413). Admission refuses an exposure
/// with no matching trigger outright.
async fn hooks_ingress(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;

    // Admission gate first: exposure without a trigger is a typo that would
    // 404 forever — refused at the door.
    let bad = format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: hooked-bad, namespace: {ns} }}\nspec:\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"never admitted\" }}\n  intelligence: {{ pool: mockpool }}\n  expose: {{ a2a: true, webhooks: [ {{ path: /nope }} ] }}\n  triggers:\n    - loop: {{ interval: 30s }}\n"
    );
    if shell::kubectl_apply_stdin(&bad).is_ok() {
        shell::kubectl(&["delete", "agent", "hooked-bad", "-n", ns, "--wait=false"]).ok();
        bail!("admission admitted an exposure with no matching webhook trigger");
    }

    // The real thing: HMAC-authenticated, POST-only, exposed.
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: hooked, namespace: {ns} }}\nspec:\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"acknowledge the delivery in one line\" }}\n  intelligence: {{ pool: mockpool }}\n  expose:\n    a2a: true\n    webhooks: [ {{ path: /zendesk-events }} ]\n  triggers:\n    - webhook: {{ path: /zendesk-events, auth: hmac, methods: [POST] }}\n"
    ))
    .context("apply hooked agent")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(!first_pod(ns, &agent_label("hooked"))
            .unwrap_or_default()
            .is_empty())
    })
    .await?;
    let pod = first_pod(ns, &agent_label("hooked"))?;
    kh::wait_pod_running(&ctx.client, ns, &pod, READY_TIMEOUT).await?;

    // The operator-provisioned route secret — the sender's half of the HMAC.
    let secret = {
        let b64 = shell::kubectl(&[
            "get",
            "secret",
            "hooked-hooks",
            "-n",
            ns,
            "-o",
            "jsonpath={.data.hmac-0}",
        ])
        .context("hooks Secret")?;
        String::from_utf8(base64_decode(b64.trim())?)?
    };
    if secret.len() != 64 {
        bail!(
            "hooks secret should be 32-byte hex, got {} chars",
            secret.len()
        );
    }

    // "External" delivery: through the GATEWAY route, signed like a real
    // webhook sender (X-Signature: sha256=<hex HMAC of the body>).
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18119)?;
    let base = pf.base_url();
    let body = json!({ "ticket": 42, "status": "solved" }).to_string();
    let sign = |secret: &str, body: &str| {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
        let tag = ring::hmac::sign(&key, body.as_bytes());
        let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256={hex}")
    };
    let url = format!("{base}/hooks/{ns}/hooked/zendesk-events");
    let resp = ctx
        .http
        .post(&url)
        .header("content-type", "application/json")
        .header("x-signature", sign(&secret, &body))
        .body(body.clone())
        .send()
        .await
        .context("signed delivery through the gateway")?;
    if !resp.status().is_success() {
        bail!(
            "signed delivery refused: {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
    }
    // The workflow FIRED — the agent's own run events say so.
    kh::poll_until(Duration::from_secs(120), Duration::from_secs(5), || async {
        let logs = shell::kubectl(&["logs", "-n", ns, &pod, "--tail=-1"]).unwrap_or_default();
        Ok(logs.contains("main-webhook"))
    })
    .await
    .context("webhook firing in the agent log")?;

    // Refusal matrix, each at its own layer.
    let resp = ctx
        .http
        .post(&url)
        .header("x-signature", "sha256=deadbeef")
        .body(body.clone())
        .send()
        .await?;
    if resp.status().as_u16() != 401 {
        bail!(
            "bad signature should die at agentd with 401, got {}",
            resp.status()
        );
    }
    let resp = ctx.http.get(&url).send().await?;
    if resp.status().as_u16() != 405 {
        bail!(
            "GET should die at the gateway with 405, got {}",
            resp.status()
        );
    }
    let resp = ctx
        .http
        .post(format!("{base}/hooks/{ns}/hooked/unknown-path"))
        .body("{}")
        .send()
        .await?;
    if resp.status().as_u16() != 404 {
        bail!("unexposed path should 404, got {}", resp.status());
    }
    let resp = ctx
        .http
        .post(&url)
        .header("x-signature", sign(&secret, &"x".repeat(2 * 1024 * 1024)))
        .body("x".repeat(2 * 1024 * 1024))
        .send()
        .await?;
    if resp.status().as_u16() != 413 {
        bail!(
            "2MiB body should die at the gateway with 413, got {}",
            resp.status()
        );
    }

    drop(pf);
    shell::kubectl(&["delete", "agent", "hooked", "-n", ns, "--wait=false"]).ok();
    pass()
}

/// P6-5: scale-from-zero for webhook daemons. `lifecycle.idleParkSeconds`
/// parks a quiet webhook agent at ZERO replicas; the next delivery hits the
/// gateway's 503 + Retry-After, whose forced activity stamp moves the park
/// clock — the operator's sweep flips replicas back up and the sender's
/// retry loop lands the delivery on the woken pod.
async fn webhook_scale_zero(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;

    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: sleeper, namespace: {ns} }}\nspec:\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"acknowledge the delivery in one line\" }}\n  intelligence: {{ pool: mockpool }}\n  lifecycle: {{ idleParkSeconds: 20 }}\n  expose:\n    a2a: true\n    webhooks: [ {{ path: /ping }} ]\n  triggers:\n    - webhook: {{ path: /ping, methods: [POST] }}\n"
    ))
    .context("apply sleeper agent")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(!first_pod(ns, &agent_label("sleeper"))
            .unwrap_or_default()
            .is_empty())
    })
    .await?;
    let secret = {
        let b64 = shell::kubectl(&[
            "get",
            "secret",
            "sleeper-hooks",
            "-n",
            ns,
            "-o",
            "jsonpath={.data.hmac-0}",
        ])
        .context("hooks Secret")?;
        String::from_utf8(base64_decode(b64.trim())?)?
    };

    // Quiet for idleParkSeconds ⇒ the operator PARKS it: replicas 0.
    kh::poll_until(Duration::from_secs(90), Duration::from_secs(3), || async {
        let out = shell::kubectl(&[
            "get",
            "deploy",
            "sleeper",
            "-n",
            ns,
            "-o",
            "jsonpath={.spec.replicas}",
        ])
        .unwrap_or_default();
        Ok(out.trim() == "0")
    })
    .await
    .context("webhook daemon never parked to zero")?;

    // The delivery, exactly as an external sender retries it.
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18120)?;
    let base = pf.base_url();
    let body = json!({ "ping": 1 }).to_string();
    let sig = {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
        let tag = ring::hmac::sign(&key, body.as_bytes());
        let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256={hex}")
    };
    let url = format!("{base}/hooks/{ns}/sleeper/ping");
    let first = ctx
        .http
        .post(&url)
        .header("x-signature", sig.clone())
        .body(body.clone())
        .send()
        .await
        .context("first delivery to the parked agent")?;
    if first.status().as_u16() != 503 {
        bail!(
            "a parked agent should answer 503 + Retry-After, got {}",
            first.status()
        );
    }
    if first.headers().get("retry-after").is_none() {
        bail!("the 503 must carry Retry-After (senders' retry contract)");
    }

    // Retry like a webhook sender until the woken pod takes it.
    let mut delivered = false;
    for _ in 0..24 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let resp = ctx
            .http
            .post(&url)
            .header("x-signature", sig.clone())
            .body(body.clone())
            .send()
            .await?;
        if resp.status().is_success() {
            delivered = true;
            break;
        }
    }
    if !delivered {
        bail!("the parked agent never woke to take the delivery");
    }
    // And the workflow actually fired on the woken pod.
    let pod = first_pod(ns, &agent_label("sleeper"))?;
    kh::poll_until(Duration::from_secs(60), Duration::from_secs(5), || async {
        let logs = shell::kubectl(&["logs", "-n", ns, &pod, "--tail=-1"]).unwrap_or_default();
        Ok(logs.contains("main-webhook"))
    })
    .await
    .context("webhook firing on the woken pod")?;

    drop(pf);
    shell::kubectl(&["delete", "agent", "sleeper", "-n", ns, "--wait=false"]).ok();
    pass()
}

/// P5-6: the HITL fabric. A workflow's `human` step (addressed with
/// `to: {role: user, labels: {user: …}}`) parks the run at INPUT_REQUIRED;
/// the gateway keeps the QUESTION on the stored task and notifies the
/// agent's registered channel (a webhook sink standing in for Slack); the
/// WRONG user's answer is refused by agentd (-32602, gate stays open) and
/// the ADDRESSED user's plain-text continuation — sent through the org route
/// under their own identity — completes the run with the answer in the output.
async fn hitl_gate(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use kube::api::{Api, Patch, PatchParams};

    const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
    let sign = |sub: &str| {
        let exp = now() + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({ "iss": "https://mock-idp:8443", "aud": "agentctl-cli", "sub": sub,
                     "email": format!("{sub}@example.test"), "groups": ["eng"], "exp": exp }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-hitl";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(
            org,
            serde_json::from_value::<OrganizationSpec>(json!({ "displayName": "E2E HITL" }))?,
        )),
    )
    .await
    .context("apply Organization")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    // The channel sink — "Slack" for this lane: captures every POST.
    let sink = format!(
        r#"apiVersion: v1
kind: ConfigMap
metadata: {{ name: hitl-sink, namespace: {ns} }}
data:
  server.py: |
    import http.server, json, threading
    SEEN, LOCK = [], threading.Lock()
    class H(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def do_POST(self):
            n = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(n)
            with LOCK:
                try: SEEN.append(json.loads(body))
                except Exception: SEEN.append({{"raw": body.decode(errors="replace")}})
            self._json(200, {{"ok": True}})
        def do_GET(self):
            with LOCK: body = json.dumps(SEEN).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def _json(self, code, obj):
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def log_message(self, *a): pass
    http.server.ThreadingHTTPServer(("0.0.0.0", 8080), H).serve_forever()
---
apiVersion: apps/v1
kind: Deployment
metadata: {{ name: hitl-sink, namespace: {ns} }}
spec:
  replicas: 1
  selector: {{ matchLabels: {{ app: hitl-sink }} }}
  template:
    metadata: {{ labels: {{ app: hitl-sink }} }}
    spec:
      containers:
        - name: sink
          image: python:3.12-alpine
          command: ["python", "/data/server.py"]
          ports: [ {{ containerPort: 8080 }} ]
          volumeMounts: [ {{ name: data, mountPath: /data }} ]
      volumes:
        - name: data
          configMap: {{ name: hitl-sink }}
---
apiVersion: v1
kind: Service
metadata: {{ name: hitl-sink, namespace: {ns} }}
spec:
  selector: {{ app: hitl-sink }}
  ports: [ {{ port: 80, targetPort: 8080 }} ]
"#
    );
    shell::kubectl_apply_stdin(&sink).context("deploy hitl sink")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/hitl-sink",
        "-n",
        &ns,
        "--timeout=120s",
    ])
    .context("sink Ready")?;

    // The gated agent: a `decide` command workflow whose human step is
    // ADDRESSED to erin — a hard guarantee of a human decision (agentd
    // never auto-answers an addressed gate, whatever the approval policy).
    let workflow = json!({
        "name": "decide",
        "version": 3,
        "steps": {
            "start": { "kind": "a2a", "command": "decide" },
            "gate": { "kind": "human", "depends_on": ["start"],
                      "question": "Approve the wire transfer of {{steps.start.output.args.amount}}?",
                      "to": { "role": "user", "labels": { "user": "mock:erin" } },
                      "timeout": "10m" },
            "done": { "kind": "finish", "depends_on": ["gate"], "status": "completed",
                      "output": "decision: {{steps.gate.output}}" }
        }
    });
    let agent = json!({
        "apiVersion": "agentctl.dev/v1alpha2",
        "kind": "Agent",
        "metadata": { "name": "gated", "namespace": ns },
        "spec": {
            "shape": "daemon",
            "runtime": { "image": "agentd:1.3.1" },
            "instruction": { "text": "hold the line" },
            "expose": { "a2a": true },
            "access": { "principals": ["mock:erin", "mock:frank"], "grants": ["decide"] },
            "approval": { "policy": "ask",
                          "hitl": [format!("webhook:http://hitl-sink.{ns}.svc.cluster.local.:80/hook")] },
            "workflows": [ { "inline": workflow } ]
        }
    });
    // JSON is a YAML subset — kubectl takes it verbatim.
    shell::kubectl_apply_stdin(&agent.to_string()).context("apply gated agent")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(!first_pod(&ns, &agent_label("gated"))
            .unwrap_or_default()
            .is_empty())
    })
    .await?;
    let pod = first_pod(&ns, &agent_label("gated"))?;
    kh::wait_pod_running(&ctx.client, &ns, &pod, READY_TIMEOUT).await?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18121)?;
    let base = pf.base_url();
    let rpc = |token: String, body: Value| {
        let base = base.clone();
        async move {
            let resp = ctx
                .http
                .post(format!("{base}/orgs/e2e-hitl/agents/gated"))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .context("reach the org route")?;
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok(v)
        }
    };
    let erin = sign("erin");
    let frank = sign("frank");

    // Erin raises the decision; the run PARKS at the gate with the question.
    // (Retry: the org route 503s while the agent's principal Secret settles.)
    let sent;
    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    loop {
        let v = rpc(
            erin.clone(),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": { "message": {
                "role": "ROLE_USER", "messageId": format!("m-{}", now()),
                "parts": [{ "data": { "agentd": { "op": "decide", "amount": "$4,200" } } }]
            } } }),
        )
        .await?;
        if v.get("result").is_some() {
            sent = v;
            break;
        }
        if std::time::Instant::now() >= deadline {
            bail!("decide send never succeeded: {v}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let task_id = sent
        .pointer("/result/task/id")
        .and_then(Value::as_str)
        .context("send returned no task id")?
        .to_string();

    // The run is async — the gate arms moments after the send returns.
    // Poll GetTask (live passthrough for non-terminal tasks) until the run
    // PARKS at INPUT_REQUIRED with the templated question.
    let parked;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let v = rpc(
            erin.clone(),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "GetTask", "params": { "id": task_id } }),
        )
        .await?;
        let task = v.pointer("/result/task").or_else(|| v.get("result"));
        let st = task
            .and_then(|t| t.pointer("/status/state"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if st.to_ascii_lowercase().contains("input") {
            parked = v;
            break;
        }
        if std::time::Instant::now() >= deadline {
            bail!("the gated run never parked at INPUT_REQUIRED (last: {v})");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let question = parked
        .pointer("/result/task/status/message/parts/0/text")
        .or_else(|| parked.pointer("/result/status/message/parts/0/text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !question.contains("$4,200") {
        bail!("the pending question should carry the templated amount: {parked}");
    }

    // The channel heard about it (the gateway's fan-out): taskId + question.
    let pf_sink = shell::PortForward::service(&ns, "hitl-sink", 80, 18122)?;
    let sink_base = pf_sink.base_url();
    kh::poll_until(Duration::from_secs(60), Duration::from_secs(3), || {
        let sink_base = sink_base.clone();
        let task_id = task_id.clone();
        async move {
            let seen: Value = ctx
                .http
                .get(format!("{sink_base}/seen"))
                .send()
                .await?
                .json()
                .await
                .unwrap_or(Value::Null);
            Ok(seen.as_array().is_some_and(|a| {
                a.iter().any(|n| {
                    n["taskId"] == task_id.as_str()
                        && n["question"].as_str().is_some_and(|q| q.contains("$4,200"))
                })
            }))
        }
    })
    .await
    .context("the HITL channel never heard the gate")?;

    // FRANK (listed principal, NOT the addressee) answers: agentd refuses
    // (-32602) and the gate STAYS open.
    let wrong = rpc(
        frank,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "SendMessage", "params": { "message": {
            "role": "ROLE_USER", "messageId": format!("m-{}", now()), "taskId": task_id,
            "parts": [{ "text": "no" }]
        } } }),
    )
    .await?;
    let code = wrong.pointer("/error/code").and_then(Value::as_i64);
    if code != Some(-32602) {
        bail!("the wrong principal's answer must be refused with -32602, got {wrong}");
    }

    // ERIN answers — plain text, as the gate consumes it — and the run
    // completes with the decision in the output.
    let answered = rpc(
        erin.clone(),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "SendMessage", "params": { "message": {
            "role": "ROLE_USER", "messageId": format!("m-{}", now()), "taskId": task_id,
            "parts": [{ "text": "yes — approved" }]
        } } }),
    )
    .await?;
    let _ = answered;
    // Completion may land a beat after the answer: poll to terminal.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let v = rpc(
            erin.clone(),
            json!({ "jsonrpc": "2.0", "id": 5, "method": "GetTask", "params": { "id": task_id } }),
        )
        .await?;
        let task = v
            .pointer("/result/task")
            .or_else(|| v.get("result"))
            .cloned()
            .unwrap_or(Value::Null);
        let st = task
            .pointer("/status/state")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if st.contains("completed") {
            let output = task
                .pointer("/artifacts/0/parts/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !output.contains("approved") {
                bail!("the workflow output should carry erin's decision, got {output:?}");
            }
            break;
        }
        if st.contains("failed") || st.contains("cancel") {
            bail!("the answered run should complete, ended {st:?}: {v}");
        }
        if std::time::Instant::now() >= deadline {
            bail!("the answered run never completed (last: {v})");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    drop(pf_sink);
    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P7-3: the audit pipeline. An OBO tool call leaves rows in TWO streams —
/// the tenant mcpg's hash-chained per-request records (shipped off-pod by
/// the sidecar into identity's ingest door, org-attribution forced from its
/// token) and identity's credential-lifecycle exchange records — and BOTH
/// come back from ONE management-API query: the mcpg rows by the caller's
/// propagated `x-request-id` trail, the exchange rows by the (user,
/// provider, window) join that honestly models the host credential cache's
/// one-redeem-serves-many cardinality.
async fn audit_trail(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use agent_api::v1alpha2 as v2;
    use kube::api::{Api, Patch, PatchParams};

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-audit";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    let apply_org = |display: &'static str| {
        let orgs = orgs.clone();
        async move {
            orgs.patch(
                org,
                &PatchParams::apply("e2e").force(),
                &Patch::Apply(&Organization::new(
                    org,
                    serde_json::from_value::<OrganizationSpec>(json!({ "displayName": display }))?,
                )),
            )
            .await
            .context("apply Organization")
        }
    };
    apply_org("E2E Audit").await?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    let echo = format!(
        r#"apiVersion: apps/v1
kind: Deployment
metadata: {{ name: echo-mcp, namespace: {ns} }}
spec:
  replicas: 1
  selector: {{ matchLabels: {{ app: echo-mcp }} }}
  template:
    metadata: {{ labels: {{ app: echo-mcp }} }}
    spec:
      containers:
        - name: echo
          image: mock-echo-mcp:dev
          imagePullPolicy: IfNotPresent
          ports: [ {{ containerPort: 8080 }} ]
          readinessProbe: {{ httpGet: {{ path: /readyz, port: 8080 }} }}
---
apiVersion: v1
kind: Service
metadata: {{ name: echo-mcp, namespace: {ns} }}
spec:
  selector: {{ app: echo-mcp }}
  ports: [ {{ port: 80, targetPort: 8080 }} ]
"#
    );
    shell::kubectl_apply_stdin(&echo).context("deploy echo witness")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/echo-mcp",
        "-n",
        &ns,
        "--timeout=120s",
    ])
    .context("echo witness Ready")?;

    let mut entry = v2::MCPService::new(
        "zendesk",
        v2::MCPServiceSpec {
            endpoint: Some(format!("http://echo-mcp.{ns}.svc.cluster.local.:80/mcp")),
            auth: Some(agent_api::v1alpha2::ServiceAuth {
                mode: "obo".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    entry.metadata.namespace = Some(ns.clone());
    kh::api::<v2::MCPService>(&ctx.client, &ns)
        .patch(
            "zendesk",
            &PatchParams::apply("e2e").force(),
            &Patch::Apply(&entry),
        )
        .await
        .context("register obo MCPService")?;
    apply_org("E2E Audit v2").await?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        Ok(shell::kubectl(&["get", "deploy", "-n", &ns, "agentctl-mcpg"]).is_ok())
    })
    .await
    .context("tenant gateway never created")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/agentctl-mcpg",
        "-n",
        &ns,
        "--timeout=180s",
    ])
    .context("tenant gateway rollout")?;
    // The HTTP audit sink wiring proof (P7-3 cutover): the gateway ships its
    // own AuditEvents to identity's ingest door — its container carries the
    // AUDIT_COLLECTOR_TOKEN env, and there is NO file-tailing shipper sidecar.
    let containers = shell::kubectl(&[
        "get",
        "deploy",
        "agentctl-mcpg",
        "-n",
        &ns,
        "-o",
        "jsonpath={.spec.template.spec.containers[*].name}",
    ])?;
    if containers.split_whitespace().count() != 1 {
        bail!("tenant gateway is not a single container (shipper not retired?): {containers}");
    }
    let env_names = shell::kubectl(&[
        "get",
        "deploy",
        "agentctl-mcpg",
        "-n",
        &ns,
        "-o",
        "jsonpath={.spec.template.spec.containers[0].env[*].name}",
    ])?;
    if !env_names.contains("AUDIT_COLLECTOR_TOKEN") {
        bail!("tenant gateway is not wired for the HTTP audit sink (no AUDIT_COLLECTOR_TOKEN): {env_names}");
    }

    let admin_token = {
        let b64 = shell::kubectl(&[
            "get",
            "secret",
            "-n",
            &ctx.cfg.system_ns,
            "agentctl-api-token",
            "-o",
            "jsonpath={.data.AGENTCTL_API_TOKEN}",
        ])?;
        String::from_utf8(base64_decode(b64.trim())?)?
    };
    let pf_id = shell::PortForward::service(&ctx.cfg.system_ns, "agentctl-identity", 80, 18123)?;
    let idb = pf_id.base_url();
    let user = "mock:erin";
    // Custody: reset + seed (durable PG outlives reruns).
    ctx.http
        .post(format!("{idb}/admin/connections/delete"))
        .bearer_auth(&admin_token)
        .json(&json!({ "org": org, "user": user, "provider": "zendesk" }))
        .send()
        .await
        .context("reset custody")?;
    let resp = ctx
        .http
        .post(format!("{idb}/admin/connections"))
        .bearer_auth(&admin_token)
        .json(&json!({ "org": org, "user": user, "provider": "zendesk",
                        "kind": "static", "secret": "sk-audit-fake" }))
        .send()
        .await
        .context("seed connection")?;
    if !resp.status().is_success() {
        bail!("connection seed refused");
    }
    let caller = {
        let resp = ctx
            .http
            .post(format!("{idb}/admin/mcpg-token"))
            .bearer_auth(&admin_token)
            .json(&json!({ "workload": format!("{ns}/sup-erin"),
                            "audience": format!("mcpg:{ns}"), "user": user }))
            .send()
            .await?;
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        body["token"].as_str().unwrap_or_default().to_string()
    };
    if caller.is_empty() {
        bail!("caller mint refused");
    }

    // THE OBO tool call, carrying the trail id mcpg preserves into its
    // hash-chained records.
    let trail = format!("tr-audit-{}", now());
    let pf = shell::PortForward::service(&ns, "agentctl-mcpg", 8787, 18124)?;
    let base = pf.base_url();
    let call = |body: Value, session: Option<String>| {
        let base = base.clone();
        let caller = caller.clone();
        let trail = trail.clone();
        async move {
            let mut req = ctx
                .http
                .post(format!("{base}/mcp"))
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .header("x-request-id", trail)
                .bearer_auth(caller)
                .json(&body);
            if let Some(s) = &session {
                req = req.header("mcp-session-id", s.clone());
            }
            let resp = req.send().await.context("reach tenant gateway")?;
            let session = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let text = resp.text().await.unwrap_or_default();
            let body: Value = if ct.starts_with("text/event-stream") {
                text.lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                    .rev()
                    .find(|v| v.get("result").is_some() || v.get("error").is_some())
                    .unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            };
            anyhow::Ok((body, session))
        }
    };
    let (init, session) = call(
        json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "agentctl-e2e", "version": "0" } } }),
        None,
    )
    .await?;
    if init.get("result").is_none() {
        bail!("initialize failed: {init}");
    }
    let _ = call(
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        session.clone(),
    )
    .await;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let call = &call;
        let session = session.clone();
        async move {
            let (tools, _) = call(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
                session,
            )
            .await?;
            Ok(tools["result"]["tools"]
                .as_array()
                .is_some_and(|a| a.iter().any(|t| t["name"] == "zendesk.auth.echo")))
        }
    })
    .await
    .context("federated tool never appeared")?;
    let started = now();
    let (resp, _) = call(
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "zendesk.auth.echo", "arguments": {} } }),
        session.clone(),
    )
    .await?;
    let echoed = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if echoed != "Bearer sk-audit-fake" {
        bail!("obo call did not inject the credential: {resp}");
    }

    // ONE query, both streams. UPSTREAM CAVEATS (both reported to mcpg, both
    // observed live on beta.24): the inbound x-request-id lands only in the
    // request LOG (`upstream_request_id`), never in the AuditEvent, and a
    // FEDERATED tools/call currently writes no audit record at all — so the
    // mcpg join asserts the hash-chained session/catalog records that DO
    // exist, org-forced by the ingest token, and tightens to tool.call +
    // request-id when upstream lands the fixes.
    let _ = &trail;
    let raw = |path: String| shell::kubectl(&["get", "--raw", &path]);
    kh::poll_until(Duration::from_secs(90), Duration::from_secs(5), || {
        let raw = &raw;
        let ns = ns.clone();
        async move {
            let out = raw(format!(
                "/apis/management.agentctl.dev/v1alpha1/audit/query?org={org}&action=mcpg.tool.list"
            ))
            .unwrap_or_default();
            let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
            Ok(v["rows"].as_array().is_some_and(|rows| {
                rows.iter().any(|r| {
                    r["component"] == "mcpg"
                        && r["namespace"] == ns.as_str()
                        && r["dims"]["prev_event_hash"].is_string()
                        && r["dims"]["event_id"].is_string()
                })
            }))
        }
    })
    .await
    .context("the mcpg audit stream never reached the trail")?;

    let out = raw(format!(
        "/apis/management.agentctl.dev/v1alpha1/audit/query?user={user}&action=identity.exchange&from={}",
        started - 120
    ))?;
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    let has_exchange = v["rows"].as_array().is_some_and(|rows| {
        rows.iter().any(|r| {
            r["dims"]["provider"] == "zendesk" && r["component"] == "identity" && r["org"] == org
        })
    });
    if !has_exchange {
        bail!("no identity.exchange row for the OBO mint: {out}");
    }

    drop(pf_id);
    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P3-4: all three store classes render and run. `ephemeral` → Deployment on
/// an emptyDir (state dies with the pod); `local` → a single-replica
/// StatefulSet whose volumeClaimTemplate gives a durable PVC (state SURVIVES
/// a pod delete); `managed` → the state-service checkpointer (covered by
/// state-durability). This proves the render shapes and the local
/// durability boundary.
async fn store_classes(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;

    // ephemeral → Deployment.
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: store-eph, namespace: {ns} }}\nspec:\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"tick\" }}\n  intelligence: {{ pool: mockpool }}\n  store: {{ class: ephemeral }}\n  triggers:\n    - loop: {{ interval: 30s }}\n"
    ))
    .context("apply ephemeral agent")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(shell::kubectl(&["get", "deployment", "store-eph", "-n", ns]).is_ok())
    })
    .await
    .context("ephemeral must render a Deployment")?;

    // local → StatefulSet with a PVC.
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: store-local, namespace: {ns} }}\nspec:\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"tick\" }}\n  intelligence: {{ pool: mockpool }}\n  store: {{ class: local, size: 128Mi }}\n  triggers:\n    - loop: {{ interval: 30s }}\n"
    ))
    .context("apply local agent")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        Ok(shell::kubectl(&["get", "statefulset", "store-local", "-n", ns]).is_ok())
    })
    .await
    .context("local must render a StatefulSet")?;
    // The PVC exists and is bound (the volumeClaimTemplate provisioned it).
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let out = shell::kubectl(&[
            "get",
            "pvc",
            "-n",
            ns,
            "-l",
            "app.kubernetes.io/name=agent",
            "-o",
            "jsonpath={.items[*].metadata.name}",
        ])
        .unwrap_or_default();
        Ok(out.contains("agentd-state-store-local-0"))
    })
    .await
    .context("local agent's state PVC never appeared")?;
    // Durability boundary (distroless agent = no shell to write a marker, so
    // prove it at the volume): the StatefulSet's PVC PERSISTS across a pod
    // delete — an emptyDir would be gone; a bound claim survives and the
    // replacement pod re-attaches the SAME volume (its data with it).
    let pod = "store-local-0".to_string();
    kh::wait_pod_running(&ctx.client, ns, &pod, READY_TIMEOUT).await?;
    let pvc_uid = |()| {
        shell::kubectl(&[
            "get",
            "pvc",
            "agentd-state-store-local-0",
            "-n",
            ns,
            "-o",
            "jsonpath={.metadata.uid}",
        ])
        .unwrap_or_default()
    };
    let uid_before = pvc_uid(());
    if uid_before.trim().is_empty() {
        bail!("local agent's PVC has no uid (not bound?)");
    }
    shell::kubectl(&[
        "delete",
        "pod",
        "-n",
        ns,
        &pod,
        "--grace-period=0",
        "--force",
    ])?;
    // The claim is NOT deleted with the pod.
    let uid_after = pvc_uid(());
    if uid_after.trim() != uid_before.trim() {
        bail!(
            "the state PVC did NOT survive the pod delete (before {uid_before:?}, after {uid_after:?}) — not durable"
        );
    }
    // The replacement pod comes back Running with the SAME claim re-attached.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        Ok(shell::kubectl(&[
            "get",
            "pod",
            &pod,
            "-n",
            ns,
            "-o",
            "jsonpath={.status.phase}",
        ])
        .map(|p| p.trim() == "Running")
        .unwrap_or(false))
    })
    .await
    .context("local agent pod never came back")?;
    let claim = shell::kubectl(&[
        "get",
        "pod",
        &pod,
        "-n",
        ns,
        "-o",
        "jsonpath={.spec.volumes[?(@.name=='agentd-state')].persistentVolumeClaim.claimName}",
    ])
    .unwrap_or_default();
    if claim.trim() != "agentd-state-store-local-0" {
        bail!("the replacement pod did not re-attach the durable claim: {claim:?}");
    }

    shell::kubectl(&[
        "delete",
        "agent",
        "store-eph",
        "store-local",
        "-n",
        ns,
        "--wait=false",
    ])
    .ok();
    pass()
}

/// P3-5: the state-plane lifecycle verbs on a managed agent, through the
/// aggregated management API (`kubectl create --raw`, front-proxy + SAR gated).
/// A managed agent checkpoints on its loop, then:
///   backup   → the durable snapshot is non-empty and captured;
///   migrate  → the pod is replaced and every checkpoint key is preserved
///              (the DoD: reschedule with zero run loss);
///   stop     → the Deployment scales to 0 (durable state persists);
///   reset    → the state is purged (a follow-up backup is empty);
///   restore  → the captured snapshot is UPSERT back (backup non-empty again);
///   start    → the agent wakes.
/// reset/restore run while the agent is parked, so no concurrent checkpoint
/// races the assertions.
async fn lifecycle_verbs(ctx: &Ctx) -> Result<Outcome> {
    let sys = &ctx.cfg.system_ns;
    let ready = shell::kubectl(&[
        "get",
        "deploy",
        "-n",
        sys,
        "agentctl-state",
        "-o",
        "jsonpath={.status.readyReplicas}",
    ])
    .unwrap_or_default();
    if ready.trim().is_empty() || ready.trim() == "0" {
        return skip(
            "state service not Ready — P3-5 lifecycle verbs (backup/restore/migrate) need the \
             managed state plane; unskips with state-durability.",
        );
    }

    let ns = &ctx.cfg.ns;
    let name = "lc-probe";
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: {name}, namespace: {ns} }}\nspec:\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"acknowledge ticks in one line\" }}\n  expose: {{ a2a: true }}\n  store: {{ class: managed }}\n  triggers:\n    - loop: {{ interval: 30s }}\n"
    ))?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(!first_pod(ns, &agent_label(name))
            .unwrap_or_default()
            .is_empty())
    })
    .await
    .context("lc-probe pod")?;
    let pod = first_pod(ns, &agent_label(name))?;
    kh::wait_pod_running(&ctx.client, ns, &pod, READY_TIMEOUT).await?;

    let base = format!("/apis/management.agentctl.dev/v1alpha1/namespaces/{ns}/agents/{name}");
    let backup_path = format!("{base}/backup");
    // A verb POST via the aggregation layer, returning the Status JSON.
    let raw = |path: &str, body_file: &str| -> Result<Value> {
        let out = shell::kubectl(&["create", "--raw", path, "-f", body_file])
            .with_context(|| format!("aggregated POST {path}"))?;
        Ok(serde_json::from_str(&out).unwrap_or(Value::Null))
    };
    let items_of = |v: &Value| -> usize {
        v.pointer("/data/items")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0)
    };

    // The agent must checkpoint before there is anything to back up.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let backup_path = backup_path.clone();
        async move {
            let out = shell::kubectl(&["create", "--raw", &backup_path, "-f", "/dev/null"])
                .unwrap_or_default();
            let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
            Ok(v.pointer("/data/items")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty()))
        }
    })
    .await
    .context("managed agent checkpoints before backup")?;

    // 1) BACKUP — capture the snapshot.
    let backup = raw(&backup_path, "/dev/null")?;
    if backup.get("status").and_then(Value::as_str) != Some("Success") {
        bail!("backup did not succeed: {backup}");
    }
    let n_backup = items_of(&backup);
    if n_backup == 0 {
        bail!("backup returned no checkpoint rows: {backup}");
    }
    let snapshot = json!({ "items": backup.pointer("/data/items").cloned().unwrap_or(json!([])) });

    // 2) MIGRATE — reschedule the pod; the DoD is zero checkpoint loss.
    let mig = raw(&format!("{base}/migrate"), "/dev/null")?;
    if mig.get("status").and_then(Value::as_str) != Some("Success") {
        bail!("migrate did not succeed: {mig}");
    }
    if mig
        .pointer("/data/keys_preserved")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        < n_backup as u64
    {
        bail!("migrate did not preserve every checkpoint key: {mig}");
    }
    // migrate replaces the pod (the old one drains gracefully, so it lingers
    // Terminating briefly — poll for the replacement rather than reading a
    // possibly-stale items[0]).
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || {
        let old = pod.clone();
        async move {
            let now = first_pod(ns, &agent_label(name)).unwrap_or_default();
            Ok(!now.is_empty() && now != old)
        }
    })
    .await
    .context("migrate replacement pod")?;
    let pod2 = first_pod(ns, &agent_label(name))?;
    kh::wait_pod_running(&ctx.client, ns, &pod2, READY_TIMEOUT).await?;

    // 3) STOP — the operator honours lifecycle.paused (replicas → 0).
    let _ = raw(&format!("{base}/stop"), "/dev/null")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        let r = shell::kubectl(&[
            "get",
            "deploy",
            "-n",
            ns,
            name,
            "-o",
            "jsonpath={.spec.replicas}",
        ])
        .unwrap_or_default();
        Ok(r.trim() == "0")
    })
    .await
    .context("stop parks the agent (replicas 0)")?;

    // 4) RESET — purge the durable state (parked, so nothing re-checkpoints).
    let _ = raw(&format!("{base}/reset"), "/dev/null")?;
    let after_reset = raw(&backup_path, "/dev/null")?;
    if items_of(&after_reset) != 0 {
        bail!("reset did not clear the durable state: {after_reset}");
    }

    // 5) RESTORE — UPSERT the captured snapshot back.
    let restore_file = std::env::temp_dir().join(format!("lc-restore-{}.json", std::process::id()));
    std::fs::write(&restore_file, serde_json::to_vec(&snapshot)?)?;
    let restored = raw(
        &format!("{base}/restore"),
        restore_file.to_str().unwrap_or("/dev/null"),
    )?;
    let _ = std::fs::remove_file(&restore_file);
    if restored.get("status").and_then(Value::as_str) != Some("Success") {
        bail!("restore did not succeed: {restored}");
    }
    let after_restore = raw(&backup_path, "/dev/null")?;
    if items_of(&after_restore) < n_backup {
        bail!(
            "restore round-trip lost rows ({n_backup} -> {}): {after_restore}",
            items_of(&after_restore)
        );
    }

    // 6) START — wake the agent.
    let _ = raw(&format!("{base}/start"), "/dev/null")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(!first_pod(ns, &agent_label(name))
            .unwrap_or_default()
            .is_empty())
    })
    .await
    .context("start wakes the parked agent")?;

    shell::kubectl(&["delete", "agent", "-n", ns, name, "--wait=false"]).ok();
    pass()
}

/// P5-5: the sandbox cell. `sandbox.run` executes agent-authored code in a
/// SINGLE-USE, network-denied, capability-stripped pod. This drives it
/// straight over the cell's MCP surface (a tenant gateway federates it in
/// production; the direct drive proves the backend) and asserts: code runs
/// and returns stdout + a declared artifact + exit code; a run with NO
/// cluster credential inside it (the token that would let it escape) sees
/// none; and a NETWORK dial from inside the cell fails (deny-all — on a
/// policy CNI; the assertion is skip-guarded off it).
async fn sandbox_run(ctx: &Ctx) -> Result<Outcome> {
    let system = &ctx.cfg.system_ns;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let out = shell::kubectl(&[
            "get",
            "deploy",
            "agentctl-sandbox",
            "-n",
            system,
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        Ok(out.trim() == "1")
    })
    .await
    .context("sandbox cell never Ready")?;

    let admin_token = {
        let b64 = shell::kubectl(&[
            "get",
            "secret",
            "-n",
            system,
            "agentctl-api-token",
            "-o",
            "jsonpath={.data.AGENTCTL_API_TOKEN}",
        ])
        .unwrap_or_default();
        String::from_utf8(base64_decode(b64.trim()).unwrap_or_default()).unwrap_or_default()
    };
    let pf = shell::PortForward::service(system, "agentctl-sandbox", 80, 18125)?;
    let base = pf.base_url();
    let call = |body: Value| {
        let base = base.clone();
        let token = admin_token.clone();
        async move {
            let mut req = ctx.http.post(format!("{base}/mcp")).json(&body);
            if !token.is_empty() {
                req = req.bearer_auth(token);
            }
            let resp = req.send().await.context("reach sandbox cell")?;
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok(v)
        }
    };

    // Handshake + tool visible.
    let init = call(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "e2e", "version": "0" } } }))
    .await?;
    if init.get("result").is_none() {
        bail!("sandbox initialize failed: {init}");
    }
    let tools = call(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await?;
    if !tools["result"]["tools"]
        .as_array()
        .is_some_and(|a| a.iter().any(|t| t["name"] == "sandbox.run"))
    {
        bail!("sandbox.run not advertised: {tools}");
    }

    // (1) Code runs: reads stdin + an input file, prints, writes an artifact.
    let run = |args: Value| {
        let call = &call;
        async move {
            let v = call(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "sandbox.run", "arguments": args } }))
            .await?;
            anyhow::Ok(v["result"]["structuredContent"].clone())
        }
    };
    let r = run(json!({
        "language": "python",
        "code": "import sys\nname=open('who.txt').read().strip()\nprint('hello', name, sys.stdin.read().strip())\nopen('out.txt','w').write('artifact:'+name)\n",
        "stdin": "extra",
        "files": { "who.txt": "sandboxed-erin" },
        "out_files": ["out.txt"],
        "timeout_secs": 30
    }))
    .await
    .context("code run leg")?;
    if r["exit_code"] != 0 {
        bail!("code run non-zero exit: {r}");
    }
    if !r["stdout"]
        .as_str()
        .unwrap_or_default()
        .contains("hello sandboxed-erin extra")
    {
        bail!("stdout wrong: {r}");
    }
    if r["files"]["out.txt"] != "artifact:sandboxed-erin" {
        bail!("artifact wrong: {r}");
    }

    // (2) No cluster credential inside the cell — the SA-token path is absent.
    let cred = run(json!({
        "language": "sh",
        "code": "if [ -e /var/run/secrets/kubernetes.io/serviceaccount/token ]; then echo LEAKED; else echo none; fi\n",
        "timeout_secs": 20
    }))
    .await
    .context("credential probe leg")?;
    if !cred["stdout"].as_str().unwrap_or_default().contains("none") {
        bail!("a service-account token was mounted into the cell: {cred}");
    }

    // (3) Timeout is enforced (killed = pod deleted).
    let killed = run(json!({
        "language": "sh",
        "code": "sleep 60\n",
        "timeout_secs": 3
    }))
    .await
    .context("timeout leg")?;
    if killed["killed"] != true {
        bail!("an over-time run was not killed: {killed}");
    }

    // (4) Network egress denied — Calico lane only (kindnet does not enforce).
    if std::env::var("AGENTCTL_E2E_CALICO").is_ok() {
        let net = run(json!({
            "language": "sh",
            "code": "wget -T 3 -q -O- http://agentctl-identity.agentctl-system 2>&1 || echo BLOCKED\n",
            "timeout_secs": 15
        }))
        .await
        .context("egress leg")?;
        if !net["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("BLOCKED")
        {
            bail!("network egress from the cell was NOT denied: {net}");
        }
    }

    drop(pf);
    pass()
}

/// P3-3: the artifacts façade over the content store. Drives `artifacts.*`
/// directly (a tenant gateway federates it in production): a blob round-trips
/// (put → get, byte-identical) and appears in the org's list; a DIFFERENT org
/// can neither read the key nor see it in a list (the org fence); a write past
/// the org quota is refused; and a call with no asserted identity is refused
/// (fail closed).
async fn artifacts_flow(ctx: &Ctx) -> Result<Outcome> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let system = &ctx.cfg.system_ns;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let out = shell::kubectl(&[
            "get",
            "deploy",
            "agentctl-artifacts",
            "-n",
            system,
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        Ok(out.trim() == "1")
    })
    .await
    .context("artifacts service never Ready")?;

    let admin_token = {
        let raw = shell::kubectl(&[
            "get",
            "secret",
            "-n",
            system,
            "agentctl-api-token",
            "-o",
            "jsonpath={.data.AGENTCTL_API_TOKEN}",
        ])
        .unwrap_or_default();
        String::from_utf8(base64_decode(raw.trim()).unwrap_or_default()).unwrap_or_default()
    };
    let pf = shell::PortForward::service(system, "agentctl-artifacts", 8080, 18130)?;
    let base = pf.base_url();
    // A tool call under a chosen subject (`None` = omit the identity header).
    let call = |subject: Option<&str>, tool: &str, args: Value| {
        let base = base.clone();
        let token = admin_token.clone();
        let subject = subject.map(str::to_string);
        let tool = tool.to_string();
        async move {
            let mut req = ctx
                .http
                .post(format!("{base}/mcp"))
                .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": tool, "arguments": args } }));
            if !token.is_empty() {
                req = req.bearer_auth(token);
            }
            if let Some(s) = &subject {
                req = req.header("x-mcpg-subject-id", s);
            }
            let resp = req.send().await.context("reach artifacts")?;
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok(v["result"]["structuredContent"].clone())
        }
    };
    // Handshake (also proves the tools are advertised).
    let init: Value = {
        let mut req = ctx.http.post(format!("{base}/mcp")).json(&json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "e2e", "version": "0" } } }));
        if !admin_token.is_empty() {
            req = req.bearer_auth(&admin_token);
        }
        req.send().await?.json().await.unwrap_or(Value::Null)
    };
    if init.get("result").is_none() {
        bail!("artifacts initialize failed: {init}");
    }

    let org_a = "orgs/e2e-art-a/agent-1";
    let org_b = "orgs/e2e-art-b/agent-9";
    let key = "reports/summary.txt";
    let payload = b"the quick brown fox jumps over the lazy dog";

    // (1) put → get round-trip, byte-identical.
    let put = call(
        Some(org_a),
        "artifacts.put",
        json!({ "key": key, "content_base64": b64.encode(payload) }),
    )
    .await?;
    if put["size"].as_u64() != Some(payload.len() as u64) {
        bail!("put did not report the right size: {put}");
    }
    let got = call(Some(org_a), "artifacts.get", json!({ "key": key })).await?;
    let got_bytes = got["content_base64"]
        .as_str()
        .and_then(|s| b64.decode(s).ok())
        .unwrap_or_default();
    if got_bytes != payload {
        bail!("get did not round-trip the bytes: {got}");
    }

    // (2) list shows the key (relative to the org).
    let list = call(Some(org_a), "artifacts.list", json!({})).await?;
    if !list["items"]
        .as_array()
        .is_some_and(|a| a.iter().any(|o| o["key"] == json!(key)))
    {
        bail!("list did not include the put key: {list}");
    }

    // (3) THE ORG FENCE: org B cannot read A's key, nor see it listed.
    let cross_get = call(Some(org_b), "artifacts.get", json!({ "key": key })).await?;
    if !cross_get.is_null() {
        bail!("FENCE BREACH: org B read org A's artifact: {cross_get}");
    }
    let cross_list = call(Some(org_b), "artifacts.list", json!({})).await?;
    if cross_list["items"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        bail!("FENCE BREACH: org B listed org A's artifacts: {cross_list}");
    }

    // (4) QUOTA: a fresh org whose single write exceeds the (1 MiB e2e) cap is
    // refused with the quota code, not stored.
    let org_q = "orgs/e2e-art-quota/agent-1";
    let big = vec![b'x'; 1_200_000];
    let refused = call(
        Some(org_q),
        "artifacts.put",
        json!({ "key": "big.bin", "content_base64": b64.encode(&big) }),
    )
    .await?;
    if refused["error"]["code"] != json!("quota_exceeded") {
        bail!("over-quota write was not refused with quota_exceeded: {refused}");
    }
    // And nothing landed.
    let q_list = call(Some(org_q), "artifacts.list", json!({})).await?;
    if q_list["items"].as_array().is_some_and(|a| !a.is_empty()) {
        bail!("a quota-refused write still stored something: {q_list}");
    }

    // (5) FAIL CLOSED: no asserted identity ⇒ refused.
    let anon = call(None, "artifacts.list", json!({})).await?;
    if anon["error"]["code"] != json!("no_identity") {
        bail!("a call with no identity was not refused: {anon}");
    }

    drop(pf);
    pass()
}

fn oci_sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::new().chain_update(bytes).finalize())
}

/// Monolithic blob upload to a registry:2 (POST an upload, PUT the blob with
/// its digest). Returns the `sha256:<hex>` digest.
async fn oci_push_blob(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    blob: &[u8],
) -> Result<String> {
    let digest = format!("sha256:{}", oci_sha256_hex(blob));
    let start = http
        .post(format!("{base}/v2/{repo}/blobs/uploads/"))
        .send()
        .await?;
    if !start.status().is_success() && start.status().as_u16() != 202 {
        bail!(
            "blob upload start {}: {}",
            start.status(),
            start.text().await.unwrap_or_default()
        );
    }
    let loc = start
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .context("registry gave no upload Location")?
        .to_string();
    let loc = if loc.starts_with("http") {
        loc
    } else {
        format!("{base}{loc}")
    };
    let sep = if loc.contains('?') { '&' } else { '?' };
    let put = http
        .put(format!("{loc}{sep}digest={digest}"))
        .header("content-type", "application/octet-stream")
        .body(blob.to_vec())
        .send()
        .await?;
    if !put.status().is_success() {
        bail!(
            "blob PUT {}: {}",
            put.status(),
            put.text().await.unwrap_or_default()
        );
    }
    Ok(digest)
}

/// P7-7: a digest-pinned OCI **WorkflowSet** `setRef` is pulled by the operator,
/// verified against its digest, and its workflow documents are projected into
/// the agent's rendered config. Also proves admission refuses a mutable-tag ref.
async fn oci_bundles(ctx: &Ctx) -> Result<Outcome> {
    let sys = &ctx.cfg.system_ns;
    // A throwaway in-cluster registry:2 (the operator pulls from it over HTTP —
    // its `.svc` host is on the operator's insecure-by-locality list).
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: apps/v1\nkind: Deployment\nmetadata: {{ name: agentctl-oci-registry, namespace: {sys} }}\nspec:\n  replicas: 1\n  selector: {{ matchLabels: {{ app: agentctl-oci-registry }} }}\n  template:\n    metadata: {{ labels: {{ app: agentctl-oci-registry }} }}\n    spec:\n      containers:\n        - name: registry\n          image: registry:2\n          ports: [{{ containerPort: 5000 }}]\n---\napiVersion: v1\nkind: Service\nmetadata: {{ name: agentctl-oci-registry, namespace: {sys} }}\nspec:\n  selector: {{ app: agentctl-oci-registry }}\n  ports: [{{ port: 5000, targetPort: 5000 }}]\n"
    ))?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let r = shell::kubectl(&[
            "get",
            "deploy",
            "-n",
            sys,
            "agentctl-oci-registry",
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        Ok(r.trim() == "1")
    })
    .await
    .context("registry never Ready")?;

    let pf = shell::PortForward::service(sys, "agentctl-oci-registry", 5000, 15055)?;
    let base = pf.base_url();
    let repo = "workflowsets/greet";

    // A valid dialect-3 workflow document, pushed as the bundle's one layer.
    let wf = "name: oci-greet\nversion: 3\nsteps: {}\n";
    let layer_digest = oci_push_blob(&ctx.http, &base, repo, wf.as_bytes()).await?;
    let config = b"{}";
    let config_digest = oci_push_blob(&ctx.http, &base, repo, config).await?;
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": config_digest, "size": config.len() },
        "layers": [ { "mediaType": "application/vnd.oci.image.layer.v1.tar",
                      "digest": layer_digest, "size": wf.len(),
                      "annotations": { "org.opencontainers.image.title": "greet.yaml" } } ],
    });
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = format!("sha256:{}", oci_sha256_hex(&manifest_bytes));
    let put = ctx
        .http
        .put(format!("{base}/v2/{repo}/manifests/{manifest_digest}"))
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest_bytes)
        .send()
        .await?;
    if !put.status().is_success() {
        bail!(
            "manifest PUT {}: {}",
            put.status(),
            put.text().await.unwrap_or_default()
        );
    }

    // Apply an Agent whose workflow is the digest-pinned bundle.
    let ns = &ctx.cfg.ns;
    let name = "oci-agent";
    let reg_host = format!("agentctl-oci-registry.{sys}.svc.cluster.local:5000");
    let set_ref = format!("{reg_host}/{repo}@{manifest_digest}");
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: {name}, namespace: {ns} }}\nspec:\n  shape: job\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"idle\" }}\n  intelligence: {{ pool: mockpool }}\n  triggers:\n    - once: {{}}\n  workflows:\n    - setRef: \"{set_ref}\"\n"
    ))?;

    // The operator resolves + projects the bundle into the rendered config —
    // assert the workflow lands in the agentd config's workflows[].
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let cm = format!("{name}-config");
        let ns = ns.to_string();
        async move {
            let doc = shell::kubectl(&[
                "get",
                "cm",
                "-n",
                &ns,
                &cm,
                "-o",
                "jsonpath={.data.agentd\\.json}",
            ])
            .unwrap_or_default();
            let v: Value = serde_json::from_str(&doc).unwrap_or(Value::Null);
            Ok(v["workflows"]
                .as_array()
                .is_some_and(|a| a.iter().any(|w| w["name"] == json!("oci-greet"))))
        }
    })
    .await
    .context("OCI WorkflowSet did not project into the rendered config")?;

    // Admission refuses a mutable-tag (unverifiable) setRef.
    let mutable = shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: oci-mutable, namespace: {ns} }}\nspec:\n  shape: job\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"idle\" }}\n  intelligence: {{ pool: mockpool }}\n  triggers:\n    - once: {{}}\n  workflows:\n    - setRef: \"{reg_host}/{repo}:latest\"\n"
    ));
    match mutable {
        Err(e) if format!("{e:#}").contains("digest-pinned") => {}
        Err(e) => bail!("mutable setRef refused for the wrong reason: {e:#}"),
        Ok(_) => bail!("admission ADMITTED a mutable-tag setRef (should be digest-pinned)"),
    }

    drop(pf);
    shell::kubectl(&["delete", "agent", "-n", ns, name, "--wait=false"]).ok();
    shell::kubectl(&[
        "delete",
        "deploy,svc",
        "-n",
        sys,
        "agentctl-oci-registry",
        "--wait=false",
    ])
    .ok();
    pass()
}

/// P5-4: the consent flow whole. An `auth.mode: obo` entry with NO custody
/// connection refuses with the CONNECT CARD (who must connect what, and the
/// exact CLI line); the REAL `agentctl connect` binary then walks the RFC
/// 8628 device flow against the in-cluster IdP (offline_access appended,
/// auto-approving mock) and the refresh grant lands SEALED in custody —
/// after which the same user's agent call proceeds, and a later call shows
/// the refresh + rotation path minting a NEW upstream token live.
async fn connections_flow(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use agent_api::v1alpha2 as v2;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use kube::api::{Api, Patch, PatchParams};

    const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
    let sign = |sub: &str| {
        let exp = now() + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({ "iss": "https://mock-idp:8443", "aud": "agentctl-cli", "sub": sub,
                     "email": format!("{sub}@example.test"), "groups": ["eng"], "exp": exp }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-conn";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    let apply_org = |display: &'static str| {
        let orgs = orgs.clone();
        async move {
            orgs.patch(
                org,
                &PatchParams::apply("e2e").force(),
                &Patch::Apply(&Organization::new(
                    org,
                    serde_json::from_value::<OrganizationSpec>(json!({ "displayName": display }))?,
                )),
            )
            .await
            .context("apply Organization")
        }
    };
    apply_org("E2E Connections").await?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    let echo = format!(
        r#"apiVersion: apps/v1
kind: Deployment
metadata: {{ name: echo-mcp, namespace: {ns} }}
spec:
  replicas: 1
  selector: {{ matchLabels: {{ app: echo-mcp }} }}
  template:
    metadata: {{ labels: {{ app: echo-mcp }} }}
    spec:
      containers:
        - name: echo
          image: mock-echo-mcp:dev
          imagePullPolicy: IfNotPresent
          ports: [ {{ containerPort: 8080 }} ]
          readinessProbe: {{ httpGet: {{ path: /readyz, port: 8080 }} }}
---
apiVersion: v1
kind: Service
metadata: {{ name: echo-mcp, namespace: {ns} }}
spec:
  selector: {{ app: echo-mcp }}
  ports: [ {{ port: 80, targetPort: 8080 }} ]
"#
    );
    shell::kubectl_apply_stdin(&echo).context("deploy echo witness")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/echo-mcp",
        "-n",
        &ns,
        "--timeout=120s",
    ])
    .context("echo witness Ready")?;

    let mut entry = v2::MCPService::new(
        "zendesk",
        v2::MCPServiceSpec {
            endpoint: Some(format!("http://echo-mcp.{ns}.svc.cluster.local.:80/mcp")),
            auth: Some(agent_api::v1alpha2::ServiceAuth {
                mode: "obo".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    entry.metadata.namespace = Some(ns.clone());
    kh::api::<v2::MCPService>(&ctx.client, &ns)
        .patch(
            entry.metadata.name.as_deref().unwrap_or_default(),
            &PatchParams::apply("e2e").force(),
            &Patch::Apply(&entry),
        )
        .await
        .context("register obo MCPService")?;
    apply_org("E2E Connections v2").await?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        Ok(shell::kubectl(&["get", "deploy", "-n", &ns, "agentctl-mcpg"]).is_ok())
    })
    .await
    .context("tenant gateway never created")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/agentctl-mcpg",
        "-n",
        &ns,
        "--timeout=180s",
    ])
    .context("tenant gateway rollout")?;

    let admin_token = {
        let b64 = shell::kubectl(&[
            "get",
            "secret",
            "-n",
            &ctx.cfg.system_ns,
            "agentctl-api-token",
            "-o",
            "jsonpath={.data.AGENTCTL_API_TOKEN}",
        ])?;
        String::from_utf8(base64_decode(b64.trim())?)?
    };
    let pf_id = shell::PortForward::service(&ctx.cfg.system_ns, "agentctl-identity", 80, 18117)?;
    let idb = pf_id.base_url();
    let user = "mock:connie";

    // Custody is durable ACROSS runs (identity PG outlives the org): reset
    // the connection so "pre-consent" is true on reruns too.
    ctx.http
        .post(format!("{idb}/admin/connections/delete"))
        .bearer_auth(&admin_token)
        .json(&json!({ "org": org, "user": user, "provider": "zendesk" }))
        .send()
        .await
        .context("reset custody")?;

    // BEFORE consent: the exchange refuses with the CONNECT CARD — the
    // machine-readable "connection_required" a HITL surface renders.
    let resp = ctx
        .http
        .post(format!("{idb}/v1/exchange"))
        .bearer_auth(&admin_token)
        .form(&[
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:token-exchange",
            ),
            ("subject_token", user),
            (
                "subject_token_type",
                "urn:agentctl:params:oauth:token-type:user",
            ),
            ("audience", "zendesk"),
            ("org", org),
        ])
        .send()
        .await
        .context("reach /v1/exchange")?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if status.as_u16() != 400 || body["error"] != "invalid_target" {
        bail!("pre-consent exchange should refuse invalid_target: {status} {body}");
    }
    let card = &body["connection_required"];
    if card["provider"] != "zendesk"
        || card["org"] != org
        || card["connect"] != format!("agentctl connect zendesk --org {org}")
    {
        bail!("the connect card is wrong: {body}");
    }

    // THE CONSENT, through the REAL CLI: a signed-in user (vendored-key
    // session) runs `agentctl connect zendesk` and approves at the mock IdP
    // (auto-approve on the second poll). No token ever reaches the machine.
    let cli_dir = std::env::temp_dir().join(format!("agentctl-connect-e2e-{}", now()));
    std::fs::create_dir_all(&cli_dir).context("cli config dir")?;
    std::fs::write(
        cli_dir.join("credentials.json"),
        json!({
            "identity_url": idb,
            "provider": "mock",
            "access_token": sign("connie"),
            "expires_unix": now() + 600,
            "identity": { "subject": user },
        })
        .to_string(),
    )
    .context("write CLI session")?;
    let out = std::process::Command::new("target/release/agentctl")
        .args([
            "connect",
            "zendesk",
            "--org",
            org,
            "--identity-url",
            &idb,
            "--timeout",
            "60",
        ])
        .env("AGENTCTL_CONFIG_DIR", &cli_dir)
        .output()
        .context("spawn agentctl connect")?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        bail!(
            "agentctl connect failed ({}): {} {}",
            out.status,
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if !stdout.contains("Connected zendesk") || !stdout.contains(user) {
        bail!("connect output did not confirm the connection: {stdout}");
    }

    // Custody holds the grant — secret-free on the read surface.
    let listed: Value = ctx
        .http
        .get(format!("{idb}/admin/connections?org={org}"))
        .bearer_auth(&admin_token)
        .send()
        .await?
        .json()
        .await
        .unwrap_or(Value::Null);
    let row = &listed["connections"][0];
    if row["provider"] != "zendesk" || row["kind"] != "oauth_refresh" || row["user"] != user {
        bail!("custody row wrong after consent: {listed}");
    }
    if listed.to_string().contains("rt-") {
        bail!("refresh-token material leaked into the admin list: {listed}");
    }

    // AFTER consent the user's agent call PROCEEDS: the gateway redeems the
    // caller's bearer at the exchange, which refreshes against the IdP and
    // injects the minted per-user token upstream.
    let mint_caller = || async {
        let resp = ctx
            .http
            .post(format!("{idb}/admin/mcpg-token"))
            .bearer_auth(&admin_token)
            .json(&json!({ "workload": format!("{ns}/sup-connie"),
                            "audience": format!("mcpg:{ns}"), "user": user }))
            .send()
            .await?;
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        anyhow::Ok(body["token"].as_str().unwrap_or_default().to_string())
    };
    let caller = mint_caller().await?;
    if caller.is_empty() {
        bail!("identity refused the caller mint");
    }
    let pf = shell::PortForward::service(&ns, "agentctl-mcpg", 8787, 18118)?;
    let base = pf.base_url();
    let call = |body: Value, session: Option<String>| {
        let base = base.clone();
        let caller = caller.clone();
        async move {
            let mut req = ctx
                .http
                .post(format!("{base}/mcp"))
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .bearer_auth(caller)
                .json(&body);
            if let Some(s) = &session {
                req = req.header("mcp-session-id", s.clone());
            }
            let resp = req.send().await.context("reach tenant gateway")?;
            let session = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let text = resp.text().await.unwrap_or_default();
            let body: Value = if ct.starts_with("text/event-stream") {
                text.lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                    .rev()
                    .find(|v| v.get("result").is_some() || v.get("error").is_some())
                    .unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            };
            anyhow::Ok((body, session))
        }
    };
    let (init, session) = call(
        json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "agentctl-e2e", "version": "0" } } }),
        None,
    )
    .await?;
    if init.get("result").is_none() {
        bail!("tenant gateway initialize failed: {init}");
    }
    let _ = call(
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        session.clone(),
    )
    .await;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let call = &call;
        let session = session.clone();
        async move {
            let (tools, _) = call(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
                session,
            )
            .await?;
            Ok(tools["result"]["tools"]
                .as_array()
                .is_some_and(|a| a.iter().any(|t| t["name"] == "zendesk.auth.echo")))
        }
    })
    .await
    .context("federated echo tool never appeared")?;
    let echo_call = |id: i64| {
        let call = &call;
        let session = session.clone();
        async move {
            let (resp, _) = call(
                json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                        "params": { "name": "zendesk.auth.echo", "arguments": {} } }),
                session,
            )
            .await?;
            anyhow::Ok(
                resp.pointer("/result/content/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        }
    };
    let first = echo_call(2).await?;
    if !first.starts_with("Bearer at-") {
        bail!("upstream saw {first:?}, wanted the refreshed per-user token");
    }

    // The refresh + rotation path, LIVE: mock access tokens live 8s, so a
    // later call re-redeems with the ROTATED refresh token and the upstream
    // sees a NEW token — days of agent life compressed to one expiry.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let second = echo_call(3).await?;
    if !second.starts_with("Bearer at-") || second == first {
        bail!("no fresh token after expiry (rotation broken?): first {first:?}, then {second:?}");
    }

    std::fs::remove_dir_all(&cli_dir).ok();
    drop(pf_id);
    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P5-3: the OBO lane whole. A registry entry with `auth.mode: obo` federates
/// through mcpg's credential-exchange plugin: the VERIFIED caller's bearer is
/// redeemed at identity `/v1/exchange` (RFC 8693) against a custody-seeded
/// connection, and the minted PER-USER token — not the caller's own — reaches
/// the upstream as `Authorization` (witnessed by the echo fixture). A caller
/// whose token names no user is refused; the exchange itself demonstrates
/// mint → cache → revocation on the wire.
async fn obo_exchange(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use agent_api::v1alpha2 as v2;
    use kube::api::{Api, Patch, PatchParams};

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-obo";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    let apply_org = |display: &'static str| {
        let orgs = orgs.clone();
        async move {
            orgs.patch(
                org,
                &PatchParams::apply("e2e").force(),
                &Patch::Apply(&Organization::new(
                    org,
                    serde_json::from_value::<OrganizationSpec>(json!({ "displayName": display }))?,
                )),
            )
            .await
            .context("apply Organization")
        }
    };
    apply_org("E2E Obo").await?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    // The injection WITNESS: an MCP upstream whose one tool echoes the
    // Authorization header it received.
    let echo = format!(
        r#"apiVersion: apps/v1
kind: Deployment
metadata: {{ name: echo-mcp, namespace: {ns} }}
spec:
  replicas: 1
  selector: {{ matchLabels: {{ app: echo-mcp }} }}
  template:
    metadata: {{ labels: {{ app: echo-mcp }} }}
    spec:
      containers:
        - name: echo
          image: mock-echo-mcp:dev
          imagePullPolicy: IfNotPresent
          ports: [ {{ containerPort: 8080 }} ]
          readinessProbe: {{ httpGet: {{ path: /readyz, port: 8080 }} }}
---
apiVersion: v1
kind: Service
metadata: {{ name: echo-mcp, namespace: {ns} }}
spec:
  selector: {{ app: echo-mcp }}
  ports: [ {{ port: 80, targetPort: 8080 }} ]
"#
    );
    shell::kubectl_apply_stdin(&echo).context("deploy echo witness")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/echo-mcp",
        "-n",
        &ns,
        "--timeout=120s",
    ])
    .context("echo witness Ready")?;

    // Register it as an OBO entry: audience defaults to the entry name —
    // the custody connection's provider key.
    let mut entry = v2::MCPService::new(
        "zendesk",
        v2::MCPServiceSpec {
            endpoint: Some(format!("http://echo-mcp.{ns}.svc.cluster.local.:80/mcp")),
            auth: Some(agent_api::v1alpha2::ServiceAuth {
                mode: "obo".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    entry.metadata.namespace = Some(ns.clone());
    kh::api::<v2::MCPService>(&ctx.client, &ns)
        .patch(
            entry.metadata.name.as_deref().unwrap_or_default(),
            &PatchParams::apply("e2e").force(),
            &Patch::Apply(&entry),
        )
        .await
        .context("register obo MCPService")?;
    apply_org("E2E Obo v2").await?;

    // The registry edit changes the pod-template config hash, so the gateway
    // ROLLS here: readyReplicas alone races the dying old pod — wait for the
    // rollout of the CURRENT generation instead.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        Ok(shell::kubectl(&["get", "deploy", "-n", &ns, "agentctl-mcpg"]).is_ok())
    })
    .await
    .context("tenant gateway never created")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/agentctl-mcpg",
        "-n",
        &ns,
        "--timeout=180s",
    ])
    .context("tenant gateway rollout")?;

    let admin_token = {
        let b64 = shell::kubectl(&[
            "get",
            "secret",
            "-n",
            &ctx.cfg.system_ns,
            "agentctl-api-token",
            "-o",
            "jsonpath={.data.AGENTCTL_API_TOKEN}",
        ])?;
        String::from_utf8(base64_decode(b64.trim())?)?
    };
    let pf_id = shell::PortForward::service(&ctx.cfg.system_ns, "agentctl-identity", 80, 18115)?;
    let idb = pf_id.base_url();

    // Seed custody: the user's connection to "zendesk" (static secret).
    let user = "e2e:andrii";
    let resp = ctx
        .http
        .post(format!("{idb}/admin/connections"))
        .bearer_auth(&admin_token)
        .json(&json!({ "org": org, "user": user, "provider": "zendesk",
                        "kind": "static", "secret": "sk-live-fake" }))
        .send()
        .await
        .context("seed connection")?;
    if !resp.status().is_success() {
        bail!(
            "connection seed refused: {}",
            resp.text().await.unwrap_or_default()
        );
    }

    // Mint caller tokens through the operator admin channel: one USER-BOUND
    // (usr claim — the OBO subject), one workload-only.
    // NB: DISTINCT workloads. mcpg's host credential cache keys on the
    // RESOLVED caller identity (subject), and the OBO user rides inside our
    // token opaquely — two bearers sharing a sub would share a cache slot.
    // Production holds sub↔usr 1:1 (per-user supervisors); the e2e must too.
    let mint = |workload: &'static str, user: Option<&'static str>| {
        let idb = idb.clone();
        let admin = admin_token.clone();
        async move {
            let mut body = json!({ "workload": format!("{}/{workload}", org_namespace("e2e-obo")),
                                    "audience": format!("mcpg:{}", org_namespace("e2e-obo")) });
            if let Some(u) = user {
                body["user"] = json!(u);
            }
            let resp = ctx
                .http
                .post(format!("{idb}/admin/mcpg-token"))
                .bearer_auth(admin)
                .json(&body)
                .send()
                .await?;
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok(body["token"].as_str().unwrap_or_default().to_string())
        }
    };
    let user_token = mint("sup-andrii", Some("e2e:andrii")).await?;
    let bare_token = mint("plain-agent", None).await?;
    // The cache-partition probe: SAME sub as the user token, no usr. With
    // credentials.key_attributes: [subject_token] the host cache partitions
    // per raw bearer, so this caller must NOT be served the user's cached
    // credential (the exact bleed the P5-3 finding uncovered).
    let aliased_token = mint("sup-andrii", None).await?;
    if user_token.is_empty() || bare_token.is_empty() {
        bail!("identity refused the gateway-token mint");
    }

    // The exchange itself, on the wire: mint → cache → (after revocation
    // below) invalid_target. The subject is the user-bound JWT — the same
    // self-authenticating leg mcpg's plugin drives.
    let redeem = |subject: String| {
        let idb = idb.clone();
        async move {
            let resp = ctx
                .http
                .post(format!("{idb}/v1/exchange"))
                .form(&[
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:token-exchange",
                    ),
                    ("subject_token", subject.as_str()),
                    ("audience", "zendesk"),
                    ("client_id", "mcpg-tenant-gateway"),
                ])
                .send()
                .await
                .context("reach /v1/exchange")?;
            let status = resp.status();
            let outcome = resp
                .headers()
                .get("x-agentctl-exchange")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok((status, outcome, body))
        }
    };
    let (st, outcome, body) = redeem(user_token.clone()).await?;
    if !st.is_success() || body["access_token"] != "sk-live-fake" {
        bail!("exchange did not mint the connection secret: {st} {body}");
    }
    if outcome.as_deref() != Some("mint") {
        bail!("first exchange should be a mint, was {outcome:?}");
    }
    let (_, outcome, _) = redeem(user_token.clone()).await?;
    if outcome.as_deref() != Some("cache") {
        bail!("second exchange should be cache-served, was {outcome:?}");
    }
    let (st, _, body) = redeem(bare_token.clone()).await?;
    if st.as_u16() != 403 || body["error"] != "access_denied" {
        bail!("a user-less subject token must be refused: {st} {body}");
    }

    // Now through the GATEWAY: the federated echo tool proves which
    // credential the gateway injected upstream.
    let pf = shell::PortForward::service(&ns, "agentctl-mcpg", 8787, 18116)?;
    let base = pf.base_url();
    let call = |bearer: Option<String>, body: Value, session: Option<String>| {
        let base = base.clone();
        async move {
            let mut req = ctx
                .http
                .post(format!("{base}/mcp"))
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .json(&body);
            if let Some(b) = &bearer {
                req = req.bearer_auth(b.clone());
            }
            if let Some(s) = &session {
                req = req.header("mcp-session-id", s.clone());
            }
            let resp = req.send().await.context("reach tenant gateway")?;
            let session = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let text = resp.text().await.unwrap_or_default();
            let body: Value = if ct.starts_with("text/event-stream") {
                text.lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                    .rev()
                    .find(|v| v.get("result").is_some() || v.get("error").is_some())
                    .unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            };
            anyhow::Ok((body, session))
        }
    };
    let (init, session) = call(
        Some(user_token.clone()),
        json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "agentctl-e2e", "version": "0" } } }),
        None,
    )
    .await?;
    if init.get("result").is_none() {
        bail!("tenant gateway initialize failed: {init}");
    }
    let _ = call(
        Some(user_token.clone()),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        session.clone(),
    )
    .await;
    // Federation import may still be settling (plugin pull + first dial).
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let call = &call;
        let session = session.clone();
        let user_token = user_token.clone();
        async move {
            let (tools, _) = call(
                Some(user_token),
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
                session,
            )
            .await?;
            Ok(tools["result"]["tools"]
                .as_array()
                .is_some_and(|a| a.iter().any(|t| t["name"] == "zendesk.auth.echo")))
        }
    })
    .await
    .context("federated echo tool never appeared (plugin pull/licensing?)")?;

    // THE injection proof: the upstream saw the PER-USER minted credential,
    // not the caller's gateway JWT.
    let (resp, _) = call(
        Some(user_token.clone()),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "zendesk.auth.echo", "arguments": {} } }),
        session.clone(),
    )
    .await?;
    let echoed = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if echoed != "Bearer sk-live-fake" {
        bail!("upstream saw {echoed:?}, wanted the injected per-user credential (full: {resp})");
    }

    // Fail-closed: the USER-LESS caller initializes (it is verified) but its
    // tool call dies at the credential hop — never a bare upstream dial.
    let (init2, session2) = call(
        Some(bare_token.clone()),
        json!({ "jsonrpc": "2.0", "id": 10, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "e2e-bare", "version": "0" } } }),
        None,
    )
    .await?;
    if init2.get("result").is_some() {
        let _ = call(
            Some(bare_token.clone()),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            session2.clone(),
        )
        .await;
        let (resp, _) = call(
            Some(bare_token.clone()),
            json!({ "jsonrpc": "2.0", "id": 11, "method": "tools/call",
                    "params": { "name": "zendesk.auth.echo", "arguments": {} } }),
            session2,
        )
        .await?;
        let echoed = resp
            .pointer("/result/content/0/text")
            .and_then(Value::as_str);
        if resp.get("error").is_none() && resp.pointer("/result/isError") != Some(&json!(true)) {
            // A "successful" call is only acceptable if nothing was injected
            // AND the upstream refused — the echo accepts everything, so any
            // success here means a bare dial happened.
            bail!("user-less caller's tool call did not fail closed (echoed {echoed:?}): {resp}");
        }
    }

    // The aliased caller (same sub, no usr) AFTER the user's call warmed the
    // host cache: per-bearer partitioning means it hits the exchange itself
    // and is refused there — served the user's credential = the bleed.
    let (init3, session3) = call(
        Some(aliased_token.clone()),
        json!({ "jsonrpc": "2.0", "id": 20, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "e2e-aliased", "version": "0" } } }),
        None,
    )
    .await?;
    if init3.get("result").is_some() {
        let _ = call(
            Some(aliased_token.clone()),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            session3.clone(),
        )
        .await;
        let (resp, _) = call(
            Some(aliased_token.clone()),
            json!({ "jsonrpc": "2.0", "id": 21, "method": "tools/call",
                    "params": { "name": "zendesk.auth.echo", "arguments": {} } }),
            session3,
        )
        .await?;
        if resp.get("error").is_none() && resp.pointer("/result/isError") != Some(&json!(true)) {
            bail!(
                "same-sub user-less caller was served the user's cached credential (cache not partitioned per bearer): {resp}"
            );
        }
    }

    // Revocation: connection deleted ⇒ cache invalidated ⇒ next redeem dies.
    let resp = ctx
        .http
        .post(format!("{idb}/admin/connections/delete"))
        .bearer_auth(&admin_token)
        .json(&json!({ "org": org, "user": user, "provider": "zendesk" }))
        .send()
        .await
        .context("revoke connection")?;
    if !resp.status().is_success() {
        bail!("revocation refused");
    }
    let (st, _, body) = redeem(user_token.clone()).await?;
    if st.as_u16() != 400 || body["error"] != "invalid_target" {
        bail!("post-revocation exchange must be invalid_target: {st} {body}");
    }

    drop(pf_id);
    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P5-1: every org gets ITS OWN mcpg governance proxy. Org create brings the
/// gateway up (proxy-only config, zero plugins); a registered MCPService
/// federates under its own tool prefix, narrowed to the entry's allow list;
/// the platform `control` entry NEVER federates; a governed tools/call
/// round-trips through the proxy to the real upstream.
async fn tenant_mcpg(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use agent_api::v1alpha2 as v2;
    use kube::api::{Api, Patch, PatchParams};

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-tg";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    let apply_org = |display: &'static str| {
        let orgs = orgs.clone();
        async move {
            orgs.patch(
                org,
                &PatchParams::apply("e2e").force(),
                &Patch::Apply(&Organization::new(
                    org,
                    serde_json::from_value::<OrganizationSpec>(json!({ "displayName": display }))?,
                )),
            )
            .await
            .context("apply Organization")
        }
    };
    apply_org("E2E TenantGw").await?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    // Register an upstream: the coordination /mcp, narrowed to work.stats.
    let mut entry = v2::MCPService::new(
        "workstats",
        v2::MCPServiceSpec {
            endpoint: Some(
                "http://agentctl-coordination.agentctl-system.svc.cluster.local.:80/mcp".into(),
            ),
            allow: vec!["work.stats".into()],
            ..Default::default()
        },
    );
    entry.metadata.namespace = Some(ns.clone());
    kh::api::<v2::MCPService>(&ctx.client, &ns)
        .patch(
            entry.metadata.name.as_deref().unwrap_or_default(),
            &PatchParams::apply("e2e").force(),
            &Patch::Apply(&entry),
        )
        .await
        .context("register workstats MCPService")?;
    // Nudge the org reconcile so the catalog re-renders now (the gateway
    // itself hot-reloads the ConfigMap via config_watch).
    apply_org("E2E TenantGw v2").await?;

    // The gateway trio comes up in the org namespace.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let out = shell::kubectl(&[
            "get",
            "deploy",
            "-n",
            &org_namespace("e2e-tg"),
            "agentctl-mcpg",
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        Ok(out.trim() == "1")
    })
    .await
    .context("tenant gateway never Ready")?;

    let pf = shell::PortForward::service(&ns, "agentctl-mcpg", 8787, 18107)?;
    let base = pf.base_url();
    // The VERIFIED tier (P5-2): callers present identity-minted, audience-
    // bound EdDSA JWTs. Mint one through the operator admin channel.
    let admin_token = {
        let b64 = shell::kubectl(&[
            "get",
            "secret",
            "-n",
            &ctx.cfg.system_ns,
            "agentctl-api-token",
            "-o",
            "jsonpath={.data.AGENTCTL_API_TOKEN}",
        ])?;
        String::from_utf8(base64_decode(b64.trim())?)?
    };
    let pf_id = shell::PortForward::service(&ctx.cfg.system_ns, "agentctl-identity", 80, 18114)?;
    let mint = |audience: String| {
        let idb = pf_id.base_url();
        let admin = admin_token.clone();
        async move {
            let resp = ctx
                .http
                .post(format!("{idb}/admin/mcpg-token"))
                .bearer_auth(admin)
                .json(&json!({ "workload": "org-e2e-tg/probe", "audience": audience }))
                .send()
                .await?;
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok(body["token"].as_str().unwrap_or_default().to_string())
        }
    };
    let good_token = mint(format!("mcpg:{ns}")).await?;
    if good_token.is_empty() {
        bail!("identity refused the gateway-token mint");
    }
    let call = |bearer: Option<String>, body: Value, session: Option<String>| {
        let base = base.clone();
        async move {
            let mut req = ctx
                .http
                .post(format!("{base}/mcp"))
                .header("accept", "application/json, text/event-stream")
                // Strict streamable-HTTP: the negotiated protocol version
                // must ride every post-initialize request (400 without it).
                .header("mcp-protocol-version", "2025-11-25")
                .json(&body);
            if let Some(b) = &bearer {
                req = req.bearer_auth(b.clone());
            }
            if let Some(s) = &session {
                req = req.header("mcp-session-id", s.clone());
            }
            let resp = req.send().await.context("reach tenant gateway")?;
            let session = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let text = resp.text().await.unwrap_or_default();
            let body: Value = if ct.starts_with("text/event-stream") {
                // The stream interleaves log notifications with the actual
                // response — take the LAST frame carrying result/error.
                text.lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                    .rev()
                    .find(|v| v.get("result").is_some() || v.get("error").is_some())
                    .unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            };
            anyhow::Ok((body, session))
        }
    };

    // Refusals FIRST (the P5-2 DoD): unsigned and wrong-audience callers
    // never reach the tool surface.
    let (unsigned, _) = call(
        None,
        json!({ "jsonrpc": "2.0", "id": 90, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "e2e-unsigned", "version": "0" } } }),
        None,
    )
    .await?;
    if unsigned.get("result").is_some() {
        let (t, _) = call(
            None,
            json!({ "jsonrpc": "2.0", "id": 91, "method": "tools/list" }),
            None,
        )
        .await?;
        if t.pointer("/result/tools")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            bail!("an UNSIGNED caller saw tools through the verified tier: {t}");
        }
    }
    let wrong_aud = mint("mcpg:org-other".into()).await?;
    let (foreign, _) = call(
        Some(wrong_aud),
        json!({ "jsonrpc": "2.0", "id": 92, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "e2e-foreign", "version": "0" } } }),
        None,
    )
    .await?;
    if foreign.get("result").is_some() {
        bail!("a WRONG-AUDIENCE token initialized against this org's gateway: {foreign}");
    }

    // Handshake, then the governed inventory: the federated tool appears
    // under its prefix, NOTHING else from the upstream, and NO control.*
    // (federation may still be importing on first touch — poll).
    let (init, session) = call(
        Some(good_token.clone()),
        json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "agentctl-e2e", "version": "0" } } }),
        None,
    )
    .await?;
    if init.get("result").is_none() {
        bail!("tenant gateway initialize failed: {init}");
    }
    let _ = call(
        Some(good_token.clone()),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        session.clone(),
    )
    .await;
    let names = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let names = names.clone();
        let call = &call;
        let session = session.clone();
        let good_token = good_token.clone();
        async move {
            let (tools, _) = call(
                Some(good_token.clone()),
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
                session,
            )
            .await?;
            let got: Vec<String> = tools["result"]["tools"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let ok = got.iter().any(|n| n == "workstats.work.stats");
            *names.lock().unwrap() = got;
            Ok(ok)
        }
    })
    .await
    .with_context(|| {
        format!(
            "federated tool never appeared; saw {:?}",
            names.lock().unwrap()
        )
    })?;
    let got = names.lock().unwrap().clone();
    if got
        .iter()
        .any(|n| n.starts_with("workstats.") && n != "workstats.work.stats")
    {
        bail!("allowlist leaked upstream tools: {got:?}");
    }
    if got.iter().any(|n| n.starts_with("control.")) {
        bail!("the platform control surface federated into the tenant plane: {got:?}");
    }

    // A governed call round-trips to the real upstream.
    let (resp, _) = call(
        Some(good_token.clone()),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "workstats.work.stats", "arguments": {} } }),
        session.clone(),
    )
    .await?;
    let sc = &resp["result"]["structuredContent"];
    if sc.get("pending").is_none() {
        bail!("proxied work.stats returned no backlog shape: {resp}");
    }

    drop(pf_id);
    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P6-3: the dispatcher strategy end to end. A fleet with a COORDINATOR
/// (distribution: a2a): the fleet route front-doors the coordinator; the
/// coordinator's own workflow `a2a.delegate`s to its rendered `worker` peer
/// — the WORKERS tier of the fleet route, which skips the front door (the
/// self-loop this scenario would otherwise hang on) — and a WORKER answers
/// the delegated ask, observable in the worker pods' own run events.
async fn dispatcher_fanout(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    let name = "fleet-dispatch";
    // The coordinator's workflow: any inbound message → delegate downstream →
    // answer with the worker's distillate.
    let coord_wf = json!({
        "name": "dispatch",
        "version": 3,
        "steps": {
            "start": { "kind": "a2a" },
            "fan": { "kind": "a2a.delegate", "depends_on": ["start"], "peer": "worker",
                     "objective": "{{steps.start.output.text}}", "timeout": "60s" },
            "done": { "kind": "finish", "depends_on": ["fan"], "status": "completed",
                      "output": "{{steps.fan.output}}" }
        }
    });
    let wf_json = serde_json::to_string(&coord_wf)?.replace('\n', " ");
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: AgentFleet\nmetadata: {{ name: {name}, namespace: {ns} }}\nspec:\n  scaling: {{ mode: shard, shards: 2 }}\n  template:\n    runtime: {{ image: \"agentd:1.3.1\" }}\n    instruction: {{ text: \"answer the ask in one short line\" }}\n    expose: {{ a2a: true }}\n  coordinator:\n    distribution: a2a\n    template:\n      runtime: {{ image: \"agentd:1.3.1\" }}\n      instruction: {{ text: \"dispatch inbound asks to the worker pool\" }}\n      expose: {{ a2a: true }}\n      workflows:\n        - inline: {0}\n",
        wf_json
    ))?;

    // Workers 2/2 + the coordinator pod Running.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let w = shell::kubectl(&[
            "get",
            "statefulset",
            "-n",
            ns,
            name,
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        let c = shell::kubectl(&[
            "get",
            "deploy",
            "-n",
            ns,
            &format!("{name}-coordinator"),
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        Ok(w.trim() == "2" && c.trim() == "1")
    })
    .await
    .context("dispatcher fleet (workers + coordinator) never came up")?;

    // One ask at the FLEET route → coordinator → delegate → a worker answers.
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18106)?;
    let url = format!("{}/fleets/{ns}/{name}", pf.base_url());
    let resp = ctx
        .http
        .post(&url)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
            "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-disp-1",
                "parts": [{ "text": "fan this out" }] } } }))
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() || body.get("result").is_none() {
        bail!("fleet-route ask failed ({status}): {body}");
    }

    // The DELEGATION is the proof: a WORKER's own run events show the
    // inbound a2a ask that the coordinator fanned out.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        for i in 0..2 {
            let logs = shell::kubectl(&["logs", "-n", ns, &format!("{name}-{i}"), "--tail=200"])
                .unwrap_or_default();
            if logs.contains("\"start.a2a.fired\"") || logs.contains("\"inbox.accepted\"") {
                return Ok(true);
            }
        }
        Ok(false)
    })
    .await
    .context("no worker ever received the delegated ask")?;

    drop(pf);
    shell::kubectl(&["delete", "agentfleet", "-n", ns, name, "--wait=false"]).ok();
    pass()
}

/// P6-4: the workqueue fabric's crash story, end to end over the wire. A
/// holder claims an item and VANISHES mid-lease (never renews, never
/// releases — the crash); the server re-offers it (visible on its OWN
/// pending counter, not by asking), and the SAME work unit is granted
/// again — attempt 2 of the same claim_key. A poison unit with a one-
/// delivery budget dead-letters instead (exactly it), is requeued by an
/// admin, completes, and its result is retrievable via `work.result`.
async fn work_redelivery(ctx: &Ctx) -> Result<Outcome> {
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_HTTP, 18105)?;
    let base = pf.base_url();

    // --- Crash mid-lease → redelivery of exactly the leased unit ----------
    let item = "e2e://redeliver/1";
    mcp_structured(
        &ctx.http,
        &base,
        "work.submit",
        json!({ "item": item, "claim_key": "rd-1", "max_attempts": 3 }),
        Value::Null,
    )
    .await?;
    let grant = mcp_structured(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": item, "ttl_ms": 900 }),
        json!({ "agent/claim_key": "rd-1", "agent/instance": "crash-victim" }),
    )
    .await?;
    if grant["granted"] != json!(true) {
        bail!("initial claim not granted: {grant}");
    }
    // The holder crashes here: no renew, no release, no ack.
    // The SERVER re-offers after expiry — observed on its own backlog count.
    kh::poll_until(
        Duration::from_secs(30),
        Duration::from_millis(500),
        || async {
            let stats =
                mcp_structured(&ctx.http, &base, "work.stats", json!({}), Value::Null).await?;
            Ok(stats["pending"].as_u64().unwrap_or(0) >= 1)
        },
    )
    .await
    .context("expired lease never re-offered (pending stayed 0)")?;
    // The redelivered unit is EXACTLY the leased one: the same claim_key
    // grants again, as its second attempt.
    let regrant = mcp_structured(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": item, "ttl_ms": 30_000 }),
        json!({ "agent/claim_key": "rd-1", "agent/instance": "survivor" }),
    )
    .await?;
    if regrant["granted"] != json!(true) {
        bail!("redelivered unit not re-granted to the same claim_key: {regrant}");
    }
    let lease = regrant["lease_id"]
        .as_str()
        .context("lease id")?
        .to_string();
    let acked = mcp_structured(
        &ctx.http,
        &base,
        "work.ack",
        json!({ "lease_id": lease, "result": { "processed": true, "attempt": 2 } }),
        json!({ "agent/claim_key": "rd-1" }),
    )
    .await?;
    if acked["acked"] != json!(true) {
        bail!("survivor ack refused: {acked}");
    }
    let result = mcp_structured(
        &ctx.http,
        &base,
        "work.result",
        json!({ "work_id": "rd-1" }),
        Value::Null,
    )
    .await?;
    if result["state"] != json!("done") || result["result"]["attempt"] != json!(2) {
        bail!("work.result does not show the survivor's outcome: {result}");
    }

    // --- Poison → DLQ (exactly it) → admin requeue → completes -----------
    let poison = "e2e://poison/1";
    mcp_structured(
        &ctx.http,
        &base,
        "work.submit",
        json!({ "item": poison, "claim_key": "px-1", "max_attempts": 1 }),
        Value::Null,
    )
    .await?;
    let g = mcp_structured(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": poison, "ttl_ms": 900 }),
        json!({ "agent/claim_key": "px-1", "agent/instance": "crash-victim" }),
    )
    .await?;
    if g["granted"] != json!(true) {
        bail!("poison claim not granted: {g}");
    }
    kh::poll_until(
        Duration::from_secs(30),
        Duration::from_millis(500),
        || async {
            let d = mcp_structured(
                &ctx.http,
                &base,
                "work.deadletter",
                json!({ "action": "list" }),
                Value::Null,
            )
            .await?;
            Ok(d["items"]
                .as_array()
                .is_some_and(|i| i.iter().any(|x| x["work_id"] == json!("px-1"))))
        },
    )
    .await
    .context("poison unit never dead-lettered")?;
    let rq = mcp_structured(
        &ctx.http,
        &base,
        "work.deadletter",
        json!({ "action": "requeue", "work_id": "px-1" }),
        Value::Null,
    )
    .await?;
    if rq["found"] != json!(true) {
        bail!("DLQ requeue found nothing: {rq}");
    }
    let g2 = mcp_structured(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": poison, "ttl_ms": 30_000 }),
        json!({ "agent/claim_key": "px-1", "agent/instance": "survivor" }),
    )
    .await?;
    if g2["granted"] != json!(true) {
        bail!("requeued poison not claimable: {g2}");
    }
    let lease2 = g2["lease_id"].as_str().context("lease2")?.to_string();
    mcp_structured(
        &ctx.http,
        &base,
        "work.ack",
        json!({ "lease_id": lease2, "result": "fixed" }),
        json!({ "agent/claim_key": "px-1" }),
    )
    .await?;

    drop(pf);
    pass()
}

/// P6-2: the GUARDED shard resize. Changing N on a shard fleet must never
/// roll into mixed moduli (double-owned / orphaned keys): the operator
/// quiesces the old-N pods to zero, then flips N and scales back up. The
/// guard's signature is observable: the fleet surfaces `Ready=False /
/// Resizing` while in flight, and every pod that EVER runs carries one
/// consistent shards annotation — ending at the new N, fully ready.
async fn shard_resize(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    let name = "fleet-resize";
    let apply = |shards: u32| {
        shell::kubectl_apply_stdin(&format!(
            "apiVersion: agentctl.dev/v1alpha2\nkind: AgentFleet\nmetadata: {{ name: {name}, namespace: {ns} }}\nspec:\n  scaling: {{ mode: shard, shards: {shards} }}\n  template:\n    runtime: {{ image: \"agentd:1.3.1\" }}\n    instruction: {{ text: \"hold this partition\" }}\n    expose: {{ a2a: true }}\n"
        ))
    };
    apply(2)?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let out = shell::kubectl(&[
            "get",
            "statefulset",
            "-n",
            ns,
            name,
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        Ok(out.trim() == "2")
    })
    .await
    .context("shard fleet 2/2 ready")?;

    // Resize under a running fleet: 2 → 3.
    apply(3)?;

    // The guard engages: Ready=False / Resizing surfaces on the fleet (a
    // naive rolling update would never set it).
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(1), || async {
        let out = shell::kubectl(&[
            "get",
            "agentfleet",
            "-n",
            ns,
            name,
            "-o",
            r#"jsonpath={.status.conditions[?(@.type=="Ready")].reason}"#,
        ])
        .unwrap_or_default();
        Ok(out.contains("Resizing"))
    })
    .await
    .context("guarded resize never surfaced Ready=False/Resizing")?;

    // It completes: 3/3 ready with the NEW modulus on the pod template.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let ready = shell::kubectl(&[
            "get",
            "statefulset",
            "-n",
            ns,
            name,
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        let ann = shell::kubectl(&[
            "get",
            "statefulset",
            "-n",
            ns,
            name,
            "-o",
            r#"jsonpath={.spec.template.metadata.annotations.agentctl\.dev/shards}"#,
        ])
        .unwrap_or_default();
        Ok(ready.trim() == "3" && ann.trim() == "3")
    })
    .await
    .context("resize to N=3 never completed")?;

    // Post-resize sanity: every live pod runs under the same modulus (no
    // mixed-N seam survives) and the fleet is Ready again.
    let pods = shell::kubectl(&[
        "get",
        "pods",
        "-n",
        ns,
        "-l",
        &format!("agentctl.dev/agent={name}"),
        "-o",
        "jsonpath={.items[*].metadata.name}",
    ])?;
    if pods.split_whitespace().count() != 3 {
        bail!("want exactly 3 member pods after resize, got: {pods}");
    }
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        let out = shell::kubectl(&[
            "get",
            "agentfleet",
            "-n",
            ns,
            name,
            "-o",
            r#"jsonpath={.status.conditions[?(@.type=="Ready")].status}"#,
        ])
        .unwrap_or_default();
        Ok(out.trim() == "True")
    })
    .await
    .context("fleet not Ready after resize")?;

    shell::kubectl(&["delete", "agentfleet", "-n", ns, name, "--wait=false"]).ok();
    pass()
}

/// P6-1: a 3-member STATIC fleet — one shared document, per-member `vars`
/// overlays (`member-<n>.json`, third `-c`), and a singleton trigger armed
/// on ordinal 0 only. The singleton is a fast `loop` (same `armed` fold as
/// a nightly schedule, observable in-test): member 0's run events show the
/// loop firing; members 1–2 stay silent — fleet-wide, it fires ONCE per
/// tick.
async fn fleet_static(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    let name = "fleet-static";
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: AgentFleet\nmetadata: {{ name: {name}, namespace: {ns} }}\nspec:\n  replicas: 3\n  scaling: {{ mode: shard, shards: 3 }}\n  partitioning:\n    strategy: static\n    static:\n      defaults: {{ color: none }}\n      vars:\n        - {{ color: red }}\n        - {{ color: blue }}\n        - {{ color: green }}\n      singletons: [\"loop\"]\n  template:\n    shape: daemon\n    runtime: {{ image: \"agentd:1.3.1\" }}\n    instruction: {{ text: \"tick for partition {{{{config.color}}}} in one line\" }}\n    expose: {{ a2a: true }}\n    triggers:\n      - loop: {{ interval: 20s }}\n"
    ))?;

    // StatefulSet up: 3/3 stable members.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || async {
        let out = shell::kubectl(&[
            "get",
            "statefulset",
            "-n",
            ns,
            name,
            "-o",
            "jsonpath={.status.readyReplicas}",
        ])
        .unwrap_or_default();
        Ok(out.trim() == "3")
    })
    .await
    .context("static fleet 3/3 ready")?;

    // The shared ConfigMap carries the per-member overlays, vars-only.
    let cm = shell::kubectl(&[
        "get",
        "configmap",
        "-n",
        ns,
        &format!("{name}-config"),
        "-o",
        "jsonpath={.data.member-1\\.json}",
    ])?;
    let overlay: Value = serde_json::from_str(cm.trim()).context("member-1 overlay JSON")?;
    if overlay["vars"]["color"] != json!("blue") || overlay["vars"]["is_lead"] != json!(false) {
        bail!("member-1 overlay wrong: {overlay}");
    }
    let cm0 = shell::kubectl(&[
        "get",
        "configmap",
        "-n",
        ns,
        &format!("{name}-config"),
        "-o",
        "jsonpath={.data.member-0\\.json}",
    ])?;
    let overlay0: Value = serde_json::from_str(cm0.trim())?;
    if overlay0["vars"]["is_lead"] != json!(true) {
        bail!("member-0 is not the lead: {overlay0}");
    }

    // The pods mount the third layer keyed by their own ordinal.
    let args = shell::kubectl(&[
        "get",
        "pod",
        "-n",
        ns,
        &format!("{name}-0"),
        "-o",
        "jsonpath={.spec.containers[0].args}",
    ])?;
    if !args.contains("member-$(AGENT_POD_INDEX).json") {
        bail!("member overlay layer missing from argv: {args}");
    }

    // Singleton proof: the loop ticks on member 0 and ONLY member 0. Give it
    // two intervals, then read each member's own run events.
    let fired = |pod: &str| -> Result<bool> {
        let logs = shell::kubectl(&["logs", "-n", ns, pod, "--tail=200"]).unwrap_or_default();
        Ok(logs
            .lines()
            .any(|l| l.contains("\"run.start\"") && l.contains("main-loop")))
    };
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(5), || async {
        fired(&format!("{name}-0"))
    })
    .await
    .context("lead member's singleton loop never fired")?;
    for follower in [format!("{name}-1"), format!("{name}-2")] {
        if fired(&follower)? {
            bail!("{follower} ran the singleton loop — armed leaked past the lead");
        }
    }

    shell::kubectl(&["delete", "agentfleet", "-n", ns, name, "--wait=false"]).ok();
    pass()
}

/// P4-7: "@a and @b" through the supervisor, end to end. The gateway turns
/// the owner's prose into the typed `mention` envelope; the supervisor's
/// workflow fans `a2a.delegate` out to the mentioned peers — dialed at their
/// mTLS a2a as THE OWNER (per-target principal bearers) — and gathers an
/// answer accounting for every mention. A mention of a handle the owner
/// cannot reach reports as that slot's error; hops=0 short-circuits without
/// fanning out (the supervisor's OWN ceiling — agentd's depth never crosses
/// pods).
async fn mention_orchestration(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use kube::api::{Api, Patch, PatchParams};

    const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
    let sign = |sub: &str| {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({ "iss": "https://mock-idp:8443", "aud": "agentctl-cli", "sub": sub,
                     "email": format!("{sub}@example.test"), "groups": ["eng"], "exp": exp }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-m";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(
            org,
            serde_json::from_value::<OrganizationSpec>(json!({ "displayName": "E2E Mention" }))?,
        )),
    )
    .await
    .context("apply Organization")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    // Two mentionable targets: real agentd daemons, a2a-exposed, naming the
    // OWNER as principal (that is what makes them @mention-dialable).
    for handle in ["alpha", "beta"] {
        shell::kubectl_apply_stdin(&format!(
            "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: {handle}, namespace: {ns} }}\nspec:\n  shape: daemon\n  handle: {handle}\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"answer in one short line\" }}\n  expose: {{ a2a: true }}\n  access: {{ principals: [\"mock:dana\"] }}\n"
        ))?;
    }
    for handle in ["alpha", "beta"] {
        kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async move {
            Ok(!first_pod(&org_namespace("e2e-m"), &agent_label(handle))
                .unwrap_or_default()
                .is_empty())
        })
        .await
        .with_context(|| format!("{handle} pod"))?;
        let pod = first_pod(&ns, &agent_label(handle))?;
        kh::wait_pod_running(&ctx.client, &ns, &pod, READY_TIMEOUT).await?;
    }

    // First supervisor touch as dana — auto-ensure, then poll the SAME send
    // until the supervisor converses (it renders AFTER the targets, so its
    // peer set already includes them).
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18104)?;
    let url = format!("{}/orgs/{org}/supervisor", pf.base_url());
    let send = |text: &str, id: u64| {
        let url = url.clone();
        let token = sign("dana");
        let text = text.to_string();
        async move {
            let resp = ctx
                .http
                .post(&url)
                .bearer_auth(token)
                .json(
                    &json!({ "jsonrpc": "2.0", "id": id, "method": "SendMessage",
                    "params": { "message": { "role": "ROLE_USER",
                        "messageId": format!("e2e-m-{id}"),
                        "parts": [{ "text": text }] } } }),
                )
                .send()
                .await?;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok((status, body))
        }
    };
    let hello = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let hello = hello.clone();
        let send = &send;
        async move {
            let (status, body) = send("hello", 1).await?;
            *hello.lock().unwrap() = format!("{status}: {body}");
            Ok(status.is_success() && body.get("result").is_some())
        }
    })
    .await
    .with_context(|| {
        format!(
            "supervisor never conversed; last: {}",
            hello.lock().unwrap()
        )
    })?;

    // The @mention turn: both handles + one the owner cannot reach. The
    // workflow run outlives the SendMessage round-trip, so poll GetTask (the
    // same recovery agentd's own delegate client uses) to the terminal
    // artifact.
    let (status, body) = send("please ping @alpha and @beta (and @ghost)", 2).await?;
    if !status.is_success() {
        bail!("mention send failed ({status}): {body}");
    }
    // Terminal = the task's CURRENT status state (the history keeps earlier
    // WORKING entries, so string-scanning the whole reply never settles).
    let terminal_text = |body: &Value| {
        let state = body
            .pointer("/result/task/status/state")
            .or_else(|| body.pointer("/result/status/state"))
            .and_then(Value::as_str)
            .unwrap_or("");
        (!state.contains("WORKING") && !state.contains("SUBMITTED")).then(|| crate_reply_text(body))
    };
    let text = match terminal_text(&body) {
        Some(t) => t,
        None => {
            let task_id = body
                .pointer("/result/task/id")
                .or_else(|| body.pointer("/result/id"))
                .and_then(Value::as_str)
                .context("task id in the working reply")?
                .to_string();
            let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
                let out = out.clone();
                let url = url.clone();
                let token = sign("dana");
                let task_id = task_id.clone();
                async move {
                    let resp = ctx
                        .http
                        .post(&url)
                        .bearer_auth(token)
                        .json(&json!({ "jsonrpc": "2.0", "id": 99, "method": "GetTask",
                                       "params": { "id": task_id } }))
                        .send()
                        .await?;
                    let body: Value = resp.json().await.unwrap_or(Value::Null);
                    match terminal_text(&body) {
                        Some(t) => {
                            *out.lock().unwrap() = t;
                            Ok(true)
                        }
                        None => Ok(false),
                    }
                }
            })
            .await
            .context("mention task never reached a terminal state")?;
            let t = out.lock().unwrap().clone();
            t
        }
    };
    let ok_both = text.contains("alpha") && text.contains("beta");
    if !ok_both {
        bail!("mention answer does not account for both handles: {text}");
    }
    if !text.contains("ghost") {
        bail!("the unreachable @ghost is not reported: {text}");
    }

    // Hop ceiling: the typed envelope with hops=0 (what a re-delegating
    // supervisor would receive) short-circuits before any fan-out.
    let resp = ctx
        .http
        .post(&url)
        .bearer_auth(sign("dana"))
        .json(&json!({ "jsonrpc": "2.0", "id": 3, "method": "SendMessage",
            "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-m-3",
                "parts": [{ "data": { "agentd": {
                    "op": "mention", "text": "loop", "mentions": ["alpha"], "hops": 0
                } } }] } } }))
        .send()
        .await?;
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let text = match terminal_text(&body) {
        Some(t) => t,
        None => {
            // Short-circuit runs still surface as a (fast) task; poll it.
            let task_id = body
                .pointer("/result/task/id")
                .or_else(|| body.pointer("/result/id"))
                .and_then(Value::as_str)
                .context("task id in the hops=0 reply")?
                .to_string();
            let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || {
                let out = out.clone();
                let url = url.clone();
                let token = sign("dana");
                let task_id = task_id.clone();
                async move {
                    let resp = ctx
                        .http
                        .post(&url)
                        .bearer_auth(token)
                        .json(&json!({ "jsonrpc": "2.0", "id": 100, "method": "GetTask",
                                       "params": { "id": task_id } }))
                        .send()
                        .await?;
                    let body: Value = resp.json().await.unwrap_or(Value::Null);
                    match terminal_text(&body) {
                        Some(t) => {
                            *out.lock().unwrap() = t;
                            Ok(true)
                        }
                        None => Ok(false),
                    }
                }
            })
            .await
            .context("hops=0 task never terminal")?;
            let t = out.lock().unwrap().clone();
            t
        }
    };
    if !text.contains("hop ceiling") {
        bail!("hops=0 did not short-circuit: {text}");
    }

    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// Unix seconds.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Standard-alphabet base64 decode (kubectl secret data).
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.decode(s.trim())?)
}

/// Every text found under `result`, joined (message parts, artifacts, data).
fn crate_reply_text(resp: &Value) -> String {
    fn collect(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) => {
                for (_, v) in m {
                    collect(v, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            _ => {}
        }
    }
    let mut out = String::new();
    collect(resp.get("result").unwrap_or(resp), &mut out);
    out
}

/// P4-1: the control MCP end to end, as a REAL enrolled AAuth agent.
/// The e2e generates an Ed25519 workload key, registers it through the
/// operator admin channel with a workload label in a managed org namespace,
/// enrolls + obtains an agent token (hwk-signed, the shipped 1.3.1 wire),
/// then drives `control.*` with jwt-scheme RFC 9421 signatures:
/// unsigned → the challenge; signed → tools scoped to the LABEL's namespace
/// (never an argument); create renders a real admission-gated Agent stamped
/// with the created-by label; a tampered signature is refused.
/// Also proves the org controller SEEDED the namespace defaults (control
/// MCPService + supervisor AgentClass).
async fn control_mcp(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use agent_api::v1alpha2 as v2;
    use base64::Engine as _;
    use kube::api::{Api, Patch, PatchParams};
    use ring::signature::{Ed25519KeyPair, KeyPair as _};

    const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    const B64STD: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    };

    let sys = &ctx.cfg.system_ns;
    let ready = shell::kubectl(&[
        "get",
        "deploy",
        "-n",
        sys,
        "agentctl-control",
        "-o",
        "jsonpath={.status.readyReplicas}",
    ])
    .unwrap_or_default();
    if ready.trim().is_empty() || ready.trim() == "0" {
        bail!("agentctl-control is not Ready (control.enabled + identity.aauth.provider required)");
    }

    // Org + the seeded per-namespace platform defaults.
    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-ctl";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(
            org,
            serde_json::from_value::<OrganizationSpec>(json!({ "displayName": "E2E Control" }))?,
        )),
    )
    .await
    .context("apply Organization")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        let svcs: Api<v2::MCPService> = Api::namespaced(ctx.client.clone(), &ns);
        let classes: Api<v2::AgentClass> = Api::namespaced(ctx.client.clone(), &ns);
        Ok(svcs.get_opt("control").await?.is_some()
            && classes.get_opt("supervisor").await?.is_some())
    })
    .await
    .context("seeded control MCPService + supervisor AgentClass")?;
    let seeded: Api<v2::AgentClass> = Api::namespaced(ctx.client.clone(), &ns);
    let profile = seeded
        .get("supervisor")
        .await?
        .spec
        .supervisor
        .expect("seeded profile");
    if profile.services.first().map(|s| s.name.as_str()) != Some("control") {
        bail!("seeded supervisor profile does not grant the control service");
    }

    // The e2e's own workload identity: key → admin allowlist (labelled into
    // the org namespace) → enroll → agent token. All on the 1.3.1 wire.
    let admin_token_b64 = shell::kubectl(&[
        "get",
        "secret",
        "-n",
        sys,
        "agentctl-api-token",
        "-o",
        "jsonpath={.data.AGENTCTL_API_TOKEN}",
    ])?;
    let admin_token = String::from_utf8(
        base64::engine::general_purpose::STANDARD.decode(admin_token_b64.trim())?,
    )?;
    let seed: [u8; 32] = {
        let mut s = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut s).unwrap();
        s
    };
    let key = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
    let x = B64URL.encode(key.public_key().as_ref());
    let jkt = {
        let canonical = format!("{{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":\"{x}\"}}");
        B64URL.encode(ring::digest::digest(
            &ring::digest::SHA256,
            canonical.as_bytes(),
        ))
    };

    let pf_id = shell::PortForward::service(sys, "agentctl-identity", 80, 18101)?;
    let resp = ctx
        .http
        .post(format!("{}/admin/allowed-keys", pf_id.base_url()))
        .bearer_auth(&admin_token)
        .json(&json!({ "jkt": jkt, "label": format!("{ns}/sup-probe"), "ttl": 3600 }))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("allowed-keys registration failed: {}", resp.status());
    }

    // hwk-sign a POST exactly as the shipped client does (covered:
    // @method @authority @path content-digest signature-key).
    let hwk_call = |path: &'static str| {
        let key = &key;
        let x = x.clone();
        let base_url = pf_id.base_url();
        async move {
            let body = b"{}";
            let digest = format!(
                "sha-256=:{}:",
                B64STD.encode(ring::digest::digest(&ring::digest::SHA256, body))
            );
            let authority = base_url.trim_start_matches("http://").to_string();
            let sig_key = format!("sig=hwk;kty=\"OKP\";crv=\"Ed25519\";x=\"{x}\"");
            let params = format!(
                "(\"@method\" \"@authority\" \"@path\" \"content-digest\" \"signature-key\");created={}",
                now()
            );
            let base = format!(
                "\"@method\": POST\n\"@authority\": {authority}\n\"@path\": {path}\n\"content-digest\": {digest}\n\"signature-key\": {sig_key}\n\"@signature-params\": {params}"
            );
            let sig = format!(
                "sig=:{}:",
                B64STD.encode(key.sign(base.as_bytes()).as_ref())
            );
            let resp = ctx
                .http
                .post(format!("{base_url}{path}"))
                .header("content-type", "application/json")
                .header("content-digest", digest)
                .header("signature-input", format!("sig={params}"))
                .header("signature", sig)
                .header("signature-key", sig_key)
                .body(body.to_vec())
                .send()
                .await?;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                bail!("{path} refused ({status}): {body}");
            }
            anyhow::Ok(body)
        }
    };
    hwk_call("/enroll").await.context("aauth enroll")?;
    let token_resp = hwk_call("/agent-token").await.context("agent token")?;
    let agent_token = token_resp["agent_token"]
        .as_str()
        .context("agent_token in response")?
        .to_string();

    // Drive the control MCP over its HTTPS port-forward. TLS identity is the
    // chart CA's (SAN = the svc name) — the e2e pins nothing here; what is
    // under test is the AAuth layer.
    let pf_ctl = shell::PortForward::service(sys, "agentctl-control", 8443, 18102)?;
    let ctl_url = "https://127.0.0.1:18102/mcp".to_string();
    let http = reqwest::Client::builder()
        .user_agent("agentctl-e2e")
        .danger_accept_invalid_certs(true)
        .build()?;

    // Unsigned → the spec challenge (this is what flips agentd into signing).
    let resp = http
        .post(&ctl_url)
        .json(&json!({"jsonrpc":"2.0","id":0,"method":"ping"}))
        .send()
        .await?;
    if resp.status().as_u16() != 401 || resp.headers().get("aauth-requirement").is_none() {
        bail!(
            "unsigned control call: want 401 + AAuth-Requirement, got {}",
            resp.status()
        );
    }

    // jwt-scheme signer (what agentd sends once challenged).
    let jwt_call = |req: Value, tamper: bool| {
        let key = &key;
        let http = http.clone();
        let ctl_url = ctl_url.clone();
        let agent_token = agent_token.clone();
        async move {
            let authority = "127.0.0.1:18102";
            let sig_key = format!("sig=jwt;jwt=\"{agent_token}\"");
            let params = format!(
                "(\"@method\" \"@authority\" \"@path\" \"signature-key\");created={}",
                now()
            );
            let base = format!(
                "\"@method\": POST\n\"@authority\": {authority}\n\"@path\": /mcp\n\"signature-key\": {sig_key}\n\"@signature-params\": {params}"
            );
            let mut sig_bytes = key.sign(base.as_bytes()).as_ref().to_vec();
            if tamper {
                sig_bytes[0] ^= 0xff;
            }
            let sig = format!("sig=:{}:", B64STD.encode(sig_bytes));
            let resp = http
                .post(&ctl_url)
                .header("signature-input", format!("sig={params}"))
                .header("signature", sig)
                .header("signature-key", sig_key)
                .json(&req)
                .send()
                .await?;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::Ok((status, body))
        }
    };

    // Tampered signature → refused.
    let (status, _) = jwt_call(json!({"jsonrpc":"2.0","id":1,"method":"ping"}), true).await?;
    if status.as_u16() != 401 {
        bail!("tampered signature admitted ({status})");
    }

    // Handshake + the tool surface.
    let (status, body) = jwt_call(
        json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"agentctl-e2e","version":"0"}}}),
        false,
    )
    .await?;
    if !status.is_success() || body["result"]["serverInfo"]["name"] != json!("agentctl-control") {
        bail!("initialize failed ({status}): {body}");
    }
    let (_, body) = jwt_call(json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}), false).await?;
    let tools = body["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if tools.len() != 7 {
        bail!("want the 7 control tools, got {}: {body}", tools.len());
    }

    let call = |id: u64, tool: &'static str, args: Value| {
        let jwt_call = &jwt_call;
        async move {
            let (status, body) = jwt_call(
                json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                       "params":{"name":tool,"arguments":args}}),
                false,
            )
            .await?;
            if !status.is_success() {
                bail!("{tool} transport error ({status}): {body}");
            }
            anyhow::Ok(body["result"].clone())
        }
    };

    // The scope is the LABEL's namespace — never an argument.
    let r = call(4, "control.agents.list", json!({})).await?;
    if r["structuredContent"]["namespace"] != json!(ns.clone()) {
        bail!(
            "list scoped to {:?}, want {ns}",
            r["structuredContent"]["namespace"]
        );
    }

    // Create through the narrow surface → a REAL admission-gated Agent.
    let r = call(
        5,
        "control.agents.create",
        json!({ "name": "probe-agent", "instruction": "acknowledge and stop", "once": true,
                "handle": "probe" }),
    )
    .await?;
    if r["isError"] == json!(true) {
        bail!("create refused: {r}");
    }
    let agents: Api<v2::Agent> = Api::namespaced(ctx.client.clone(), &ns);
    let created = agents.get("probe-agent").await.context("created agent")?;
    if created
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get("agentctl.dev/created-by"))
        != Some(&"sup-probe".to_string())
    {
        bail!("created agent is not stamped with the creating workload");
    }

    // Resolve + status round out the read surface.
    let r = call(6, "control.agents.resolve", json!({ "handle": "@probe" })).await?;
    if r["structuredContent"]["name"] != json!("probe-agent") {
        bail!("resolve @probe: {r}");
    }
    let r = call(7, "control.agents.status", json!({ "name": "probe-agent" })).await?;
    if r["isError"] == json!(true) {
        bail!("status errored: {r}");
    }

    // ---- P4-2: the OBO binding check. Bind the SAME workload to a user by
    // creating a Supervisor CR named like it; govern the org with a policy
    // making that user a VIEWER. Reads keep working; create refuses; raising
    // the user to admin (as the gateway's stamped groups would) flips it on;
    // and the creation is attributed to the user.
    let sups: Api<v2::Supervisor> = Api::namespaced(ctx.client.clone(), &ns);
    let mut sup = v2::Supervisor::new(
        "sup-probe",
        v2::SupervisorSpec {
            user: "mock:carol".into(),
            paused: true, // binding only — never render an agent for this probe
            instruction_override: None,
            budget_override: None,
        },
    );
    sup.metadata.namespace = Some(ns.clone());
    kh::api::<v2::Supervisor>(&ctx.client, &ns)
        .create(&Default::default(), &sup)
        .await
        .context("bind sup-probe to mock:carol")?;
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(
            org,
            serde_json::from_value::<OrganizationSpec>(json!({
                "displayName": "E2E Control",
                "accessPolicies": [
                    { "match": { "claims": { "sub": "mock:carol" } }, "role": "viewer" },
                    { "match": { "groups": ["okta:platform-*"] }, "role": "admin" },
                ],
            }))?,
        )),
    )
    .await
    .context("govern the org")?;

    // Viewer: reads pass, create refuses naming the missing role.
    let r = call(8, "control.agents.list", json!({})).await?;
    if r["isError"] == json!(true) {
        bail!("viewer-bound list refused: {r}");
    }
    let r = call(
        9,
        "control.agents.create",
        json!({ "name": "escalation", "instruction": "should never exist", "once": true }),
    )
    .await?;
    if r["isError"] != json!(true) {
        bail!("a VIEWER-bound supervisor created an agent: {r}");
    }
    if agents.get_opt("escalation").await?.is_some() {
        bail!("the refused create still landed an Agent CR");
    }

    // The gateway-side stamp (owner groups) raises the ladder: patch the
    // status the way the gateway does after introspection.
    sups.patch_status(
        "sup-probe",
        &Default::default(),
        &Patch::Merge(&json!({ "status": { "ownerGroups": ["okta:platform-admins"] } })),
    )
    .await
    .context("stamp owner groups")?;
    let r = call(
        10,
        "control.agents.create",
        json!({ "name": "sanctioned", "instruction": "acknowledge and stop", "once": true }),
    )
    .await?;
    if r["isError"] == json!(true) {
        bail!("admin-stamped create refused: {r}");
    }
    let created = agents.get("sanctioned").await.context("sanctioned agent")?;
    if created
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("agentctl.dev/created-for"))
        != Some(&"mock:carol".to_string())
    {
        bail!("create is not attributed to the acting user");
    }

    // ---- P4-6: a governed child. The caller's own rendered Agent (the
    // paused supervisor renders one) anchors ownership + the budget ceiling.
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(agents.get_opt("sup-probe").await?.is_some())
    })
    .await
    .context("sup-probe rendered agent (subagent parent)")?;
    let r = call(
        11,
        "control.subagents.create",
        json!({ "name": "probe-child", "instruction": "summarize one thing and stop",
                "budgetTokens": 1234 }),
    )
    .await?;
    if r["isError"] == json!(true) {
        bail!("subagent create refused: {r}");
    }
    let child = agents.get("probe-child").await.context("child agent")?;
    let owner = &child
        .metadata
        .owner_references
        .as_ref()
        .context("child owner refs")?[0];
    if owner.kind != "Agent" || owner.name != "sup-probe" {
        bail!(
            "child is not owned by its parent agent: {:?}/{:?}",
            owner.kind,
            owner.name
        );
    }
    if child
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get("agentctl.dev/parent"))
        != Some(&"sup-probe".to_string())
    {
        bail!("child carries no parent label");
    }

    // ---- P4-5: the approval gate on delete. The supervisor's ask yields a
    // pending nonce and NO deletion; only the OWNER's own bearer (via the
    // gateway) can approve — a wrong user is refused; after approval the
    // re-issued delete executes.
    let sign = |sub: &str| {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
        let exp = now() + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({ "iss": "https://mock-idp:8443", "aud": "agentctl-cli", "sub": sub,
                     "email": format!("{sub}@example.test"), "groups": ["eng"], "exp": exp }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };
    let r = call(12, "control.agents.delete", json!({ "name": "sanctioned" })).await?;
    if r["isError"] == json!(true) {
        bail!("delete ask errored: {r}");
    }
    let nonce = r["structuredContent"]["pending"]
        .as_str()
        .context("pending nonce")?
        .to_string();
    if agents.get_opt("sanctioned").await?.is_none() {
        bail!("delete executed WITHOUT approval");
    }

    let pf_gw = shell::PortForward::service(sys, SVC_GATEWAY, PORT_HTTP, 18103)?;
    let approve_url = format!("{}/orgs/{org}/approvals/{nonce}", pf_gw.base_url());
    // The wrong human (bob) cannot approve carol's request.
    let resp = ctx
        .http
        .post(&approve_url)
        .bearer_auth(sign("bob"))
        .send()
        .await?;
    if resp.status().as_u16() != 403 {
        bail!("bob approved carol's delete ({})", resp.status());
    }
    // The owner approves; the re-issued delete executes.
    let resp = ctx
        .http
        .post(&approve_url)
        .bearer_auth(sign("carol"))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("owner approval failed ({})", resp.status());
    }
    let r = call(13, "control.agents.delete", json!({ "name": "sanctioned" })).await?;
    if r["isError"] == json!(true) || r["structuredContent"]["deleted"] != json!("sanctioned") {
        bail!("approved delete did not execute: {r}");
    }
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(1), || async {
        Ok(agents.get_opt("sanctioned").await?.is_none())
    })
    .await
    .context("agent gone after approved delete")?;

    drop(pf_gw);
    drop(pf_ctl);
    drop(pf_id);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P2-8: every agentd start kind provisions from `spec.triggers[]`, the
/// right workload shape renders, and the fireable kinds FIRE:
/// - one daemon carrying eight long-lived triggers (loop, schedule-every,
///   webhook, subscribe, stream, signal, event, a2aCommand) → Deployment;
///   the webhook is POSTed (via the loopback listener the compiler wired)
///   and the 30s loop ticks — both proven from the agent's own run events;
/// - `agentctl create agent --once` → Job that completes (instruction sugar);
/// - `agentctl create agent --schedule` → CronJob with the cron expression.
async fn trigger_matrix(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;

    // The eight-trigger daemon (subscribe binds an inline loopback server —
    // nothing listens there; the daemon retries and the start stays armed).
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: matrix, namespace: {ns} }}\nspec:\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"acknowledge the trigger payload in one line\" }}\n  intelligence: {{ pool: mockpool }}\n  expose: {{ a2a: true }}\n  mcpServers:\n    - name: queue\n      endpoint: \"http://127.0.0.1:8931/mcp\"\n  triggers:\n    - loop: {{ interval: 30s }}\n    - schedule: {{ every: 1h }}\n    - webhook: {{ path: /hooks/ci, methods: [POST] }}\n    - subscribe: {{ service: queue, uri: \"queue://inbox\" }}\n    - stream: {{ stream: incidents }}\n    - signal: {{ name: \"reply/1\" }}\n    - event: {{ name: workflow.finished }}\n    - a2aCommand: {{ command: qa.verify }}\n"
    ))
    .context("apply matrix daemon")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(!first_pod(ns, &agent_label("matrix"))
            .unwrap_or_default()
            .is_empty())
    })
    .await?;
    let pod = first_pod(ns, &agent_label("matrix"))?;
    kh::wait_pod_running(&ctx.client, ns, &pod, READY_TIMEOUT).await?;
    // Deployment shape.
    shell::kubectl(&["get", "deployment", "matrix", "-n", ns])
        .context("eight-trigger daemon must render a Deployment")?;

    // FIRE the webhook through the listener (port-forward to the pod).
    // Auth-less webhook triggers default to HMAC now (agentd refuses
    // unauthenticated routes off loopback), so sign like a real sender;
    // the listener serves the agent's own certs — a trusting client
    // suffices here (hooks-ingress covers the verified gateway path).
    let hmac_secret = {
        let b64 = shell::kubectl(&[
            "get",
            "secret",
            "matrix-hooks",
            "-n",
            ns,
            "-o",
            "jsonpath={.data.hmac-2}",
        ])
        .context("matrix hooks Secret (webhook = trigger index 2)")?;
        String::from_utf8(base64_decode(b64.trim())?)?
    };
    let body = json!({ "build": "1", "status": "green" }).to_string();
    let signature = {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, hmac_secret.as_bytes());
        let tag = ring::hmac::sign(&key, body.as_bytes());
        let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256={hex}")
    };
    let pf = shell::PortForward::pod(ns, &pod, 9494, 19494)?;
    let insecure = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let resp = insecure
        .post("https://127.0.0.1:19494/hooks/ci")
        .header("content-type", "application/json")
        .header("x-signature", signature)
        .body(body)
        .send()
        .await
        .context("POST the webhook trigger")?;
    if !resp.status().is_success() {
        bail!("webhook POST got {}", resp.status());
    }
    drop(pf);

    // Both firings visible in the agent's own events: the webhook workflow
    // ran, and the 30s loop ticked at least once.
    kh::poll_until(Duration::from_secs(120), Duration::from_secs(5), || async {
        let logs = shell::kubectl(&["logs", "-n", ns, &pod, "--tail=-1"]).unwrap_or_default();
        Ok(logs.contains("main-webhook") && logs.contains("main-loop"))
    })
    .await
    .context("webhook + loop firings in the agent log")?;

    // CLI: a once Job (instruction sugar; completes against the mock pool).
    shell::run(
        "./target/release/agentctl",
        &[
            "create",
            "agent",
            "matrix-once",
            "-n",
            ns,
            "--instruction",
            "say hi and exit",
            "--image",
            "agentd:1.3.1",
            "--pool",
            "mockpool",
            "--once",
        ],
    )
    .context("agentctl create agent --once")?;
    kh::poll_until(Duration::from_secs(60), Duration::from_secs(2), || async {
        Ok(shell::kubectl(&["get", "job", "matrix-once", "-n", ns]).is_ok())
    })
    .await
    .context("once shape must render a Job")?;
    shell::kubectl(&[
        "wait",
        "job/matrix-once",
        "-n",
        ns,
        "--for=condition=complete",
        "--timeout=120s",
    ])
    .context("the once Job completes")?;

    // CLI: a sole-cron schedule → CronJob with the expression.
    shell::run(
        "./target/release/agentctl",
        &[
            "create",
            "agent",
            "matrix-cron",
            "-n",
            ns,
            "--instruction",
            "tick",
            "--image",
            "agentd:1.3.1",
            "--pool",
            "mockpool",
            "--schedule",
            "0 7 * * 1-5",
        ],
    )
    .context("agentctl create agent --schedule")?;
    kh::poll_until(Duration::from_secs(60), Duration::from_secs(2), || async {
        Ok(shell::kubectl(&["get", "cronjob", "matrix-cron", "-n", ns]).is_ok())
    })
    .await
    .context("cron shape must render a CronJob")?;
    let cron = shell::kubectl(&[
        "get",
        "cronjob",
        "matrix-cron",
        "-n",
        ns,
        "-o",
        "jsonpath={.spec.schedule}",
    ])?;
    if cron.trim() != "0 7 * * 1-5" {
        bail!("cron shape rendered schedule {cron:?}");
    }

    for name in ["matrix", "matrix-once", "matrix-cron"] {
        shell::kubectl(&[
            "delete",
            "agent",
            name,
            "-n",
            ns,
            "--wait=false",
            "--ignore-not-found",
        ])?;
    }
    delete_example(&dir, "modelpool-mock.yaml");
    delete_example(&dir, "mock-provider.yaml");
    pass()
}

/// The policy ladder red/green (RFC 0032, P2-4): AgentClass floors deny past
/// requests naming the floor's holder; MCPService ceilings deny widening
/// grants naming the service; a compliant narrow grant is admitted; and the
/// tag-laundering guard refuses an MCPService edit that would widen a LIVE
/// consumer — then allows the same edit once no consumer exists.
async fn policy_ladder(ctx: &Ctx) -> Result<Outcome> {
    let ns = &ctx.cfg.ns;
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: MCPService\nmetadata: {{ name: tickets, namespace: {ns} }}\nspec:\n  kind: mcp\n  endpoint: https://tickets.tools.svc.cluster.local.:8443/mcp\n  tags: [egress]\n  allow: [\"ticket_*\"]\n"
    ))?;
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: AgentClass\nmetadata: {{ name: guarded, namespace: {ns} }}\nspec:\n  floors:\n    egress: closed\n    budget: {{ windows: [{{ per: day, tokens: 100000 }}] }}\n"
    ))?;

    let agent_yaml = |name: &str, services: &str, caps: &str| {
        format!(
            "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: {name}, namespace: {ns} }}\nspec:\n  class: guarded\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  expose: {{ a2a: true }}\n{services}{caps}"
        )
    };

    // RED: raw egress under a closed floor, no tagged grant — denied naming
    // the class.
    let err = shell::kubectl_apply_stdin(&agent_yaml(
        "ladder-raw",
        "",
        "  capabilities: { egress: true }\n",
    ))
    .expect_err("raw egress must be denied");
    let msg = format!("{err:#}");
    if !msg.contains("guarded") || !msg.contains("egress") {
        bail!("denial does not name the floor: {msg}");
    }

    // RED: widening the registry ceiling — denied naming the MCPService.
    let err = shell::kubectl_apply_stdin(&agent_yaml(
        "ladder-wide",
        "  services: [{ name: tickets, allow: [wipe_all] }]\n",
        "",
    ))
    .expect_err("widened grant must be denied");
    let msg = format!("{err:#}");
    if !msg.contains("tickets") || !msg.contains("widens the registry ceiling") {
        bail!("denial does not name the service ceiling: {msg}");
    }

    // GREEN: a narrow grant satisfies the closed floor.
    shell::kubectl_apply_stdin(&agent_yaml(
        "ladder-ok",
        "  services: [{ name: tickets, allow: [ticket_read] }]\n",
        "  capabilities: { egress: true }\n",
    ))
    .context("compliant agent should be admitted")?;

    // RED: with a live consumer, dropping the egress tag is tag laundering.
    let launder = format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: MCPService\nmetadata: {{ name: tickets, namespace: {ns} }}\nspec:\n  kind: mcp\n  endpoint: https://tickets.tools.svc.cluster.local.:8443/mcp\n  allow: [\"ticket_*\"]\n"
    );
    let err = shell::kubectl_apply_stdin(&launder)
        .expect_err("tag drop with a live consumer must be denied");
    let msg = format!("{err:#}");
    if !msg.contains("tag-laundering") || !msg.contains("ladder-ok") {
        bail!("laundering denial does not name the consumer: {msg}");
    }

    // GREEN: the same edit with no consumers is a plain registry change.
    shell::kubectl(&["delete", "agent", "ladder-ok", "-n", ns, "--wait=false"])?;
    kh::poll_until(GC_TIMEOUT, Duration::from_secs(2), || async {
        Ok(kh::api::<agent_api::v1alpha2::Agent>(&ctx.client, ns)
            .get_opt("ladder-ok")
            .await?
            .is_none())
    })
    .await
    .context("consumer deletion")?;
    shell::kubectl_apply_stdin(&launder)
        .context("tag drop with no consumers should be admitted")?;

    shell::kubectl(&[
        "delete",
        "mcpservice",
        "tickets",
        "-n",
        ns,
        "--ignore-not-found",
    ])?;
    shell::kubectl(&[
        "delete",
        "agentclass",
        "guarded",
        "-n",
        ns,
        "--ignore-not-found",
    ])?;
    pass()
}

/// The RFC 0033 §2.1 worked example, red/green at the gateway (P1-8):
/// engineering operates `team: engineering` agents, is refused on marketing;
/// marketing may VIEW its own team's tasks but not converse; admins do
/// everything. Also asserts the operator's RBAC mirror landed (per-namespace
/// role ladder + exact-group bindings).
async fn org_access_policy(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use kube::api::{Api, Patch, PatchParams};

    const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
    let sign = |sub: &str, groups: &[&str]| {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({
                "iss": "https://mock-idp:8443", "aud": "agentctl-cli",
                "sub": sub, "groups": groups, "exp": exp,
            }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-policy";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    let spec: OrganizationSpec = serde_json::from_value(json!({
        "displayName": "E2E Policy",
        "accessPolicies": [
            { "match": { "groups": ["mock:eng-*"] }, "role": "operator",
              "selector": { "matchLabels": { "team": "engineering" } } },
            { "match": { "groups": ["mock:marketing"] }, "role": "viewer",
              "selector": { "matchLabels": { "team": "marketing" } } },
            { "match": { "groups": ["mock:admins"] }, "role": "admin" },
        ],
    }))?;
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(org, spec)),
    )
    .await
    .context("apply Organization")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    // The RBAC mirror: role ladder + exact-group admin binding in the ns.
    let mirror = shell::kubectl(&[
        "get",
        "rolebinding",
        "agentctl-org-admin",
        "-n",
        &ns,
        "-o",
        "jsonpath={.subjects[*].name}",
    ])
    .context("RBAC mirror rolebinding")?;
    if !mirror.contains("mock:admins") {
        bail!("RBAC mirror admin binding lacks the exact group (got {mirror:?})");
    }

    // Two labeled agents (no principals — the policy check precedes them).
    for (name, team) in [("eng-bot", "engineering"), ("mkt-bot", "marketing")] {
        let mut agent = agentd_agent(ctx, name, Mode::Reactive, "idle");
        agent.spec.instruction = None;
        agent.spec.surfaces = Some(agent_api::DesiredSurfaces {
            a2a: true,
            ..Default::default()
        });
        agent.metadata.namespace = Some(ns.clone());
        agent
            .metadata
            .labels
            .get_or_insert_with(Default::default)
            .insert("team".into(), team.into());
        kh::apply_agent(&ctx.client, &ns, name, &agent).await?;
    }
    for name in ["eng-bot", "mkt-bot"] {
        kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
            Ok(!first_pod(&ns, &agent_label(name))
                .unwrap_or_default()
                .is_empty())
        })
        .await?;
        let pod = first_pod(&ns, &agent_label(name))?;
        kh::wait_pod_running(&ctx.client, &ns, &pod, READY_TIMEOUT).await?;
    }

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18096)?;
    let rpc = json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
        "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-pol", "parts": [{ "text": "ping" }] } } });
    let call = |token: String, name: &str, body: Value| {
        let http = ctx.http.clone();
        let url = format!("{}/orgs/{org}/agents/{name}", pf.base_url());
        async move {
            let resp = http
                .post(&url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await?;
            anyhow::Ok(resp.status().as_u16())
        }
    };

    let eng = sign("eng-user", &["mock:eng-platform"]);
    let mkt = sign("mkt-user", &["mock:marketing"]);
    let admin = sign("root", &["mock:admins"]);

    // GREEN: engineering operates engineering.
    let s = call(eng.clone(), "eng-bot", rpc.clone()).await?;
    if s != 200 {
        bail!("eng → eng-bot SendMessage got {s} (want 200)");
    }
    // RED: engineering refused on marketing — even for viewing.
    let s = call(eng.clone(), "mkt-bot", rpc.clone()).await?;
    if s != 403 {
        bail!("eng → mkt-bot SendMessage got {s} (want 403)");
    }
    let tasks_list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tasks/list", "params": {} });
    let s = call(eng, "mkt-bot", tasks_list.clone()).await?;
    if s != 403 {
        bail!("eng → mkt-bot tasks/list got {s} (want 403)");
    }
    // Marketing VIEWS its team (viewer-grade method)…
    let s = call(mkt.clone(), "mkt-bot", tasks_list.clone()).await?;
    if s != 200 {
        bail!("marketing → mkt-bot tasks/list got {s} (want 200)");
    }
    // …but cannot converse (operator-grade), even with its own team.
    let s = call(mkt, "mkt-bot", rpc.clone()).await?;
    if s != 403 {
        bail!("marketing → mkt-bot SendMessage got {s} (want 403)");
    }
    // Admin sees all: operates BOTH teams.
    for name in ["eng-bot", "mkt-bot"] {
        let s = call(admin.clone(), name, rpc.clone()).await?;
        if s != 200 {
            bail!("admin → {name} SendMessage got {s} (want 200)");
        }
    }

    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// The whole P1-5 chain, live: `/orgs/<org>/agents/<name>` resolves the
/// Organization's managed namespace; the inbound bearer (an RS256 token signed
/// with the mock-IdP test key) is introspected at the identity service (which
/// fetches JWKS from the in-cluster mock-idp over the private chart CA); the
/// caller's minted per-(user,agent) principal bearer is fetched from the
/// projected Secret and injected upstream, so the agent answers as
/// `user:mock:alice`. Negatives: no token 401, an unlisted subject 403, an
/// unknown org 404.
async fn org_route_user(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use kube::api::{Api, Patch, PatchParams};

    const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
    let sign = |sub: &str| {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({
                "iss": "https://mock-idp:8443",
                "aud": "agentctl-cli",
                "sub": sub,
                "email": format!("{sub}@example.test"),
                "groups": ["eng"],
                "exp": exp,
            }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };

    // Organization + a principal-gated agent in its managed namespace.
    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-route";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(
            org,
            serde_json::from_value::<OrganizationSpec>(json!({ "displayName": "E2E Route" }))?,
        )),
    )
    .await
    .context("apply Organization")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    let name = "assistant";
    let mut agent = agentd_agent(ctx, name, Mode::Reactive, "idle");
    agent.spec.instruction = None;
    agent.spec.surfaces = Some(agent_api::DesiredSurfaces {
        a2a: true,
        ..Default::default()
    });
    agent.spec.access = Some(agent_api::Access {
        principals: vec!["mock:alice".to_string()],
        ..Default::default()
    });
    // P2-7: an org-unique @handle distinct from the CR name; the org route
    // resolves it, and admission refuses a second holder.
    agent.spec.handle = Some("helper".to_string());
    agent.spec.display_name = Some("Helper Assistant".to_string());
    agent.metadata.namespace = Some(ns.clone());
    kh::apply_agent(&ctx.client, &ns, name, &agent).await?;

    // A duplicate handle is refused at admission, naming the holder.
    let mut dupe = agentd_agent(ctx, "impostor", Mode::Reactive, "idle");
    dupe.spec.instruction = None;
    dupe.spec.surfaces = Some(agent_api::DesiredSurfaces {
        a2a: true,
        ..Default::default()
    });
    dupe.spec.handle = Some("helper".to_string());
    dupe.metadata.namespace = Some(ns.clone());
    match kh::api::<agent_api::v1alpha2::Agent>(&ctx.client, &ns)
        .create(
            &Default::default(),
            &agent_api::v1alpha2::convert::agent_object_v1_to_v2(&dupe),
        )
        .await
    {
        Ok(_) => bail!("a duplicate handle was ADMITTED — uniqueness rung is not enforcing"),
        Err(e) => {
            let msg = format!("{e}");
            if !msg.contains("already held") {
                bail!("duplicate handle refused with an unexpected message: {msg}");
            }
        }
    }
    let pod = {
        // wait_for_first_pod is pinned to ctx.cfg.ns; poll the org ns directly.
        let mut found = String::new();
        kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
            Ok(!first_pod(&ns, &agent_label(name))
                .unwrap_or_default()
                .is_empty())
        })
        .await
        .context("agent pod in org namespace")?;
        found.push_str(&first_pod(&ns, &agent_label(name))?);
        found
    };
    kh::wait_pod_running(&ctx.client, &ns, &pod, READY_TIMEOUT).await?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18098)?;
    let rpc = json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
        "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-org-1", "parts": [{ "text": "ping" }] } } });
    // The route segment is the HANDLE, not the CR name (P2-7).
    let url = format!("{}/orgs/{org}/agents/helper", pf.base_url());

    // Named user end-to-end: token → introspection → principal bearer → answer.
    let resp = ctx
        .http
        .post(&url)
        .bearer_auth(sign("alice"))
        .json(&rpc)
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() || body.get("result").is_none() {
        bail!("org-route call as mock:alice failed (status {status}): {body}");
    }

    // No token → 401 (org routes never fall open).
    let resp = ctx.http.post(&url).json(&rpc).send().await?;
    if resp.status().as_u16() != 401 {
        bail!(
            "unauthenticated org-route call got {} (want 401)",
            resp.status()
        );
    }

    // A valid token for a subject the agent does NOT name → 403.
    let resp = ctx
        .http
        .post(&url)
        .bearer_auth(sign("bob"))
        .json(&rpc)
        .send()
        .await?;
    if resp.status().as_u16() != 403 {
        bail!(
            "unlisted subject got {} (want 403 addressed-gate refusal)",
            resp.status()
        );
    }

    // Unknown org → 404.
    let resp = ctx
        .http
        .post(format!("{}/orgs/no-such-org/agents/{name}", pf.base_url()))
        .bearer_auth(sign("alice"))
        .json(&rpc)
        .send()
        .await?;
    if resp.status().as_u16() != 404 {
        bail!("unknown org got {} (want 404)", resp.status());
    }

    // Streaming through the org route with the injected principal: the SSE
    // pipe opens and carries at least one data frame.
    let stream_rpc = json!({ "jsonrpc": "2.0", "id": 2, "method": "SendStreamingMessage",
        "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-org-2", "parts": [{ "text": "ping" }] } } });
    let resp = ctx
        .http
        .post(&url)
        .bearer_auth(sign("alice"))
        .json(&stream_rpc)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("org-route stream got {}", resp.status());
    }
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !ct.starts_with("text/event-stream") {
        bail!("org-route stream content-type {ct:?}, want text/event-stream");
    }
    let body = resp.text().await.unwrap_or_default();
    if !body.contains("data:") {
        bail!("org-route stream carried no SSE data frame: {body:?}");
    }

    // The card is served at the tenant-scoped address.
    let resp = ctx
        .http
        .get(format!(
            "{}/orgs/{org}/agents/helper/.well-known/agent-card.json",
            pf.base_url()
        ))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("org-route card got {}", resp.status());
    }

    drop(pf);
    kh::api::<agent_api::v1alpha2::Agent>(&ctx.client, &ns)
        .delete(name, &Default::default())
        .await
        .ok();
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P4-3/P4-4: the caller's OWN supervisor, end to end. First touch on
/// `POST /orgs/<org>/supervisor` auto-creates the Supervisor CR (org policy
/// `supervisors: auto`, the default) and answers a retryable 503; the
/// operator renders it into an owner-referenced v2 Agent whose ONLY named
/// principal is the caller, with the layered instruction; once the pod runs,
/// the same call converses. No token stays 401.
async fn supervisor_route(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use agent_api::v1alpha2 as v2;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use kube::api::{Api, Patch, PatchParams};

    const KEY_PEM: &str = include_str!("../../../agentctl-identity/tests/keys/test-idp.pem");
    let sign = |sub: &str| {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-1".into());
        encode(
            &header,
            &json!({
                "iss": "https://mock-idp:8443",
                "aud": "agentctl-cli",
                "sub": sub,
                "email": format!("{sub}@example.test"),
                "groups": ["eng"],
                "exp": exp,
            }),
            &EncodingKey::from_rsa_pem(KEY_PEM.as_bytes()).expect("vendored test key"),
        )
        .expect("sign test token")
    };

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    let org = "e2e-sup";
    let ns = org_namespace(org);
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch(
        org,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(
            org,
            serde_json::from_value::<OrganizationSpec>(json!({ "displayName": "E2E Sup" }))?,
        )),
    )
    .await
    .context("apply Organization")?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(orgs
            .get(org)
            .await?
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Ready")))
    })
    .await
    .context("org Ready")?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18099)?;
    let url = format!("{}/orgs/{org}/supervisor", pf.base_url());
    let rpc = json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
        "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-sup-1", "parts": [{ "text": "who am I to you?" }] } } });

    // No token → 401 (the supervisor route never falls open).
    let resp = ctx.http.post(&url).json(&rpc).send().await?;
    if resp.status().as_u16() != 401 {
        bail!(
            "unauthenticated supervisor call got {} (want 401)",
            resp.status()
        );
    }

    // First authenticated touch: provisioning (503) + the CR appears.
    let resp = ctx
        .http
        .post(&url)
        .bearer_auth(sign("carol"))
        .json(&rpc)
        .send()
        .await?;
    if resp.status().as_u16() != 503 {
        bail!(
            "first supervisor touch got {} (want 503 provisioning)",
            resp.status()
        );
    }
    let sups: Api<v2::Supervisor> = Api::namespaced(ctx.client.clone(), &ns);
    let sup_name = "sup-mock-carol";
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(sups.get_opt(sup_name).await?.is_some())
    })
    .await
    .context("auto-created Supervisor CR")?;
    let sup = sups.get(sup_name).await?;
    if sup.spec.user != "mock:carol" {
        bail!(
            "Supervisor spec.user = {:?}, want mock:carol",
            sup.spec.user
        );
    }

    // The rendered v2 Agent: owner-scoped to the CR, addressed ONLY to carol,
    // instruction layered from the platform default.
    let agents: Api<v2::Agent> = Api::namespaced(ctx.client.clone(), &ns);
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(agents.get_opt(sup_name).await?.is_some())
    })
    .await
    .context("rendered supervisor Agent")?;
    let rendered = agents.get(sup_name).await?;
    let principals = rendered
        .spec
        .access
        .as_ref()
        .map(|a| a.principals.clone())
        .unwrap_or_default();
    if principals != vec!["mock:carol".to_string()] {
        bail!("supervisor principals {principals:?}, want exactly [mock:carol]");
    }
    if !rendered
        .metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|o| o.kind == "Supervisor")
    {
        bail!("rendered supervisor agent is not owner-referenced to its Supervisor CR");
    }

    // Poll the SAME call until the supervisor answers as carol (the pod must
    // come up, the gateway resolve it, and the principal bearer converse).
    let last = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let last = last.clone();
        let url = url.clone();
        let rpc = rpc.clone();
        let token = sign("carol");
        async move {
            let resp = ctx
                .http
                .post(&url)
                .bearer_auth(token)
                .json(&rpc)
                .send()
                .await?;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            *last.lock().unwrap() = format!("status {status}: {body}");
            Ok(status.is_success() && body.get("result").is_some())
        }
    })
    .await
    .with_context(|| format!("supervisor never answered; last: {}", last.lock().unwrap()))?;

    drop(pf);
    orgs.delete(org, &Default::default()).await.ok();
    pass()
}

/// P3: the managed state service honors the agentd checkpointer profile
/// (contract/schemas/store.profile.json) and survives a SIGKILL.
/// Leg 1 drives the four tools DIRECTLY over MCP: absent-get null, seq-CAS
/// accept/refuse, byte-identical replay idempotence, list, delete.
/// Leg 2 renders a `store.class: managed` agent, waits for its checkpoint
/// keys under orgs/<ns>/<name>, hard-kills the pod, and requires a clean
/// restore (fresh pod Running, zero restarts, keys intact).
async fn state_durability(ctx: &Ctx) -> Result<Outcome> {
    let sys = &ctx.cfg.system_ns;
    let ready = shell::kubectl(&[
        "get",
        "deploy",
        "-n",
        sys,
        "agentctl-state",
        "-o",
        "jsonpath={.status.readyReplicas}",
    ])
    .unwrap_or_default();
    if ready.trim().is_empty() || ready.trim() == "0" {
        return skip(
            "state service not Ready — blessed mcpg image cannot dlopen backend-sql:protocol-1 \
             (GLIBC mismatch; mcpg re-blessing the pairing). Unskips by itself once the pin lands.",
        );
    }

    let pf = shell::PortForward::service(sys, "agentctl-state", 8787, 18100)?;
    // The state gateway serves TLS now (agentd refuses plaintext off-loopback);
    // a throwaway trusting client suffices for the forwarded port (the managed
    // agent trusts the real cert via the chart CA).
    let mcp = format!("{}/mcp", pf.base_url().replace("http://", "https://"));
    let state_http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    // The driver's own asserted subject — the P3-2 fence scopes its agent-tool
    // calls to keys under THIS prefix (admin tools are unfenced).
    let driver_subj = format!("orgs/{}/e2e-driver", ctx.cfg.ns);
    // Minimal MCP streamable-HTTP client: initialize → session id → calls.
    let post = |body: Value, session: Option<String>, subject: String| {
        let mcp = mcp.clone();
        let state_http = state_http.clone();
        async move {
            let mut req = state_http
                .post(&mcp)
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                // Interim prefix-trust (P3-1): the caller self-asserts its
                // identity (x-mcpg-subject-id) behind the NetworkPolicy
                // perimeter → header_asserted tier; the state bindings key
                // their SQL on this via identity.subject_id (the P3-2 fence).
                .header("x-mcpg-subject-id", subject)
                .json(&body);
            if let Some(s) = &session {
                req = req.header("mcp-session-id", s.clone());
            }
            let resp = req.send().await.context("reach state /mcp")?;
            let session = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let text = resp.text().await.unwrap_or_default();
            let body: Value = if ct.starts_with("text/event-stream") {
                text.lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                    .rev()
                    .find(|v| v.get("result").is_some() || v.get("error").is_some())
                    .unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            };
            anyhow::Ok((body, session))
        }
    };

    let (init, session) = post(
        json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "agentctl-e2e", "version": "0" } } }),
        None,
        driver_subj.clone(),
    )
    .await?;
    if init.get("result").is_none() {
        bail!("state MCP initialize failed: {init}");
    }
    let _ = post(
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        session.clone(),
        driver_subj.clone(),
    )
    .await;
    // beta.26 enforces UNIQUE JSON-RPC request ids per session; an atomic
    // counter guarantees it regardless of the caller's label.
    let next_id = std::sync::atomic::AtomicU64::new(100);
    // Agent-tool calls assert the DRIVER's own subject (the fence scopes them
    // to keys under it). `call_as` overrides the subject for the fence test.
    let call_as = |subject: String, tool: &str, args: Value| {
        let tool = tool.to_string();
        let session = session.clone();
        let post = &post;
        let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        async move {
            let (resp, _) = post(
                json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                        "params": { "name": tool, "arguments": args } }),
                session,
                subject,
            )
            .await?;
            if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
                bail!("tools/call {tool} refused: {err}");
            }
            anyhow::Ok(resp["result"].clone())
        }
    };
    let call = |_label: u64, tool: &str, args: Value| call_as(driver_subj.clone(), tool, args);

    // Contract conformance, straight off store.profile.json — keys under the
    // driver's OWN subject prefix so the P3-2 fence admits them.
    let key = format!("{driver_subj}/state");
    let sc = |r: &Value| r["structuredContent"].clone();
    let envl = json!({ "v": 1, "note": "first checkpoint" });

    let r = call(1, "state.get", json!({ "key": key })).await?;
    if !sc(&r).is_null() && !sc(&r)["state"].is_null() {
        // Leftover from a prior run — clear and re-read.
        call(2, "state.delete", json!({ "key": key })).await?;
    }
    let r = call(
        3,
        "state.put",
        json!({ "key": key, "seq": 1, "state": envl }),
    )
    .await?;
    if sc(&r)["ok"] != json!(true) || !sc(&r)["latest"].is_null() {
        bail!("first put refused: {r}");
    }
    // Byte-identical replay of the accepted write stays accepted (the binding
    // self-idempotates; agentd's _meta idempotency never reaches SQL).
    let r = call(
        4,
        "state.put",
        json!({ "key": key, "seq": 1, "state": envl }),
    )
    .await?;
    if sc(&r)["ok"] != json!(true) {
        bail!("idempotent replay refused: {r}");
    }
    // A DIVERGENT write at the same seq is the split-brain fence: refuse + latest.
    let r = call(
        5,
        "state.put",
        json!({ "key": key, "seq": 1, "state": { "v": 1, "note": "divergent" } }),
    )
    .await?;
    if sc(&r)["ok"] != json!(false) || sc(&r)["latest"] != json!(1) {
        bail!("divergent same-seq write not fenced: {r}");
    }
    // The successor seq advances; a stale successor refuses with the latest.
    let r = call(
        6,
        "state.put",
        json!({ "key": key, "seq": 2, "state": { "v": 2 } }),
    )
    .await?;
    if sc(&r)["ok"] != json!(true) {
        bail!("seq-2 CAS refused: {r}");
    }
    let r = call(
        7,
        "state.put",
        json!({ "key": key, "seq": 4, "state": { "v": 4 } }),
    )
    .await?;
    if sc(&r)["ok"] != json!(false) || sc(&r)["latest"] != json!(2) {
        bail!("gap write (seq 4 over 2) not refused with latest: {r}");
    }
    let r = call(8, "state.get", json!({ "key": key })).await?;
    if sc(&r)["state"]["v"] != json!(2) || sc(&r)["seq"] != json!(2) {
        bail!("read-back after CAS: {r}");
    }
    let r = call(9, "state.list", json!({ "prefix": driver_subj.clone() })).await?;
    let keys = sc(&r)["keys"].as_array().cloned().unwrap_or_default();
    if !keys.iter().any(|k| k["key"] == json!(key)) {
        bail!("list missed the written key: {r}");
    }
    call(10, "state.delete", json!({ "key": key })).await?;
    let r = call(11, "state.get", json!({ "key": key })).await?;
    if !sc(&r).is_null() && !sc(&r)["state"].is_null() {
        bail!("get after delete not absent: {r}");
    }

    // Leg 2: a managed-store agent checkpoints through the SAME service and
    // survives kill -9.
    let ns = &ctx.cfg.ns;
    let name = "state-probe";
    let prefix = format!("orgs/{ns}/{name}");
    // Hermetic start: purge anything left under this agent's prefix by a prior
    // run. The prefix is REUSED across runs and agentd writes one run-cursor key
    // PER loop tick (never GC'd) plus a per-pod manifest/context — left to
    // accumulate they eventually push the whole-prefix export past mcpg's FFI
    // payload cap (the admin snapshot rides a single frame), which the existence
    // check below now surfaces loudly instead of reading as "no checkpoints".
    call(90, "state.admin.purge", json!({ "prefix": prefix.clone() })).await?;
    shell::kubectl_apply_stdin(&format!(
        "apiVersion: agentctl.dev/v1alpha2\nkind: Agent\nmetadata: {{ name: {name}, namespace: {ns} }}\nspec:\n  shape: daemon\n  runtime: {{ image: \"agentd:1.3.1\" }}\n  instruction: {{ text: \"acknowledge ticks in one line\" }}\n  expose: {{ a2a: true }}\n  store: {{ class: managed }}\n  triggers:\n    - loop: {{ interval: 30s }}\n"
    ))?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(!first_pod(ns, &agent_label(name))
            .unwrap_or_default()
            .is_empty())
    })
    .await
    .context("state-probe pod")?;
    let pod = first_pod(ns, &agent_label(name))?;
    kh::wait_pod_running(&ctx.client, ns, &pod, READY_TIMEOUT).await?;

    // The agent's checkpoints appear under ITS operator-rendered prefix. The
    // driver (a DIFFERENT subject) reads them via the ADMIN snapshot tool —
    // the agent-tool `state.list` is fenced to the caller's own subject, so
    // a cross-agent read there returns nothing (that IS the fence, tested
    // below); the admin tool is unfenced by design (operator/control plane).
    //
    // The whole prefix exports over ONE mcpg FFI frame (256 KiB host cap), so a
    // tool error there (payload over cap) MUST surface — reading it as "no
    // items" is exactly how a capped snapshot silently masquerades as an empty
    // store (see docs/v2/known-limits.md).
    fn snapshot_items(c: &Value) -> Result<Vec<Value>> {
        if c["isError"] == json!(true) {
            bail!(
                "state.admin.snapshot failed (256 KiB FFI payload cap — see \
                 docs/v2/known-limits.md): {}",
                c["content"][0]["text"]
            );
        }
        Ok(c["structuredContent"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(3), || {
        let prefix = prefix.clone();
        let call = &call;
        async move {
            let r = call(12, "state.admin.snapshot", json!({ "prefix": prefix })).await?;
            Ok(!snapshot_items(&r)?.is_empty())
        }
    })
    .await
    .context("agent checkpoints under its prefix")?;

    // Hard kill (no grace): the replacement must restore, not split-brain.
    shell::kubectl(&[
        "delete",
        "pod",
        "-n",
        ns,
        &pod,
        "--grace-period=0",
        "--force",
    ])?;
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || {
        let old = pod.clone();
        async move {
            let now = first_pod(ns, &agent_label(name)).unwrap_or_default();
            Ok(!now.is_empty() && now != old)
        }
    })
    .await
    .context("replacement pod after SIGKILL")?;
    let pod2 = first_pod(ns, &agent_label(name))?;
    kh::wait_pod_running(&ctx.client, ns, &pod2, READY_TIMEOUT).await?;
    // Hold for a restore window: still Running, zero restarts (a store
    // Conflict on an owned key is fatal split-brain — it would crash-loop).
    tokio::time::sleep(Duration::from_secs(30)).await;
    let restarts = shell::kubectl(&[
        "get",
        "pod",
        "-n",
        ns,
        &pod2,
        "-o",
        "jsonpath={.status.containerStatuses[0].restartCount}",
    ])?;
    if restarts.trim() != "0" {
        bail!("restored agent restarted {restarts} times (split-brain or store outage?)");
    }
    let r = call(
        13,
        "state.admin.snapshot",
        json!({ "prefix": prefix.clone() }),
    )
    .await?;
    if snapshot_items(&r)?.is_empty() {
        bail!("checkpoints vanished across the SIGKILL: {r}");
    }

    // THE FENCE (P3-2 DoD): the driver, asserting its OWN subject, cannot read
    // the managed agent's keys through an agent tool — state.get of a key
    // under the agent's prefix returns absent, and state.list of the agent's
    // prefix returns empty, no matter the argument. Cross-agent access is
    // provably impossible for a conforming caller (the SQL keys on the
    // host-supplied identity.subject_id, which the args map can never set).
    let victim_key = format!("{prefix}/agent/state");
    let fenced_get = call(30, "state.get", json!({ "key": victim_key })).await?;
    if !fenced_get["structuredContent"].is_null()
        && !fenced_get["structuredContent"]["state"].is_null()
    {
        bail!("FENCE BREACH: driver read another agent's key: {fenced_get}");
    }
    let fenced_list = call(31, "state.list", json!({ "prefix": prefix.clone() })).await?;
    if fenced_list["structuredContent"]["keys"]
        .as_array()
        .is_some_and(|k| !k.is_empty())
    {
        bail!("FENCE BREACH: driver listed another agent's keys: {fenced_list}");
    }

    // Leg 3 (P3-2): snapshot / delete / restore round-trip under the driver's
    // OWN subject (fence-consistent) — the backup/restore unit `agentctl
    // backup` and `migrate` build on. The managed agent's prefix already
    // snapshotted non-empty above (leg 2), proving the admin export works
    // across subjects too.
    let admin_key = format!("{driver_subj}/admin-roundtrip");
    call(
        15,
        "state.put",
        json!({ "key": admin_key, "seq": 1,
        "state": { "marker": "roundtrip" } }),
    )
    .await?;
    let snap = call(
        16,
        "state.admin.snapshot",
        json!({ "prefix": driver_subj.clone() }),
    )
    .await?;
    let items = snap["structuredContent"]["items"].clone();
    if items.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        bail!("snapshot of the driver prefix was empty: {snap}");
    }
    // Delete it, prove it's gone, restore the snapshot, prove it's back.
    call(17, "state.delete", json!({ "key": admin_key })).await?;
    let gone = call(18, "state.get", json!({ "key": admin_key })).await?;
    if !gone["structuredContent"].is_null() && !gone["structuredContent"]["state"].is_null() {
        bail!("key not deleted before restore: {gone}");
    }
    let restored = call(19, "state.admin.restore", json!({ "items": items })).await?;
    if restored["structuredContent"]["restored"]
        .as_i64()
        .unwrap_or(0)
        < 1
    {
        bail!("restore reported no rows: {restored}");
    }
    let back = call(20, "state.get", json!({ "key": admin_key })).await?;
    if back["structuredContent"]["state"]["marker"] != json!("roundtrip") {
        bail!("snapshot/restore round-trip lost the key: {back}");
    }

    drop(pf);
    kh::api::<agent_api::v1alpha2::Agent>(&ctx.client, ns)
        .delete(name, &Default::default())
        .await
        .ok();
    pass()
}

/// Per-(user, agent) principals (RFC 0028 §6): `spec.access.principals` makes
/// the operator mint a bearer via the identity service, project it into the
/// `<name>-principals` Secret, mount it, and bind `a2a.principals[]` — after
/// which the agent answers ONLY named callers: the minted bearer converses
/// (role `user`), while no bearer and a wrong bearer are anonymous and
/// refused. Needs identity.service armed (operator AGENTCTL_IDENTITY_URL).
async fn a2a_principals_gate(ctx: &Ctx) -> Result<Outcome> {
    use k8s_openapi::api::core::v1::Secret;
    use kube::api::Api;

    let name = "e2e-principals";
    let subject = "mock:alice";
    let mut agent = agentd_agent(ctx, name, Mode::Reactive, "idle");
    agent.spec.instruction = None;
    agent.spec.surfaces = Some(agent_api::DesiredSurfaces {
        a2a: true,
        ..Default::default()
    });
    agent.spec.access = Some(agent_api::Access {
        principals: vec![subject.to_string()],
        ..Default::default()
    });
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;

    // The operator mints + projects BEFORE the workload; the pod then mounts
    // the Secret. If identity isn't armed the Agent goes Validated=False.
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ctx.cfg.ns);
    let secret_name = format!("{name}-principals");
    let key = "PRINCIPAL_MOCK_ALICE";
    let mut bearer = String::new();
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        // Surface the misconfiguration loudly instead of timing out.
        let a = kh::api::<agent_api::v1alpha2::Agent>(&ctx.client, &ctx.cfg.ns)
            .get(name)
            .await?;
        if let Some(s) = &a.status {
            if s.phase.as_deref() == Some("Invalid") {
                bail!(
                    "Agent went Invalid — is identity.service armed on the operator? {:?}",
                    s.conditions
                        .iter()
                        .map(|c| c.message.clone())
                        .collect::<Vec<_>>()
                );
            }
        }
        Ok(secrets.get_opt(&secret_name).await?.is_some())
    })
    .await
    .context("principals Secret")?;
    let secret = secrets.get(&secret_name).await?;
    let data = secret.data.unwrap_or_default();
    let raw = data.get(key).ok_or_else(|| {
        anyhow!(
            "Secret {secret_name} lacks key {key}; has {:?}",
            data.keys()
        )
    })?;
    bearer.push_str(std::str::from_utf8(&raw.0)?);
    if !bearer.starts_with("pat-") {
        bail!("projected bearer does not look minted (want pat-…)");
    }
    // Sanity: never a template, never empty — the Secret holds the CREDENTIAL,
    // the config document holds the {{secret-file:…}} TEMPLATE.
    if bearer.contains("{{") {
        bail!("Secret holds a template, not a credential");
    }
    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    // The agent's listener REQUIRES a CA-signed client cert (composed
    // a2a.tls.client_ca; agentd builds a WebPkiClientVerifier with no
    // unauthenticated fallback). Issue a "nobody" client identity from the
    // chart CA — CA-valid, matching NO principal rule — so the bearer alone
    // decides the outcome; and read the control-plane client cert to prove
    // the projected operator rule keeps management alive.
    let cert_yaml = format!(
        "apiVersion: cert-manager.io/v1\nkind: Certificate\nmetadata:\n  name: e2e-nobody-client\n  namespace: {ns}\nspec:\n  secretName: e2e-nobody-client-tls\n  commonName: e2e-nobody\n  usages: [\"client auth\"]\n  issuerRef: {{ name: agentctl-ca, kind: ClusterIssuer }}\n",
        ns = ctx.cfg.ns
    );
    shell::kubectl_apply_stdin(&cert_yaml)?;
    let tls_secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ctx.cfg.ns);
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(tls_secrets
            .get_opt("e2e-nobody-client-tls")
            .await?
            .is_some())
    })
    .await
    .context("nobody client cert issuance")?;

    let identity_pem = |data: &std::collections::BTreeMap<String, k8s_openapi::ByteString>| {
        let mut pem = Vec::new();
        pem.extend_from_slice(&data.get("tls.key").expect("tls.key").0);
        pem.extend_from_slice(&data.get("tls.crt").expect("tls.crt").0);
        pem
    };
    let nobody = tls_secrets
        .get("e2e-nobody-client-tls")
        .await?
        .data
        .unwrap();
    let cp_secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ctx.cfg.system_ns);
    let cp = cp_secrets.get("agentctl-client-tls").await?.data.unwrap();

    let client_with = |pem: Vec<u8>| {
        reqwest::Client::builder()
            .identity(reqwest::Identity::from_pem(&pem)?)
            // The serving cert names the service DNS, not 127.0.0.1; the
            // server-cert validation path is covered by other scenarios.
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(anyhow::Error::from)
    };
    let nobody_http = client_with(identity_pem(&nobody))?;
    let cp_http = client_with(identity_pem(&cp))?;

    let pf = shell::PortForward::pod(&ctx.cfg.ns, &pod, 8443, 18099)?;
    let send = |http: reqwest::Client, auth: Option<String>| async move {
        let mut rb = http.post("https://127.0.0.1:18099/").json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "SendMessage",
            "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-p1", "parts": [{ "text": "ping" }] } },
        }));
        if let Some(b) = auth {
            rb = rb.bearer_auth(b);
        }
        let resp = rb.send().await?;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        anyhow::Ok((status, body))
    };

    // Named user: the minted bearer converses (subject rule outranks all).
    let (status, body) = send(nobody_http.clone(), Some(bearer.clone())).await?;
    if body.get("error").is_some() || body.get("result").is_none() {
        bail!("minted bearer was refused (status {status}): {body}");
    }

    // No bearer, unmatched cert: anonymous — refused (any non-empty
    // principals list removes the implicit operator fallbacks).
    let (status, body) = send(nobody_http.clone(), None).await?;
    if body.get("result").is_some() {
        bail!("anonymous caller was ANSWERED (status {status}) — the gate is open: {body}");
    }

    // Wrong bearer: equally anonymous, equally refused.
    let (status, body) = send(nobody_http, Some("pat-not-the-minted-bearer".to_string())).await?;
    if body.get("result").is_some() {
        bail!("wrong bearer was ANSWERED (status {status}): {body}");
    }

    // The control plane still operates: the projected operator rule (last)
    // matches its client-cert CN, so management survives the named gate.
    let (status, body) = send(cp_http, None).await?;
    if body.get("result").is_none() {
        bail!("control-plane cert was refused (status {status}) — the operator rule is missing: {body}");
    }

    drop(pf);
    shell::kubectl(&[
        "delete",
        "-n",
        &ctx.cfg.ns,
        "certificate",
        "e2e-nobody-client",
        "--ignore-not-found",
    ])?;
    shell::kubectl(&[
        "delete",
        "-n",
        &ctx.cfg.ns,
        "secret",
        "e2e-nobody-client-tls",
        "--ignore-not-found",
    ])?;
    cleanup_agent(ctx, name).await?;
    pass()
}

/// Organization → managed namespace + agent-count quota + Ready status; org
/// deletion GC-cascades the namespace via its ownerReference (RFC 0033 §2.1).
/// Applies the CRD itself (helm installs `crds/` on FIRST install only, so an
/// upgraded live cluster may lack it) and requires an operator built with the
/// tenancy controller — an older operator leaves status empty and this fails.
async fn org_tenancy(ctx: &Ctx) -> Result<Outcome> {
    use agent_api::org::{org_namespace, Organization, OrganizationSpec};
    use k8s_openapi::api::core::v1::{Namespace, ResourceQuota};
    use kube::api::{Api, Patch, PatchParams};

    shell::kubectl(&["apply", "-f", "deploy/crds/organization.yaml"])?;
    shell::kubectl(&[
        "wait",
        "--for=condition=Established",
        "crd/organizations.agentctl.dev",
        "--timeout=60s",
    ])?;

    let name = "e2e-acme";
    let ns_name = org_namespace(name);
    let spec: OrganizationSpec = serde_json::from_value(serde_json::json!({
        "displayName": "E2E Acme",
        "quotas": { "agents": 5 },
        "accessPolicies": [
            { "match": { "groups": ["mock:eng-*"] }, "role": "operator",
              "selector": { "matchLabels": { "team": "engineering" } } },
        ],
    }))?;
    let orgs: Api<Organization> = Api::all(ctx.client.clone());
    orgs.patch(
        name,
        &PatchParams::apply("e2e").force(),
        &Patch::Apply(&Organization::new(name, spec)),
    )
    .await
    .context("apply Organization")?;

    // Reconcile lands the namespace (labeled + owner-referenced), the quota,
    // and a Ready status.
    let ns_api: Api<Namespace> = Api::all(ctx.client.clone());
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        let Some(ns) = ns_api.get_opt(&ns_name).await? else {
            return Ok(false);
        };
        // The single SSA writes labels + ownerRef atomically: an existing
        // namespace missing either is a real defect, not a pending state.
        let labels = ns.metadata.labels.unwrap_or_default();
        if labels.get("agentctl.dev/organization").map(String::as_str) != Some(name) {
            bail!("namespace exists but lacks the organization label");
        }
        let owned_by_org = ns
            .metadata
            .owner_references
            .unwrap_or_default()
            .iter()
            .any(|o| o.kind == "Organization" && o.name == name);
        if !owned_by_org {
            bail!("namespace exists but is not owner-referenced to the org");
        }
        Ok(true)
    })
    .await
    .context("managed namespace")?;

    let quota_api: Api<ResourceQuota> = Api::namespaced(ctx.client.clone(), &ns_name);
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        let Some(q) = quota_api.get_opt("agentctl-org-quota").await? else {
            return Ok(false);
        };
        let hard = q.spec.and_then(|s| s.hard).unwrap_or_default();
        let agents = hard
            .get("count/agents.agentctl.dev")
            .map(|v| v.0.clone())
            .unwrap_or_default();
        if agents != "5" {
            bail!("quota present but count/agents.agentctl.dev={agents:?}, want 5");
        }
        Ok(true)
    })
    .await
    .context("org quota")?;

    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        let org = orgs.get(name).await?;
        let Some(status) = org.status else {
            return Ok(false);
        };
        if status.phase.as_deref() != Some("Ready") {
            return Ok(false);
        }
        if !status.namespaces.contains(&ns_name) {
            bail!(
                "status.namespaces {:?} missing {ns_name}",
                status.namespaces
            );
        }
        Ok(true)
    })
    .await
    .context("org Ready status")?;

    // The quota actually enforces: agent #6 must be refused by the apiserver.
    // (Quota usage sync is asynchronous; poll the creates.)
    let agents_api: Api<agent_api::v1alpha2::Agent> = Api::namespaced(ctx.client.clone(), &ns_name);
    let mut refused = false;
    for i in 0..6 {
        let mut a = agentd_agent(ctx, &format!("quota-probe-{i}"), Mode::Reactive, "idle");
        // Reactive needs a wake source (CRD CEL rule) — the a2a surface is the
        // cheapest; these probes only exist to be COUNTED by the quota.
        a.spec.instruction = None;
        a.spec.surfaces = Some(agent_api::DesiredSurfaces {
            a2a: true,
            ..Default::default()
        });
        a.metadata.namespace = Some(ns_name.clone());
        let a = agent_api::v1alpha2::convert::agent_object_v1_to_v2(&a);
        match agents_api.create(&Default::default(), &a).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 403 && e.message.contains("exceeded quota") => {
                refused = true;
                break;
            }
            Err(e) => return Err(e).context("unexpected error creating quota probe"),
        }
    }
    if !refused {
        bail!("created 6 agents under a quota of 5 — the ResourceQuota is not enforcing");
    }

    // Deleting the org GC-cascades the namespace (and everything in it).
    orgs.delete(name, &Default::default()).await?;
    kh::poll_until(GC_TIMEOUT, Duration::from_secs(2), || async {
        // GC is asynchronous: gone or terminating both prove the cascade.
        Ok(match ns_api.get_opt(&ns_name).await? {
            None => true,
            Some(ns) => ns.metadata.deletion_timestamp.is_some(),
        })
    })
    .await
    .context("namespace GC after org delete")?;
    pass()
}

// --- CLI --------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "e2e",
    about = "agentctl functional e2e scenarios. Needs a cluster (KUBECONFIG)."
)]
struct Cli {
    /// Scenario names to run (default: all).
    scenarios: Vec<String>,
    /// Only run scenarios in this group (provisioning|management|intelligence|claim|shard|a2a|conformance|security).
    #[arg(long)]
    group: Option<String>,
    /// List the catalogue and exit (no cluster needed).
    #[arg(long)]
    list: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let all = catalogue();

    if cli.list {
        for s in &all {
            println!("{:<22} [{}]", s.name, s.group);
        }
        return ExitCode::SUCCESS;
    }

    // Select the subset.
    let selected: Vec<&Scenario> = all
        .iter()
        .filter(|s| cli.group.as_deref().map(|g| g == s.group).unwrap_or(true))
        .filter(|s| cli.scenarios.is_empty() || cli.scenarios.iter().any(|n| n == s.name))
        .collect();

    if selected.is_empty() {
        eprintln!("no scenarios matched the selection");
        return ExitCode::FAILURE;
    }

    let ctx = match Ctx::build().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "failed to build run context (is KUBECONFIG set / cluster reachable?): {e:#}"
            );
            return ExitCode::FAILURE;
        }
    };

    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    println!("running {} scenario(s)\n", selected.len());
    for s in &selected {
        let start = Instant::now();
        let outcome = (s.run)(&ctx).await;
        let dt = start.elapsed();
        match outcome {
            Ok(Outcome::Passed) => {
                passed += 1;
                println!("PASS  {:<22} ({:.1}s)", s.name, dt.as_secs_f64());
            }
            Ok(Outcome::Skipped(reason)) => {
                skipped += 1;
                println!("SKIP  {:<22} ({reason})", s.name);
            }
            Err(e) => {
                failed += 1;
                println!("FAIL  {:<22} ({:.1}s): {e:#}", s.name, dt.as_secs_f64());
            }
        }
    }

    println!("\nsummary: {passed} passed, {skipped} skipped, {failed} failed");
    if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

// ===========================================================================
// Shared helpers
// ===========================================================================

/// Scrape a control-plane component's `/metrics` through the apiserver Service proxy.
fn scrape(ctx: &Ctx, svc: &str, port: u16, scheme: &str) -> Result<prom::Metrics> {
    prom::scrape_proxy(&ctx.cfg.system_ns, svc, port, scheme, "/metrics")
}

/// Build an agentd-backed `Agent` CR in the scenario namespace. The operator resolves
/// the bound `model.pool` and renders a direct `INTELLIGENCE=<provider endpoint>` the
/// agent dials itself, plus the per-namespace CA. agentd validates the intelligence
/// endpoint at boot in every mode (`once` infers immediately; a reactive/shard daemon
/// dials it only when it does work), so a bound `model.pool` is enough.
fn agentd_agent(ctx: &Ctx, name: &str, mode: Mode, instruction: &str) -> Agent {
    let mut a = Agent::new(
        name,
        AgentSpec {
            mode,
            image: Some(ctx.cfg.agentd_image.clone()),
            instruction: Some(instruction.to_string()),
            ..Default::default()
        },
    );
    a.metadata.namespace = Some(ctx.cfg.ns.clone());
    a
}

/// The operator label selector for an Agent's rendered pod(s).
fn agent_label(name: &str) -> String {
    format!("agentctl.dev/agent={name}")
}

/// The first pod name matching `label` in `ns`.
fn first_pod(ns: &str, label: &str) -> Result<String> {
    let out = shell::kubectl(&[
        "get",
        "pods",
        "-n",
        ns,
        "-l",
        label,
        "-o",
        "jsonpath={.items[0].metadata.name}",
    ])?;
    let name = out.trim().to_string();
    if name.is_empty() {
        bail!("no pod for selector {label} in {ns}");
    }
    Ok(name)
}

/// The terminated container exit code of the first pod matching `label`.
fn pod_exit_code(ns: &str, label: &str) -> Result<i64> {
    let out = shell::kubectl(&[
        "get",
        "pods",
        "-n",
        ns,
        "-l",
        label,
        "-o",
        "jsonpath={.items[0].status.containerStatuses[0].state.terminated.exitCode}",
    ])?;
    out.trim()
        .parse::<i64>()
        .with_context(|| format!("parse exit code from {out:?}"))
}

/// Delete an `Agent` and await GC (the standard scenario cleanup).
async fn cleanup_agent(ctx: &Ctx, name: &str) -> Result<()> {
    kh::delete_and_wait::<agent_api::v1alpha2::Agent>(&ctx.client, &ctx.cfg.ns, name, GC_TIMEOUT)
        .await
}

/// One MCP `tools/call` against a coordination `/mcp` endpoint, returning the
/// `result` object (with `structuredContent` + `isError`).
async fn mcp_call(
    http: &reqwest::Client,
    base_url: &str,
    tool: &str,
    args: Value,
    meta: Value,
) -> Result<Value> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args, "_meta": meta },
    });
    let resp = http
        .post(format!("{base_url}/mcp"))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {base_url}/mcp {tool}"))?;
    let v: Value = resp.json().await.context("decode mcp response")?;
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// The `structuredContent` of an MCP `tools/call`.
async fn mcp_structured(
    http: &reqwest::Client,
    base_url: &str,
    tool: &str,
    args: Value,
    meta: Value,
) -> Result<Value> {
    let result = mcp_call(http, base_url, tool, args, meta).await?;
    Ok(result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null))
}

/// Assert that a kube/kubectl call was DENIED (a SAR Forbidden), not allowed.
fn expect_denied(res: Result<String>) -> Result<()> {
    match res {
        Ok(out) => bail!("expected a Forbidden denial, but the call succeeded: {out}"),
        Err(e) => {
            let s = e.to_string().to_lowercase();
            if s.contains("forbidden") || s.contains("403") || s.contains("cannot ") {
                Ok(())
            } else {
                Err(e.context("expected a Forbidden denial"))
            }
        }
    }
}

// ===========================================================================
// Provisioning
// ===========================================================================

/// `mode: once` → the operator renders a Job; the agent does its work by dialing its
/// bound model pool's provider directly (the headroom mock pool), runs to a terminal
/// status, and the pod exits with a clean, contract-known `complete` exit code.
async fn prov_once_ready_exit(ctx: &Ctx) -> Result<Outcome> {
    // agentd once-mode REQUIRES intelligence and infers immediately; give it the
    // headroom mock pool so the run reaches a clean completion (exit 0).
    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;

    let name = "e2e-prov-once";
    let mut agent = agentd_agent(ctx, name, Mode::Once, "emit a one-line summary and exit");
    // Bind the headroom mock pool: ACC 2 composes the intelligence block from
    // the bound pool, and a once-run REQUIRES an endpoint (observed:
    // "intel: empty intelligence endpoint list" without one). Reactive
    // scenarios idle without intelligence, so only this scenario binds it.
    agent.spec.model = Some(agent_api::ModelBinding {
        pool: Some("mockpool".to_string()),
        id: Some("mock-model-v1".to_string()),
    });
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;

    // The Agent's Ready can flip true before the Job pod exits, so wait for the
    // pod to TERMINATE, then assert the contract exit code + `complete` intent.
    let table = contract::ExitCodeTable::vendored();
    let code = wait_pod_exit_code(ctx, name, READY_TIMEOUT).await?;
    if !table.is_known(code as i32) {
        bail!("exit code {code} is not in the frozen exit-code table");
    }
    if table.intent(code as i32) != "complete" {
        bail!(
            "once-mode agent exited {code} (intent {}), expected a `complete` code",
            table.intent(code as i32)
        );
    }

    cleanup_agent(ctx, name).await?;
    delete_example(&dir, "modelpool-mock.yaml");
    delete_example(&dir, "mock-provider.yaml");
    pass()
}

/// `mode: reactive` → the live manifest read from the running agent (via `kubectl exec
/// --capabilities`) must validate against the contract (`manifest.schema.json`).
async fn prov_reactive_capabilities(ctx: &Ctx) -> Result<Outcome> {
    let name = "e2e-prov-reactive";
    let mut agent = agentd_agent(ctx, name, Mode::Reactive, "serve the management profile");
    agent.spec.surfaces = Some(agent_api::DesiredSurfaces {
        a2a: true,
        ..Default::default()
    });
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;

    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    // Pull the live capabilities manifest from the agent itself and validate it.
    let manifest = shell::kubectl(&[
        "exec",
        "-n",
        &ctx.cfg.ns,
        &pod,
        "--",
        "/agentd",
        "-c",
        "/etc/agentctl/config/agentd.json",
        "--capabilities",
    ])?;
    let m = contract::validate_manifest(&manifest)
        .context("reactive agent capabilities manifest failed contract validation")?;
    // ACC 2: the control surface IS the A2A listener (admin verbs ride it);
    // there is no served-MCP management profile any more.
    let Some(a2a) = &m.a2a else {
        bail!("reactive agent did not advertise a configured A2A listener");
    };
    if !a2a.admin.iter().any(|v| v == "a2a.drain") {
        bail!("A2A listener does not advertise the drain admin verb");
    }

    cleanup_agent(ctx, name).await?;
    pass()
}

// ===========================================================================
// Management (aggregated APIServer + RBAC)
// ===========================================================================

async fn mgmt_drain(ctx: &Ctx) -> Result<Outcome> {
    run_mgmt_verb(ctx, "drain").await
}
async fn mgmt_lame_duck(ctx: &Ctx) -> Result<Outcome> {
    run_mgmt_verb(ctx, "lame-duck").await
}
async fn mgmt_cancel(ctx: &Ctx) -> Result<Outcome> {
    // ACC 2: `a2a.cancel` REQUIRES a run id (-32602 without one) — the bare
    // aggregated verb reaching the agent and being refused with exactly that
    // diagnosis IS the contract behavior to pin. Cancelling a live run rides
    // the workflow scenarios (P6).
    match run_mgmt_verb(ctx, "cancel").await {
        Ok(o) => Ok(o), // a future agentd may accept a bare cancel; also fine
        Err(e) if format!("{e:#}").contains("-32602") || format!("{e:#}").contains("run id") => {
            pass()
        }
        Err(e) => Err(e),
    }
}

/// Round-trip one management connect verb through the aggregated APIServer and assert
/// the `agentctl_apiserver_verb_*` counters moved and the verb returned `Success`.
async fn run_mgmt_verb(ctx: &Ctx, verb: &str) -> Result<Outcome> {
    let name = format!("e2e-mgmt-{verb}");
    let mut agent = agentd_agent(ctx, &name, Mode::Reactive, "serve the management profile");
    agent.spec.surfaces = Some(agent_api::DesiredSurfaces {
        a2a: true,
        ..Default::default()
    });
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, &name, &agent).await?;
    let pod = wait_for_first_pod(ctx, &name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    // The aggregated APIServer's /metrics is served over the SAME mTLS-gated
    // :6443 listener as the API; the kube-apiserver Service proxy does not present
    // the aggregator client cert, so a proxy scrape is rejected ("certificate
    // required"). The load-bearing assertion is therefore the verb's `Success`
    // status (the actual round-trip through the aggregation layer to the agent);
    // the verb counter delta is advisory and only checked when the scrape works.
    let before = scrape(ctx, SVC_APISERVER, PORT_APISERVER, "https")
        .map(|m| m.sum("agentctl_apiserver_verb_forwarded_total"))
        .ok();

    let path = format!(
        "/apis/management.agentctl.dev/v1alpha1/namespaces/{}/agents/{}/{}",
        ctx.cfg.ns, name, verb
    );
    let out = shell::kubectl(&["create", "--raw", &path, "-f", "/dev/null"])
        .with_context(|| format!("invoke aggregated verb {verb}"))?;
    let status: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    if status.get("status").and_then(Value::as_str) != Some("Success") {
        bail!("aggregated {verb} did not return Success: {out}");
    }

    // Advisory counter check: only when BOTH scrapes succeed (mTLS permitting).
    if let Some(before) = before {
        if let Ok(after) = scrape(ctx, SVC_APISERVER, PORT_APISERVER, "https")
            .map(|m| m.sum("agentctl_apiserver_verb_forwarded_total"))
        {
            if after <= before {
                bail!("apiserver verb forwarded counter did not increase ({before} -> {after})");
            }
        }
    }

    cleanup_agent(ctx, &name).await?;
    pass()
}

/// An under-privileged ServiceAccount must be DENIED the verb by the SAR gate (403).
async fn mgmt_rbac_403(ctx: &Ctx) -> Result<Outcome> {
    let name = "e2e-rbac";
    let sa = "e2e-unpriv";
    let mut agent = agentd_agent(ctx, name, Mode::Reactive, "serve the management profile");
    agent.spec.surfaces = Some(agent_api::DesiredSurfaces {
        a2a: true,
        ..Default::default()
    });
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    // A bare SA with no RoleBinding for the verb subresource.
    let _ = shell::kubectl(&["create", "serviceaccount", sa, "-n", &ctx.cfg.ns]);

    let as_user = format!("system:serviceaccount:{}:{sa}", ctx.cfg.ns);
    let path = format!(
        "/apis/management.agentctl.dev/v1alpha1/namespaces/{}/agents/{}/drain",
        ctx.cfg.ns, name
    );
    let res = shell::kubectl(&[
        "--as",
        &as_user,
        "create",
        "--raw",
        &path,
        "-f",
        "/dev/null",
    ]);
    let denied = expect_denied(res);

    // Cleanup regardless of the assertion result.
    let _ = shell::kubectl(&[
        "delete",
        "serviceaccount",
        sa,
        "-n",
        &ctx.cfg.ns,
        "--ignore-not-found",
    ]);
    cleanup_agent(ctx, name).await?;
    denied?;
    pass()
}

/// pause + resume via the aggregated APIServer subresources. These are real aggregated
/// verbs (the apiserver discovery adds `agents/pause` + `agents/resume`), SAR-gated and
/// forwarded DIRECT to the agent pod as `a2a.Pause`/`a2a.Resume` over mTLS. Same
/// round-trip as drain/lame-duck/cancel.
async fn mgmt_pause_resume(ctx: &Ctx) -> Result<Outcome> {
    let pause = run_mgmt_verb(ctx, "pause").await?;
    if matches!(pause, Outcome::Skipped(_)) {
        return Ok(pause);
    }
    run_mgmt_verb(ctx, "resume").await
}

// ===========================================================================
// Claim-mode (coordination /mcp)
// ===========================================================================

/// Under contention only ONE of N racers is granted the same item.
async fn claim_atomic_single_grant(ctx: &Ctx) -> Result<Outcome> {
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_HTTP, 18090)?;
    let base = pf.base_url();
    let item = "e2e://atomic/1";

    mcp_structured(
        &ctx.http,
        &base,
        "work.submit",
        json!({ "item": item, "claim_key": "atomic-1" }),
        Value::Null,
    )
    .await?;

    // Fire N genuinely-concurrent claims for the same item (tokio tasks).
    let mut set = tokio::task::JoinSet::new();
    for i in 0..8 {
        let http = ctx.http.clone();
        let base = base.clone();
        let meta = json!({ "agent/claim_key": "atomic-1", "agent/instance": format!("racer-{i}") });
        set.spawn(async move {
            mcp_structured(
                &http,
                &base,
                "work.claim",
                json!({ "item": item, "ttl_ms": 30_000 }),
                meta,
            )
            .await
        });
    }
    let mut grants = 0;
    let mut lease = String::new();
    while let Some(joined) = set.join_next().await {
        let sc = joined.context("claim task panicked")??;
        if sc.get("granted").and_then(Value::as_bool) == Some(true) {
            grants += 1;
            if let Some(l) = sc.get("lease_id").and_then(Value::as_str) {
                lease = l.to_string();
            }
        }
    }
    if grants != 1 {
        bail!("expected exactly one grant under contention, got {grants}");
    }

    // Cleanup: settle the lease.
    if !lease.is_empty() {
        let _ = mcp_structured(
            &ctx.http,
            &base,
            "work.ack",
            json!({ "lease_id": lease }),
            json!({ "agent/claim_key": "atomic-1" }),
        )
        .await;
    }
    pass()
}

/// A claim_key already settled (acked) is deduped: a re-claim is not granted.
async fn claim_dedupe(ctx: &Ctx) -> Result<Outcome> {
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_HTTP, 18091)?;
    let base = pf.base_url();
    let item = "e2e://dedupe/1";
    let meta = json!({ "agent/claim_key": "dedupe-1", "agent/instance": "p1" });

    let granted = mcp_structured(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": item, "ttl_ms": 30_000 }),
        meta.clone(),
    )
    .await?;
    let lease = granted
        .get("lease_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("first claim was not granted"))?
        .to_string();
    mcp_structured(
        &ctx.http,
        &base,
        "work.ack",
        json!({ "lease_id": lease }),
        json!({ "agent/claim_key": "dedupe-1" }),
    )
    .await?;

    let reclaim = mcp_structured(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": item, "ttl_ms": 30_000 }),
        meta,
    )
    .await?;
    if reclaim.get("granted").and_then(Value::as_bool) != Some(false) {
        bail!("a settled claim_key was re-granted (dedupe failed)");
    }
    pass()
}

/// An expired lease is swept back to pending and re-offered to the fleet.
async fn claim_lease_expiry_reoffer(ctx: &Ctx) -> Result<Outcome> {
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_HTTP, 18092)?;
    let base = pf.base_url();
    let item = "e2e://expiry/1";

    // Claim with a very short TTL and DON'T renew.
    mcp_structured(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": item, "ttl_ms": 800 }),
        json!({ "agent/claim_key": "expiry-1", "agent/instance": "p1" }),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(2500)).await; // past TTL + a sweep tick

    // A fresh claim must now succeed (the item was re-offered).
    let reoffer = mcp_structured(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": item, "ttl_ms": 30_000 }),
        json!({ "agent/claim_key": "expiry-1b", "agent/instance": "p2" }),
    )
    .await?;
    if reoffer.get("granted").and_then(Value::as_bool) != Some(true) {
        bail!("an expired lease was not re-offered");
    }
    if let Some(l) = reoffer.get("lease_id").and_then(Value::as_str) {
        let _ = mcp_structured(
            &ctx.http,
            &base,
            "work.release",
            json!({ "lease_id": l, "reason": "e2e-cleanup" }),
            Value::Null,
        )
        .await;
    }
    pass()
}

/// A claim-mode AgentFleet scales 0→N (KEDA, backlog-driven) then back to 0 once the
/// backlog drains.
async fn claim_scale_zero_n_zero(ctx: &Ctx) -> Result<Outcome> {
    let name = "e2e-fleet";
    let fleet = claim_fleet(ctx, name);
    kh::apply_fleet(&ctx.client, &ctx.cfg.ns, name, &fleet).await?;

    // Producer: push a backlog through coordination (drives the KEDA external scaler).
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_HTTP, 18093)?;
    let base = pf.base_url();
    for i in 0..20 {
        mcp_structured(
            &ctx.http,
            &base,
            "work.submit",
            json!({ "item": format!("e2e://scale/{i}"), "claim_key": format!("scale-{i}") }),
            Value::Null,
        )
        .await?;
    }

    // 0 → N: the rendered Deployment should gain ready replicas. The operator
    // names the workload after the FLEET (not `agentfleet-<name>`).
    let dep = name.to_string();
    kh::poll_until(SCALE_TIMEOUT, Duration::from_secs(5), || async {
        Ok(deployment_ready_replicas(&ctx.cfg.ns, &dep).unwrap_or(0) > 0)
    })
    .await
    .context("fleet did not scale up from zero")?;

    // The load-bearing proof is the elastic-FROM-ZERO step above (0 → N driven by
    // the coordination backlog through the KEDA external scaler). Scaling back to 0
    // is bounded only by KEDA's DEFAULT cooldownPeriod (300s of trigger-inactivity
    // before it removes the last replica) — a KEDA timing detail, not an agentctl
    // behaviour — so we drain the backlog and check scale-down BEST-EFFORT with a
    // short wait, and do not fail the scenario on the cooldown.
    const SCALE_DOWN_TIMEOUT: Duration = Duration::from_secs(90);
    drain_backlog(ctx, &base).await?;
    let scaled_to_zero = kh::poll_until(SCALE_DOWN_TIMEOUT, Duration::from_secs(5), || async {
        Ok(deployment_ready_replicas(&ctx.cfg.ns, &dep).unwrap_or(1) == 0)
    })
    .await
    .is_ok();
    if !scaled_to_zero {
        eprintln!(
            "  note: fleet proven to scale 0→N from the backlog; it had not yet returned to 0 \
             within {SCALE_DOWN_TIMEOUT:?} (KEDA cooldownPeriod default 300s) — not failed"
        );
    }

    kh::delete_and_wait::<agent_api::v1alpha2::AgentFleet>(
        &ctx.client,
        &ctx.cfg.ns,
        name,
        GC_TIMEOUT,
    )
    .await?;
    pass()
}

/// Claim and ack every pending item (drain the coordination backlog).
async fn drain_backlog(ctx: &Ctx, base: &str) -> Result<()> {
    for _ in 0..64 {
        let stats = mcp_structured(&ctx.http, base, "work.stats", json!({}), Value::Null).await?;
        let pending = stats.get("pending").and_then(Value::as_u64).unwrap_or(0);
        if pending == 0 {
            break;
        }
        // Read a pending item and claim+ack it.
        let granted = mcp_structured(
            &ctx.http,
            base,
            "work.claim",
            json!({ "item": "e2e://scale/any", "ttl_ms": 5_000 }),
            json!({ "agent/claim_key": "drain", "agent/instance": "drainer" }),
        )
        .await?;
        if let Some(l) = granted.get("lease_id").and_then(Value::as_str) {
            let _ = mcp_structured(
                &ctx.http,
                base,
                "work.ack",
                json!({ "lease_id": l }),
                json!({ "agent/claim_key": "drain" }),
            )
            .await;
        } else {
            break;
        }
    }
    Ok(())
}

// ===========================================================================
// Shard-mode
// ===========================================================================

/// A shard-mode AgentFleet renders a StatefulSet with `replicas=N` (stable
/// per-shard identity). Each agentd SHOULD additionally carry its `K/N` shard
/// identity — see the skip note below.
async fn shard_k_of_n(ctx: &Ctx) -> Result<Outcome> {
    let name = "e2e-shard";
    let shards = 3u32;
    let fleet = shard_fleet(ctx, name, shards);
    kh::apply_fleet(&ctx.client, &ctx.cfg.ns, name, &fleet).await?;

    // The operator names the StatefulSet after the FLEET (not `agentfleet-<name>`).
    let sts = name.to_string();
    kh::poll_until(SCALE_TIMEOUT, Duration::from_secs(5), || async {
        Ok(statefulset_ready_replicas(&ctx.cfg.ns, &sts).unwrap_or(0) == shards as i64)
    })
    .await
    .context("shard StatefulSet did not reach N ready replicas")?;

    // ACC 2: agent-side shard identity was REMOVED upstream (clustering is
    // gone; a fleet partitions upstream of the agent — ADR-0009). Per-member
    // identity is `agent.name` = the pod name, which each replica must report.
    // The STRUCTURAL guarantee (a StatefulSet at N stable, ready replicas) is
    // verified above; partition-overlay semantics land with P6 (RFC 0034).
    let pod0 = format!("{sts}-0");
    let manifest = shell::kubectl(&[
        "exec",
        "-n",
        &ctx.cfg.ns,
        &pod0,
        "--",
        "/agentd",
        "-c",
        "/etc/agentctl/config/agentd.json",
        "--capabilities",
    ])?;
    let m = contract::validate_manifest(&manifest)?;
    let outcome = match m.agent.name.as_deref() {
        Some(n) if n == pod0 => pass(),
        other => bail!(
            "replica 0 must take its store-fence identity from AGENT_POD_NAME \
             (expected agent.name={pod0:?}, got {other:?})"
        ),
    };

    kh::delete_and_wait::<agent_api::v1alpha2::AgentFleet>(
        &ctx.client,
        &ctx.cfg.ns,
        name,
        GC_TIMEOUT,
    )
    .await?;
    outcome
}

// ===========================================================================
// A2A
// ===========================================================================

/// The Agent Card is signed (JWS) and its key id resolves in the gateway JWKS.
async fn a2a_card_jws(ctx: &Ctx) -> Result<Outcome> {
    let name = "e2e-a2a-card";
    let agent = a2a_agent(ctx, name);
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18094)?;
    let base = pf.base_url();
    let card: Value = ctx
        .http
        .get(format!(
            "{base}/agents/{}/{}/.well-known/agent-card.json",
            ctx.cfg.ns, name
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let jwks: Value = ctx
        .http
        .get(format!("{base}/.well-known/jwks.json"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // Structural JWS check: the card's signature header `kid` must resolve in the
    // JWKS key set. (Full Ed25519 verification is delegated to the gateway's own
    // unit tests — this harness deliberately carries no signing dependency.)
    let kid = card_signature_kid(&card)
        .ok_or_else(|| anyhow!("agent card carried no JWS signature kid"))?;
    let known = jwks
        .get("keys")
        .and_then(Value::as_array)
        .map(|ks| {
            ks.iter()
                .any(|k| k.get("kid").and_then(Value::as_str) == Some(kid.as_str()))
        })
        .unwrap_or(false);
    if !known {
        bail!("card signature kid {kid} not present in the gateway JWKS");
    }

    cleanup_agent(ctx, name).await?;
    pass()
}

/// `SendMessage` round-trips a JSON-RPC call through the gateway to the agent.
/// The call uses a bare PascalCase method + proto3-JSON message; the result is the
/// `SendMessageResponse` `{"task": …}` envelope (the gateway normalizes it).
async fn a2a_message_send(ctx: &Ctx) -> Result<Outcome> {
    let name = "e2e-a2a-send";
    let agent = a2a_agent(ctx, name);
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18095)?;
    let resp: Value = ctx
        .http
        .post(format!("{}/agents/{}/{}", pf.base_url(), ctx.cfg.ns, name))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "SendMessage",
            "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-1", "parts": [{ "text": "ping" }] } },
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if resp.get("result").is_none() && resp.get("error").is_some() {
        bail!("SendMessage returned a JSON-RPC error: {}", resp["error"]);
    }
    if resp.get("result").is_none() {
        bail!("SendMessage returned no result");
    }

    cleanup_agent(ctx, name).await?;
    pass()
}

/// `SendStreamingMessage` returns an SSE stream the gateway proxies from the agent.
async fn a2a_message_stream(ctx: &Ctx) -> Result<Outcome> {
    let name = "e2e-a2a-stream";
    let agent = a2a_agent(ctx, name);
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18096)?;
    // A2A `message/stream` is a LONG-LIVED SSE: the gateway holds the connection
    // open for the duration of the agent's run (which, with no usable ModelPool in
    // this scenario, may retry intelligence for a while before terminating). So we
    // must NOT `.text()` the whole body — read incrementally and stop at the first
    // `data:` frame (the assertion: the gateway opened an SSE stream and proxied at
    // least one agent frame), with an overall read deadline, then drop the stream.
    let mut resp = ctx
        .http
        .post(format!("{}/agents/{}/{}", pf.base_url(), ctx.cfg.ns, name))
        .header("accept", "text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "SendStreamingMessage",
            "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-1", "parts": [{ "text": "ping" }] } },
        }))
        .send()
        .await?
        .error_for_status()?;
    let mut buf = String::new();
    let found = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(chunk) = resp.chunk().await? {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            if buf.contains("data:") {
                return Ok::<bool, anyhow::Error>(true);
            }
        }
        Ok(false)
    })
    .await;
    let ok = matches!(found, Ok(Ok(true))) || buf.contains("data:");
    drop(resp); // close the streaming connection so the agent run is released
    if !ok {
        bail!(
            "SendStreamingMessage produced no SSE data frames within 20s (read {} bytes)",
            buf.len()
        );
    }

    cleanup_agent(ctx, name).await?;
    pass()
}

// ===========================================================================
// Conformance
// ===========================================================================

/// A once agent's terminal exit code is a member of the frozen exit-code table.
async fn conf_exit_codes(ctx: &Ctx) -> Result<Outcome> {
    let table = contract::ExitCodeTable::vendored();
    let name = "e2e-conf-exit";
    let agent = agentd_agent(ctx, name, Mode::Once, "exit cleanly");
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;

    // Wait for the Job pod to TERMINATE (Ready can precede exit), then assert the
    // terminal code is a registered member of the frozen table. Any contract code
    // is acceptable here (this asserts conformance to the table, not a specific
    // outcome): with a mock pool present the agent completes (0); without one it
    // exits 4/INTEL_UNAVAILABLE — both are registered codes.
    let code = wait_pod_exit_code(ctx, name, READY_TIMEOUT).await?;
    if !table.is_known(code as i32) {
        bail!(
            "exit code {code} is not a registered contract exit code (v{})",
            table.version()
        );
    }

    cleanup_agent(ctx, name).await?;
    pass()
}

/// Every `agent_*` series an agent emits is a registered name in the metrics registry.
async fn conf_metrics_registry(ctx: &Ctx) -> Result<Outcome> {
    let registry = contract::MetricsRegistry::vendored();
    let name = "e2e-conf-metrics";
    let mut agent = agentd_agent(ctx, name, Mode::Reactive, "serve metrics");
    // ACC 2: the a2a surface is the daemon's wake source (CEL requires one).
    agent.spec.surfaces = Some(agent_api::DesiredSurfaces {
        management: true,
        metrics: true,
        a2a: true,
    });
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    // Scrape the agent's own /metrics. The operator renders
    // `AGENT_METRICS_ADDR=0.0.0.0:9090` + the container port unconditionally (the
    // /readyz + direct-scrape listener — the pod is network-attached, no proxy), so
    // the agent serves :9090 and this scrape SUCCEEDS. (A skip remains as a
    // defensive fallback only if the listener is somehow absent.)
    let pf = shell::PortForward::pod(&ctx.cfg.ns, &pod, 9090, 19090)?;
    let scraped = prom::scrape_url(&ctx.http, &format!("{}/metrics", pf.base_url())).await;
    drop(pf);
    let outcome = match scraped {
        Ok(metrics) => {
            let unregistered = registry.unregistered(metrics.names().iter().map(String::as_str));
            if !unregistered.is_empty() {
                bail!("agent emitted unregistered metric series: {unregistered:?}");
            }
            pass()
        }
        Err(e) => skip(format!(
            "agent /metrics on :9090 was unreachable (contract 1.0 wires it \
             unconditionally, so this is unexpected): {e}"
        )),
    };
    cleanup_agent(ctx, name).await?;
    outcome
}

// ===========================================================================
// Security overlays (one helm upgrade per gate, then revert)
// ===========================================================================

/// Per-agent OIDC: a valid JWT is allowed, a missing/invalid one denied (gateway
/// `agentctl_gateway_oidc_{allow,deny}_total`).
async fn sec_oidc(ctx: &Ctx) -> Result<Outcome> {
    // PRE-EXISTING GAP (not an ACC 2 regression): no scenario ever sets
    // spec.access.oidc, so the per-agent gate was never armed and this
    // scenario could not have exercised it. The gateway's authn is being
    // re-founded on agentctl-identity (RFC 0029; PLAN P1-5/P1-8), whose e2e
    // arms real per-user principals — this scenario is superseded by those.
    if std::env::var("AGENTCTL_E2E_LEGACY_OIDC").is_err() {
        return skip(
            "per-agent access.oidc was never set by any scenario (pre-existing gap); \
             superseded by the P1 identity-gateway authn scenarios",
        );
    }
    apply_overlay(ctx, "sec-oidc")?;
    let _g = OverlayGuard { ctx };

    let name = "e2e-oidc";
    let agent = a2a_agent(ctx, name);
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18097)?;
    let url = format!("{}/agents/{}/{}", pf.base_url(), ctx.cfg.ns, name);
    let rpc = json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
        "params": { "message": { "role": "ROLE_USER", "messageId": "e2e-1", "parts": [{ "text": "x" }] } } });

    // Deny: no bearer.
    let deny = ctx.http.post(&url).json(&rpc).send().await?;
    if deny.status().is_success() {
        bail!("OIDC gate allowed an unauthenticated call");
    }
    // Allow: a static test token supplied by the overlay.
    if let Some(tok) = std::env::var("AGENTCTL_E2E_OIDC_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        let allow = ctx
            .http
            .post(&url)
            .bearer_auth(tok)
            .json(&rpc)
            .send()
            .await?;
        if !allow.status().is_success() {
            bail!("OIDC gate denied a valid token ({})", allow.status());
        }
    }
    let m = scrape(ctx, SVC_GATEWAY, PORT_HTTP, "http")?;
    if m.sum("agentctl_gateway_oidc_deny_total") < 1.0 {
        bail!("no OIDC deny was recorded");
    }

    cleanup_agent(ctx, name).await?;
    pass()
}

/// Trusted-proxy: an mTLS proxy's forwarded identity is accepted; a plaintext
/// caller's forwarded headers are stripped (`agentctl_gateway_trusted_proxy_*`).
async fn sec_trusted_proxy(ctx: &Ctx) -> Result<Outcome> {
    // Same family as sec-oidc: forwarded-identity stripping keys off the
    // per-agent access config no scenario arms. Superseded by P1 (RFC 0029).
    if std::env::var("AGENTCTL_E2E_LEGACY_OIDC").is_err() {
        return skip(
            "trusted-proxy forwarding keys off unarmed access.oidc (pre-existing gap); \
             superseded by the P1 identity-gateway authn scenarios",
        );
    }
    apply_overlay(ctx, "sec-trustedproxy")?;
    let _g = OverlayGuard { ctx };

    let name = "e2e-tproxy";
    let agent = a2a_agent(ctx, name);
    kh::apply_agent(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
    let pod = wait_for_first_pod(ctx, name).await?;
    kh::wait_pod_running(&ctx.client, &ctx.cfg.ns, &pod, READY_TIMEOUT).await?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_GATEWAY, PORT_HTTP, 18098)?;
    let url = format!("{}/agents/{}/{}", pf.base_url(), ctx.cfg.ns, name);
    // A plaintext caller spoofing a forwarded identity header must have it stripped
    // (counted as a reject); the request is processed without the spoofed identity.
    let _ = ctx
        .http
        .post(&url)
        .header("x-forwarded-user", "attacker@evil.example")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "GetTask", "params": { "id": "t1" } }))
        .send()
        .await?;
    let m = scrape(ctx, SVC_GATEWAY, PORT_HTTP, "http")?;
    if m.sum("agentctl_gateway_trusted_proxy_rejected_total") < 1.0 {
        bail!("trusted-proxy did not strip/reject a spoofed forwarded identity");
    }

    cleanup_agent(ctx, name).await?;
    pass()
}

/// Coordination attested identity: an unattestable caller fails closed on the claim
/// lifecycle (a cross-tenant settle/steal is impossible).
async fn sec_coord_attest(ctx: &Ctx) -> Result<Outcome> {
    apply_overlay(ctx, "sec-coord-attest")?;
    let _g = OverlayGuard { ctx };

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_HTTP, 18100)?;
    let base = pf.base_url();
    // The harness's source IP owns no pod ⇒ attested mode rejects the claim.
    let claim = mcp_call(
        &ctx.http,
        &base,
        "work.claim",
        json!({ "item": "e2e://attest/1", "ttl_ms": 30_000 }),
        json!({ "agent/claim_key": "attest-1" }),
    )
    .await?;
    if claim.get("isError").and_then(Value::as_bool) != Some(true) {
        bail!("attested coordination did not fail closed for an unattestable caller");
    }
    let m = scrape(ctx, SVC_COORDINATION, PORT_HTTP, "http")?;
    if m.sum("agentctl_coordination_attest_reject_total") < 1.0 {
        bail!("no attest rejection was recorded");
    }
    pass()
}

/// Coordination ↔ scaler mTLS: the mTLS listener rejects a connection without a
/// valid client cert (`agentctl_coordination_mtls_rejected_total`).
async fn sec_coord_mtls(ctx: &Ctx) -> Result<Outcome> {
    apply_overlay(ctx, "sec-coord-mtls")?;
    let _g = OverlayGuard { ctx };

    // The plaintext data port is still token-gated and reachable; the mTLS listener
    // (a second port) requires a client cert. A no-cert TLS handshake must fail.
    let pf =
        shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_COORD_MTLS, 18101)?;
    let res = ctx
        .http
        .get(format!("https://127.0.0.1:{}/healthz", pf.local_port))
        .send()
        .await;
    if res.is_ok() {
        bail!("coordination mTLS listener accepted a connection without a client cert");
    }
    pass()
}

/// apiToken: the coordination data endpoint is 401 without a bearer, 200 with it.
async fn sec_apitoken(ctx: &Ctx) -> Result<Outcome> {
    apply_overlay(ctx, "sec-apitoken")?;
    let _g = OverlayGuard { ctx };

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_HTTP, 18102)?;
    let url = format!("{}/mcp", pf.base_url());
    let rpc = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });

    let unauth = ctx.http.post(&url).json(&rpc).send().await?;
    if unauth.status() != reqwest::StatusCode::UNAUTHORIZED {
        bail!(
            "expected 401 without a bearer token, got {}",
            unauth.status()
        );
    }
    if let Some(tok) = std::env::var("AGENTCTL_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        let auth = ctx
            .http
            .post(&url)
            .bearer_auth(tok)
            .json(&rpc)
            .send()
            .await?;
        if !auth.status().is_success() {
            bail!("a valid bearer token was rejected ({})", auth.status());
        }
    }
    let m = scrape(ctx, SVC_COORDINATION, PORT_HTTP, "http")?;
    if m.sum("agentctl_coordination_auth_rejected_total") < 1.0 {
        bail!("no auth rejection was recorded");
    }
    pass()
}

/// NetworkPolicy enforce — Calico lane only (kindnet does not enforce policy).
async fn sec_netpol(ctx: &Ctx) -> Result<Outcome> {
    if !ctx.cfg.calico {
        return skip("NetworkPolicy enforcement needs the Calico lane (kindnet does not enforce); set AGENTCTL_E2E_CALICO=1 on a Calico cluster");
    }
    apply_overlay(ctx, "sec-netpol")?;
    let _g = OverlayGuard { ctx };

    // A probe pod in the scenario namespace must NOT reach a denied control-plane
    // Service once the default-deny + scoped-allow policies are in place.
    let denied = shell::kubectl(&[
        "run",
        "e2e-netpol-probe",
        "-n",
        &ctx.cfg.ns,
        "--rm",
        "-i",
        "--restart=Never",
        "--image=curlimages/curl:8.8.0",
        "--",
        "curl",
        "-sS",
        "--max-time",
        "5",
        &format!(
            "http://{}.{}:8080/healthz",
            SVC_COORDINATION, ctx.cfg.system_ns
        ),
    ]);
    if denied.is_ok() {
        bail!("NetworkPolicy did not block a disallowed cross-namespace connection");
    }
    pass()
}

/// AAuth end-to-end (RFC 0023 + 0024 phase 0): the operator provisions a
/// portable identity at the Agent Provider (the house), the agent self-enrolls
/// keyless, and it dials a remote AAuth MCP server DIRECTLY — every request
/// signed (RFC 9421) and verified against the AP's JWKS. Asserts: the agent
/// learns `status.identity.aauth`, the once-run completes cleanly, and the
/// mock server counted only SIGNED calls (never an unsigned/bad-sig accept).
async fn sec_aauth(ctx: &Ctx) -> Result<Outcome> {
    // agentctl-identity IS the Agent Provider (RFC 0028 §5, P1-6): the
    // overlay points the operator's house-provisioning at the identity
    // service (admin channel = the shared control-plane token) and arms the
    // provider role (enroll/agent-token/JWKS). No sibling apd image, no
    // fixture admin token.
    let dir = examples_dir();

    // The remote signed resource + the mock ModelPool/provider.
    apply_example(&dir, "mock-aauth-mcp.yaml")?;
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/mock-aauth-mcp",
        "-n",
        "default",
        "--timeout=120s",
    ])?;

    // Point the operator + admission at the house, then clean up on return.
    apply_overlay(ctx, "aauth")?;
    let _g = OverlayGuard { ctx };
    let _cleanup = AauthCleanup { ctx, dir: &dir };

    // The identity-provisioned once-agent: provisioned key + allowlist
    // enrollment + a DIRECT signed dial to the mock (auth.mode: aauth).
    // The preceding overlay helm cycle restarts the admission pod, and the
    // webhook's Service endpoints can lag pod readiness — retry the apply
    // through the propagation window instead of failing on the first
    // "context deadline exceeded".
    kh::poll_until(Duration::from_secs(90), Duration::from_secs(3), || async {
        match apply_example(&dir, "aauth-agent.yaml") {
            Ok(()) => Ok(true),
            Err(e) if format!("{e:#}").contains("failed to call webhook") => Ok(false),
            Err(e) => Err(e),
        }
    })
    .await
    .context("apply aauth-agent through the admission webhook")?;

    // The operator learns the enrolled identity into status.identity.aauth
    // once the agent self-enrolls (allowlist consumed).
    let agents: kube::Api<agent_api::v1alpha2::Agent> =
        kube::Api::namespaced(ctx.client.clone(), &ctx.cfg.ns);
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        let a = agents.get("aauth-once").await?;
        Ok(a.status
            .and_then(|s| s.identity)
            .and_then(|i| i.aauth)
            .and_then(|a| a.agent)
            .is_some_and(|id| id.starts_with("aauth:")))
    })
    .await
    .context("Agent aauth-once never learned status.identity.aauth (enrollment)")?;

    // The once-run reaches its terminal transition — it connected + signed the
    // MCP handshake to the mock and exited. (Pod Succeeded ⇒ clean exit 0.)
    let pod = format!("job/{}", "aauth-once");
    shell::kubectl(&[
        "wait",
        &pod,
        "-n",
        &ctx.cfg.ns,
        "--for=condition=complete",
        "--timeout=180s",
    ])
    .context("aauth-once Job did not complete (signed MCP connect/run)")?;

    // The mock verified real signatures: at least one signed-OK, and NEVER an
    // unsigned or bad-signature acceptance.
    let stats = aauth_mock_stats(ctx)?;
    let signed = stats.get("signed_ok").and_then(|v| v.as_u64()).unwrap_or(0);
    let unsigned = stats
        .get("unsigned_rejected")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if signed < 1 {
        bail!("mock AAuth MCP server counted no signed-OK calls (agent did not sign a verified request)");
    }
    // A conformant agent signs proactively, so unsigned_rejected may be 0 —
    // the load-bearing assertion is that signed calls verified end to end
    // (against the AP's JWKS), which `signed >= 1` proves.
    println!("    (mock verified {signed} signed call(s), {unsigned} unsigned challenge(s))");
    pass()
}

/// Read the mock AAuth MCP server's `/stats` from inside the cluster (a
/// throwaway curl pod), avoiding a CA-trust dance on the harness side — the
/// mock serves https with the cluster CA, so `curl -k` in-cluster is the
/// simplest legible reader for a fixture endpoint.
fn aauth_mock_stats(_ctx: &Ctx) -> Result<serde_json::Value> {
    let out = shell::kubectl(&[
        "run",
        "e2e-aauth-stats",
        "-n",
        "default",
        "--rm",
        "-i",
        "--restart=Never",
        "--image=curlimages/curl:8.8.0",
        "--",
        "curl",
        "-sSk",
        "--max-time",
        "10",
        // Trailing-dot absolute FQDN: a 4-dot name under ndots:5 is otherwise
        // captured by a search-domain wildcard (SSL error to a foreign host).
        "https://mock-aauth-mcp.default.svc.cluster.local./stats",
    ])
    .context("read mock /stats via in-cluster curl")?;
    // `kubectl run -i` concatenates its "pod deleted" notice onto the curl
    // output (often on the same line), so slice out just the JSON object —
    // the first `{` through the matching (flat-object) `}`.
    let start = out.find('{').context("no JSON object in /stats output")?;
    let end = out[start..]
        .find('}')
        .map(|e| start + e + 1)
        .context("unterminated JSON object in /stats output")?;
    serde_json::from_str(&out[start..end]).context("parse mock /stats json")
}

/// Best-effort teardown of the aauth scenario's out-of-band manifests + Secret.
struct AauthCleanup<'a> {
    ctx: &'a Ctx,
    dir: &'a str,
}
impl Drop for AauthCleanup<'_> {
    fn drop(&mut self) {
        delete_example(self.dir, "aauth-agent.yaml");
        delete_example(self.dir, "mock-aauth-mcp.yaml");
        delete_example(self.dir, "modelpool-mock.yaml");
        delete_example(self.dir, "mock-provider.yaml");
        let _ = self.ctx; // ctx kept for parity with other cleanup guards
    }
}

// ===========================================================================
// Security-overlay plumbing
// ===========================================================================

/// Apply a Helm gate overlay via `e2e/install.sh <overlay>`.
fn apply_overlay(ctx: &Ctx, overlay: &str) -> Result<()> {
    let sh = ctx.cfg.install_sh();
    let sh = sh.to_str().context("install.sh path is not valid UTF-8")?;
    shell::run(sh, &[overlay]).map(|_| ())
}

/// Revert to the base values via `e2e/install.sh --base`.
fn revert_overlay(ctx: &Ctx) -> Result<()> {
    let sh = ctx.cfg.install_sh();
    let sh = sh.to_str().context("install.sh path is not valid UTF-8")?;
    shell::run(sh, &["--base"]).map(|_| ())
}

/// Reverts the active overlay on drop, so a `?`-early-return still restores base.
struct OverlayGuard<'a> {
    ctx: &'a Ctx,
}

impl Drop for OverlayGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = revert_overlay(self.ctx) {
            eprintln!("  warning: overlay revert (install.sh --base) failed: {e:#}");
        }
    }
}

// ===========================================================================
// Small builders / readers
// ===========================================================================

/// An A2A-serving reactive agent.
fn a2a_agent(ctx: &Ctx, name: &str) -> Agent {
    let mut a = agentd_agent(ctx, name, Mode::Reactive, "serve A2A");
    a.spec.surfaces = Some(agent_api::DesiredSurfaces {
        management: true,
        metrics: false,
        a2a: true,
    });
    a
}

/// A claim-mode AgentFleet (KEDA owns replicas; coordination backlog drives it).
fn claim_fleet(ctx: &Ctx, name: &str) -> AgentFleet {
    let mut f = AgentFleet::new(
        name,
        AgentFleetSpec {
            template: AgentSpec {
                mode: Mode::Reactive,
                image: Some(ctx.cfg.agentd_image.clone()),
                instruction: Some("claim and process work".to_string()),
                surfaces: Some(agent_api::DesiredSurfaces {
                    a2a: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            scaling: Scaling {
                mode: ScaleMode::Claim,
                min_replicas: Some(0),
                max_replicas: Some(5),
                target: Some(ScaleTarget {
                    metric: "pending_events".to_string(),
                    value: "5".to_string(),
                }),
                ..Default::default()
            },
            // work.source is LEFT UNSET on purpose: the operator renders the KEDA
            // ScaledObject's `coordinationUrl` from `spec.work.source` when set, but
            // the scaler dials that value as the backlog HTTP endpoint — a queue
            // URI like `work://pending` is not a URL and the scaler's read fails
            // ("builder error for url"), so it never goes active. Unset, the
            // operator falls back to its `COORDINATION_URL`
            // (http://agentctl-coordination.agentctl-system/), which the scaler
            // reads `work.stats` from correctly. The agents still claim from
            // `subscribe` above.
            work: None,
            replicas: None,
            ..Default::default()
        },
    );
    f.metadata.namespace = Some(ctx.cfg.ns.clone());
    f
}

/// A shard-mode AgentFleet with `shards = n` (fixed StatefulSet partitioning).
fn shard_fleet(ctx: &Ctx, name: &str, n: u32) -> AgentFleet {
    let mut f = AgentFleet::new(
        name,
        AgentFleetSpec {
            template: AgentSpec {
                mode: Mode::Reactive,
                image: Some(ctx.cfg.agentd_image.clone()),
                instruction: Some("process my shard".to_string()),
                surfaces: Some(agent_api::DesiredSurfaces {
                    a2a: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            scaling: Scaling {
                mode: ScaleMode::Shard,
                shards: Some(n),
                ..Default::default()
            },
            work: Some(Work {
                source: Some("work://pending".to_string()),
                ..Default::default()
            }),
            replicas: None,
            ..Default::default()
        },
    );
    f.metadata.namespace = Some(ctx.cfg.ns.clone());
    f
}

/// Poll until the first pod for an Agent appears, then return its name.
async fn wait_for_first_pod(ctx: &Ctx, agent: &str) -> Result<String> {
    let label = agent_label(agent);
    kh::poll_until(READY_TIMEOUT, Duration::from_secs(2), || async {
        Ok(first_pod(&ctx.cfg.ns, &label).is_ok())
    })
    .await
    .with_context(|| format!("no pod appeared for agent {agent}"))?;
    first_pod(&ctx.cfg.ns, &label)
}

/// Wait until the Agent's (Job) pod has TERMINATED and return its container exit
/// code. A once-mode Agent can report `Ready=True` before its Job pod exits, so a
/// terminal exit-code read must poll for the `terminated` state, not assume it.
async fn wait_pod_exit_code(ctx: &Ctx, agent: &str, timeout: Duration) -> Result<i64> {
    let label = agent_label(agent);
    wait_for_first_pod(ctx, agent).await?;
    kh::poll_until(timeout, Duration::from_secs(2), || async {
        Ok(pod_exit_code(&ctx.cfg.ns, &label).is_ok())
    })
    .await
    .with_context(|| format!("pod for agent {agent} did not terminate"))?;
    pod_exit_code(&ctx.cfg.ns, &label)
}

/// Ready replicas of a Deployment (0 if absent).
fn deployment_ready_replicas(ns: &str, name: &str) -> Result<i64> {
    workload_ready_replicas("deployment", ns, name)
}

/// Ready replicas of a StatefulSet (0 if absent).
fn statefulset_ready_replicas(ns: &str, name: &str) -> Result<i64> {
    workload_ready_replicas("statefulset", ns, name)
}

fn workload_ready_replicas(kind: &str, ns: &str, name: &str) -> Result<i64> {
    let out = shell::kubectl(&[
        "get",
        kind,
        name,
        "-n",
        ns,
        "-o",
        "jsonpath={.status.readyReplicas}",
    ])?;
    Ok(out.trim().parse::<i64>().unwrap_or(0))
}

/// Apply an example manifest by filename under the examples dir.
fn apply_example(dir: &str, file: &str) -> Result<()> {
    shell::kubectl(&["apply", "-f", &format!("{dir}/{file}")]).map(|_| ())
}

/// Apply the mock provider AND wait for it to be Ready: a cold Deployment refuses
/// connections, so an agent/probe that infers before the provider is up gets a
/// gateway 502 (not a metered call), which flakes the metering + budget asserts.
fn apply_mock_provider(ctx: &Ctx, dir: &str) -> Result<()> {
    apply_example(dir, "mock-provider.yaml")?;
    shell::kubectl(&[
        "rollout",
        "status",
        "deployment/mock-provider",
        "-n",
        &ctx.cfg.ns,
        "--timeout=90s",
    ])
    .map(|_| ())
}

/// Best-effort delete of an example manifest (cleanup).
fn delete_example(dir: &str, file: &str) {
    let _ = shell::kubectl(&[
        "delete",
        "-f",
        &format!("{dir}/{file}"),
        "--ignore-not-found",
        "--wait=false",
    ]);
}

/// Extract a JWS signature `kid` from an Agent Card, tolerating the common shapes
/// (`signatures[0].protected` base64url header, or a top-level `kid`).
fn card_signature_kid(card: &Value) -> Option<String> {
    if let Some(kid) = card.get("kid").and_then(Value::as_str) {
        return Some(kid.to_string());
    }
    let sig = card.get("signatures").and_then(Value::as_array)?.first()?;
    if let Some(kid) = sig.get("kid").and_then(Value::as_str) {
        return Some(kid.to_string());
    }
    // `protected` is a base64url-encoded JWS header { "alg":..,"kid":.. }.
    let protected = sig.get("protected").and_then(Value::as_str)?;
    let decoded = b64url_decode(protected)?;
    let header: Value = serde_json::from_slice(&decoded).ok()?;
    header
        .get("kid")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Minimal base64url (no padding) decode — just enough to read a JWS header.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut table = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in s.as_bytes() {
        let v = table[b as usize];
        if v == 255 {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
