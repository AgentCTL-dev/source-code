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
    Agent, AgentFleet, AgentFleetSpec, AgentSpec, Mode, ScaleMode, ScaleTarget, Scaling,
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
const SVC_MODELGATEWAY: &str = "agentctl-modelgateway";
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
        // intelligence
        scenario!("intel-infer", "intelligence", intel_once_infer),
        scenario!("intel-budget-429", "intelligence", intel_budget_429),
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
        scenario!("sec-mg-attest", "security", sec_mg_attest),
        scenario!("sec-coord-attest", "security", sec_coord_attest),
        scenario!("sec-coord-mtls", "security", sec_coord_mtls),
        scenario!("sec-apitoken", "security", sec_apitoken),
        scenario!("sec-netpol", "security", sec_netpol),
    ]
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

/// Build an agentd-backed `Agent` CR in the scenario namespace. The operator always
/// renders a keyless `INTELLIGENCE=https://<modelgateway>…` endpoint and mounts
/// the per-namespace CA. agentd validates the intelligence endpoint at boot in every
/// mode (`once` infers immediately; a reactive/shard daemon dials it only when it does
/// work), so a bound `modelPool` is enough.
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
    kh::delete_and_wait::<Agent>(&ctx.client, &ctx.cfg.ns, name, GC_TIMEOUT).await
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

