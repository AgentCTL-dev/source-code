//! The `node-agent` binary (RFC 0008, Tier A — control bridge).
//!
//! Runs one per node (a DaemonSet), mounts the stock-unix hostPath socket root,
//! and on a cadence **discovers** local agent management sockets and **bridges**
//! to each over the contract management profile — reading its capabilities and
//! operator tools. This is the on-node keystone the operator/CLI reach the data
//! plane through (the operator↔node-agent API + the A2A data path are layered on
//! next, RFC 0009/0013).

use std::path::PathBuf;
use std::time::Duration;

use agentctl_node_agent::{discover, DiscoveredAgent, ManagementClient};

fn main() {
    let root: PathBuf = std::env::var("AGENTCTL_SOCKET_ROOT")
        .unwrap_or_else(|_| "/run/agentctl/sockets".to_string())
        .into();
    let interval = Duration::from_secs(
        std::env::var("AGENTCTL_DISCOVERY_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
    );

    eprintln!(
        "node-agent: discovering management sockets under {} every {}s",
        root.display(),
        interval.as_secs()
    );

    loop {
        match discover(&root) {
            Ok(agents) => {
                eprintln!("node-agent: discovered {} agent socket(s)", agents.len());
                for agent in &agents {
                    match probe(agent) {
                        Ok(line) => eprintln!("  + {line}"),
                        Err(e) => eprintln!("  ! pod {} probe failed: {e}", agent.pod_uid),
                    }
                }
            }
            Err(e) => eprintln!("node-agent: discovery error: {e}"),
        }
        std::thread::sleep(interval);
    }
}

/// Connect to a discovered agent over the management profile and summarize what
/// it advertises (the bridge in action).
fn probe(agent: &DiscoveredAgent) -> Result<String, Box<dyn std::error::Error>> {
    let mut client = ManagementClient::connect(&agent.socket)?;
    client.initialize()?;
    let manifest = client.read_capabilities()?;
    let tools = client.list_tools()?;
    Ok(format!(
        "pod {} -> server={} contract={} mode={:?} mgmt={:?} operator_tools={:?}",
        agent.pod_uid,
        client.server_name.as_deref().unwrap_or("?"),
        manifest.contract_version,
        manifest.mode.as_deref().unwrap_or("?"),
        manifest.surfaces.management.addr().unwrap_or("(none)"),
        tools,
    ))
}
