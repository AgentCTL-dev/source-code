// SPDX-License-Identifier: BUSL-1.1
//! `bench` — the agentctl scale/resource benchmark sweeps + report renderer.
//!
//! Sweeps:
//!   * **(a) density ceiling** — sweep N idle agentd pods until `Pending` and record
//!     max-Running + the binding resource (host-bound: a *trend*, not a capacity claim).
//!   * **(b) per-agent overhead** — agentd pod mem/CPU and the marginal
//!     control-plane Δ per agent (the only two per-agent cost components).
//!   * **(c) CP scaling trends** — operator reconcile p50/p95 from the histogram +
//!     control-plane CPU/mem vs N.
//!   * **(d) coordination throughput** — a concurrent tokio load-gen on `/mcp`
//!     submit/claim/ack at rising concurrency (submit/claim/sec + p50/p99), memory vs
//!     Postgres store.
//!   * **(e) latency** — provisioning (apply N → all Running) + scale-from-zero
//!     (0→1, 0→N), decomposed.
//!
//! Raw CSV/JSON land under `e2e/results/<ts>/`; `--report` renders `docs/benchmarks.md`
//! (host-profile header, per-agent overhead table, density ceiling, trends, a
//! PROMINENT host-bound caveat, and the multi-node re-run command).
//!
//! Excluded from the workspace; the sweeps need a cluster, `--report` only needs an
//! existing results dir.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde_json::{json, Value};

use agentctl_e2e::{host, prom, results, shell, Ctx};

const SVC_COORDINATION: &str = "agentctl-coordination";
const SVC_OPERATOR: &str = "agentctl-operator";

/// The coordination http Service port (chart Service.port `:80` -> container
/// `:8080`); kubectl port-forward addresses the Service port, not the targetPort.
/// (The operator Service is genuinely `:8080`, so its scrape keeps that literal.)
const PORT_HTTP: u16 = 80;

