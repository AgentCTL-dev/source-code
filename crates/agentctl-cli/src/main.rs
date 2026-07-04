// SPDX-License-Identifier: Apache-2.0
//! # agentctl
//!
//! The agentctl CLI: read-only `get` / `describe` over the Kubernetes API for
//! [`Agent`] resources. Management/lifecycle verbs (`tree`, `drain`, …) reach an
//! agent by dialing its pod directly over mTLS as the Management origin and are
//! not part of this read-only surface.
//!
//! The binary is named `agentctl`. Installed via Krew as `kubectl-agent` /
//! `kubectl-agents` it serves `kubectl agent[s] …`; a second `[[bin]]` or an
//! argv0 dispatch can route that without changing this logic. The table columns
//! deliberately mirror the `Agent` CRD printer columns declared in `agent-api`.

mod install;

use agent_api::{Agent, AgentStatus, Mode, Substrate};
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use k8s_openapi::jiff::Timestamp;
use kube::{Api, Client, ResourceExt};

/// agentctl — the kubectl-style CLI for conformant agents.
#[derive(Parser)]
#[command(
    name = "agentctl",
    version,
    about = "Control plane CLI for conformant agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List agents as an aligned table (mirrors the CRD printer columns).
    Get(GetArgs),
    /// Show one agent's spec summary and its status conditions.
    Describe(DescribeArgs),
    /// Install (or upgrade) the agentctl control plane via Helm.
    Install(install::InstallArgs),
    /// Uninstall the agentctl control plane via Helm.
    Uninstall(install::UninstallArgs),
}

#[derive(Args)]
struct GetArgs {
    /// Resource type to list. Only `agents` (alias `agent`) is supported.
    #[arg(default_value = "agents")]
    resource: String,
    /// Namespace to read from (defaults to the kubeconfig context namespace).
    #[arg(short = 'n', long)]
    namespace: Option<String>,
    /// List across all namespaces (adds a NAMESPACE column).
    #[arg(short = 'A', long = "all-namespaces")]
    all_namespaces: bool,
    /// Output format. Only `wide` is supported (adds the MODEL column).
    #[arg(short = 'o', long = "output")]
    output: Option<String>,
}

#[derive(Args)]
struct DescribeArgs {
    /// Agent name.
    name: String,
    /// Namespace to read from (defaults to the kubeconfig context namespace).
    #[arg(short = 'n', long)]
    namespace: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Get(args) => run_get(args).await,
        Command::Describe(args) => run_describe(args).await,
        Command::Install(args) => install::run_install(args).await,
        Command::Uninstall(args) => install::run_uninstall(args).await,
    }
}

// ===========================================================================
// Commands (the only impure layer: build a client, hit the apiserver, print).
// ===========================================================================

async fn run_get(args: GetArgs) -> Result<()> {
    ensure_agents_resource(&args.resource)?;
    let wide = wants_wide(args.output.as_deref())?;

    let client = Client::try_default().await?;
    let now = Timestamp::now();

    let mut rows = vec![header_row(wide, args.all_namespaces)];
    let empty;
    let scope;

    if args.all_namespaces {
        let agents = Api::<Agent>::all(client).list(&Default::default()).await?;
        empty = agents.items.is_empty();
        scope = None;
        for agent in &agents.items {
            let mut row = vec![agent.namespace().unwrap_or_else(|| "-".to_string())];
            row.extend(get_row(agent, now, wide));
            rows.push(row);
        }
    } else {
        let ns = args
            .namespace
            .clone()
            .unwrap_or_else(|| client.default_namespace().to_string());
        let agents = Api::<Agent>::namespaced(client, &ns)
            .list(&Default::default())
            .await?;
        empty = agents.items.is_empty();
        scope = Some(ns);
        for agent in &agents.items {
            rows.push(get_row(agent, now, wide));
        }
    }

    if empty {
        match scope {
            Some(ns) => println!("No agents found in {ns} namespace."),
            None => println!("No agents found."),
        }
    } else {
        print!("{}", render_table(&rows));
    }
    Ok(())
}

