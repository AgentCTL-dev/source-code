// SPDX-License-Identifier: Apache-2.0
//! `agentctl install` / `agentctl uninstall` — a thin, opinionated wrapper around
//! the agentctl Helm chart.
//!
//! Helm owns the actual rollout; this wrapper only does the two things Helm can't
//! reliably do for the agentctl chart:
//!   1. **Preflight** — fail fast if `helm` isn't on `PATH`, and check that
//!      cert-manager is installed (its CRDs are a hard prerequisite for every
//!      serving/mTLS cert the chart issues).
//!   2. **Own the namespace** — Helm can't reliably own the namespace it installs
//!      into, and the chart's control-plane workloads need a *privileged*
//!      PodSecurity level, so we create + label the namespace ourselves.
//!
//! Then it shells out to `helm upgrade --install …`, inheriting stdio and
//! propagating Helm's exit status.

use anyhow::{bail, Context, Result};
use clap::Args;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Patch, PatchParams, PostParams};
use kube::{Api, Client};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

/// The Helm release name the chart is always installed under.
const RELEASE: &str = "agentctl";
/// The default namespace for the control plane.
const DEFAULT_NAMESPACE: &str = "agentctl-system";
/// The default chart reference (the published OCI chart; may be a local path).
const DEFAULT_CHART: &str = "oci://ghcr.io/agentctl-dev/charts/agentctl";
/// A cert-manager CRD whose presence stands in for "cert-manager is installed".
const CERT_MANAGER_CRD: &str = "certificates.cert-manager.io";

/// Install (or upgrade) the agentctl control plane via Helm.
#[derive(Args)]
pub struct InstallArgs {
    /// Namespace to install into.
    #[arg(short = 'n', long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,

    /// Chart reference: an OCI ref, a repo chart, or a local path (e.g.
    /// `./charts/agentctl`).
    #[arg(long, default_value = DEFAULT_CHART)]
    chart: String,

    /// Chart version to install (passed to `helm --version`).
    #[arg(long)]
    version: Option<String>,

    /// Image registry override (sets `image.registry`).
    #[arg(long)]
    registry: Option<String>,

    /// Image tag override (sets `image.tag`).
    #[arg(long)]
    tag: Option<String>,

    /// Extra `--set key=value` overrides (repeatable, passed through verbatim).
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set: Vec<String>,

    /// Create the namespace if it does not already exist.
    #[arg(
        long,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_value_t = true,
        default_missing_value = "true",
    )]
    create_namespace: bool,

    /// Render and validate without mutating the cluster (passes `helm --dry-run`;
    /// skips the namespace step and downgrades the cert-manager check to a warning).
    #[arg(long)]
    dry_run: bool,
}

/// Remove the agentctl control plane via Helm.
#[derive(Args)]
pub struct UninstallArgs {
    /// Namespace the release was installed into.
    #[arg(short = 'n', long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,
}

// ===========================================================================
// Commands (the impure layer: preflight, touch the cluster, then exec helm).
// ===========================================================================

/// Run `agentctl install`.
pub async fn run_install(args: InstallArgs) -> Result<()> {
    preflight(args.dry_run).await?;

    if args.dry_run {
        eprintln!("--dry-run: skipping namespace creation (no cluster mutations).");
    } else {
        ensure_namespace(&args.namespace, args.create_namespace).await?;
    }

    let helm_args = build_helm_install_args(
        &args.chart,
        &args.namespace,
        args.version.as_deref(),
        args.registry.as_deref(),
        args.tag.as_deref(),
        &args.set,
        args.dry_run,
    );
    run_helm(&helm_args)
}

/// Run `agentctl uninstall`.
pub async fn run_uninstall(args: UninstallArgs) -> Result<()> {
    if which_in_path("helm").is_none() {
        bail!(helm_missing_message());
    }
    run_helm(&build_helm_uninstall_args(&args.namespace))
}

// ===========================================================================
// Preflight.
// ===========================================================================

