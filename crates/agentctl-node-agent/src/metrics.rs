//! Merge + relabel per-agent Prometheus metrics into one exposition (RFC 0010).
//!
//! The node-agent reads each local agent's metrics over the socket, then this
//! module merges them into a single `/metrics` body: each metric's `# HELP`/
//! `# TYPE` is emitted once (deduped across agents), and every sample gets an
//! `agent_pod_uid="<uid>"` label so Prometheus can tell agents apart even though
//! the agent pods are not on the network.

use std::collections::BTreeMap;

/// Merge `(pod_uid, prometheus_text)` pairs into one exposition. Also emits a
/// node-agent self-gauge of how many agents were collected.
pub fn merge(agents: &[(String, String)]) -> String {
    // metric name -> (help line, type line); first occurrence wins.
    let mut meta: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    // metric name -> relabeled sample lines.
    let mut samples: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (uid, text) in agents {
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let name = rest.split_whitespace().next().unwrap_or_default();
                meta.entry(name.to_string())
                    .or_default()
                    .0
                    .get_or_insert(line.to_string());
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let name = rest.split_whitespace().next().unwrap_or_default();
                meta.entry(name.to_string())
                    .or_default()
                    .1
                    .get_or_insert(line.to_string());
            } else if line.starts_with('#') {
                continue;
            } else {
                let (name, relabeled) = relabel(line, uid);
                samples.entry(name).or_default().push(relabeled);
            }
        }
    }

    let mut out = String::new();
    out.push_str(
        "# HELP agentctl_node_agent_agents Agents this node-agent collected metrics from.\n",
    );
    out.push_str("# TYPE agentctl_node_agent_agents gauge\n");
    out.push_str(&format!("agentctl_node_agent_agents {}\n", agents.len()));

    for (name, samps) in &samples {
        if let Some((help, typ)) = meta.get(name) {
            if let Some(h) = help {
                out.push_str(h);
                out.push('\n');
            }
            if let Some(t) = typ {
                out.push_str(t);
                out.push('\n');
            }
        }
        for s in samps {
            out.push_str(s);
            out.push('\n');
        }
    }
    out
}

/// Inject `agent_pod_uid="<uid>"` into a sample line, returning `(metric_name,
/// relabeled_line)`. Handles `name value`, `name{} value`, and `name{l=…} value`.
fn relabel(line: &str, uid: &str) -> (String, String) {
    let label = format!("agent_pod_uid=\"{uid}\"");
    if let Some(brace) = line.find('{') {
        let name = line[..brace].to_string();
        let after = &line[brace + 1..]; // "existing} value" or "} value"
        let relabeled = if let Some(rest) = after.strip_prefix('}') {
            format!("{name}{{{label}}}{rest}") // empty label set
        } else {
            format!("{name}{{{label},{after}") // prepend to existing labels
        };
        (name, relabeled)
    } else {
        let mut parts = line.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default().to_string();
        let rest = parts.next().unwrap_or_default();
        let relabeled = format!("{name}{{{label}}} {rest}");
        (name, relabeled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relabel_handles_all_shapes() {
        assert_eq!(relabel("m 5", "u").1, r#"m{agent_pod_uid="u"} 5"#);
        assert_eq!(relabel("m{} 5", "u").1, r#"m{agent_pod_uid="u"} 5"#);
        assert_eq!(
            relabel(r#"m{a="b"} 5"#, "u").1,
            r#"m{agent_pod_uid="u",a="b"} 5"#
        );
    }

    #[test]
    fn merge_dedups_meta_and_labels_samples() {
        let a = "# HELP m help\n# TYPE m gauge\nm 1\n".to_string();
        let b = "# HELP m help\n# TYPE m gauge\nm 2\n".to_string();
        let out = merge(&[("pod-a".into(), a), ("pod-b".into(), b)]);

        // self gauge present, count = 2
        assert!(out.contains("agentctl_node_agent_agents 2"));
        // HELP/TYPE for m appear exactly once each (deduped)
        assert_eq!(out.matches("# TYPE m gauge").count(), 1);
        assert_eq!(out.matches("# HELP m help").count(), 1);
        // both samples present, each labelled by its pod
        assert!(out.contains(r#"m{agent_pod_uid="pod-a"} 1"#));
        assert!(out.contains(r#"m{agent_pod_uid="pod-b"} 2"#));
    }

    #[test]
    fn empty_is_just_the_self_gauge() {
        let out = merge(&[]);
        assert!(out.contains("agentctl_node_agent_agents 0"));
    }
}