async fn run_describe(args: DescribeArgs) -> Result<()> {
    let client = Client::try_default().await?;
    let ns = args
        .namespace
        .unwrap_or_else(|| client.default_namespace().to_string());
    let agent = Api::<Agent>::namespaced(client, &ns)
        .get(&args.name)
        .await?;
    print!("{}", describe_agent(&agent, Timestamp::now()));
    Ok(())
}

// ===========================================================================
// Pure helpers (no clock, no network — unit-tested below).
// ===========================================================================

/// The Ready condition's status (`"True"`/`"False"`/`"Unknown"`), or `"-"` when
/// the agent advertises no Ready condition.
fn ready_of(status: &AgentStatus) -> &str {
    status
        .conditions
        .iter()
        .find(|c| c.type_ == "Ready")
        .map(|c| c.status.as_str())
        .unwrap_or("-")
}

/// The coarse phase, or `"-"`.
fn phase_of(status: &AgentStatus) -> &str {
    status.phase.as_deref().unwrap_or("-")
}

/// Canonical wire spelling of a [`Mode`] (matches the serde rename).
fn mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Once => "once",
        Mode::Loop => "loop",
        Mode::Reactive => "reactive",
        Mode::Schedule => "schedule",
        Mode::Workflow => "workflow",
    }
}

/// Canonical wire spelling of a [`Substrate`].
fn substrate_str(substrate: Substrate) -> &'static str {
    match substrate {
        Substrate::StockUnix => "stock-unix",
        Substrate::KataHybrid => "kata-hybrid",
        Substrate::SidecarEmptydir => "sidecar-emptydir",
    }
}

