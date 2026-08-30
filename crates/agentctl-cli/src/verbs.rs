// SPDX-License-Identifier: Apache-2.0
//! Management/lifecycle verbs (`agentctl drain|lame-duck|pause|resume|cancel`)
//! — POSTs to the aggregated management API
//! (`management.agentctl.dev/v1alpha1`), so they ride the caller's kubeconfig
//! and Kubernetes RBAC exactly like `kubectl`. The apiserver reaches the agent
//! pod over mTLS as the Management origin; the CLI never dials a pod.

#[cfg(test)]
use anyhow::bail;
use anyhow::{Context, Result};
use clap::Args;
use kube::Client;

#[derive(Args)]
pub struct VerbArgs {
    /// Agent (or, with --fleet, AgentFleet) name.
    pub name: String,
    /// Namespace (defaults to the kubeconfig context namespace).
    #[arg(short = 'n', long)]
    pub namespace: Option<String>,
    /// Target an AgentFleet (the verb fans out per the apiserver's policy).
    #[arg(long)]
    pub fleet: bool,
}

/// The aggregated-API path for a management verb.
fn verb_path(ns: &str, resource: &str, name: &str, verb: &str) -> String {
    format!("/apis/management.agentctl.dev/v1alpha1/namespaces/{ns}/{resource}/{name}/{verb}")
}

pub async fn run_verb(verb: &str, args: VerbArgs) -> Result<()> {
    let client = Client::try_default().await?;
    let ns = args
        .namespace
        .unwrap_or_else(|| client.default_namespace().to_string());
    let resource = if args.fleet { "agentfleets" } else { "agents" };
    let path = verb_path(&ns, resource, &args.name, verb);

    let req = http::Request::post(&path)
        .header("content-type", "application/json")
        .body(Vec::from(&b"{}"[..]))
        .context("build management request")?;
    let resp: serde_json::Value = client
        .request(req)
        .await
        .with_context(|| format!("{verb} {resource}/{} in {ns}", args.name))?;
    match resp.get("message").and_then(serde_json::Value::as_str) {
        Some(msg) => println!("{msg}"),
        None => println!("{}", serde_json::to_string_pretty(&resp)?),
    }
    Ok(())
}

/// The frozen management verb set (each is its own subcommand; this is the
/// single place a new verb gets vetted before wiring).
#[cfg(test)]
fn known_verb(verb: &str) -> Result<&str> {
    match verb {
        "drain" | "lame-duck" | "pause" | "resume" | "cancel" => Ok(verb),
        other => bail!("unknown management verb {other:?} (drain|lame-duck|pause|resume|cancel)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_aggregated_api_shape() {
        assert_eq!(
            verb_path("org-acme", "agents", "triage", "drain"),
            "/apis/management.agentctl.dev/v1alpha1/namespaces/org-acme/agents/triage/drain"
        );
        assert_eq!(
            verb_path("default", "agentfleets", "workers", "pause"),
            "/apis/management.agentctl.dev/v1alpha1/namespaces/default/agentfleets/workers/pause"
        );
    }

    #[test]
    fn verbs_are_the_frozen_management_set() {
        for v in ["drain", "lame-duck", "pause", "resume", "cancel"] {
            assert!(known_verb(v).is_ok());
        }
        assert!(known_verb("restart").is_err());
    }
}
