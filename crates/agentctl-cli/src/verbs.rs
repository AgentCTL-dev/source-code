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

/// `agentctl expose webhook <ns>/<name> --path /zendesk` — the P7-1 exposure
/// helper: prints the external hooks URL for a declared+exposed webhook and,
/// with `--show-secret`, the operator-provisioned HMAC/bearer value the
/// SENDER side needs (read from the `<name>-hooks` Secret; the only
/// sanctioned way to see it).
#[derive(clap::Args)]
pub struct ExposeWebhookArgs {
    /// The agent, as `<namespace>/<name>` (or bare name in `default`).
    pub agent: String,
    /// The webhook path (must match a declared webhook trigger).
    #[arg(long)]
    pub path: String,
    /// External hooks host (e.g. hooks.example.com). Without it the
    /// gateway-relative path is printed.
    #[arg(long)]
    pub host: Option<String>,
    /// Print the route's signing secret (hmac) / bearer value.
    #[arg(long, default_value_t = false)]
    pub show_secret: bool,
}

pub async fn run_expose_webhook(args: ExposeWebhookArgs) -> anyhow::Result<()> {
    use anyhow::{bail, Context as _};
    let (ns, name) = match args.agent.split_once('/') {
        Some((ns, n)) => (ns.to_string(), n.to_string()),
        None => ("default".to_string(), args.agent.clone()),
    };
    let client = Client::try_default().await?;
    let agents: kube::Api<agent_api::v1alpha2::Agent> = kube::Api::namespaced(client.clone(), &ns);
    let agent = agents
        .get_opt(&name)
        .await
        .context("read Agent")?
        .with_context(|| format!("no Agent {ns}/{name}"))?;
    let trigger_index = agent
        .spec
        .triggers
        .iter()
        .position(|t| t.webhook.as_ref().is_some_and(|w| w.path == args.path));
    let Some(idx) = trigger_index else {
        bail!(
            "no webhook trigger at {:?} on {ns}/{name} (declared: {:?})",
            args.path,
            agent
                .spec
                .triggers
                .iter()
                .filter_map(|t| t.webhook.as_ref().map(|w| w.path.clone()))
                .collect::<Vec<_>>()
        );
    };
    let exposed = agent
        .spec
        .expose
        .as_ref()
        .is_some_and(|e| e.webhooks.iter().any(|w| w.path == args.path));
    let route = format!("/hooks/{ns}/{name}{}", args.path);
    match &args.host {
        Some(h) => println!("URL:    https://{h}{route}"),
        None => println!("Route:  {route}  (front with your gateway's external host)"),
    }
    if !exposed {
        println!(
            "NOTE:   not yet exposed — add {{path: {:?}}} to spec.expose.webhooks (deliveries 404 until then)",
            args.path
        );
    }
    let auth = agent
        .spec
        .triggers
        .get(idx)
        .and_then(|t| t.webhook.as_ref())
        .and_then(|w| w.auth.clone())
        .unwrap_or_else(|| "none".into());
    println!("Auth:   {auth}");
    if args.show_secret && (auth == "hmac" || auth == "bearer") {
        use k8s_openapi::api::core::v1::Secret;
        let secrets: kube::Api<Secret> = kube::Api::namespaced(client, &ns);
        let sec = secrets
            .get_opt(&format!("{name}-hooks"))
            .await
            .context("read hooks Secret")?
            .with_context(|| {
                format!("{name}-hooks Secret not provisioned yet (operator reconciles it)")
            })?;
        let key = format!("{auth}-{idx}");
        let value = sec
            .data
            .as_ref()
            .and_then(|d| d.get(&key))
            .with_context(|| format!("hooks Secret has no {key} yet"))?;
        println!("Secret: {}", String::from_utf8_lossy(&value.0));
        if auth == "hmac" {
            println!("Sign:   X-Signature: sha256=HMAC_SHA256(secret, body) (hex)");
        }
    } else if args.show_secret {
        println!("Secret: (auth {auth:?} has none)");
    }
    Ok(())
}

// -- dlq (P7-5): the work-fabric dead-letter queue -------------------------

