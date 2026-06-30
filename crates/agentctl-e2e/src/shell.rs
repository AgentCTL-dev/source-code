// SPDX-License-Identifier: BUSL-1.1
//! Shell-outs to `kubectl` and `helm`, plus the resource-quantity parsing the
//! benchmark needs.
//!
//! The harness is a hybrid (the plan's "bash/Make bringup + a Rust kube-rs crate"):
//! typed CR apply/watch goes through kube-rs ([`crate::kube_helpers`]), while
//! cluster-wide reads that are simpler as a one-liner — `kubectl top`, `helm
//! upgrade`, the apiserver Service proxy — shell out here. Everything honors the
//! ambient `KUBECONFIG`, so the identical suite runs against a real cluster.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Run `kubectl <args>` and return stdout, erroring (with stderr) on a nonzero exit.
pub fn kubectl(args: &[&str]) -> Result<String> {
    run("kubectl", args)
}

/// Run `helm <args>` and return stdout, erroring (with stderr) on a nonzero exit.
pub fn helm(args: &[&str]) -> Result<String> {
    run("helm", args)
}

/// Run an arbitrary program, capturing stdout. A nonzero exit is an error that
/// carries the program's stderr (so a scenario failure is legible in the summary).
pub fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("spawn `{program} {}`", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "`{program} {}` exited {}: {}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A `helm upgrade --install` invocation: render `chart` into `release` in `ns`,
/// layering `values_files` (in order) and `--set k=v` overrides, and `--wait`.
pub fn helm_upgrade(
    release: &str,
    chart: &str,
    ns: &str,
    values_files: &[&str],
    sets: &[(&str, &str)],
) -> Result<String> {
    let mut args: Vec<String> = vec![
        "upgrade".into(),
        "--install".into(),
        release.into(),
        chart.into(),
        "--namespace".into(),
        ns.into(),
        "--create-namespace".into(),
        "--wait".into(),
    ];
    for f in values_files {
        args.push("--values".into());
        args.push((*f).into());
    }
    for (k, v) in sets {
        args.push("--set".into());
        args.push(format!("{k}={v}"));
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    helm(&refs)
}

/// `helm template` a chart to its rendered YAML (used to assert the gate overlays
/// flip the right env/flags without touching the cluster).
pub fn helm_template(release: &str, chart: &str, values_files: &[&str]) -> Result<String> {
    let mut args: Vec<String> = vec!["template".into(), release.into(), chart.into()];
    for f in values_files {
        args.push("--values".into());
        args.push((*f).into());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    helm(&refs)
}

/// A live `kubectl port-forward` child, exposing a remote port on `127.0.0.1`.
/// The process is killed on drop, so a scenario can scrape/hit a cluster Service
/// over plain HTTP (the plan's "port-forward" scrape path + the coordination `/mcp`
/// throughput load-gen) without leaving a tunnel behind.
#[derive(Debug)]
pub struct PortForward {
    child: Child,
    /// The chosen local port.
    pub local_port: u16,
}

impl PortForward {
    /// Forward `127.0.0.1:<local_port>` → `svc/<svc>:<remote_port>` in `ns`. Blocks
    /// briefly to let the tunnel establish.
    pub fn service(ns: &str, svc: &str, remote_port: u16, local_port: u16) -> Result<Self> {
        Self::spawn(ns, &format!("svc/{svc}"), remote_port, local_port)
    }

    /// Forward `127.0.0.1:<local_port>` → `pod/<pod>:<remote_port>` in `ns`.
    pub fn pod(ns: &str, pod: &str, remote_port: u16, local_port: u16) -> Result<Self> {
        Self::spawn(ns, &format!("pod/{pod}"), remote_port, local_port)
    }

    fn spawn(ns: &str, target: &str, remote_port: u16, local_port: u16) -> Result<Self> {
        let child = Command::new("kubectl")
            .args([
                "port-forward",
                "-n",
                ns,
                target,
                &format!("{local_port}:{remote_port}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn kubectl port-forward {target}"))?;
        // Give the tunnel a moment to bind before the first request.
        std::thread::sleep(Duration::from_millis(1500));
        Ok(PortForward { child, local_port })
    }

    /// The local base URL (no trailing slash).
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A parsed `kubectl top` row (a pod or a node): name + CPU millicores + memory MiB.
#[derive(Debug, Clone, PartialEq)]
pub struct TopRow {
    /// Pod or node name.
    pub name: String,
    /// CPU usage in millicores (e.g. `"12m"` → 12, `"1"` → 1000).
    pub cpu_millicores: f64,
    /// Memory usage in mebibytes (e.g. `"34Mi"` → 34, `"1Gi"` → 1024).
    pub mem_mib: f64,
}

/// `kubectl top pods -n <ns> --no-headers` parsed into [`TopRow`]s.
pub fn top_pods(ns: &str) -> Result<Vec<TopRow>> {
    let out = kubectl(&["top", "pods", "-n", ns, "--no-headers"])?;
    parse_top(&out, 1)
}

/// `kubectl top nodes --no-headers` parsed into [`TopRow`]s. The node table has a
/// percentage column after CPU and after memory; we take columns 0,1,3.
pub fn top_nodes() -> Result<Vec<TopRow>> {
    let out = kubectl(&["top", "nodes", "--no-headers"])?;
    parse_top(&out, 2)
}

/// Parse a `kubectl top` table. `stride` is the column offset between CPU and
/// memory: pods are `NAME CPU MEM` (stride 1), nodes are `NAME CPU CPU% MEM MEM%`
/// (stride 2).
fn parse_top(text: &str, stride: usize) -> Result<Vec<TopRow>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 1 + stride + 1 {
            continue;
        }
        rows.push(TopRow {
            name: cols[0].to_string(),
            cpu_millicores: parse_cpu_millicores(cols[1])?,
            mem_mib: parse_mem_mib(cols[1 + stride])?,
        });
    }
    Ok(rows)
}

/// Parse a Kubernetes CPU quantity into millicores. `"12m"` → 12.0, `"1"` → 1000.0,
/// `"1500m"` → 1500.0, `"0.5"` → 500.0.
pub fn parse_cpu_millicores(s: &str) -> Result<f64> {
    let s = s.trim();
    if let Some(m) = s.strip_suffix('m') {
        return m
            .parse::<f64>()
            .with_context(|| format!("parse cpu millicores {s:?}"));
    }
    let cores = s
        .parse::<f64>()
        .with_context(|| format!("parse cpu cores {s:?}"))?;
    Ok(cores * 1000.0)
}

/// Parse a Kubernetes memory quantity into MiB. Handles binary (`Ki`/`Mi`/`Gi`) and
/// decimal (`k`/`M`/`G`) suffixes and a bare byte count.
pub fn parse_mem_mib(s: &str) -> Result<f64> {
    let s = s.trim();
    let mib = 1024.0 * 1024.0;
    let (num, factor): (&str, f64) = if let Some(n) = s.strip_suffix("Ki") {
        (n, 1024.0)
    } else if let Some(n) = s.strip_suffix("Mi") {
        (n, mib)
    } else if let Some(n) = s.strip_suffix("Gi") {
        (n, 1024.0 * mib)
    } else if let Some(n) = s.strip_suffix('k') {
        (n, 1000.0)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1_000_000.0)
    } else if let Some(n) = s.strip_suffix('G') {
        (n, 1_000_000_000.0)
    } else {
        (s, 1.0)
    };
    let v = num
        .parse::<f64>()
        .with_context(|| format!("parse memory quantity {s:?}"))?;
    Ok(v * factor / mib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_quantities() {
        assert_eq!(parse_cpu_millicores("12m").unwrap(), 12.0);
        assert_eq!(parse_cpu_millicores("1").unwrap(), 1000.0);
        assert_eq!(parse_cpu_millicores("0.5").unwrap(), 500.0);
    }

    #[test]
    fn mem_quantities() {
        assert_eq!(parse_mem_mib("34Mi").unwrap(), 34.0);
        assert_eq!(parse_mem_mib("1Gi").unwrap(), 1024.0);
        assert_eq!(parse_mem_mib("2048Ki").unwrap(), 2.0);
    }

    #[test]
    fn parses_pod_and_node_tables() {
        let pods = parse_top("agentd-0   12m   34Mi\nagentd-1   3m   20Mi\n", 1).unwrap();
        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].name, "agentd-0");
        assert_eq!(pods[0].cpu_millicores, 12.0);
        assert_eq!(pods[1].mem_mib, 20.0);

        let nodes = parse_top("kind-control-plane  250m  3%  2048Mi  13%\n", 2).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].cpu_millicores, 250.0);
        assert_eq!(nodes[0].mem_mib, 2048.0);
    }
}
