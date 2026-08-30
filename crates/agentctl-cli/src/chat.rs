// SPDX-License-Identifier: Apache-2.0
//! `agentctl chat <org>/<agent> [message…]` — converse with an agent through
//! the gateway's tenant-scoped route (`/orgs/<org>/agents/<name>`), as the
//! signed-in user (the saved login session's access token). The gateway
//! introspects the token, injects the caller's per-agent principal bearer,
//! and the agent answers as `user:<subject>` — attribution and per-user
//! quotas apply, never the operator identity.
//!
//! With no message arguments, reads lines from stdin (a minimal REPL; EOF or
//! an empty line ends the session).

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::{json, Value};

use crate::auth;

#[derive(Args)]
pub struct ChatArgs {
    /// Target as `<org>/<agent>`, `<org>/fleets/<fleet>`, or
    /// `<org>/supervisor` (your own supervisor; auto-provisioned on first
    /// contact when the org allows it).
    pub target: String,
    /// The message. Omitted, lines are read from stdin.
    pub message: Vec<String>,
    /// Gateway base URL (or AGENTCTL_GATEWAY_URL). From a workstation:
    /// `kubectl -n agentctl-system port-forward svc/agentctl-gateway 8080:80`
    /// then http://127.0.0.1:8080.
    #[arg(long)]
    pub gateway_url: Option<String>,
}

/// A parsed chat target.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub org: String,
    /// `agents`, `fleets`, or `supervisor` (the caller's own — no name).
    pub resource: &'static str,
    pub name: String,
}

impl Target {
    /// The gateway route path under the base URL.
    pub fn path(&self) -> String {
        if self.resource == "supervisor" {
            format!("/orgs/{}/supervisor", self.org)
        } else {
            format!("/orgs/{}/{}/{}", self.org, self.resource, self.name)
        }
    }
}

/// Parse `<org>/<agent>`, `<org>/fleets/<fleet>`, or `<org>/supervisor`.
/// The literal `supervisor` names the caller's OWN supervisor (RFC 0027) and
/// shadows any agent handle of the same name — pick a different handle.
pub fn parse_target(raw: &str) -> Result<Target> {
    let parts: Vec<&str> = raw.split('/').collect();
    match parts.as_slice() {
        [org, "supervisor"] if !org.is_empty() => Ok(Target {
            org: org.to_string(),
            resource: "supervisor",
            name: String::new(),
        }),
        [org, name] if !org.is_empty() && !name.is_empty() => Ok(Target {
            org: org.to_string(),
            resource: "agents",
            name: name.to_string(),
        }),
        [org, "fleets", name] if !org.is_empty() && !name.is_empty() => Ok(Target {
            org: org.to_string(),
            resource: "fleets",
            name: name.to_string(),
        }),
        _ => bail!(
            "target must be <org>/<agent>, <org>/fleets/<fleet>, or <org>/supervisor (got {raw:?})"
        ),
    }
}

#[derive(Args)]
pub struct ApproveArgs {
    /// The organization the request lives in.
    pub org: String,
    /// The approval code your supervisor relayed.
    pub nonce: String,
    /// Gateway base URL (or AGENTCTL_GATEWAY_URL).
    #[arg(long)]
    pub gateway_url: Option<String>,
}

/// `agentctl approve <org> <nonce>` — the owner's out-of-band YES to a
/// destructive request their supervisor asked about (P4-5). The gateway
/// verifies THIS session's bearer is the requesting owner; the supervisor
/// never holds it, so it cannot approve what it asked.
pub async fn run_approve(args: ApproveArgs) -> Result<()> {
    let url = format!(
        "{}/orgs/{}/approvals/{}",
        gateway_url(args.gateway_url.as_deref())?,
        args.org,
        args.nonce
    );
    let session = auth::load_session()?;
    let http = auth::api_client()?;
    let resp = http
        .post(&url)
        .bearer_auth(session.access_token)
        .send()
        .await
        .context("reach the gateway")?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    match status.as_u16() {
        200 => {
            println!(
                "approved — {} may now proceed (tell your supervisor to retry)",
                body["approved"].as_str().unwrap_or("the request")
            );
            Ok(())
        }
        401 => bail!("session refused (401) — run `agentctl login` again"),
        403 => bail!("this approval is addressed to the requesting owner, not you"),
        404 => bail!("no live pending approval with that code (it may have expired — ask again)"),
        _ => bail!("gateway refused ({status}): {body}"),
    }
}

/// Resolve the gateway URL: flag > AGENTCTL_GATEWAY_URL. No silent default.
fn gateway_url(flag: Option<&str>) -> Result<String> {
    let raw = match flag {
        Some(u) => u.to_string(),
        None => match std::env::var("AGENTCTL_GATEWAY_URL") {
            Ok(u) if !u.trim().is_empty() => u,
            _ => bail!(
                "no gateway URL: pass --gateway-url or set AGENTCTL_GATEWAY_URL \
                 (from a workstation: `kubectl -n agentctl-system port-forward \
                 svc/agentctl-gateway 8080:80` then http://127.0.0.1:8080)"
            ),
        },
    };
    Ok(raw.trim_end_matches('/').to_string())
}

