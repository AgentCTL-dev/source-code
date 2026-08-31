// SPDX-License-Identifier: BUSL-1.1
//! # agentctl sandbox cell (P5-5, RFC 0031 `sandbox.*`)
//!
//! The `sandbox.run` MCP backend: code executes in SINGLE-USE, capability-
//! stripped pods inside a dedicated CELL namespace whose NetworkPolicy denies
//! all traffic. A warm pool per language hides pod-start latency; every run
//! leases a warm pod, execs the work, and DELETES the pod (no cross-run
//! contamination), with a replacement spawned behind it.
//!
//! Containment layers (the threat model doc walks them):
//! - cell namespace + deny-all NetworkPolicy (egress AND ingress);
//! - `automountServiceAccountToken: false` — no cluster credentials exist
//!   inside the pod even if the code escapes the runtime;
//! - non-root, all capabilities dropped, no privilege escalation, read-only
//!   rootfs with one writable `/work` emptyDir;
//! - CPU/memory limits + wall-clock timeout (killed = pod deleted);
//! - output caps (stdout and each artifact truncated server-side);
//! - optional `runtimeClassName` (Kata et al.) for kernel isolation.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, AttachParams, DeleteParams, ListParams, PostParams};
use serde_json::{json, Value};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Server-side output caps: stdout and each returned artifact.
const STDOUT_CAP: usize = 64 * 1024;
const FILE_CAP: usize = 256 * 1024;
const MAX_OUT_FILES: usize = 8;
/// Wall-clock ceiling a caller may request.
const MAX_TIMEOUT_SECS: u64 = 300;

#[derive(Clone)]
struct AppState {
    client: kube::Client,
    cell_ns: String,
    images: Arc<BTreeMap<String, String>>,
    runtime_class: Option<String>,
    warm_per_language: usize,
    auth_token: Option<Arc<String>>,
    /// Pods currently leased to a run (never handed out twice).
    leased: Arc<Mutex<HashSet<String>>>,
    metrics: Arc<Metrics>,
}

#[derive(Default)]
struct Metrics {
    runs: AtomicU64,
    failures: AtomicU64,
    timeouts: AtomicU64,
}

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| d.to_string())
}

