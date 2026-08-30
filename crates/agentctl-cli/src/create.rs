// SPDX-License-Identifier: Apache-2.0
//! `agentctl create agent` — spin up any agent shape agentd supports from
//! flags (RFC 0033 §2.2, P2-8): every trigger kind is a flag, combinable;
//! the server-side trigger compiler turns them into generated workflows and
//! the admission ladder holds them to the org's floors.
//!
//!   agentctl create agent reporter --instruction "write the weekly report" \
//!       --schedule "0 7 * * 1" --image agentd:1.3.1
//!   agentctl create agent hookbot --instruction "triage the payload" \
//!       --webhook /hooks/ci --loop 10m
//!   agentctl create agent oneshot --instruction "say hi" --once

use anyhow::{bail, Context, Result};
use clap::Args;
use kube::api::PostParams;
use kube::{Api, Client};

use agent_api::v1alpha2 as v2;

#[derive(Args)]
pub struct CreateAgentArgs {
    /// Agent name (the CR name; also the default @handle).
    pub name: String,
    #[arg(short = 'n', long)]
    pub namespace: Option<String>,
    /// The standing instruction/persona (required for trigger sugar).
    #[arg(long)]
    pub instruction: Option<String>,
    /// Runtime image (else the class/operator default).
    #[arg(long)]
    pub image: Option<String>,
    /// AgentClass to resolve defaults/floors through.
    #[arg(long)]
    pub class: Option<String>,
    /// Org-unique @handle (defaults to the name).
    #[arg(long)]
    pub handle: Option<String>,
    /// ModelPool binding.
    #[arg(long)]
    pub pool: Option<String>,

    /// One-shot run (Job).
    #[arg(long)]
    pub once: bool,
    /// Manual trigger (fired via `workflow.run` from chat/CLI).
    #[arg(long)]
    pub manual: bool,
    /// Loop cadence (`10m`).
    #[arg(long, value_name = "INTERVAL")]
    pub r#loop: Option<String>,
    /// Cron schedule (`0 7 * * 1-5`) — a SOLE schedule renders a CronJob.
    #[arg(long, value_name = "CRON")]
    pub schedule: Option<String>,
    /// Interval schedule (`1h`) — always an in-daemon schedule start.
    #[arg(long, value_name = "DUR")]
    pub every: Option<String>,
    /// Webhook trigger path (`/hooks/ci`).
    #[arg(long, value_name = "PATH")]
    pub webhook: Option<String>,
    /// MCP resource subscription as `<service>:<uri>` (the service must be a
    /// granted MCPService or inline server).
    #[arg(long, value_name = "SVC:URI")]
    pub subscribe: Option<String>,
    /// Stream trigger (stream name).
    #[arg(long, value_name = "STREAM")]
    pub stream: Option<String>,
    /// Signal trigger (signal name).
    #[arg(long, value_name = "NAME")]
    pub signal: Option<String>,
    /// Runtime-event trigger (`workflow.finished`, …).
    #[arg(long, value_name = "EVENT")]
    pub event: Option<String>,
    /// Typed A2A command trigger (command name).
    #[arg(long, value_name = "CMD")]
    pub command: Option<String>,
    /// MCPService grants (`--service zendesk`, repeatable).
    #[arg(long = "service", value_name = "NAME")]
    pub services: Vec<String>,
    /// Do NOT serve the A2A surface (daemons then need another wake source).
    #[arg(long)]
    pub no_a2a: bool,
}