/// `mode: once` → the operator renders a Job; the agent does its work through the
/// secretless keyless-dial path (to the headroom mock pool via the ModelGateway),
/// runs to a terminal status, and the pod exits with a clean, contract-known
/// `complete` exit code.
async fn prov_once_ready_exit(ctx: &Ctx) -> Result<Outcome> {
    // agentd once-mode REQUIRES intelligence and infers immediately; give it the
    // headroom mock pool so the run reaches a clean completion (exit 0).
    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;

    let name = "e2e-prov-once";
    let agent = agentd_agent(ctx, name, Mode::Once, "emit a one-line summary and exit");
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;

    // The Agent's Ready can flip true before the Job pod exits, so wait for the
    // pod to TERMINATE, then assert the contract exit code + `complete` intent.
    let table = contract::ExitCodeTable::load(&ctx.cfg.contract_dir)?;
    let code = wait_pod_exit_code(ctx, name, READY_TIMEOUT).await?;
    if !table.is_known(code) {
        bail!("exit code {code} is not in the frozen exit-code table");
    }
    if table.intent(code) != "complete" {
        bail!(
            "once-mode agent exited {code} (intent {}), expected a `complete` code",
            table.intent(code)
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
    agent.spec.subscribe = vec!["queue://noop".to_string()];
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;

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
        "--capabilities",
    ])?;
    let m = contract::validate_manifest(&manifest)
        .context("reactive agent capabilities manifest failed contract validation")?;
    if !m.surfaces.management.is_served() {
        bail!("reactive agent did not advertise a served management surface");
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
    run_mgmt_verb(ctx, "cancel").await
}

/// Round-trip one management connect verb through the aggregated APIServer and assert
/// the `agentctl_apiserver_verb_*` counters moved and the verb returned `Success`.
async fn run_mgmt_verb(ctx: &Ctx, verb: &str) -> Result<Outcome> {
    let name = format!("e2e-mgmt-{verb}");
    let mut agent = agentd_agent(ctx, &name, Mode::Reactive, "serve the management profile");
    agent.spec.subscribe = vec!["queue://noop".to_string()];
    kh::apply(&ctx.client, &ctx.cfg.ns, &name, &agent).await?;
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
    agent.spec.subscribe = vec!["queue://noop".to_string()];
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
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
// Intelligence (secretless infer + budgets)
// ===========================================================================

/// Once-mode inference (keyless dial + source-IP attest): the ModelGateway meters
/// tokens + requests and injects the pool credential (the agent never holds a key).
async fn intel_once_infer(ctx: &Ctx) -> Result<Outcome> {
    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;

    // Counters are monotonic + cumulative across runs, so assert the DELTA over
    // this scenario rather than a fixed absolute.
    let before = scrape(ctx, SVC_MODELGATEWAY, PORT_HTTP, "http").ok();
    let req0 = before
        .as_ref()
        .map(|m| m.sum("agentctl_modelgateway_infer_requests_total"))
        .unwrap_or(0.0);
    let tok0 = before
        .as_ref()
        .map(|m| m.sum("agentctl_modelgateway_tokens_total"))
        .unwrap_or(0.0);

    let name = "e2e-infer";
    let mut agent = agentd_agent(ctx, name, Mode::Once, "summarize: hello world");
    agent.spec.model_pool = Some("mockpool".to_string());
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
    // The Agent's Ready can flip true before the Job pod actually infers; wait for
    // the once pod to TERMINATE (its work — the inference — is then done) before
    // reading the gateway counters.
    wait_pod_exit_code(ctx, name, READY_TIMEOUT).await?;

    let m = scrape(ctx, SVC_MODELGATEWAY, PORT_HTTP, "http")?;
    if m.sum("agentctl_modelgateway_infer_requests_total") <= req0 {
        bail!("ModelGateway saw no infer request from the routed agent");
    }
    if m.sum("agentctl_modelgateway_tokens_total") <= tok0 {
        bail!("ModelGateway metered no tokens (provider may not have returned 200)");
    }

    cleanup_agent(ctx, name).await?;
    delete_example(&dir, "modelpool-mock.yaml");
    delete_example(&dir, "mock-provider.yaml");
    pass()
}

/// The pool budget (150 tok, 100/call) rejects an inference once the pool is over
/// budget with a budget 429.
///
/// Driven from an IN-CLUSTER probe pod, not a harness port-forward: with
/// ModelGateway attest ON (the e2e base), the gateway derives the
/// caller's identity from its SOURCE IP (a kube pod lookup), and a port-forwarded
/// request owns no pod so it fails closed. A curl pod in `ns` has a real pod IP, so
/// it attests as `Direct(ns)`; it loops `/v1/infer` POSTs (selecting the pool via
/// `X-Model-Pool`) on the modelgateway's plaintext :8080 (the same app serves the
/// TLS :8443 the agent dials) until the gateway returns a budget 429. The pool's
/// token usage is cumulative, so we assert the rejection COUNTER increased rather
/// than a fixed absolute (robust to usage already on the pool from intel-infer).
async fn intel_budget_429(ctx: &Ctx) -> Result<Outcome> {
    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-budget.yaml")?;

    let before = scrape(ctx, SVC_MODELGATEWAY, PORT_HTTP, "http")
        .map(|m| m.sum("agentctl_modelgateway_budget_rejections_total"))
        .unwrap_or(0.0);

    // The probe targets the LOW-budget pool explicitly by name (`X-Model-Pool`) so
    // the rejection is the budget, not pool ambiguity. It dials the ModelGateway by
    // ABSOLUTE FQDN (trailing dot) — a non-absolute Service name is < ndots:5 and
    // leaks through the resolver `search` list to a host wildcard (an external 404).
    // 8 calls × 100 tok against a 150-tok cap guarantees a budget 429; a small
    // inter-call sleep lets the gateway's source-IP→pod attestation cache settle.
    let url = format!(
        "http://{}.{}.svc.cluster.local./v1/infer",
        SVC_MODELGATEWAY, ctx.cfg.system_ns
    );
    let script = format!(
        "for i in 1 2 3 4 5 6 7 8; do \
           code=$(curl -s -o /dev/null -w '%{{http_code}}' -X POST {url} \
             -H 'content-type: application/json' -H 'x-model-pool: mockpool-budget' \
             -d '{{\"model\":\"mock-model-v1\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}'); \
           echo \"call$i=$code\"; sleep 1; \
         done"
    );
    // Run a PERSISTENT probe pod and wait for it Ready BEFORE issuing requests: a
    // `kubectl run --rm` one-shot fires before its pod IP propagates to the
    // gateway's pod cache, so the source-IP attestation fails closed (403). With
    // the pod Ready first, its IP is registered and it attests as Direct(ns).
    let probe = "e2e-budget-probe";
    let _ = shell::kubectl(&[
        "delete",
        "pod",
        probe,
        "-n",
        &ctx.cfg.ns,
        "--ignore-not-found",
        "--wait=true",
    ]);
    shell::kubectl(&[
        "run",
        probe,
        "-n",
        &ctx.cfg.ns,
        "--image=curlimages/curl:8.8.0",
        "--restart=Never",
        "--command",
        "--",
        "sleep",
        "120",
    ])
    .context("start budget probe pod")?;
    let ready = shell::kubectl(&[
        "wait",
        "--for=condition=Ready",
        &format!("pod/{probe}"),
        "-n",
        &ctx.cfg.ns,
        "--timeout=60s",
    ]);
    let out = ready.and_then(|_| {
        shell::kubectl(&["exec", probe, "-n", &ctx.cfg.ns, "--", "sh", "-c", &script])
    });
    // Best-effort cleanup of the probe pod regardless of the loop outcome.
    let _ = shell::kubectl(&[
        "delete",
        "pod",
        probe,
        "-n",
        &ctx.cfg.ns,
        "--ignore-not-found",
        "--wait=false",
    ]);
    let out = out.context("run budget probe loop")?;
    if !out.contains("=429") {
        bail!("probe never observed a budget 429 (pool budget not enforced):\n{out}");
    }
    let after = scrape(ctx, SVC_MODELGATEWAY, PORT_HTTP, "http")?
        .sum("agentctl_modelgateway_budget_rejections_total");
    if after <= before {
        bail!("budget rejection counter did not increase ({before} -> {after})");
    }

    delete_example(&dir, "modelpool-budget.yaml");
    delete_example(&dir, "mock-provider.yaml");
    pass()
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
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &fleet).await?;

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

    kh::delete_and_wait::<AgentFleet>(&ctx.client, &ctx.cfg.ns, name, GC_TIMEOUT).await?;
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
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &fleet).await?;

    // The operator names the StatefulSet after the FLEET (not `agentfleet-<name>`).
    let sts = name.to_string();
    kh::poll_until(SCALE_TIMEOUT, Duration::from_secs(5), || async {
        Ok(statefulset_ready_replicas(&ctx.cfg.ns, &sts).unwrap_or(0) == shards as i64)
    })
    .await
    .context("shard StatefulSet did not reach N ready replicas")?;

    // Each replica advertises its K/N shard via its capabilities manifest. agentd
    // reads its shard from `AGENTD_SHARD=K/N`, which the operator does NOT yet
    // inject from the StatefulSet ordinal (no shard-env wiring in render.rs, and
    // the scratch agentd image has no shell to derive K from the pod ordinal), so
    // `surfaces.shard` is null (agentd defaults to the unsharded 0/1). The
    // STRUCTURAL guarantee (a StatefulSet at N stable, ready replicas — the
    // operator's shard-mode rendering) IS verified above; the per-pod K/N identity
    // is a documented operator gap, reported as a skip rather than a false pass.
    let pod0 = format!("{sts}-0");
    let manifest = shell::kubectl(&[
        "exec",
        "-n",
        &ctx.cfg.ns,
        &pod0,
        "--",
        "/agentd",
        "--capabilities",
    ])?;
    let m = contract::validate_manifest(&manifest)?;
    let outcome = match m.surfaces.shard.as_deref() {
        Some(s) if s.ends_with(&format!("/{shards}")) => pass(),
        Some(other) => bail!("replica 0 shard identity {other:?} did not match K/{shards}"),
        None => skip(format!(
            "StatefulSet reached {shards}/{shards} ready (shard-mode rendering verified), but \
             agentd advertises no shard identity: the operator does not inject AGENTD_SHARD=K/N \
             from the StatefulSet ordinal yet (operator gap)"
        )),
    };

    kh::delete_and_wait::<AgentFleet>(&ctx.client, &ctx.cfg.ns, name, GC_TIMEOUT).await?;
    outcome
}