/// Fail fast on missing prerequisites: `helm` on `PATH`, and cert-manager
/// installed. Under `--dry-run` the cert-manager check is a warning, not an error.
async fn preflight(dry_run: bool) -> Result<()> {
    if which_in_path("helm").is_none() {
        bail!(helm_missing_message());
    }

    match cert_manager_present().await {
        Ok(true) => Ok(()),
        Ok(false) => {
            let msg = cert_manager_missing_message();
            if dry_run {
                eprintln!("warning: {msg}");
                Ok(())
            } else {
                bail!(msg)
            }
        }
        Err(e) => {
            if dry_run {
                eprintln!(
                    "warning: could not verify cert-manager is installed \
                     (continuing because --dry-run): {e:#}"
                );
                Ok(())
            } else {
                Err(e).context("failed to check for the cert-manager CRD")
            }
        }
    }
}

/// Whether the cert-manager CRD is present in the cluster.
async fn cert_manager_present() -> Result<bool> {
    let client = Client::try_default()
        .await
        .context("failed to build a Kubernetes client from the kubeconfig context")?;
    let crds: Api<CustomResourceDefinition> = Api::all(client);
    Ok(crds.get_opt(CERT_MANAGER_CRD).await?.is_some())
}

// ===========================================================================
// Namespace ownership.
// ===========================================================================

/// Ensure the install namespace exists and carries the privileged PodSecurity
/// labels the chart's control-plane workloads require. Creates it when `create`
/// is set; otherwise errors if it is absent. Existing namespaces are patched to
/// (re)apply labels.
async fn ensure_namespace(ns: &str, create: bool) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("failed to build a Kubernetes client from the kubeconfig context")?;
    let api: Api<Namespace> = Api::all(client);

    match api
        .get_opt(ns)
        .await
        .with_context(|| format!("failed to look up namespace {ns}"))?
    {
        Some(_) => {
            let patch = serde_json::json!({ "metadata": { "labels": pss_labels() } });
            api.patch(ns, &PatchParams::default(), &Patch::Merge(&patch))
                .await
                .with_context(|| format!("failed to apply PodSecurity labels to namespace {ns}"))?;
            println!("Namespace {ns} already exists; ensured privileged PodSecurity labels.");
        }
        None => {
            if !create {
                bail!(
                    "namespace {ns} does not exist and --create-namespace=false.\n  \
                     Create it yourself (and label it \
                     pod-security.kubernetes.io/enforce=privileged) or re-run with \
                     --create-namespace."
                );
            }
            let nsobj = Namespace {
                metadata: ObjectMeta {
                    name: Some(ns.to_string()),
                    labels: Some(pss_labels()),
                    ..Default::default()
                },
                ..Default::default()
            };
            api.create(&PostParams::default(), &nsobj)
                .await
                .with_context(|| format!("failed to create namespace {ns}"))?;
            println!("Created namespace {ns} with privileged PodSecurity labels.");
        }
    }
    Ok(())
}

// ===========================================================================
// Pure helpers (no clock, no network — unit-tested below).
// ===========================================================================