/// Build the v1alpha2 spec from flags — pure, unit-tested.
pub fn build_spec(a: &CreateAgentArgs) -> Result<v2::AgentSpec> {
    let mut triggers = Vec::new();
    if a.once {
        triggers.push(v2::Trigger {
            once: Some(v2::OnceTrigger {}),
            ..Default::default()
        });
    }
    if a.manual {
        triggers.push(v2::Trigger {
            manual: Some(v2::ManualTrigger {}),
            ..Default::default()
        });
    }
    if let Some(interval) = &a.r#loop {
        triggers.push(v2::Trigger {
            loop_: Some(v2::LoopTrigger {
                interval: interval.clone(),
                until: None,
            }),
            ..Default::default()
        });
    }
    if let Some(cron) = &a.schedule {
        triggers.push(v2::Trigger {
            schedule: Some(v2::ScheduleTrigger {
                cron: Some(cron.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(every) = &a.every {
        triggers.push(v2::Trigger {
            schedule: Some(v2::ScheduleTrigger {
                every: Some(every.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(path) = &a.webhook {
        triggers.push(v2::Trigger {
            webhook: Some(v2::WebhookTrigger {
                path: path.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(sub) = &a.subscribe {
        let (svc, uri) = sub
            .split_once(':')
            .context("--subscribe wants <service>:<uri>")?;
        triggers.push(v2::Trigger {
            subscribe: Some(v2::SubscribeTrigger {
                service: svc.to_string(),
                uri: uri.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(stream) = &a.stream {
        triggers.push(v2::Trigger {
            stream: Some(v2::StreamTrigger {
                stream: stream.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(name) = &a.signal {
        triggers.push(v2::Trigger {
            signal: Some(v2::SignalTrigger {
                name: name.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(name) = &a.event {
        triggers.push(v2::Trigger {
            event: Some(v2::EventTrigger {
                name: name.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(cmd) = &a.command {
        triggers.push(v2::Trigger {
            a2a_command: Some(v2::A2aCommandTrigger {
                command: cmd.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    // The shape mirrors the compiler's inference so the user sees what will
    // render: only once/manual ⇒ job; a SOLE cron schedule ⇒ cron; else
    // daemon (which then needs a wake source: a trigger or the a2a surface).
    let only_short = !triggers.is_empty()
        && triggers
            .iter()
            .all(|t| t.once.is_some() || t.manual.is_some());
    let sole_cron = triggers.len() == 1 && a.schedule.is_some();
    let shape = if only_short {
        v2::Shape::Job
    } else if sole_cron {
        v2::Shape::Cron
    } else {
        v2::Shape::Daemon
    };
    if triggers.is_empty() && a.no_a2a {
        bail!("an agent needs a wake source: pass a trigger flag or drop --no-a2a");
    }
    if a.instruction.is_none() {
        bail!("--instruction is required (the persona the triggers run)");
    }

    Ok(v2::AgentSpec {
        class: a.class.clone(),
        handle: a.handle.clone(),
        shape,
        schedule: sole_cron.then(|| a.schedule.clone().unwrap()),
        instruction: a.instruction.as_ref().map(|text| v2::Instruction {
            text: Some(text.clone()),
            config_map_ref: None,
        }),
        triggers,
        runtime: a.image.as_ref().map(|image| v2::RuntimeSelector {
            version: None,
            image: Some(image.clone()),
        }),
        intelligence: a.pool.as_ref().map(|pool| v2::Intelligence {
            pool: Some(pool.clone()),
            ..Default::default()
        }),
        services: a
            .services
            .iter()
            .map(|name| v2::ServiceGrant {
                name: name.clone(),
                allow: Vec::new(),
            })
            .collect(),
        expose: Some(v2::Expose {
            a2a: !a.no_a2a,
            webhooks: Vec::new(),
        }),
        ..Default::default()
    })
}

pub async fn run_create_agent(args: CreateAgentArgs) -> Result<()> {
    let spec = build_spec(&args)?;
    let shape = spec.shape;
    let client = Client::try_default().await?;
    let ns = args
        .namespace
        .clone()
        .unwrap_or_else(|| client.default_namespace().to_string());
    let agents: Api<v2::Agent> = Api::namespaced(client, &ns);
    let agent = v2::Agent::new(&args.name, spec);
    agents
        .create(&PostParams::default(), &agent)
        .await
        .with_context(|| format!("create agent {}/{}", ns, args.name))?;
    println!("agent {}/{} created (shape: {:?})", ns, args.name, shape);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrap {
        #[command(flatten)]
        a: CreateAgentArgs,
    }
    fn parse(line: &str) -> CreateAgentArgs {
        Wrap::parse_from(line.split_whitespace()).a
    }

    #[test]
    fn sole_cron_is_a_cronjob_and_short_triggers_a_job() {
        let s = build_spec(&parse("x reporter --instruction p --schedule '0_7_*_*_1'")).unwrap();
        assert_eq!(s.shape, v2::Shape::Cron);
        assert!(s.schedule.is_some());

        let s = build_spec(&parse("x oneshot --instruction p --once")).unwrap();
        assert_eq!(s.shape, v2::Shape::Job);

        // Combining flips to daemon (internal schedule).
        let s = build_spec(&parse(
            "x multi --instruction p --schedule c --webhook /h --loop 10m",
        ))
        .unwrap();
        assert_eq!(s.shape, v2::Shape::Daemon);
        assert_eq!(s.triggers.len(), 3);
    }

    #[test]
    fn subscribe_parses_service_and_uri() {
        let s = build_spec(&parse(
            "x s --instruction p --subscribe queue:queue://inbox",
        ))
        .unwrap();
        let sub = s.triggers[0].subscribe.as_ref().unwrap();
        assert_eq!(sub.service, "queue");
        assert_eq!(sub.uri, "queue://inbox");
    }

    #[test]
    fn wake_source_and_instruction_are_required() {
        assert!(build_spec(&parse("x bare --instruction p --no-a2a")).is_err());
        assert!(build_spec(&parse("x noinstr --once")).is_err());
    }
}