#[tokio::main]
async fn main() {
    agentctl_telemetry::init("agentctl-sandbox");
    let _ = rustls::crypto::ring::default_provider().install_default();

    let client = kube::Client::try_default()
        .await
        .expect("in-cluster kube client (the cell runner needs pod exec)");
    let mut images = BTreeMap::new();
    images.insert(
        "python".to_string(),
        env_or("SANDBOX_IMAGE_PYTHON", "python:3.12-alpine"),
    );
    images.insert("sh".to_string(), env_or("SANDBOX_IMAGE_SH", "busybox:1.36"));
    let state = AppState {
        client,
        cell_ns: env_or("SANDBOX_NAMESPACE", "agentctl-sandbox-cell"),
        images: Arc::new(images),
        runtime_class: std::env::var("SANDBOX_RUNTIME_CLASS")
            .ok()
            .filter(|v| !v.is_empty()),
        warm_per_language: env_or("SANDBOX_WARM_POOL", "1").parse().unwrap_or(1),
        auth_token: std::env::var("AGENTCTL_API_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(Arc::new),
        leased: Arc::new(Mutex::new(HashSet::new())),
        metrics: Arc::new(Metrics::default()),
    };

    // The pool keeper: converge each language to `warm_per_language` ready
    // pods (leaks heal on the next sweep; a dead cell degrades to cold
    // starts, never to refusals).
    {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = converge_pool(&st).await {
                    tracing::warn!(error = %e, "pool convergence failed (cold starts until it heals)");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    let app = Router::new()
        .route("/", post(rpc))
        .route("/mcp", post(rpc))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route("/metrics", get(serve_metrics))
        .with_state(state);
    let addr: std::net::SocketAddr = "0.0.0.0:8080".parse().unwrap();
    tracing::info!(%addr, "agentctl sandbox cell serving sandbox.*");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("serve");
}

async fn serve_metrics(State(st): State<AppState>) -> impl IntoResponse {
    let m = &st.metrics;
    let leased = st.leased.lock().unwrap().len();
    (
        [("content-type", "text/plain; version=0.0.4")],
        format!(
            "# TYPE agentctl_sandbox_runs_total counter\nagentctl_sandbox_runs_total {}\n\
             # TYPE agentctl_sandbox_failures_total counter\nagentctl_sandbox_failures_total {}\n\
             # TYPE agentctl_sandbox_timeouts_total counter\nagentctl_sandbox_timeouts_total {}\n\
             # TYPE agentctl_sandbox_leased pods\nagentctl_sandbox_leased {leased}\n",
            m.runs.load(Ordering::Relaxed),
            m.failures.load(Ordering::Relaxed),
            m.timeouts.load(Ordering::Relaxed),
        ),
    )
}

/// The MCP endpoint. The coarse bearer gate mirrors coordination's: with
/// `AGENTCTL_API_TOKEN` set, callers present it (tenant gateways add their
/// own verified-caller governance in front).
async fn rpc(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<Value>,
) -> axum::response::Response {
    if let Some(expected) = &st.auth_token {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        if presented != expected.as_str() {
            return (StatusCode::UNAUTHORIZED, "bearer required").into_response();
        }
    }
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    if method.starts_with("notifications/") {
        return StatusCode::ACCEPTED.into_response();
    }
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let resp = match method {
        "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "agentctl-sandbox", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "sandbox.run executes code in a single-use, network-denied, capability-stripped pod. Outputs are capped; declare out_files to read artifacts back.",
        } }),
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [{
            "name": "sandbox.run",
            "description": "Run code in the sandbox cell: single-use pod, no network, no cluster credentials, CPU/memory/time caps. Returns exit_code, capped stdout, and the declared out_files.",
            "inputSchema": { "type": "object", "required": ["language", "code"], "properties": {
                "language": { "type": "string", "enum": ["python", "sh"] },
                "code": { "type": "string" },
                "stdin": { "type": "string" },
                "files": { "type": "object", "additionalProperties": { "type": "string" } },
                "out_files": { "type": "array", "items": { "type": "string" } },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300 }
            } }
        }] } }),
        "tools/call" => {
            let name = params
                .pointer("/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            if name != "sandbox.run" {
                json!({ "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32602, "message": format!("no tool {name:?}") } })
            } else {
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                let (body, is_error) = match run_tool(&st, &args).await {
                    Ok(v) => (v, false),
                    Err(e) => {
                        st.metrics.failures.fetch_add(1, Ordering::Relaxed);
                        (json!({ "error": e }), true)
                    }
                };
                let text = serde_json::to_string(&body).unwrap_or_default();
                json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": body,
                    "isError": is_error,
                } })
            }
        }
        other => json!({ "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("method not found: {other}") } }),
    };
    Json(resp).into_response()
}

/// The desired warm pod for a language. Everything the threat model promises
/// lives HERE — the runner never weakens it per call.
fn warm_pod(st: &AppState, language: &str, image: &str) -> Pod {
    let name = format!(
        "cell-{language}-{:08x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
            .unwrap_or(0)
    );
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": st.cell_ns,
            "labels": {
                "app.kubernetes.io/name": "agentctl-sandbox-cell",
                "agentctl.dev/sandbox-language": language,
            },
        },
        "spec": {
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 0,
            "runtimeClassName": st.runtime_class,
            "containers": [{
                "name": "cell",
                "image": image,
                "imagePullPolicy": "IfNotPresent",
                "command": ["sh", "-c", "while true; do sleep 3600; done"],
                "workingDir": "/work",
                "resources": {
                    "requests": { "cpu": "50m", "memory": "64Mi" },
                    "limits": { "cpu": "500m", "memory": "256Mi" },
                },
                "securityContext": {
                    "runAsNonRoot": true,
                    "runAsUser": 65534,
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": { "drop": ["ALL"] },
                    "seccompProfile": { "type": "RuntimeDefault" },
                },
                "volumeMounts": [
                    { "name": "work", "mountPath": "/work" },
                    { "name": "tmp", "mountPath": "/tmp" },
                ],
            }],
            "volumes": [
                { "name": "work", "emptyDir": { "sizeLimit": "64Mi" } },
                { "name": "tmp", "emptyDir": { "sizeLimit": "16Mi" } },
            ],
        },
    }))
    .expect("static pod tree")
}