/// Pull the reply text out of an A2A response: every text part found under
/// the result (message parts or task artifacts), joined; falls back to the
/// pretty JSON when no text part exists.
pub fn reply_text(resp: &Value) -> String {
    fn collect(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if let Some(t) = m.get("text").and_then(Value::as_str) {
                    out.push(t.to_string());
                }
                for (k, v) in m {
                    if k != "text" {
                        collect(v, out);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
            _ => {}
        }
    }
    let mut texts = Vec::new();
    if let Some(result) = resp.get("result") {
        collect(result, &mut texts);
    }
    if texts.is_empty() {
        serde_json::to_string_pretty(resp.get("result").unwrap_or(resp))
            .unwrap_or_else(|_| resp.to_string())
    } else {
        texts.join("\n")
    }
}

pub async fn run_chat(args: ChatArgs) -> Result<()> {
    let target = parse_target(&args.target)?;
    let url = format!(
        "{}{}",
        gateway_url(args.gateway_url.as_deref())?,
        target.path()
    );
    let session = auth::load_session()?;
    let http = auth::api_client()?;

    let send = |text: String, msg_id: usize| {
        let http = http.clone();
        let url = url.clone();
        let token = session.access_token.clone();
        async move {
            // A supervisor's first contact auto-provisions it; the gateway
            // answers 503 while the agent spins up. Poll through that window.
            let mut waited = false;
            let mut tries = 0u32;
            let (status, body) = loop {
                let resp = http
                    .post(&url)
                    .bearer_auth(token.clone())
                    .json(&json!({
                        "jsonrpc": "2.0",
                        "id": msg_id,
                        "method": "SendMessage",
                        "params": { "message": {
                            "role": "ROLE_USER",
                            "messageId": format!("cli-{msg_id}"),
                            "parts": [{ "text": text }],
                        } },
                    }))
                    .send()
                    .await
                    .context("reach the gateway")?;
                let status = resp.status();
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                if status.as_u16() == 503 && tries < 40 {
                    if !waited {
                        eprintln!("supervisor is provisioning — waiting…");
                        waited = true;
                    }
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
                break (status, body);
            };
            if status.as_u16() == 401 {
                bail!("session refused (401) — run `agentctl login` again");
            }
            if status.as_u16() == 403 {
                bail!(
                    "you are not a named principal on this agent (403): {}",
                    body["error"].as_str().unwrap_or("")
                );
            }
            if !status.is_success() {
                bail!("gateway refused ({status}): {body}");
            }
            if let Some(err) = body.get("error").filter(|e| !e.is_null()) {
                bail!("agent returned an error: {err}");
            }
            anyhow::Ok(reply_text(&body))
        }
    };

    if !args.message.is_empty() {
        println!("{}", send(args.message.join(" "), 1).await?);
        return Ok(());
    }

    // Minimal REPL: one line in, one reply out; EOF/empty line ends it.
    let stdin = std::io::stdin();
    let mut n = 0usize;
    loop {
        let mut line = String::new();
        if stdin.read_line(&mut line).context("read stdin")? == 0 {
            return Ok(());
        }
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        n += 1;
        println!("{}", send(line.to_string(), n).await?);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_org_agent_and_fleet_forms() {
        assert_eq!(
            parse_target("acme/triage").unwrap(),
            Target {
                org: "acme".into(),
                resource: "agents",
                name: "triage".into()
            }
        );
        assert_eq!(
            parse_target("acme/fleets/workers").unwrap().resource,
            "fleets"
        );
        assert!(parse_target("just-a-name").is_err());
        assert!(parse_target("a/b/c").is_err());
        assert!(parse_target("/x").is_err());
    }

    #[test]
    fn supervisor_target_routes_unnamed() {
        let t = parse_target("acme/supervisor").unwrap();
        assert_eq!(t.resource, "supervisor");
        assert_eq!(t.path(), "/orgs/acme/supervisor");
        // A named form still hits the ordinary agent route.
        assert_eq!(
            parse_target("acme/fleets/workers").unwrap().path(),
            "/orgs/acme/fleets/workers"
        );
    }

    #[test]
    fn reply_text_prefers_text_parts_and_falls_back_to_json() {
        let resp = json!({ "result": { "message": { "parts": [
            { "text": "hello" }, { "data": { "k": 1 } }, { "text": "world" }
        ] } } });
        assert_eq!(reply_text(&resp), "hello\nworld");

        let no_text = json!({ "result": { "task": { "id": "t1", "state": "completed" } } });
        assert!(reply_text(&no_text).contains("\"state\": \"completed\""));
    }
}