#[derive(Parser, Debug)]
#[command(
    name = "bench",
    about = "agentctl scale/resource benchmark sweeps + report renderer."
)]
struct Cli {
    /// Render docs/benchmarks.md from an existing results dir (no sweeps run).
    #[arg(long)]
    report: bool,
    /// Base directory for results (a `<ts>/` run dir is created/selected under it).
    #[arg(long, default_value = "e2e/results")]
    results_base: String,
    /// For --report: the specific run dir to render (default: the latest under base).
    #[arg(long)]
    results_dir: Option<String>,
    /// For --report: the markdown output path.
    #[arg(long, default_value = "docs/benchmarks.md")]
    report_out: String,
    /// Which sweeps to run (comma list of a,b,c,d,e). Default: all.
    #[arg(long, value_delimiter = ',')]
    sweeps: Vec<String>,
    /// Max N for the density + latency sweeps.
    #[arg(long, default_value_t = 100)]
    max_n: u32,
    /// Coordination load-gen concurrency steps (comma list). Default: 1,4,16,64,256.
    #[arg(long, value_delimiter = ',')]
    concurrency: Vec<u32>,
    /// Seconds of load per concurrency step.
    #[arg(long, default_value_t = 10)]
    duration_secs: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let r = if cli.report {
        render_report(&cli)
    } else {
        run_sweeps(&cli).await
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bench failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Run the selected sweeps, writing CSVs + `summary.json` under a fresh run dir.
async fn run_sweeps(cli: &Cli) -> Result<()> {
    let selected: Vec<&str> = if cli.sweeps.is_empty() {
        vec!["a", "b", "c", "d", "e"]
    } else {
        cli.sweeps.iter().map(String::as_str).collect()
    };

    let ctx = Ctx::build().await?;
    let rd = results::ResultsDir::create(Path::new(&cli.results_base))?;
    let profile = host::HostProfile::capture();
    rd.write_json("host", &serde_json::to_value(&profile)?)?;
    println!("results: {}", rd.dir.display());

    let mut sweeps = serde_json::Map::new();
    for s in selected {
        println!("== sweep {s} ==");
        let v = match s {
            "a" => sweep_density(&ctx, &rd, cli.max_n).await?,
            "b" => sweep_overhead(&ctx, &rd).await?,
            "c" => sweep_cp_trends(&ctx, &rd, cli.max_n).await?,
            "d" => sweep_throughput(&ctx, &rd, &concurrency_steps(cli), cli.duration_secs).await?,
            "e" => sweep_latency(&ctx, &rd, cli.max_n).await?,
            other => bail!("unknown sweep {other:?} (want a|b|c|d|e)"),
        };
        sweeps.insert(s.to_string(), v);
    }

    let summary = json!({
        "stamp": rd.stamp,
        "host": profile,
        "sweeps": sweeps,
    });
    let p = rd.write_json("summary", &summary)?;
    println!("summary: {}", p.display());
    Ok(())
}

/// Resolve the load-gen concurrency steps (CLI override or the default ladder).
fn concurrency_steps(cli: &Cli) -> Vec<u32> {
    if cli.concurrency.is_empty() {
        vec![1, 4, 16, 64, 256]
    } else {
        cli.concurrency.clone()
    }
}

// ===========================================================================
// (a) density ceiling
// ===========================================================================

/// Sweep N idle agentd pods upward until the scheduler can't place them, recording
/// the max running and the binding resource.
async fn sweep_density(ctx: &Ctx, rd: &results::ResultsDir, max_n: u32) -> Result<Value> {
    let name = "e2e-bench-density";
    let label = format!("app={name}");
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut max_running = 0usize;
    let mut binding = "none".to_string();

    for n in step_series(max_n) {
        scale_idle(ctx, name, n)?;
        // Let the scheduler settle, then read running/pending.
        wait_settle(Duration::from_secs(20));
        let running = count_phase(&ctx.cfg.ns, &label, "Running");
        let pending = count_phase(&ctx.cfg.ns, &label, "Pending");
        rows.push(vec![
            n.to_string(),
            running.to_string(),
            pending.to_string(),
        ]);
        max_running = max_running.max(running);
        if pending > 0 {
            binding = binding_resource(ctx);
            break;
        }
    }

    teardown(ctx, name);
    rd.write_csv("density", &["requested", "running", "pending"], &rows)?;
    Ok(json!({ "max_running": max_running, "binding_resource": binding }))
}

/// The classic doubling/decade ladder, capped at `max_n`: 1, 10, 50, 100, 200, …
fn step_series(max_n: u32) -> Vec<u32> {
    let base = [1u32, 10, 50, 100, 200, 400, 800];
    let mut out: Vec<u32> = base.into_iter().filter(|&n| n <= max_n).collect();
    if out.last().copied() != Some(max_n) && max_n >= 1 {
        out.push(max_n);
    }
    out.dedup();
    out
}

/// Guess the binding resource from node conditions (a coarse signal for the report).
fn binding_resource(_ctx: &Ctx) -> String {
    let conds = shell::kubectl(&[
        "get",
        "nodes",
        "-o",
        "jsonpath={.items[*].status.conditions[*].type}",
    ])
    .unwrap_or_default();
    if conds.contains("MemoryPressure") {
        "memory".to_string()
    } else if conds.contains("DiskPressure") {
        "disk".to_string()
    } else {
        "pods/cpu".to_string()
    }
}

// ===========================================================================
// (b) per-agent overhead
// ===========================================================================

/// The headline overhead numbers: one agentd pod's mem/CPU and the marginal
/// control-plane Δ per agent (CP usage at N minus at 0, ÷ N).
async fn sweep_overhead(ctx: &Ctx, rd: &results::ResultsDir) -> Result<Value> {
    let name = "e2e-bench-overhead";
    let probe_n = 10u32;

    // Baseline control-plane footprint (no bench agents).
    teardown(ctx, name);
    wait_settle(Duration::from_secs(10));
    let cp0 = cp_usage(ctx);

    // Bring up the probe agents.
    scale_idle(ctx, name, probe_n)?;
    wait_settle(Duration::from_secs(30));
    let cpn = cp_usage(ctx);

    // Per-agentd-pod usage (average over the probe pods).
    let pods: Vec<shell::TopRow> = shell::top_pods(&ctx.cfg.ns)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.name.starts_with(name))
        .collect();
    let agent_cpu = mean(pods.iter().map(|p| p.cpu_millicores));
    let agent_mem = mean(pods.iter().map(|p| p.mem_mib));

    // The per-agent footprint is fully captured by two terms: the agentd pod
    // itself plus the marginal control-plane delta.

    let n = probe_n.max(1) as f64;
    let cp_cpu_delta = (cpn.0 - cp0.0).max(0.0) / n;
    let cp_mem_delta = (cpn.1 - cp0.1).max(0.0) / n;

    teardown(ctx, name);

    let rows = vec![
        vec!["agentd-pod".into(), fmt2(agent_cpu), fmt2(agent_mem)],
        vec![
            "control-plane-marginal".into(),
            fmt2(cp_cpu_delta),
            fmt2(cp_mem_delta),
        ],
    ];
    rd.write_csv(
        "overhead",
        &["component", "cpu_millicores", "mem_mib"],
        &rows,
    )?;

    Ok(json!({
        "agentd_pod": { "cpu_millicores": agent_cpu, "mem_mib": agent_mem },
        "control_plane_marginal_per_agent": { "cpu_millicores": cp_cpu_delta, "mem_mib": cp_mem_delta },
        "probe_n": probe_n,
    }))
}

/// Total control-plane (system namespace) CPU millicores + memory MiB, from `top`.
fn cp_usage(ctx: &Ctx) -> (f64, f64) {
    let rows = shell::top_pods(&ctx.cfg.system_ns).unwrap_or_default();
    let cpu = rows.iter().map(|r| r.cpu_millicores).sum();
    let mem = rows.iter().map(|r| r.mem_mib).sum();
    (cpu, mem)
}

// ===========================================================================
// (c) control-plane scaling trends
// ===========================================================================

/// Operator reconcile p50/p95 (from the histogram) and control-plane CPU/mem as N
/// grows.
async fn sweep_cp_trends(ctx: &Ctx, rd: &results::ResultsDir, max_n: u32) -> Result<Value> {
    let name = "e2e-bench-trends";
    let mut rows: Vec<Vec<String>> = Vec::new();

    for n in step_series(max_n) {
        scale_idle(ctx, name, n)?;
        wait_settle(Duration::from_secs(20));
        let (cpu, mem) = cp_usage(ctx);
        let op = prom::scrape_proxy(&ctx.cfg.system_ns, SVC_OPERATOR, 8080, "http", "/metrics")
            .unwrap_or_default();
        let p50 = histogram_quantile(&op, "agentctl_operator_reconcile_duration_seconds", 0.50)
            .unwrap_or(f64::NAN);
        let p95 = histogram_quantile(&op, "agentctl_operator_reconcile_duration_seconds", 0.95)
            .unwrap_or(f64::NAN);
        rows.push(vec![
            n.to_string(),
            fmt4(p50),
            fmt4(p95),
            fmt2(cpu),
            fmt2(mem),
        ]);
    }

    teardown(ctx, name);
    rd.write_csv(
        "cp_trends",
        &[
            "n",
            "reconcile_p50_s",
            "reconcile_p95_s",
            "cp_cpu_millicores",
            "cp_mem_mib",
        ],
        &rows,
    )?;
    Ok(json!({ "points": rows.len() }))
}

/// A Prometheus-style histogram quantile from cumulative `_bucket{le=…}` series +
/// `_count`. Linear interpolation within the bucket containing the rank.
fn histogram_quantile(m: &prom::Metrics, base: &str, q: f64) -> Option<f64> {
    let count = m.scalar(&format!("{base}_count"))?;
    if count <= 0.0 {
        return None;
    }
    // Collect (le, cumulative) ascending by le; `+Inf` becomes f64::INFINITY.
    let mut buckets: Vec<(f64, f64)> = m
        .series(&format!("{base}_bucket"))
        .filter_map(|s| {
            let le = s.labels.get("le")?;
            let le = if le == "+Inf" {
                f64::INFINITY
            } else {
                le.parse::<f64>().ok()?
            };
            Some((le, s.value))
        })
        .collect();
    buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    if buckets.is_empty() {
        return None;
    }

    let rank = q * count;
    let mut prev_le = 0.0;
    let mut prev_cum = 0.0;
    for (le, cum) in buckets {
        if cum >= rank {
            if le.is_infinite() {
                return Some(prev_le);
            }
            let span = (cum - prev_cum).max(1e-9);
            let frac = (rank - prev_cum) / span;
            return Some(prev_le + (le - prev_le) * frac);
        }
        prev_le = le;
        prev_cum = cum;
    }
    Some(prev_le)
}

// ===========================================================================
// (d) coordination throughput
// ===========================================================================

/// A concurrent load-gen against the coordination `/mcp` surface at rising
/// concurrency: each client loops submit→claim→ack; we record ops/sec + p50/p99.
async fn sweep_throughput(
    ctx: &Ctx,
    rd: &results::ResultsDir,
    concurrency: &[u32],
    duration_secs: u64,
) -> Result<Value> {
    let store = std::env::var("AGENTCTL_E2E_STORE").unwrap_or_else(|_| "memory".to_string());
    let pf = shell::PortForward::service(&ctx.cfg.system_ns, SVC_COORDINATION, PORT_HTTP, 18200)?;
    let base = pf.base_url();

    let mut rows: Vec<Vec<String>> = Vec::new();
    for &c in concurrency {
        let res = loadgen(&ctx.http, &base, c, Duration::from_secs(duration_secs)).await;
        rows.push(vec![
            store.clone(),
            c.to_string(),
            fmt2(res.ops_per_sec),
            fmt4(res.p50_ms),
            fmt4(res.p99_ms),
            res.ops.to_string(),
            res.errors.to_string(),
        ]);
        println!(
            "  c={c:<4} {:.0} ops/s  p50={:.1}ms p99={:.1}ms ({} ops, {} err)",
            res.ops_per_sec, res.p50_ms, res.p99_ms, res.ops, res.errors
        );
    }

    drop(pf);
    rd.write_csv(
        "throughput",
        &[
            "store",
            "concurrency",
            "ops_per_sec",
            "p50_ms",
            "p99_ms",
            "ops",
            "errors",
        ],
        &rows,
    )?;
    Ok(json!({ "store": store, "steps": rows.len() }))
}

/// One load-gen result.
struct LoadResult {
    ops: u64,
    errors: u64,
    ops_per_sec: f64,
    p50_ms: f64,
    p99_ms: f64,
}

/// Drive `concurrency` clients for `dur`, each repeating a submit→claim→ack cycle
/// against the coordination `/mcp` endpoint. Returns aggregate throughput +
/// per-operation latency percentiles.
async fn loadgen(
    http: &reqwest::Client,
    base: &str,
    concurrency: u32,
    dur: Duration,
) -> LoadResult {
    let mut set = tokio::task::JoinSet::new();
    let started = Instant::now();
    for worker in 0..concurrency.max(1) {
        let http = http.clone();
        let base = base.to_string();
        set.spawn(async move {
            let mut lat: Vec<f64> = Vec::new();
            let mut ops = 0u64;
            let mut errors = 0u64;
            let mut seq = 0u64;
            while started.elapsed() < dur {
                seq += 1;
                let key = format!("lg-{worker}-{seq}");
                let item = format!("lg://{worker}/{seq}");
                let op_start = Instant::now();
                let ok = cycle(&http, &base, &item, &key).await;
                lat.push(op_start.elapsed().as_secs_f64() * 1000.0);
                ops += 1;
                if !ok {
                    errors += 1;
                }
            }
            (ops, errors, lat)
        });
    }

    let mut all_lat: Vec<f64> = Vec::new();
    let mut ops = 0u64;
    let mut errors = 0u64;
    while let Some(joined) = set.join_next().await {
        if let Ok((o, e, lat)) = joined {
            ops += o;
            errors += e;
            all_lat.extend(lat);
        }
    }
    let elapsed = started.elapsed().as_secs_f64().max(1e-9);
    all_lat.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    LoadResult {
        ops,
        errors,
        ops_per_sec: ops as f64 / elapsed,
        p50_ms: percentile(&all_lat, 0.50),
        p99_ms: percentile(&all_lat, 0.99),
    }
}

/// One submit→claim→ack cycle; `true` if every step's HTTP call succeeded.
async fn cycle(http: &reqwest::Client, base: &str, item: &str, key: &str) -> bool {
    let submit = rpc(
        http,
        base,
        "work.submit",
        json!({ "item": item, "claim_key": key }),
        Value::Null,
    )
    .await;
    let claim = rpc(
        http,
        base,
        "work.claim",
        json!({ "item": item, "ttl_ms": 10_000 }),
        json!({ "agent/claim_key": key, "agent/instance": "loadgen" }),
    )
    .await;
    let lease = claim.as_ref().ok().and_then(|v| {
        v.get("result")?
            .get("structuredContent")?
            .get("lease_id")?
            .as_str()
            .map(str::to_string)
    });
    let ack = if let Some(l) = lease {
        rpc(
            http,
            base,
            "work.ack",
            json!({ "lease_id": l }),
            json!({ "agent/claim_key": key }),
        )
        .await
        .is_ok()
    } else {
        true
    };
    submit.is_ok() && claim.is_ok() && ack
}

/// One MCP `tools/call`, returning the raw JSON-RPC response.
async fn rpc(
    http: &reqwest::Client,
    base: &str,
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
    let v = http
        .post(format!("{base}/mcp"))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    Ok(v)
}

// ===========================================================================
// (e) latency
// ===========================================================================

/// Provisioning latency (apply N → all Running) + scale-from-zero (0→1, 0→N).
async fn sweep_latency(ctx: &Ctx, rd: &results::ResultsDir, max_n: u32) -> Result<Value> {
    let name = "e2e-bench-latency";
    let label = format!("app={name}");
    let mut rows: Vec<Vec<String>> = Vec::new();

    // 0→1 and 0→N provisioning latency.
    for n in [1u32, max_n.max(1)] {
        teardown(ctx, name);
        wait_settle(Duration::from_secs(8));
        let start = Instant::now();
        scale_idle(ctx, name, n)?;
        let all_running = wait_running(&ctx.cfg.ns, &label, n as usize, Duration::from_secs(300));
        let secs = start.elapsed().as_secs_f64();
        rows.push(vec![
            format!("provision_0_to_{n}"),
            n.to_string(),
            fmt2(secs),
            all_running.to_string(),
        ]);
    }

    teardown(ctx, name);
    rd.write_csv(
        "latency",
        &["phase", "n", "wall_secs", "reached_target"],
        &rows,
    )?;
    Ok(json!({ "points": rows.len() }))
}

// ===========================================================================
// Cluster manipulation (idle agentd Deployment)
// ===========================================================================

/// Create-or-scale a Deployment of `replicas` idle agentd pods (label `app=<name>`).
///
/// agentd is NOT a bare idle binary: in EVERY mode it validates an intelligence
/// endpoint at boot (exits 2/USAGE without one), and a `reactive` daemon needs a
/// mode + trigger. So a plain `kubectl create deployment --image agentd` pod
/// crash-loops and never reaches Running — bogus density/overhead numbers. We
/// instead render a Deployment running `agentd --mode reactive` with a dummy
/// (never-dialed, since idle) HTTPS intelligence endpoint and a trigger; the pod
/// arms its reactor and idles, which is exactly the per-agent footprint the sweep
/// measures. `imagePullPolicy: IfNotPresent` keeps it on the kind-loaded image.
fn scale_idle(ctx: &Ctx, name: &str, replicas: u32) -> Result<()> {
    let manifest = format!(
        "apiVersion: apps/v1\n\
         kind: Deployment\n\
         metadata:\n\
         \x20 name: {name}\n\
         \x20 namespace: {ns}\n\
         \x20 labels: {{ app: {name} }}\n\
         spec:\n\
         \x20 replicas: {replicas}\n\
         \x20 selector: {{ matchLabels: {{ app: {name} }} }}\n\
         \x20 template:\n\
         \x20   metadata:\n\
         \x20     labels: {{ app: {name} }}\n\
         \x20   spec:\n\
         \x20     terminationGracePeriodSeconds: 2\n\
         \x20     shareProcessNamespace: true\n\
         \x20     containers:\n\
         \x20       - name: agentd\n\
         \x20         image: {image}\n\
         \x20         imagePullPolicy: IfNotPresent\n\
         \x20         args: [\"--mode\",\"reactive\",\"--instruction\",\"idle\",\"--subscribe\",\"file:///tmp/inbox\",\"--intelligence\",\"https://api.anthropic.com/\"]\n",
        name = name,
        ns = ctx.cfg.ns,
        replicas = replicas,
        image = ctx.cfg.agentd_image,
    );
    let path = std::env::temp_dir().join(format!("agentctl-bench-{name}.yaml"));
    fs::write(&path, manifest).with_context(|| format!("write {path:?}"))?;
    shell::kubectl(&["apply", "-f", path.to_str().unwrap_or_default()])
        .map(|_| ())
        .with_context(|| format!("apply idle deployment {name} (replicas={replicas})"))
}

/// Best-effort delete of the bench Deployment.
fn teardown(ctx: &Ctx, name: &str) {
    let _ = shell::kubectl(&[
        "delete",
        "deployment",
        name,
        "-n",
        &ctx.cfg.ns,
        "--ignore-not-found",
        "--wait=false",
    ]);
}

/// Count pods matching `label` in `ns` whose `.status.phase` equals `phase`.
fn count_phase(ns: &str, label: &str, phase: &str) -> usize {
    shell::kubectl(&[
        "get",
        "pods",
        "-n",
        ns,
        "-l",
        label,
        "-o",
        "jsonpath={.items[*].status.phase}",
    ])
    .map(|s| s.split_whitespace().filter(|p| *p == phase).count())
    .unwrap_or(0)
}

/// Block until `target` pods are Running or the timeout elapses; returns whether the
/// target was reached.
fn wait_running(ns: &str, label: &str, target: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if count_phase(ns, label, "Running") >= target {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// A coarse settle delay between a scale and a measurement.
fn wait_settle(d: Duration) {
    std::thread::sleep(d);
}

// ===========================================================================
// math helpers
// ===========================================================================

/// The value at quantile `q` of a sorted slice (nearest-rank), 0.0 if empty.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((q * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

/// Arithmetic mean of an iterator, 0.0 if empty.
fn mean<I: Iterator<Item = f64>>(it: I) -> f64 {
    let (sum, n) = it.fold((0.0, 0u64), |(s, n), v| (s + v, n + 1));
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

fn fmt2(v: f64) -> String {
    if v.is_nan() {
        "n/a".to_string()
    } else {
        format!("{v:.2}")
    }
}

fn fmt4(v: f64) -> String {
    if v.is_nan() {
        "n/a".to_string()
    } else {
        format!("{v:.4}")
    }
}

// ===========================================================================
// report rendering
// ===========================================================================

/// Render `docs/benchmarks.md` from a results run dir's `summary.json`.
fn render_report(cli: &Cli) -> Result<()> {
    let dir = match &cli.results_dir {
        Some(d) => PathBuf::from(d),
        None => latest_run_dir(Path::new(&cli.results_base))?,
    };
    let rd = results::ResultsDir::open(&dir)?;
    let summary = rd
        .read_json("summary")
        .with_context(|| format!("read summary.json under {}", dir.display()))?;

    let md = render_markdown(&summary, &rd);
    let out = Path::new(&cli.report_out);
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {parent:?}"))?;
    }
    fs::write(out, md).with_context(|| format!("write {}", out.display()))?;
    println!("wrote {}", out.display());
    Ok(())
}

/// The newest `<ts>` subdirectory under `base`.
fn latest_run_dir(base: &Path) -> Result<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(base).with_context(|| format!("read {}", base.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(ts) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if best.as_ref().map(|(b, _)| ts > *b).unwrap_or(true) {
                best = Some((ts, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| anyhow::anyhow!("no run dirs under {}", base.display()))
}

/// Read a results CSV and render it as a GitHub-flavoured Markdown table. Returns
/// `None` when the file is absent (the sweep was not run) or has no data rows, so
/// the committed report only ever shows real measured numbers.
fn csv_table(rd: &results::ResultsDir, name: &str) -> Option<String> {
    let path = rd.dir.join(format!("{name}.csv"));
    let body = fs::read_to_string(path).ok()?;
    let mut lines = body.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next()?;
    let rows: Vec<&str> = lines.collect();
    if rows.is_empty() {
        return None;
    }
    let cols: Vec<&str> = header.split(',').collect();
    let mut t = String::new();
    t.push_str("| ");
    t.push_str(&cols.join(" | "));
    t.push_str(" |\n|");
    for _ in &cols {
        t.push_str("---|");
    }
    t.push('\n');
    for r in rows {
        t.push_str("| ");
        t.push_str(&r.split(',').collect::<Vec<_>>().join(" | "));
        t.push_str(" |\n");
    }
    Some(t)
}

/// Build the benchmark report markdown from a `summary.json` value + the run dir's
/// CSVs (inlined as tables so the committed doc carries the real numbers; the
/// CSVs themselves are git-ignored under `e2e/results/`).
fn render_markdown(summary: &Value, rd: &results::ResultsDir) -> String {
    let mut s = String::new();
    s.push_str("# agentctl benchmarks\n\n");
    s.push_str(
        "> **Host-bound results.** These numbers were captured on a single node \
         (kind by default). They are **trends + per-agent overhead**, NOT a capacity \
         claim: the density ceiling is bounded by THIS host's memory/CPU, not by \
         agentctl. Re-run on real, multi-node hardware before quoting any ceiling.\n\n",
    );

    // Host profile header.
    if let Some(host) = summary.get("host") {
        if let Ok(profile) = serde_json::from_value::<host::HostProfile>(host.clone()) {
            s.push_str("## Host profile\n\n");
            s.push_str(&profile.markdown());
            s.push('\n');
        }
    }

    let sweeps = summary.get("sweeps").cloned().unwrap_or(Value::Null);

    // (b) per-agent overhead — the headline table.
    if let Some(b) = sweeps.get("b") {
        s.push_str("## Per-agent overhead (headline)\n\n");
        s.push_str("| Component | CPU (millicores) | Memory (MiB) |\n|---|---|---|\n");
        push_overhead_row(&mut s, "agentd pod", b.get("agentd_pod"));
        push_overhead_row(
            &mut s,
            "control-plane (marginal / agent)",
            b.get("control_plane_marginal_per_agent"),
        );
        s.push('\n');
    }

    // (a) density ceiling.
    if let Some(a) = sweeps.get("a") {
        s.push_str("## Density ceiling (host-bound trend)\n\n");
        let max_running = a.get("max_running").and_then(Value::as_u64).unwrap_or(0);
        let binding = a
            .get("binding_resource")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        s.push_str(&format!(
            "- Max running idle agents on this host: **{max_running}**\n- Binding resource: **{binding}**\n\n"
        ));
        if let Some(t) = csv_table(rd, "density") {
            s.push_str("Requested vs scheduled (the sweep stops at the first `Pending`):\n\n");
            s.push_str(&t);
            s.push('\n');
        }
    }

    // (d) coordination throughput.
    if let Some(d) = sweeps.get("d") {
        let store = d.get("store").and_then(Value::as_str).unwrap_or("memory");
        s.push_str("## Coordination throughput\n\n");
        s.push_str(&format!(
            "Concurrent load-gen on `/mcp` submit→claim→ack (store: **{store}**). The single \
             serializing `Mutex` is the in-memory ceiling; the Postgres store trades raw \
             throughput for durability/HA.\n\n",
        ));
        if let Some(t) = csv_table(rd, "throughput") {
            s.push_str(&t);
            s.push('\n');
        }
    }

    // (c) control-plane scaling trends.
    if sweeps.get("c").is_some() {
        s.push_str("## Control-plane scaling trends\n\n");
        s.push_str(
            "Operator reconcile p50/p95 (from the `agentctl_operator_reconcile_duration_seconds` \
             histogram) and total control-plane CPU/mem as the agent count `n` grows:\n\n",
        );
        if let Some(t) = csv_table(rd, "cp_trends") {
            s.push_str(&t);
            s.push('\n');
        } else {
            s.push_str("_(no data captured)_\n\n");
        }
    }

    // (e) latency.
    if sweeps.get("e").is_some() {
        s.push_str("## Latency\n\n");
        s.push_str(
            "Provisioning wall-clock (apply N idle agents → all Running). `reached_target=false` \
             means the host could not fit N (host-bound, not an agentctl limit):\n\n",
        );
        if let Some(t) = csv_table(rd, "latency") {
            s.push_str(&t);
            s.push('\n');
        } else {
            s.push_str("_(no data captured)_\n\n");
        }
    }

    s.push_str("## Re-run on a real cluster\n\n");
    s.push_str(
        "```sh\n\
         make -C e2e e2e bench report KUBECONFIG=<real-kubeconfig> SKIP_BRINGUP=1\n\
         ```\n",
    );
    s
}

/// Append one overhead table row from a `{cpu_millicores, mem_mib}` object.
fn push_overhead_row(s: &mut String, label: &str, obj: Option<&Value>) {
    let (cpu, mem) = obj
        .map(|o| {
            (
                o.get("cpu_millicores")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                o.get("mem_mib").and_then(Value::as_f64).unwrap_or(0.0),
            )
        })
        .unwrap_or((0.0, 0.0));
    s.push_str(&format!("| {label} | {cpu:.2} | {mem:.2} |\n"));
}