/// Converge the pool: each language holds `warm_per_language` READY,
/// unleased pods; Succeeded/Failed leftovers are reaped.
async fn converge_pool(st: &AppState) -> Result<(), String> {
    let pods: Api<Pod> = Api::namespaced(st.client.clone(), &st.cell_ns);
    let list = pods
        .list(&ListParams::default().labels("app.kubernetes.io/name=agentctl-sandbox-cell"))
        .await
        .map_err(|e| e.to_string())?;
    for (language, image) in st.images.iter() {
        let mut ready = 0usize;
        for p in &list.items {
            let lang = p
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("agentctl.dev/sandbox-language"));
            if lang.map(String::as_str) != Some(language) {
                continue;
            }
            let name = p.metadata.name.clone().unwrap_or_default();
            let phase = p
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("");
            match phase {
                "Running" | "Pending" => {
                    if !st.leased.lock().unwrap().contains(&name) {
                        ready += 1;
                    }
                }
                // A finished cell is a used cell — reap it.
                _ => {
                    let _ = pods.delete(&name, &DeleteParams::default()).await;
                }
            }
        }
        for _ in ready..st.warm_per_language {
            let pod = warm_pod(st, language, image);
            if let Err(e) = pods.create(&PostParams::default(), &pod).await {
                return Err(format!("spawn warm {language}: {e}"));
            }
        }
    }
    Ok(())
}

/// Lease a Running warm pod for `language`, waiting briefly for a cold start
/// when the pool is empty.
async fn lease_pod(st: &AppState, language: &str) -> Result<String, String> {
    let pods: Api<Pod> = Api::namespaced(st.client.clone(), &st.cell_ns);
    for _ in 0..60 {
        let list = pods
            .list(&ListParams::default().labels(&format!(
                "app.kubernetes.io/name=agentctl-sandbox-cell,agentctl.dev/sandbox-language={language}"
            )))
            .await
            .map_err(|e| e.to_string())?;
        for p in &list.items {
            let name = p.metadata.name.clone().unwrap_or_default();
            let running = p
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .is_some_and(|ph| ph == "Running");
            if running && st.leased.lock().unwrap().insert(name.clone()) {
                return Ok(name);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("no {language} cell became ready in 30s"))
}

/// One run: lease → exec the wrapper script → parse the delimited output →
/// DELETE the pod (single-use; the keeper replenishes).
async fn run_tool(st: &AppState, args: &Value) -> Result<Value, String> {
    let language = args
        .get("language")
        .and_then(Value::as_str)
        .ok_or("language is required (python | sh)")?;
    if !st.images.contains_key(language) {
        return Err(format!("unknown language {language:?} (python | sh)"));
    }
    let code = args
        .get("code")
        .and_then(Value::as_str)
        .ok_or("code is required")?;
    let stdin = args.get("stdin").and_then(Value::as_str).unwrap_or("");
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(1, MAX_TIMEOUT_SECS);
    let files: Vec<(String, String)> = args
        .get("files")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let out_files: Vec<String> = args
        .get("out_files")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .take(MAX_OUT_FILES)
                .collect()
        })
        .unwrap_or_default();
    for name in files.iter().map(|(n, _)| n).chain(out_files.iter()) {
        if name.contains("..") || name.contains('/') || name.is_empty() {
            return Err(format!("file name {name:?} must be a bare name"));
        }
    }

    // The wrapper: materialize inputs, run under the interpreter with stdin,
    // then emit each artifact as a delimited base64 block on stdout. Code
    // and file contents travel base64 — nothing is shell-interpolated.
    let mut script = String::from("set -u; cd /work\n");
    for (name, content) in &files {
        script.push_str(&format!(
            "printf %s '{}' | base64 -d > '{name}'\n",
            B64.encode(content)
        ));
    }
    script.push_str(&format!(
        "printf %s '{}' | base64 -d > __code\n",
        B64.encode(code)
    ));
    script.push_str(&format!(
        "printf %s '{}' | base64 -d > __stdin\n",
        B64.encode(stdin)
    ));
    let runner = match language {
        "python" => "python3 __code < __stdin",
        _ => "sh __code < __stdin",
    };
    script.push_str(&format!("{runner}\nrc=$?\n"));
    for name in &out_files {
        script.push_str(&format!(
            "echo '__AGENTCTL_FILE__{name}__'\nif [ -f '{name}' ]; then base64 '{name}'; fi\necho '__AGENTCTL_END__'\n"
        ));
    }
    script.push_str("echo \"__AGENTCTL_RC__${rc}__\"\n");

    let pod = lease_pod(st, language).await?;
    let pods: Api<Pod> = Api::namespaced(st.client.clone(), &st.cell_ns);
    let started = std::time::Instant::now();
    let exec = async {
        let mut proc = pods
            .exec(
                &pod,
                vec!["sh", "-c", &script],
                &AttachParams::default().stdout(true).stderr(true),
            )
            .await
            .map_err(|e| format!("exec: {e}"))?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        use tokio::io::AsyncReadExt as _;
        if let Some(mut s) = proc.stdout() {
            let _ = s.read_to_string(&mut stdout).await;
        }
        if let Some(mut s) = proc.stderr() {
            let _ = s.read_to_string(&mut stderr).await;
        }
        let _ = proc.join().await;
        Ok::<(String, String), String>((stdout, stderr))
    };
    let outcome = tokio::time::timeout(Duration::from_secs(timeout_secs), exec).await;

    // SINGLE-USE: the pod dies whatever happened (a timed-out run is killed
    // by this delete; the keeper replenishes the pool).
    let _ = pods.delete(&pod, &DeleteParams::default()).await;
    st.leased.lock().unwrap().remove(&pod);
    st.metrics.runs.fetch_add(1, Ordering::Relaxed);

    let (raw_out, stderr) = match outcome {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            st.metrics.timeouts.fetch_add(1, Ordering::Relaxed);
            return Ok(json!({
                "killed": true,
                "timeout_secs": timeout_secs,
                "duration_ms": started.elapsed().as_millis() as u64,
            }));
        }
    };

    let (stdout, artifacts, exit_code) = parse_wrapped_output(&raw_out, &out_files);
    let mut files_out = serde_json::Map::new();
    for (name, content) in artifacts {
        files_out.insert(
            name,
            json!(content.chars().take(FILE_CAP).collect::<String>()),
        );
    }
    Ok(json!({
        "exit_code": exit_code,
        "stdout": stdout.chars().take(STDOUT_CAP).collect::<String>(),
        "stderr": stderr.chars().take(STDOUT_CAP).collect::<String>(),
        "files": files_out,
        "killed": false,
        "duration_ms": started.elapsed().as_millis() as u64,
    }))
}