/// The privileged PodSecurity labels the chart's control-plane workloads require
/// on the install namespace.
fn pss_labels() -> BTreeMap<String, String> {
    [
        ("pod-security.kubernetes.io/enforce", "privileged"),
        ("pod-security.kubernetes.io/warn", "privileged"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect()
}

/// `which`-style PATH lookup: first directory in `$PATH` holding a file named
/// `bin`, or `None`.
fn which_in_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// Build the `helm upgrade --install …` argv (excluding the `helm` program name).
fn build_helm_install_args(
    chart: &str,
    namespace: &str,
    version: Option<&str>,
    registry: Option<&str>,
    tag: Option<&str>,
    sets: &[String],
    dry_run: bool,
) -> Vec<String> {
    let mut args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        RELEASE.to_string(),
        chart.to_string(),
        "-n".to_string(),
        namespace.to_string(),
    ];
    if let Some(v) = version {
        args.push("--version".to_string());
        args.push(v.to_string());
    }
    if let Some(r) = registry {
        args.push("--set".to_string());
        args.push(format!("image.registry={r}"));
    }
    if let Some(t) = tag {
        args.push("--set".to_string());
        args.push(format!("image.tag={t}"));
    }
    for s in sets {
        args.push("--set".to_string());
        args.push(s.clone());
    }
    if dry_run {
        args.push("--dry-run".to_string());
    }
    args
}

/// Build the `helm uninstall …` argv (excluding the `helm` program name).
fn build_helm_uninstall_args(namespace: &str) -> Vec<String> {
    vec![
        "uninstall".to_string(),
        RELEASE.to_string(),
        "-n".to_string(),
        namespace.to_string(),
    ]
}

/// The error message shown when `helm` isn't on `PATH`.
fn helm_missing_message() -> String {
    "`helm` was not found on PATH.\n  \
     agentctl install/uninstall shells out to Helm to manage the chart.\n  \
     Install Helm (https://helm.sh/docs/intro/install/) and re-run."
        .to_string()
}

/// The error/warning shown when cert-manager doesn't appear to be installed.
fn cert_manager_missing_message() -> String {
    format!(
        "cert-manager does not appear to be installed: the `{CERT_MANAGER_CRD}` CRD was not found.\n  \
         The agentctl chart requires cert-manager (>= 1.13) — it issues every serving/mTLS cert\n  \
         and injects the caBundles. Install it first, e.g.:\n    \
         kubectl apply -f https://github.com/cert-manager/cert-manager/releases/latest/download/cert-manager.yaml"
    )
}

// ===========================================================================
// helm exec.
// ===========================================================================

/// Run `helm` with the given args, inheriting stdio. On a non-zero Helm exit,
/// propagate Helm's exit code as our own (so callers/scripts see it verbatim).
fn run_helm(args: &[String]) -> Result<()> {
    let status = Command::new("helm")
        .args(args)
        .status()
        .context("failed to execute `helm` (is it installed and on PATH?)")?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pss_labels_are_privileged_enforce_and_warn() {
        let labels = pss_labels();
        assert_eq!(
            labels
                .get("pod-security.kubernetes.io/enforce")
                .map(String::as_str),
            Some("privileged")
        );
        assert_eq!(
            labels
                .get("pod-security.kubernetes.io/warn")
                .map(String::as_str),
            Some("privileged")
        );
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn install_args_minimal() {
        let args = build_helm_install_args(
            DEFAULT_CHART,
            "agentctl-system",
            None,
            None,
            None,
            &[],
            false,
        );
        assert_eq!(
            args,
            vec![
                "upgrade",
                "--install",
                "agentctl",
                DEFAULT_CHART,
                "-n",
                "agentctl-system",
            ]
        );
    }

    #[test]
    fn install_args_full() {
        let sets = vec![
            "postgres.mode=external".to_string(),
            "gateway.enabled=false".to_string(),
        ];
        let args = build_helm_install_args(
            "./charts/agentctl",
            "agents",
            Some("1.2.3"),
            Some("ghcr.io/agentctl-dev"),
            Some("v0.4.0"),
            &sets,
            true,
        );
        assert_eq!(
            args,
            vec![
                "upgrade",
                "--install",
                "agentctl",
                "./charts/agentctl",
                "-n",
                "agents",
                "--version",
                "1.2.3",
                "--set",
                "image.registry=ghcr.io/agentctl-dev",
                "--set",
                "image.tag=v0.4.0",
                "--set",
                "postgres.mode=external",
                "--set",
                "gateway.enabled=false",
                "--dry-run",
            ]
        );
    }

    #[test]
    fn install_args_registry_and_tag_only() {
        let args = build_helm_install_args(
            DEFAULT_CHART,
            "agentctl-system",
            None,
            Some("my.registry.io"),
            Some("dev"),
            &[],
            false,
        );
        assert_eq!(
            args,
            vec![
                "upgrade",
                "--install",
                "agentctl",
                DEFAULT_CHART,
                "-n",
                "agentctl-system",
                "--set",
                "image.registry=my.registry.io",
                "--set",
                "image.tag=dev",
            ]
        );
    }

    #[test]
    fn uninstall_args_shape() {
        assert_eq!(
            build_helm_uninstall_args("agentctl-system"),
            vec!["uninstall", "agentctl", "-n", "agentctl-system"]
        );
    }

    #[test]
    fn which_in_path_finds_a_real_binary() {
        // `sh` exists on any POSIX PATH used to run these tests.
        assert!(which_in_path("sh").is_some());
        assert!(which_in_path("this-binary-does-not-exist-agentctl").is_none());
    }
}