/// kubectl-style coarse age: largest single unit, e.g. `"45s"`, `"5m"`, `"3h"`,
/// `"2d"`, `"1y"`. `now` is passed in so the function stays clock-free/testable.
fn format_age(now: Timestamp, creation: Timestamp) -> String {
    let secs = now.as_second() - creation.as_second();
    if secs < 60 {
        return format!("{}s", secs.max(0));
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 365 {
        return format!("{days}d");
    }
    format!("{}y", days / 365)
}

/// One table row for an agent, in printer-column order: NAME, MODE, [MODEL when
/// `wide`], READY, PHASE, AGE. NAMESPACE (for `-A`) is prepended by the caller.
fn get_row(agent: &Agent, now: Timestamp, wide: bool) -> Vec<String> {
    let ready = agent.status.as_ref().map_or("-", ready_of).to_string();
    let phase = agent.status.as_ref().map_or("-", phase_of).to_string();
    let age = agent
        .creation_timestamp()
        .map_or_else(|| "-".to_string(), |t| format_age(now, t.0));

    let mut row = vec![agent.name_any(), mode_str(agent.spec.mode).to_string()];
    if wide {
        row.push(agent.spec.model.clone().unwrap_or_else(|| "-".to_string()));
    }
    row.push(ready);
    row.push(phase);
    row.push(age);
    row
}

/// The header row matching [`get_row`] under the same `wide`/`all_ns` flags.
fn header_row(wide: bool, all_ns: bool) -> Vec<String> {
    let mut h = Vec::new();
    if all_ns {
        h.push("NAMESPACE".to_string());
    }
    h.push("NAME".to_string());
    h.push("MODE".to_string());
    if wide {
        h.push("MODEL".to_string());
    }
    h.push("READY".to_string());
    h.push("PHASE".to_string());
    h.push("AGE".to_string());
    h
}

/// Left-align rows into a column-padded table (header expected as `rows[0]`).
/// Columns are separated by three spaces; trailing padding is trimmed.
fn render_table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        let last = row.len().saturating_sub(1);
        for (i, cell) in row.iter().enumerate() {
            if i == last {
                line.push_str(cell);
            } else {
                let pad = widths[i] - cell.len();
                line.push_str(cell);
                line.push_str(&" ".repeat(pad + 3));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Human-readable spec summary + status conditions table for `describe`.
fn describe_agent(agent: &Agent, now: Timestamp) -> String {
    let spec = &agent.spec;
    let mut out = String::new();

    out.push_str(&format!("Name:        {}\n", agent.name_any()));
    if let Some(ns) = agent.namespace() {
        out.push_str(&format!("Namespace:   {ns}\n"));
    }
    out.push_str(&format!("Mode:        {}\n", mode_str(spec.mode)));
    out.push_str(&format!(
        "Image:       {}\n",
        spec.image.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!(
        "Model:       {}\n",
        spec.model.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!(
        "Substrate:   {}\n",
        spec.substrate.map_or("-", substrate_str)
    ));
    if let Some(t) = agent.creation_timestamp() {
        out.push_str(&format!("Age:         {}\n", format_age(now, t.0)));
    }

    if let Some(status) = &agent.status {
        out.push_str(&format!("Phase:       {}\n", phase_of(status)));
        out.push_str(&format!("Ready:       {}\n", ready_of(status)));
        if let Some(contract) = &status.contract {
            if let Some(v) = &contract.contract_version {
                out.push_str(&format!("Contract:    {v}\n"));
            }
        }
        if !status.conditions.is_empty() {
            out.push_str("\nConditions:\n");
            let mut rows = vec![vec![
                "TYPE".to_string(),
                "STATUS".to_string(),
                "REASON".to_string(),
                "MESSAGE".to_string(),
            ]];
            for c in &status.conditions {
                rows.push(vec![
                    c.type_.clone(),
                    c.status.clone(),
                    c.reason.clone().unwrap_or_else(|| "-".to_string()),
                    c.message.clone().unwrap_or_else(|| "-".to_string()),
                ]);
            }
            for line in render_table(&rows).lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
    }
    out
}

/// Reject resource arguments other than `agents`/`agent`.
fn ensure_agents_resource(resource: &str) -> Result<()> {
    match resource {
        "agents" | "agent" => Ok(()),
        other => anyhow::bail!("unknown resource {other:?} (only 'agents' is supported)"),
    }
}

/// Map `-o` into the wide flag; reject unsupported formats.
fn wants_wide(output: Option<&str>) -> Result<bool> {
    match output {
        None => Ok(false),
        Some("wide") => Ok(true),
        Some(other) => {
            anyhow::bail!("unsupported output format {other:?} (only 'wide' is supported)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_api::{AgentSpec, Condition, ContractStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    /// A fixed reference instant (2023-11-14T22:13:20Z) for clock-free tests.
    const NOW: i64 = 1_700_000_000;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn cond(type_: &str, status: &str) -> Condition {
        Condition {
            type_: type_.to_string(),
            status: status.to_string(),
            ..Default::default()
        }
    }

    fn agent(name: &str, spec: AgentSpec) -> Agent {
        Agent::new(name, spec)
    }

    fn status_with(conditions: Vec<Condition>) -> AgentStatus {
        AgentStatus {
            conditions,
            ..Default::default()
        }
    }

    #[test]
    fn ready_of_reads_ready_condition() {
        let s = status_with(vec![cond("Validated", "True"), cond("Ready", "False")]);
        assert_eq!(ready_of(&s), "False");

        let s = status_with(vec![cond("Ready", "True")]);
        assert_eq!(ready_of(&s), "True");

        // No Ready condition → "-".
        let s = status_with(vec![cond("Degraded", "True")]);
        assert_eq!(ready_of(&s), "-");
    }

    #[test]
    fn phase_of_falls_back_to_dash() {
        assert_eq!(phase_of(&status_with(vec![])), "-");
        let s = AgentStatus {
            phase: Some("Running".to_string()),
            ..Default::default()
        };
        assert_eq!(phase_of(&s), "Running");
    }

    #[test]
    fn mode_and_substrate_spellings() {
        assert_eq!(mode_str(Mode::Once), "once");
        assert_eq!(mode_str(Mode::Reactive), "reactive");
        assert_eq!(substrate_str(Substrate::KataHybrid), "kata-hybrid");
        assert_eq!(
            substrate_str(Substrate::SidecarEmptydir),
            "sidecar-emptydir"
        );
    }

    #[test]
    fn format_age_picks_largest_unit() {
        let now = ts(NOW);
        assert_eq!(format_age(now, ts(NOW - 45)), "45s");
        assert_eq!(format_age(now, ts(NOW - 5 * 60)), "5m");
        assert_eq!(format_age(now, ts(NOW - 3 * 3600)), "3h");
        assert_eq!(format_age(now, ts(NOW - 2 * 86_400)), "2d");
        assert_eq!(format_age(now, ts(NOW - 800 * 86_400)), "2y");
        // Clock skew (creation in the future) clamps to 0s, never panics.
        assert_eq!(format_age(now, ts(NOW + 30)), "0s");
    }

    #[test]
    fn get_row_narrow_and_wide() {
        let now = ts(NOW);
        let mut a = agent(
            "demo",
            AgentSpec {
                mode: Mode::Reactive,
                model: Some("frontier-1".to_string()),
                ..Default::default()
            },
        );
        a.status = Some(AgentStatus {
            phase: Some("Running".to_string()),
            conditions: vec![cond("Ready", "True")],
            ..Default::default()
        });
        a.metadata.creation_timestamp = Some(Time(ts(NOW - 2 * 86_400)));

        let narrow = get_row(&a, now, false);
        assert_eq!(narrow, vec!["demo", "reactive", "True", "Running", "2d"]);

        let wide = get_row(&a, now, true);
        assert_eq!(
            wide,
            vec!["demo", "reactive", "frontier-1", "True", "Running", "2d"]
        );
    }

    #[test]
    fn get_row_handles_missing_status_and_creation() {
        let now = ts(NOW);
        let a = agent("bare", AgentSpec::default());
        // Default mode is `once`; no status, no creationTimestamp.
        assert_eq!(get_row(&a, now, false), vec!["bare", "once", "-", "-", "-"]);
    }

    #[test]
    fn render_table_aligns_columns() {
        let rows = vec![
            vec!["NAME".to_string(), "MODE".to_string(), "AGE".to_string()],
            vec!["a".to_string(), "once".to_string(), "5m".to_string()],
            vec![
                "longname".to_string(),
                "reactive".to_string(),
                "2d".to_string(),
            ],
        ];
        let table = render_table(&rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines[0], "NAME       MODE       AGE");
        assert_eq!(lines[1], "a          once       5m");
        assert_eq!(lines[2], "longname   reactive   2d");
        // No trailing whitespace on any line.
        assert!(lines.iter().all(|l| l == &l.trim_end()));
    }

    #[test]
    fn describe_renders_summary_and_conditions() {
        let now = ts(NOW);
        let mut a = agent(
            "demo",
            AgentSpec {
                mode: Mode::Loop,
                image: Some("ghcr.io/example/agent:1".to_string()),
                model: Some("frontier-1".to_string()),
                substrate: Some(Substrate::StockUnix),
                ..Default::default()
            },
        );
        a.metadata.creation_timestamp = Some(Time(ts(NOW - 5 * 3600)));
        a.status = Some(AgentStatus {
            phase: Some("Running".to_string()),
            conditions: vec![cond("Ready", "True"), cond("Degraded", "False")],
            contract: Some(ContractStatus {
                contract_version: Some("0.5".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });

        let text = describe_agent(&a, now);
        assert!(text.contains("Name:        demo"));
        assert!(text.contains("Mode:        loop"));
        assert!(text.contains("Image:       ghcr.io/example/agent:1"));
        assert!(text.contains("Substrate:   stock-unix"));
        assert!(text.contains("Age:         5h"));
        assert!(text.contains("Contract:    0.5"));
        assert!(text.contains("Conditions:"));
        assert!(text.contains("Ready"));
        assert!(text.contains("Degraded"));
    }

    #[test]
    fn resource_and_output_validation() {
        assert!(ensure_agents_resource("agents").is_ok());
        assert!(ensure_agents_resource("agent").is_ok());
        assert!(ensure_agents_resource("pods").is_err());

        assert!(!wants_wide(None).unwrap());
        assert!(wants_wide(Some("wide")).unwrap());
        assert!(wants_wide(Some("json")).is_err());
    }
}
