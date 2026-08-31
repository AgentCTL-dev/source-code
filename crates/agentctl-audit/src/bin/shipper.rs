// SPDX-License-Identifier: Apache-2.0
//! The audit shipper (P7-3): a sidecar that tails a hash-chained mcpg audit
//! file (one JSON record per line, group-committed) and POSTs `audit/v1`
//! batches to identity's `/v1/audit/ingest` door, authenticated by an
//! operator-minted workload JWT (audience `agentctl:audit-ingest`, mounted
//! as a Secret and re-read per batch so rotation just works).
//!
//! Offset durability: the byte offset of the last SHIPPED line is persisted
//! beside the log (same writable mount), so a container restart resumes
//! instead of re-shipping; a truncated/rotated file (size < offset) resets.

use std::collections::BTreeMap;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| d.to_string())
}

/// Minimal RFC3339 UTC → unix seconds (`2026-08-31T01:40:51.416Z` shapes);
/// anything unparsable maps to "now" — a shipped record must never be lost
/// over a timestamp dialect.
fn rfc3339_to_unix(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days-from-civil (Howard Hinnant).
    let y_adj = y - i64::from(mo <= 2);
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

/// Map one mcpg `AuditEvent` line to our `audit/v1` record. Unknown shapes
/// ship as `mcpg.raw` rather than being dropped — an audit pipeline that
/// discards what it cannot parse is not an audit pipeline.
fn map_record(line: &str, workload: &str) -> agentctl_audit::Record {
    let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
    // beta.24 records already carry the `mcpg.` prefix; never double it.
    let action = v
        .get("action")
        .and_then(|a| a.as_str())
        .map(|a| {
            if a.starts_with("mcpg.") {
                a.to_string()
            } else {
                format!("mcpg.{a}")
            }
        })
        .unwrap_or_else(|| "mcpg.raw".to_string());
    let outcome = match v.get("outcome").and_then(|o| o.as_str()) {
        Some(o) if o.eq_ignore_ascii_case("success") || o.eq_ignore_ascii_case("ok") => {
            agentctl_audit::OUTCOME_OK
        }
        Some(_) => agentctl_audit::OUTCOME_REFUSED,
        None => agentctl_audit::OUTCOME_OK,
    };
    // org/namespace are FORCED by the ingest door from the token; empty here.
    let mut r = agentctl_audit::Record::new("mcpg", "", "", workload.to_string(), &action, outcome);
    if let Some(ts) = v
        .get("occurred_at")
        .and_then(|t| t.as_str())
        .and_then(rfc3339_to_unix)
    {
        r.ts = ts;
    }
    if let Some(sub) = v.pointer("/actor/subject_id").and_then(|s| s.as_str()) {
        r = r.user(sub.to_string());
    }
    if let Some(req) = v.get("request_id").and_then(|s| s.as_str()) {
        r = r.trail(req.to_string());
    }
    let mut dims = BTreeMap::new();
    for (k, ptr) in [
        ("resource", "/resource"),
        ("event_id", "/event_id"),
        ("prev_event_hash", "/prev_event_hash"),
        ("node_id", "/node_id"),
        ("trust", "/actor/trust_level"),
    ] {
        if let Some(val) = v.pointer(ptr).and_then(|s| s.as_str()) {
            dims.insert(k.to_string(), val.to_string());
        }
    }
    if action == "mcpg.raw" {
        dims.insert("raw".to_string(), line.chars().take(500).collect());
    }
    r.dims = dims;
    r
}

#[tokio::main]
async fn main() {
    // Workspace feature-unification arms reqwest's rustls backend; without
    // an installed provider Client::new() panics ("No provider set").
    let _ = rustls::crypto::ring::default_provider().install_default();
    let file = env_or("AUDIT_FILE", "/var/log/mcpg/audit.log");
    let offset_file = env_or("AUDIT_OFFSET_FILE", &format!("{file}.shipped"));
    let ingest = env_or(
        "AUDIT_INGEST_URL",
        "http://agentctl-identity.agentctl-system/v1/audit/ingest",
    );
    let token_file = env_or("AUDIT_TOKEN_FILE", "/etc/agentctl/audit/token");
    let workload = env_or("AUDIT_WORKLOAD", "agentctl-mcpg");
    let interval = env_or("AUDIT_INTERVAL_SECS", "2").parse().unwrap_or(2u64);
    let http = reqwest::Client::new();
    eprintln!("audit-shipper: tailing {file} -> {ingest}");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let Ok(content) = tokio::fs::read(&file).await else {
            continue; // not written yet
        };
        let mut offset: usize = tokio::fs::read_to_string(&offset_file)
            .await
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if offset > content.len() {
            offset = 0; // truncation/rotation
        }
        // Only COMPLETE lines ship (the tail may be a partial group commit).
        let new = &content[offset..];
        let Some(last_nl) = new.iter().rposition(|b| *b == b'\n') else {
            continue;
        };
        let chunk = &new[..=last_nl];
        let records: Vec<agentctl_audit::Record> = chunk
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .filter_map(|l| std::str::from_utf8(l).ok())
            .map(|l| map_record(l, &workload))
            .collect();
        if records.is_empty() {
            // Whitespace-only chunk: advance past it.
            let _ = tokio::fs::write(&offset_file, (offset + last_nl + 1).to_string()).await;
            continue;
        }
        let token = tokio::fs::read_to_string(&token_file)
            .await
            .unwrap_or_default();
        let resp = http
            .post(&ingest)
            .bearer_auth(token.trim())
            .json(&records)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let _ = tokio::fs::write(&offset_file, (offset + last_nl + 1).to_string()).await;
            }
            Ok(r) => eprintln!("audit-shipper: ingest refused {} (will retry)", r.status()),
            Err(e) => eprintln!("audit-shipper: ingest unreachable: {e} (will retry)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_tool_call_line() {
        let line = r#"{"event_id":"ev-1","occurred_at":"2026-08-31T01:40:51.416Z","actor":{"subject_id":"org-a/sup-erin","trust_level":"Verified"},"action":"tool.call","resource":"tool://zendesk.auth.echo","outcome":"success","request_id":"tr-abc","node_id":"n1","prev_event_hash":"h0"}"#;
        let r = map_record(line, "agentctl-mcpg");
        assert_eq!(r.component, "mcpg");
        assert_eq!(r.action, "mcpg.tool.call");
        assert_eq!(r.outcome, agentctl_audit::OUTCOME_OK);
        assert_eq!(r.user.as_deref(), Some("org-a/sup-erin"));
        assert_eq!(r.trail_id.as_deref(), Some("tr-abc"));
        assert_eq!(r.dims["resource"], "tool://zendesk.auth.echo");
        assert_eq!(r.ts, rfc3339_to_unix("2026-08-31T01:40:51Z").unwrap());
    }

    #[test]
    fn rfc3339_parses_and_garbage_ships_as_raw() {
        assert_eq!(rfc3339_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_to_unix("2026-08-31T00:00:00.5Z").unwrap() % 86_400,
            0
        );
        let r = map_record("not json at all", "w");
        assert_eq!(r.action, "mcpg.raw");
        assert!(r.dims["raw"].contains("not json"));
    }
}