/// `agentctl dlq list|requeue|drop` — the coordination server's dead-letter
/// admin surface (`work.deadletter`), for the operator draining a fleet's
/// poison items. Speaks MCP `tools/call` to the coordination `/mcp`; the URL
/// comes from `--coordination-url`/`AGENTCTL_COORDINATION_URL` (in-cluster:
/// `http://agentctl-coordination.<ns>`; from a workstation, port-forward
/// `svc/agentctl-coordination 8080:80` then `http://127.0.0.1:8080`), and the
/// bearer from `--token`/`AGENTCTL_API_TOKEN` when the server gates on it.
#[derive(clap::Subcommand)]
pub enum DlqCommand {
    /// List the dead-lettered items (work_id, attempts, the item body).
    List(DlqListArgs),
    /// Requeue one dead-lettered item back onto the claim fabric.
    Requeue(DlqItemArgs),
    /// Drop one dead-lettered item permanently.
    Drop(DlqItemArgs),
}

#[derive(Args)]
pub struct DlqListArgs {
    #[command(flatten)]
    pub conn: DlqConn,
}

#[derive(Args)]
pub struct DlqItemArgs {
    /// The item's work_id (from `dlq list`).
    pub work_id: String,
    #[command(flatten)]
    pub conn: DlqConn,
}

#[derive(Args)]
pub struct DlqConn {
    /// Coordination server base URL (or AGENTCTL_COORDINATION_URL).
    #[arg(long)]
    pub coordination_url: Option<String>,
    /// Bearer token if the server gates on one (or AGENTCTL_API_TOKEN).
    #[arg(long)]
    pub token: Option<String>,
}

impl DlqConn {
    fn url(&self) -> Result<String> {
        let raw = match &self.coordination_url {
            Some(u) => u.clone(),
            None => std::env::var("AGENTCTL_COORDINATION_URL")
                .ok()
                .filter(|u| !u.trim().is_empty())
                .context(
                    "no coordination URL: pass --coordination-url or set \
                     AGENTCTL_COORDINATION_URL (workstation: `kubectl -n agentctl-system \
                     port-forward svc/agentctl-coordination 8080:80` then http://127.0.0.1:8080)",
                )?,
        };
        Ok(raw.trim_end_matches('/').to_string())
    }
    fn token(&self) -> Option<String> {
        self.token
            .clone()
            .or_else(|| std::env::var("AGENTCTL_API_TOKEN").ok())
            .filter(|t| !t.trim().is_empty())
    }
}

async fn dlq_call(conn: &DlqConn, arguments: serde_json::Value) -> Result<serde_json::Value> {
    use anyhow::bail;
    let url = conn.url()?;
    let http = reqwest::Client::builder()
        .build()
        .context("build http client")?;
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "work.deadletter", "arguments": arguments }
    });
    let mut req = http.post(format!("{url}/mcp")).json(&body);
    if let Some(t) = conn.token() {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.context("reach the coordination server")?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        bail!("coordination server refused ({status})");
    }
    if let Some(err) = v.get("error") {
        bail!("work.deadletter error: {err}");
    }
    // The dual result shape: structuredContent is the parsed body.
    Ok(v.pointer("/result/structuredContent")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

pub async fn run_dlq(cmd: DlqCommand) -> Result<()> {
    use anyhow::bail;
    match cmd {
        DlqCommand::List(args) => {
            let out = dlq_call(&args.conn, serde_json::json!({ "action": "list" })).await?;
            let items = out
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                println!("no dead-lettered items.");
                return Ok(());
            }
            println!("{:<40}  {:>8}  ITEM", "WORK_ID", "ATTEMPTS");
            for it in &items {
                println!(
                    "{:<40}  {:>8}  {}",
                    it["work_id"].as_str().unwrap_or("?"),
                    it["attempts"].as_i64().unwrap_or(0),
                    it["item"]
                );
            }
        }
        DlqCommand::Requeue(args) => {
            let out = dlq_call(
                &args.conn,
                serde_json::json!({ "action": "requeue", "work_id": args.work_id }),
            )
            .await?;
            if out["found"] == serde_json::Value::Bool(true) {
                println!("requeued {}.", args.work_id);
            } else {
                bail!("no dead-lettered item {}", args.work_id);
            }
        }
        DlqCommand::Drop(args) => {
            let out = dlq_call(
                &args.conn,
                serde_json::json!({ "action": "drop", "work_id": args.work_id }),
            )
            .await?;
            if out["found"] == serde_json::Value::Bool(true) {
                println!("dropped {}.", args.work_id);
            } else {
                bail!("no dead-lettered item {}", args.work_id);
            }
        }
    }
    Ok(())
}