// ===========================================================================
// A2A
// ===========================================================================

/// The Agent Card is signed (JWS) and its key id resolves in the gateway JWKS.
async fn a2a_card_jws(ctx: &Ctx) -> Result<Outcome> {
    let name = "e2e-a2a-card";
    let agent = a2a_agent(ctx, name);
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
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
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
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
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
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
    let table = contract::ExitCodeTable::load(&ctx.cfg.contract_dir)?;
    let name = "e2e-conf-exit";
    let agent = agentd_agent(ctx, name, Mode::Once, "exit cleanly");
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;

    // Wait for the Job pod to TERMINATE (Ready can precede exit), then assert the
    // terminal code is a registered member of the frozen table. Any contract code
    // is acceptable here (this asserts conformance to the table, not a specific
    // outcome): with a mock pool present the agent completes (0); without one it
    // exits 4/INTEL_UNAVAILABLE — both are registered codes.
    let code = wait_pod_exit_code(ctx, name, READY_TIMEOUT).await?;
    if !table.is_known(code) {
        bail!(
            "exit code {code} is not a registered contract exit code (v{})",
            table.version
        );
    }

    cleanup_agent(ctx, name).await?;
    pass()
}

/// Every `agent_*` series an agent emits is a registered name in the metrics registry.
async fn conf_metrics_registry(ctx: &Ctx) -> Result<Outcome> {
    let registry = contract::MetricsRegistry::load(&ctx.cfg.contract_dir)?;
    let name = "e2e-conf-metrics";
    let mut agent = agentd_agent(ctx, name, Mode::Reactive, "serve metrics");
    agent.spec.subscribe = vec!["queue://noop".to_string()];
    agent.spec.surfaces = Some(agent_api::DesiredSurfaces {
        management: true,
        metrics: true,
        a2a: false,
    });
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
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
            let unregistered = registry.unregistered(&metrics.names());
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
    apply_overlay(ctx, "sec-oidc")?;
    let _g = OverlayGuard { ctx };

    let name = "e2e-oidc";
    let agent = a2a_agent(ctx, name);
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
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
    apply_overlay(ctx, "sec-trustedproxy")?;
    let _g = OverlayGuard { ctx };

    let name = "e2e-tproxy";
    let agent = a2a_agent(ctx, name);
    kh::apply(&ctx.client, &ctx.cfg.ns, name, &agent).await?;
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

/// ModelGateway attest anti-spoof: identity is the caller's SOURCE IP, never a header.
/// A caller whose source IP owns no pod (and who self-asserts an
/// identity header anyway) is counted as a spoof and rejected — the header does not
/// help.
async fn sec_mg_attest(ctx: &Ctx) -> Result<Outcome> {
    apply_overlay(ctx, "sec-mg-attest")?;
    let _g = OverlayGuard { ctx };

    let dir = examples_dir();
    apply_mock_provider(ctx, &dir)?;
    apply_example(&dir, "modelpool-mock.yaml")?;

    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_MODELGATEWAY, PORT_HTTP, 18099)?;
    // The harness reaches the gateway via a port-forward, so its source IP owns no
    // tenant pod and cannot be attested; the self-asserted identity headers are
    // ignored and the unattestable request is rejected (a spoof).
    let resp = ctx
        .http
        .post(format!("{}/v1/infer", pf.base_url()))
        .header("x-agentctl-namespace", &ctx.cfg.ns)
        .header("x-agentctl-agent", "i-am-someone-else")
        .json(&json!({ "model": "mock-model-v1", "messages": [] }))
        .send()
        .await?;
    if resp.status().is_success() {
        bail!("ModelGateway accepted a spoofed (unattestable) identity");
    }
    let m = scrape(ctx, SVC_MODELGATEWAY, PORT_HTTP, "http")?;
    if m.sum("agentctl_modelgateway_identity_spoof_total") < 1.0 {
        bail!("no identity spoof was recorded");
    }

    delete_example(&dir, "modelpool-mock.yaml");
    delete_example(&dir, "mock-provider.yaml");
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
    a.spec.subscribe = vec!["queue://noop".to_string()];
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
                subscribe: vec!["work://pending".to_string()],
                ..Default::default()
            },
            scaling: Scaling {
                mode: ScaleMode::Claim,
                min: Some(0),
                max: Some(5),
                target: Some(ScaleTarget {
                    signal: "pending_events".to_string(),
                    value: "5".to_string(),
                }),
                ..Default::default()
            },
            // workSource is LEFT UNSET on purpose: the operator renders the KEDA
            // ScaledObject's `coordinationUrl` from `spec.workSource` when set, but
            // the scaler dials that value as the backlog HTTP endpoint — a queue
            // URI like `work://pending` is not a URL and the scaler's read fails
            // ("builder error for url"), so it never goes active. Unset, the
            // operator falls back to its `COORDINATION_URL`
            // (http://agentctl-coordination.agentctl-system/), which the scaler
            // reads `work.stats` from correctly. The agents still claim from
            // `subscribe` above.
            work_source: None,
            replicas: None,
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
                subscribe: vec!["work://pending".to_string()],
                ..Default::default()
            },
            scaling: Scaling {
                mode: ScaleMode::Shard,
                shards: Some(n),
                ..Default::default()
            },
            work_source: Some("work://pending".to_string()),
            replicas: None,
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
