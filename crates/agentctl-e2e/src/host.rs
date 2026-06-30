// SPDX-License-Identifier: BUSL-1.1
//! Host-profile capture — the header every benchmark run stamps so a number is
//! never read without its (single-node, host-bound) context. The plan makes this
//! caveat load-bearing: a kind density ceiling is a *trend*, not a capacity claim.

use serde::{Deserialize, Serialize};

use crate::shell;

/// A snapshot of the machine + cluster a benchmark ran on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostProfile {
    /// `uname -s -r -m` (OS / kernel / arch), best-effort.
    pub uname: String,
    /// Logical CPU count.
    pub cpus: usize,
    /// Total system memory in MiB (from `/proc/meminfo`), 0 if unknown.
    pub mem_total_mib: u64,
    /// CPU model string (from `/proc/cpuinfo`), empty if unknown.
    pub cpu_model: String,
    /// `kubectl version` server gitVersion, empty if unreachable.
    pub kube_version: String,
    /// Node count reported by the cluster, 0 if unknown (single-node kind ⇒ 1).
    pub node_count: usize,
}

impl HostProfile {
    /// Capture the current host + cluster profile (every probe is best-effort; a
    /// missing value degrades to a neutral default rather than failing the run).
    pub fn capture() -> Self {
        HostProfile {
            uname: uname(),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            mem_total_mib: mem_total_mib(),
            cpu_model: cpu_model(),
            kube_version: kube_server_version(),
            node_count: node_count(),
        }
    }

    /// A compact Markdown table for the `docs/benchmarks.md` header.
    pub fn markdown(&self) -> String {
        format!(
            "| Field | Value |\n|---|---|\n\
             | Host | {} |\n\
             | CPU | {} ({} logical) |\n\
             | Memory | {} MiB |\n\
             | Kubernetes | {} |\n\
             | Nodes | {} |\n",
            esc(&self.uname),
            esc(&self.cpu_model),
            self.cpus,
            self.mem_total_mib,
            esc(&self.kube_version),
            self.node_count,
        )
    }
}

/// Escape a value for a Markdown table cell (only `|` matters here).
fn esc(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        "unknown".to_string()
    } else {
        t.replace('|', "\\|")
    }
}

fn uname() -> String {
    shell::run("uname", &["-s", "-r", "-m"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn mem_total_mib() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            // `MemTotal:  16331756 kB`
            if let Some(kb) = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
            {
                return kb / 1024;
            }
        }
    }
    0
}

fn cpu_model() -> String {
    let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") else {
        return String::new();
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("model name") {
            if let Some((_, v)) = rest.split_once(':') {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

fn kube_server_version() -> String {
    // `-o json` is stable across kubectl versions; pull serverVersion.gitVersion.
    let Ok(out) = shell::kubectl(&["version", "-o", "json"]) else {
        return String::new();
    };
    serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|v| {
            v.get("serverVersion")
                .and_then(|s| s.get("gitVersion"))
                .and_then(|g| g.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn node_count() -> usize {
    shell::kubectl(&["get", "nodes", "--no-headers"])
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}
