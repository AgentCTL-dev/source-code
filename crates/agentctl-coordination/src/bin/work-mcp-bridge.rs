// SPDX-License-Identifier: BUSL-1.1
//! `work-mcp-bridge` — a stdio<->HTTP MCP bridge (agentctl RFC 0011 §3.2; plan
//! Phase 4 Finding B).
//!
//! agentd's MCP client spawns an MCP server as a **child process** and speaks MCP
//! JSON-RPC 2.0 as **NDJSON over stdin/stdout** (one compact JSON object per line,
//! agentd RFC 0004 framing). The reference coordination server, however, serves the
//! frozen `work.*` contract over **HTTP JSON-RPC** (`POST /` and `POST /mcp`). No
//! stdio→HTTP shim ships otherwise, so the real agentd↔coordination claim loop
//! cannot run without this.
//!
//! This bridge is that shim: it reads NDJSON JSON-RPC requests from stdin, POSTs
//! each one verbatim to the coordination `/mcp` endpoint, and writes each HTTP JSON
//! response back to stdout as one NDJSON line. `initialize` / `notifications/*` /
//! `tools/list` / `tools/call` (the `work.*` tools) all pass through transparently —
//! the bridge is pure transport and never inspects method names.
//!
//! Config:
//!   * base URL — `--url <BASE>` arg, else `COORDINATION_URL` env, else
//!     `http://127.0.0.1:8080`. `/mcp` is appended unless the URL already ends in
//!     `/mcp`.
//!   * `AGENTCTL_API_TOKEN` (if set, non-empty) → `Authorization: Bearer <token>`
//!     on every POST, matching the server's optional bearer gate.

use std::process::ExitCode;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> ExitCode {
    let endpoint = match resolve_endpoint() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("work-mcp-bridge: {e}");
            return ExitCode::from(2);
        }
    };
    let bearer = std::env::var("AGENTCTL_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("work-mcp-bridge: build HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "work-mcp-bridge: forwarding NDJSON MCP JSON-RPC from stdin to {endpoint} \
         (bearer {})",
        if bearer.is_some() { "set" } else { "unset" }
    );

    // Sequential request/response: agentd's stdio transport pairs each response by
    // JSON-RPC `id`, but the simplest correct bridge forwards one line, writes one
    // line, and moves on. `BufReader::lines()` is robust to partial/last lines with
    // no trailing newline (it yields the final unterminated line on EOF).
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break, // clean EOF: agentd closed our stdin → shut down.
            Err(e) => {
                eprintln!("work-mcp-bridge: read stdin: {e}");
                return ExitCode::FAILURE;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue; // keep-alive / blank line: nothing to forward.
        }
        if let Some(resp_line) = forward(&client, &endpoint, bearer.as_deref(), trimmed).await {
            // One compact JSON object + '\n' — the NDJSON frame agentd's reader expects.
            if let Err(e) = out.write_all(resp_line.as_bytes()).await {
                eprintln!("work-mcp-bridge: write stdout: {e}");
                return ExitCode::FAILURE;
            }
            if out.write_all(b"\n").await.is_err() || out.flush().await.is_err() {
                eprintln!("work-mcp-bridge: flush stdout failed");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

/// POST one raw JSON-RPC line to the coordination endpoint and return the response
/// as a single compact NDJSON line, or `None` when there is nothing to write back
/// (a notification: the server answers `202 Accepted` with an empty body).
///
/// A transport failure does not kill the bridge: if the request carried an `id` we
/// synthesize a JSON-RPC error reply (so agentd's pending-by-id waiter resolves
/// instead of hanging); an id-less request (a notification) is dropped silently.
async fn forward(
    client: &reqwest::Client,
    endpoint: &str,
    bearer: Option<&str>,
    line: &str,
) -> Option<String> {
    let id = extract_id(line);
    let mut req = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(line.to_owned());
    if let Some(tok) = bearer {
        req = req.bearer_auth(tok);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return id.map(|id| rpc_error(id, -32000, &format!("bridge transport: {e}"))),
    };
    let status = resp.status();
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return id.map(|id| rpc_error(id, -32000, &format!("bridge read body: {e}"))),
    };
    // 202 / empty body ⇒ notification (no response frame). Otherwise compact the
    // body to guarantee a single-line NDJSON frame (no embedded newlines).
    if status == reqwest::StatusCode::ACCEPTED || body.trim().is_empty() {
        return None;
    }
    if !status.is_success() {
        return id.map(|id| rpc_error(id, -32000, &format!("coordination HTTP {status}")));
    }
    Some(compact(&body))
}

/// Resolve the coordination base URL and append `/mcp`. `--url <BASE>` wins, then
/// `COORDINATION_URL`, else a localhost default.
fn resolve_endpoint() -> Result<String, String> {
    let mut base: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--url" => base = Some(args.next().ok_or("--url requires a value")?),
            other => {
                if let Some(v) = other.strip_prefix("--url=") {
                    base = Some(v.to_owned());
                } else {
                    return Err(format!("unknown argument: {other}"));
                }
            }
        }
    }
    let base = base
        .or_else(|| std::env::var("COORDINATION_URL").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_owned());
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/mcp") {
        Ok(trimmed.to_owned())
    } else {
        Ok(format!("{trimmed}/mcp"))
    }
}

/// Extract the JSON-RPC `id` from a request line (for synthesizing error replies).
/// `None` for an id-less message (a notification) or unparseable input.
fn extract_id(line: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .filter(|id| !id.is_null())
}

/// Re-serialize a JSON body compactly so it occupies exactly one NDJSON line. If it
/// somehow is not valid JSON, fall back to stripping embedded newlines.
fn compact(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => v.to_string(),
        Err(_) => body.replace(['\n', '\r'], " "),
    }
}

/// A JSON-RPC 2.0 error envelope as a compact line, preserving the request id.
fn rpc_error(id: serde_json::Value, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_appends_mcp_and_respects_existing_suffix() {
        std::env::remove_var("COORDINATION_URL");
        // Default localhost.
        assert_eq!(resolve_endpoint().unwrap(), "http://127.0.0.1:8080/mcp");
    }

    #[test]
    fn endpoint_from_env_trims_and_appends() {
        std::env::set_var("COORDINATION_URL", "http://coord:8080/");
        assert_eq!(resolve_endpoint().unwrap(), "http://coord:8080/mcp");
        std::env::set_var("COORDINATION_URL", "http://coord:8080/mcp");
        assert_eq!(resolve_endpoint().unwrap(), "http://coord:8080/mcp");
        std::env::remove_var("COORDINATION_URL");
    }

    #[test]
    fn extract_id_handles_requests_and_notifications() {
        assert_eq!(
            extract_id(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#),
            Some(serde_json::json!(7))
        );
        // A notification (no id) and garbage both yield None.
        assert_eq!(extract_id(r#"{"jsonrpc":"2.0","method":"x"}"#), None);
        assert_eq!(extract_id("not json"), None);
    }

    #[test]
    fn compact_collapses_to_single_line() {
        let pretty = "{\n  \"a\": 1\n}";
        let c = compact(pretty);
        assert!(!c.contains('\n'));
        assert_eq!(c, r#"{"a":1}"#);
    }

    #[test]
    fn rpc_error_preserves_id_and_is_single_line() {
        let e = rpc_error(serde_json::json!(3), -32000, "boom");
        assert!(!e.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&e).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32000);
    }
}
