// SPDX-License-Identifier: BUSL-1.1
//! # agentctl-e2e
//!
//! Shared library for the agentctl end-to-end + scale-benchmark harness (Phase 4).
//! The two binaries — `e2e` (functional scenarios) and `bench` (scale/resource
//! sweeps) — both build a [`Ctx`] from the ambient environment and then lean on the
//! helpers here:
//!
//! * [`kube_helpers`] — a kube client from `KUBECONFIG`, typed CR apply/delete, and
//!   the poll/wait oracles (pod Running, Agent phase, a metric crossing a threshold).
//! * [`prom`] — scrape + parse Prometheus `/metrics` (apiserver proxy OR
//!   port-forward) into queryable samples.
//! * [`shell`] — `kubectl`/`helm` shell-outs incl. `kubectl top` parsing.
//! * [`results`] — the per-run CSV/JSON sink under `e2e/results/<ts>/`.
//! * [`host`] — the host-profile header every benchmark stamps.
//! * [`contract`] — load the frozen contract schemas as assertion oracles.
//!
//! It reuses `agent-api` (the typed `Agent`/`AgentFleet`/`ModelPool` CRs) and
//! `agent-contract-client` (capabilities-manifest validation) — principle P0: the
//! harness drives the *contract*, never a specific agent's internals.

use std::path::PathBuf;

use anyhow::Result;

pub mod contract;
pub mod host;
pub mod kube_helpers;
pub mod prom;
pub mod results;
pub mod shell;

/// Default reference agent image — agentd v1.0.0 (the plan's density subject).
///
/// This is the LOCAL tag `e2e/images.sh` builds (from `/root/agentd-dev`) or
/// re-tags the GHCR image to, and `kind load`s, and that every `e2e/manifests/*`
/// hard-codes. It is the full-featured build (`build_features`: serve-mcp, a2a,
/// shard/cluster, cron, metrics, …) the a2a + shard scenarios need — the public
/// `ghcr.io/agentd-dev/agentd:1.0.0` is a minimal build without those surfaces.
/// Override `AGENTD_IMAGE` to a registry-qualified ref for a real cluster.
pub const DEFAULT_AGENTD_IMAGE: &str = "agentd:1.0.0";
/// Default control-plane (Helm release) namespace.
pub const DEFAULT_SYSTEM_NS: &str = "agentctl-system";
/// Default workload namespace the scenarios apply CRs into.
pub const DEFAULT_NS: &str = "default";

/// Environment-derived configuration. Every value has a default so the suite runs
/// against kind out of the box; override any knob to retarget a real cluster.
#[derive(Debug, Clone)]
pub struct Config {
    /// Namespace the scenarios apply Agents/Fleets/ModelPools into.
    pub ns: String,
    /// The control-plane namespace (operator, gateways, coordination, apiserver).
    pub system_ns: String,
    /// The Helm chart path (for the gate-overlay `helm upgrade`s + `helm template`).
    pub chart: String,
    /// The Helm release name.
    pub release: String,
    /// The `e2e/` tree holding `install.sh` + `values/` overlays + `manifests/`.
    pub e2e_dir: PathBuf,
    /// The contract schema directory (assertion oracles).
    pub contract_dir: PathBuf,
    /// The reference agent image under test.
    pub agentd_image: String,
    /// Whether the cluster runs the Calico CNI (NetworkPolicy enforces). When
    /// false, the netpol scenario skips-with-reason (kindnet does not enforce).
    pub calico: bool,
}

impl Config {
    /// Build the config from environment variables (all optional).
    pub fn from_env() -> Self {
        Config {
            ns: env_or("AGENTCTL_E2E_NAMESPACE", DEFAULT_NS),
            system_ns: env_or("AGENTCTL_E2E_SYSTEM_NAMESPACE", DEFAULT_SYSTEM_NS),
            chart: env_or("AGENTCTL_CHART", "charts/agentctl"),
            release: env_or("AGENTCTL_RELEASE", "agentctl"),
            e2e_dir: PathBuf::from(env_or("AGENTCTL_E2E_DIR", "e2e")),
            contract_dir: PathBuf::from(env_or("AGENTCTL_CONTRACT_DIR", "contract/schemas")),
            agentd_image: env_or("AGENTD_IMAGE", DEFAULT_AGENTD_IMAGE),
            calico: std::env::var("AGENTCTL_E2E_CALICO")
                .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
        }
    }

    /// Path to `e2e/install.sh` (the security overlays shell out to it).
    pub fn install_sh(&self) -> PathBuf {
        self.e2e_dir.join("install.sh")
    }

    /// Path to an `e2e/values/<name>.yaml` overlay.
    pub fn values_overlay(&self, name: &str) -> PathBuf {
        self.e2e_dir.join("values").join(format!("{name}.yaml"))
    }

    /// Path to an `e2e/manifests/<name>.yaml` CR set.
    pub fn manifest(&self, name: &str) -> PathBuf {
        self.e2e_dir.join("manifests").join(format!("{name}.yaml"))
    }
}

/// The run context threaded through every scenario + sweep: the kube client, an
/// HTTP client, and the resolved [`Config`].
pub struct Ctx {
    /// Typed kube-rs client (CR apply/watch/status).
    pub client: kube::Client,
    /// HTTP client for `/metrics` (port-forward), the A2A gateway, the
    /// ModelGateway, and the coordination `/mcp` load-gen.
    pub http: reqwest::Client,
    /// Resolved configuration.
    pub cfg: Config,
}

impl Ctx {
    /// Build the context: a kube client from `KUBECONFIG` + a default reqwest client.
    pub async fn build() -> Result<Self> {
        Ok(Ctx {
            client: kube_helpers::client().await?,
            http: reqwest::Client::builder()
                .user_agent("agentctl-e2e")
                .build()?,
            cfg: Config::from_env(),
        })
    }
}

/// Read an environment variable or fall back to a default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}