/// Split the wrapper's stdout into (user stdout, artifacts, exit code).
fn parse_wrapped_output(raw: &str, out_files: &[String]) -> (String, Vec<(String, String)>, i64) {
    let mut user = String::new();
    let mut artifacts = Vec::new();
    let mut exit_code = -1i64;
    let mut lines = raw.lines().peekable();
    'outer: while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("__AGENTCTL_RC__") {
            if let Some(code) = rest.strip_suffix("__").and_then(|c| c.parse().ok()) {
                exit_code = code;
            }
            continue;
        }
        for name in out_files {
            if line == format!("__AGENTCTL_FILE__{name}__") {
                let mut b64 = String::new();
                for l in lines.by_ref() {
                    if l == "__AGENTCTL_END__" {
                        break;
                    }
                    b64.push_str(l.trim());
                }
                if !b64.is_empty() {
                    if let Ok(bytes) = B64.decode(&b64) {
                        artifacts
                            .push((name.clone(), String::from_utf8_lossy(&bytes).into_owned()));
                    }
                }
                continue 'outer;
            }
        }
        user.push_str(line);
        user.push('\n');
    }
    (user, artifacts, exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_output_parses_stdout_artifacts_and_rc() {
        let out_files = vec!["result.txt".to_string()];
        let raw = format!(
            "hello from code\n__AGENTCTL_FILE__result.txt__\n{}\n__AGENTCTL_END__\n__AGENTCTL_RC__0__\n",
            B64.encode("artifact body")
        );
        let (stdout, files, rc) = parse_wrapped_output(&raw, &out_files);
        assert_eq!(stdout, "hello from code\n");
        assert_eq!(
            files,
            vec![("result.txt".to_string(), "artifact body".to_string())]
        );
        assert_eq!(rc, 0);
    }

    #[test]
    fn missing_artifact_and_nonzero_rc() {
        let out_files = vec!["gone.txt".to_string()];
        let raw = "__AGENTCTL_FILE__gone.txt__\n__AGENTCTL_END__\n__AGENTCTL_RC__7__\n";
        let (stdout, files, rc) = parse_wrapped_output(raw, &out_files);
        assert!(stdout.is_empty());
        assert!(files.is_empty());
        assert_eq!(rc, 7);
    }
}
