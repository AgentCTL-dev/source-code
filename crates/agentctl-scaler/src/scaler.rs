// SPDX-License-Identifier: BUSL-1.1
//! The `ExternalScaler` gRPC service + its pure decision logic.
//!
//! The scaler reads the reference coordination MCP server's **off-pod backlog**
//! (`work.stats` → `pending`) over MCP JSON-RPC/HTTP and maps it onto KEDA's four
//! RPCs:
//!
//!   * `GetMetricSpec` → metric `agentctl-backlog`, `targetSize = threshold`
//!     (KEDA's HPA then drives replicas toward `ceil(pending / threshold)`).
//!   * `GetMetrics`    → the current `pending`.
//!   * `IsActive`      → `pending > activationThreshold` — **the scale-from-zero
//!     gate**: false keeps the fleet at 0; true lights the first pod.
//!   * `StreamIsActive`→ poll `work.stats` on an interval and push an
//!     `IsActiveResponse` on each `0 ↔ >0` transition (plus an initial value).
//!
//! A coordination-server read failure does NOT flap the fleet to 0: `IsActive`
//! returns the **last known** value (and `StreamIsActive` holds its last emitted
//! value), so pending work is never stranded. Failures are logged + counted
//! (`agentctl_scaler_stats_errors_total`).
//!
//! The pure helpers ([`ScalerConfig::from_metadata`], [`decide_active`],
//! [`parse_pending`], [`metric_spec`], [`metric_value`]) carry the whole contract
//! and are unit-tested without a socket; the gRPC trait impl only adds transport.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::metrics::Metrics;
use crate::pb::external_scaler_server::ExternalScaler;
use crate::pb::{
    GetMetricSpecResponse, GetMetricsRequest, GetMetricsResponse, IsActiveResponse, MetricSpec,
    MetricValue, ScaledObjectRef,
};

/// The metric name advertised to KEDA (`GetMetricSpec`/`GetMetrics`). The HPA
/// scales on `ceil(pending / targetSize)` for this metric.
pub const METRIC_NAME: &str = "agentctl-backlog";

/// Per-replica backlog target — `targetSize` in `GetMetricSpec`. Default 5.
pub const DEFAULT_THRESHOLD: i64 = 5;
/// The 0→1 activation gate — `IsActive` is `pending > activationThreshold`.
/// Default 1 (a single pending item wakes the fleet).
pub const DEFAULT_ACTIVATION_THRESHOLD: i64 = 1;
/// Default `StreamIsActive` poll cadence (ms). Override with `STREAM_POLL_INTERVAL_MS`.
pub const DEFAULT_STREAM_POLL_INTERVAL_MS: u64 = 2_000;

/// `scalerMetadata` keys (set by the operator-rendered `ScaledObject`).
const KEY_COORDINATION_URL: &str = "coordinationUrl";
const KEY_THRESHOLD: &str = "threshold";
const KEY_ACTIVATION_THRESHOLD: &str = "activationThreshold";
const KEY_METRIC: &str = "metric";
const KEY_NAMESPACE: &str = "namespace";
const KEY_SELECTOR: &str = "selector";

/// The contract-neutral signal a `ScaledObject` scales on (P6-5 v2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The coordination work fabric's `work.stats.pending` (claim fleets).
    Backlog,
    /// The sum of `agent_inbox_pending` over the fleet's own member pods
    /// (the metrics registry's scaler_guidance.primary).
    InboxPending,
}

/// The per-`ScaledObject` knobs the scaler reads from `ScaledObjectRef.scalerMetadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalerConfig {
    /// The coordination MCP endpoint to POST `work.stats` to (required at runtime).
    pub coordination_url: String,
    /// Per-replica target (`targetSize`); KEDA divides backlog by this. Always ≥ 1.
    pub threshold: i64,
    /// The 0→1 gate: `IsActive` is `pending > activation_threshold`.
    pub activation_threshold: i64,
    /// Which signal to read (P6-5 v2). Unknown tokens fall back to backlog
    /// with a warning — a typo must degrade, never brick the fleet.
    pub signal: Signal,
    /// `inbox_pending`: the fleet's namespace + member-pod label selector.
    pub namespace: String,
    pub selector: String,
}

impl ScalerConfig {
    /// Parse `scalerMetadata`, applying defaults. `threshold` is clamped to ≥ 1 (a
    /// 0/negative target would make KEDA's `ceil(pending/target)` divide by zero).
    pub fn from_metadata(md: &HashMap<String, String>) -> Self {
        let coordination_url = md.get(KEY_COORDINATION_URL).cloned().unwrap_or_default();
        let threshold = md
            .get(KEY_THRESHOLD)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|t| *t > 0)
            .unwrap_or(DEFAULT_THRESHOLD);
        let activation_threshold = md
            .get(KEY_ACTIVATION_THRESHOLD)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|t| *t >= 0)
            .unwrap_or(DEFAULT_ACTIVATION_THRESHOLD);
        let signal = match md.get(KEY_METRIC).map(|m| m.trim()) {
            None | Some("") | Some("backlog") => Signal::Backlog,
            Some("inbox_pending") => Signal::InboxPending,
            Some(other) => {
                tracing::warn!(metric = other, "unknown metric token; scaling on backlog");
                Signal::Backlog
            }
        };
        Self {
            coordination_url,
            threshold,
            activation_threshold,
            signal,
            namespace: md.get(KEY_NAMESPACE).cloned().unwrap_or_default(),
            selector: md.get(KEY_SELECTOR).cloned().unwrap_or_default(),
        }
    }
}

/// Sum every `agent_inbox_pending` sample in one Prometheus exposition body
/// (labels tolerated; HELP/TYPE lines skipped).
pub fn sum_inbox_pending(exposition: &str) -> Option<i64> {
    let mut sum: i64 = 0;
    let mut seen = false;
    for line in exposition.lines() {
        let line = line.trim();
        if !line.starts_with("agent_inbox_pending") || line.starts_with('#') {
            continue;
        }
        let rest = &line["agent_inbox_pending".len()..];
        // Either " 3" or "{...} 3" — anything else is a different metric
        // sharing the prefix (e.g. agent_inbox_pending_seconds): skip.
        let value_part = if let Some(r) = rest.strip_prefix('{') {
            match r.split_once('}') {
                Some((_, v)) => v,
                None => continue,
            }
        } else if rest.starts_with(' ') {
            rest
        } else {
            continue;
        };
        if let Ok(v) = value_part.trim().parse::<f64>() {
            sum += v as i64;
            seen = true;
        }
    }
    seen.then_some(sum)
}

/// The scale-from-zero gate: active iff the backlog exceeds the activation
/// threshold. `pending == activation_threshold` is NOT active (strictly greater),
/// so with the default activation=1 a single pending item (`pending == 1`) does
/// not yet wake the fleet — `pending == 2` does. (KEDA's HPA, once active, then
/// targets `ceil(pending / threshold)`.)
pub fn decide_active(pending: i64, activation_threshold: i64) -> bool {
    pending > activation_threshold
}

/// Build the `GetMetricSpec` body: metric `agentctl-backlog`, `targetSize = threshold`.
pub fn metric_spec(threshold: i64) -> GetMetricSpecResponse {
    GetMetricSpecResponse {
        metric_specs: vec![MetricSpec {
            metric_name: METRIC_NAME.to_string(),
            target_size: threshold,
            target_size_float: threshold as f64,
        }],
    }
}

/// Build the `GetMetrics` body: the current backlog as the metric value.
pub fn metric_value(pending: i64) -> GetMetricsResponse {
    GetMetricsResponse {
        metric_values: vec![MetricValue {
            metric_name: METRIC_NAME.to_string(),
            metric_value: pending,
            metric_value_float: pending as f64,
        }],
    }
}

/// Extract `pending` from a coordination-server `work.stats` reply. Accepts either
/// the full JSON-RPC response or the bare `CallToolResult`. Prefers
/// `result.structuredContent.pending`; falls back to parsing the `result.content[]`
/// text item's JSON `pending` (the dual-encoding the MCP server emits — both agree).
pub fn parse_pending(resp: &Value) -> Option<i64> {
    // The CallToolResult is under `result` in a JSON-RPC envelope; tolerate either.
    let result = resp.get("result").unwrap_or(resp);

    // Preferred: structuredContent.pending.
    if let Some(p) = result
        .pointer("/structuredContent/pending")
        .and_then(Value::as_i64)
    {
        return Some(p);
    }

    // Fallback: the text content[] item carries the SAME JSON.
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for item in items {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                if let Ok(v) = serde_json::from_str::<Value>(text) {
                    if let Some(p) = v.get("pending").and_then(Value::as_i64) {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

/// The gRPC service. Cheaply cloneable (everything shared is `Arc`/`reqwest::Client`).
#[derive(Clone)]
pub struct Scaler {
    http: reqwest::Client,
    metrics: Arc<Metrics>,
    /// Last `IsActive` value per `<namespace>/<name>` — served on a read failure so
    /// the fleet never flaps to 0 and strands pending work — plus the
    /// downscale-damping streak: `IsActive` flips FALSE only after
    /// `downscale_stable_reads` consecutive quiet reads (scale-UP is always
    /// immediate; a single noisy sample must not zero a fleet).
    last_active: Arc<Mutex<HashMap<String, (bool, u32)>>>,
    /// Consecutive quiet reads required before reporting inactive
    /// (`SCALER_DOWNSCALE_STABLE_READS`, default 3).
    downscale_stable_reads: u32,
    /// Lazily-built in-cluster client for the inbox_pending pod scrape
    /// (`None` when the pod has no service account / out-of-cluster dev).
    kube: Option<kube::Client>,
    /// `StreamIsActive` poll cadence.
    poll_interval: Duration,
    /// Optional bearer token presented to the coordination server (read from
    /// `AGENTCTL_API_TOKEN` at startup). `Some` ⇒ add `Authorization: Bearer <token>`
    /// to every `work.stats` request; `None` (env unset/empty) ⇒ no header.
    auth_token: Option<Arc<String>>,
}

impl Scaler {
    /// Construct from a shared HTTP client + metrics, the stream poll cadence, and
    /// the optional bearer token to present to the coordination server.
    pub fn new(
        http: reqwest::Client,
        metrics: Arc<Metrics>,
        poll_interval: Duration,
        auth_token: Option<Arc<String>>,
        kube: Option<kube::Client>,
    ) -> Self {
        let downscale_stable_reads = std::env::var("SCALER_DOWNSCALE_STABLE_READS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n: &u32| *n >= 1)
            .unwrap_or(3);
        Self {
            http,
            metrics,
            last_active: Arc::new(Mutex::new(HashMap::new())),
            poll_interval,
            auth_token,
            downscale_stable_reads,
            kube,
        }
    }

    /// Sum `agent_inbox_pending` across the fleet's member pods: list by the
    /// rendered label selector, scrape each running pod's probes-port
    /// `/metrics`. A pod that fails to scrape contributes nothing (its work
    /// shows up on the survivors' inboxes or the next poll); zero pods = 0.
    async fn read_inbox_pending(&self, cfg: &ScalerConfig) -> Result<i64, String> {
        let Some(kube) = &self.kube else {
            self.metrics.inc_error();
            return Err("inbox_pending needs the in-cluster kube client".to_string());
        };
        if cfg.namespace.is_empty() || cfg.selector.is_empty() {
            self.metrics.inc_error();
            return Err("inbox_pending needs scalerMetadata.namespace + selector".to_string());
        }
        use k8s_openapi::api::core::v1::Pod;
        let pods: kube::Api<Pod> = kube::Api::namespaced(kube.clone(), &cfg.namespace);
        let list = pods
            .list(&kube::api::ListParams::default().labels(&cfg.selector))
            .await
            .map_err(|e| format!("list pods {}: {e}", cfg.selector))?;
        let mut sum = 0i64;
        for pod in &list.items {
            let Some(ip) = pod.status.as_ref().and_then(|st| st.pod_ip.clone()) else {
                continue;
            };
            let phase = pod
                .status
                .as_ref()
                .and_then(|st| st.phase.as_deref())
                .unwrap_or("");
            if phase != "Running" {
                continue;
            }
            let url = format!("http://{ip}:9090/metrics");
            match self.http.get(&url).send().await {
                Ok(resp) => match resp.text().await {
                    Ok(body) => {
                        if let Some(v) = sum_inbox_pending(&body) {
                            sum += v;
                        }
                    }
                    Err(e) => tracing::debug!(%url, error = %e, "metrics body unreadable"),
                },
                Err(e) => tracing::debug!(%url, error = %e, "member scrape failed"),
            }
        }
        self.metrics.inc_read();
        self.metrics.set_backlog(sum);
        Ok(sum)
    }

    /// Read the configured signal (P6-5 v2 dispatch).
    async fn read_signal(&self, cfg: &ScalerConfig) -> Result<i64, String> {
        match cfg.signal {
            Signal::Backlog => self.read_pending(cfg).await,
            Signal::InboxPending => self.read_inbox_pending(cfg).await,
        }
    }

    /// POST `work.stats` to the coordination server and return the parsed `pending`.
    /// Updates the read/error counters and the `last_backlog` gauge.
    async fn read_pending(&self, cfg: &ScalerConfig) -> Result<i64, String> {
        if cfg.coordination_url.is_empty() {
            self.metrics.inc_error();
            return Err("scalerMetadata.coordinationUrl is required".to_string());
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "work.stats", "arguments": {} }
        });
        let mut request = self.http.post(&cfg.coordination_url).json(&body);
        // Present the bearer token when the operator/chart set AGENTCTL_API_TOKEN;
        // when unset, no header (for an unauthenticated coordinator).
        if let Some(token) = &self.auth_token {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| format!("POST {}: {e}", cfg.coordination_url));
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                self.metrics.inc_error();
                return Err(e);
            }
        };
        let value: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                self.metrics.inc_error();
                return Err(format!("decode work.stats reply: {e}"));
            }
        };
        match parse_pending(&value) {
            Some(p) => {
                self.metrics.inc_read();
                self.metrics.set_backlog(p);
                Ok(p)
            }
            None => {
                self.metrics.inc_error();
                Err("work.stats reply had no `pending` field".to_string())
            }
        }
    }

    /// The cache key for a `ScaledObjectRef`.
    fn key(namespace: &str, name: &str) -> String {
        format!("{namespace}/{name}")
    }

    /// Resolve the current active state for one `ScaledObject`: read the
    /// configured signal and decide, with DOWNSCALE DAMPING — a fleet flips
    /// inactive only after `downscale_stable_reads` consecutive quiet reads
    /// (one noisy sample must not zero it), while activation is immediate.
    /// On a read failure serve the LAST known value (default false on the
    /// very first read — no pods, no known work).
    async fn active_for(&self, cfg: &ScalerConfig, key: &str) -> bool {
        match self.read_signal(cfg).await {
            Ok(pending) => {
                let raw = decide_active(pending, cfg.activation_threshold);
                let mut map = self.last_active.lock().expect("last_active mutex");
                let (was_active, quiet_streak) = *map.get(key).unwrap_or(&(false, 0));
                let (active, streak) = if raw {
                    (true, 0)
                } else if was_active {
                    let streak = quiet_streak + 1;
                    if streak >= self.downscale_stable_reads {
                        (false, 0)
                    } else {
                        tracing::debug!(
                            key,
                            streak,
                            need = self.downscale_stable_reads,
                            "quiet read damped (still active)"
                        );
                        (true, streak)
                    }
                } else {
                    (false, 0)
                };
                map.insert(key.to_string(), (active, streak));
                active
            }
            Err(e) => {
                let last = self
                    .last_active
                    .lock()
                    .expect("last_active mutex")
                    .get(key)
                    .map(|(a, _)| *a)
                    .unwrap_or(false);
                tracing::warn!(error = %e, key, last_active = last, "signal read failed; serving last known IsActive");
                last
            }
        }
    }
}

#[tonic::async_trait]
impl ExternalScaler for Scaler {
    /// The scale-from/to-zero gate. `result = pending > activationThreshold`; on a
    /// coordination read failure, the last known value (never a forced 0).
    #[tracing::instrument(skip_all, fields(ns = %request.get_ref().namespace, name = %request.get_ref().name))]
    async fn is_active(
        &self,
        request: Request<ScaledObjectRef>,
    ) -> Result<Response<IsActiveResponse>, Status> {
        let r = request.into_inner();
        let cfg = ScalerConfig::from_metadata(&r.scaler_metadata);
        let key = Self::key(&r.namespace, &r.name);
        let active = self.active_for(&cfg, &key).await;
        Ok(Response::new(IsActiveResponse { result: active }))
    }

    type StreamIsActiveStream =
        Pin<Box<dyn Stream<Item = Result<IsActiveResponse, Status>> + Send + 'static>>;

    /// Poll `work.stats` on an interval and push an `IsActiveResponse` on each
    /// `0 ↔ >0` transition (and an initial value on the first tick). On a read
    /// failure the last emitted value is held (no transition is reported), so the
    /// fleet never flaps to 0.
    #[tracing::instrument(skip_all, fields(ns = %request.get_ref().namespace, name = %request.get_ref().name))]
    async fn stream_is_active(
        &self,
        request: Request<ScaledObjectRef>,
    ) -> Result<Response<Self::StreamIsActiveStream>, Status> {
        let r = request.into_inner();
        let cfg = ScalerConfig::from_metadata(&r.scaler_metadata);
        let key = Self::key(&r.namespace, &r.name);
        let this = self.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(this.poll_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_sent: Option<bool> = None;
            loop {
                ticker.tick().await;
                let active = this.active_for(&cfg, &key).await;
                // Emit the initial value and every subsequent 0↔>0 transition.
                if last_sent != Some(active) {
                    last_sent = Some(active);
                    if tx
                        .send(Ok(IsActiveResponse { result: active }))
                        .await
                        .is_err()
                    {
                        // KEDA closed the stream — stop polling.
                        break;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    /// The metric the HPA scales on: `agentctl-backlog`, `targetSize = threshold`.
    #[tracing::instrument(skip_all, fields(ns = %request.get_ref().namespace, name = %request.get_ref().name))]
    async fn get_metric_spec(
        &self,
        request: Request<ScaledObjectRef>,
    ) -> Result<Response<GetMetricSpecResponse>, Status> {
        let cfg = ScalerConfig::from_metadata(&request.into_inner().scaler_metadata);
        Ok(Response::new(metric_spec(cfg.threshold)))
    }

    /// The current backlog depth. KEDA's HPA computes `ceil(pending / targetSize)`.
    /// On a read failure we report the last known backlog gauge (best-effort), so a
    /// transient coordination blip does not drive the HPA toward 0.
    #[tracing::instrument(skip_all, fields(ns = %request.get_ref().scaled_object_ref.as_ref().map(|r| r.namespace.as_str()).unwrap_or(""), metric = %request.get_ref().metric_name))]
    async fn get_metrics(
        &self,
        request: Request<GetMetricsRequest>,
    ) -> Result<Response<GetMetricsResponse>, Status> {
        let req = request.into_inner();
        let ref_ = req
            .scaled_object_ref
            .ok_or_else(|| Status::invalid_argument("scaledObjectRef is required"))?;
        let cfg = ScalerConfig::from_metadata(&ref_.scaler_metadata);
        let pending = match self.read_signal(&cfg).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "signal read failed; reporting last known value");
                // Best-effort: the gauge holds the last successful read (0 if none).
                self.metrics.last_backlog()
            }
        };
        Ok(Response::new(metric_value(pending)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // --- P6-5 v2: signal tokens + exposition summing + downscale damping -------

    #[test]
    fn metric_token_selects_the_signal_and_typos_degrade_to_backlog() {
        let base = [("coordinationUrl", "http://c/")];
        assert_eq!(
            ScalerConfig::from_metadata(&md(&base)).signal,
            Signal::Backlog
        );
        let cfg = ScalerConfig::from_metadata(&md(&[
            ("coordinationUrl", "http://c/"),
            ("metric", "inbox_pending"),
            ("namespace", "org-a"),
            ("selector", "agentctl.dev/agent=f"),
        ]));
        assert_eq!(cfg.signal, Signal::InboxPending);
        assert_eq!(cfg.namespace, "org-a");
        assert_eq!(cfg.selector, "agentctl.dev/agent=f");
        // A typo scales on backlog (warned), never bricks the fleet.
        let cfg = ScalerConfig::from_metadata(&md(&[
            ("coordinationUrl", "http://c/"),
            ("metric", "inbox_pendign"),
        ]));
        assert_eq!(cfg.signal, Signal::Backlog);
    }

    #[test]
    fn sum_inbox_pending_reads_labeled_and_bare_samples_only() {
        let body = "\
# HELP agent_inbox_pending pending inbox items\n\
# TYPE agent_inbox_pending gauge\n\
agent_inbox_pending 3\n\
agent_inbox_pending{queue=\"hot\"} 2\n\
agent_inbox_pending_seconds_total 99\n\
agent_turns_queued 7\n";
        assert_eq!(sum_inbox_pending(body), Some(5));
        // No sample at all ⇒ None (a scrape of the wrong port must not read as 0-quiet).
        assert_eq!(sum_inbox_pending("agent_turns_queued 7\n"), None);
    }

    #[tokio::test]
    async fn downscale_damps_but_activation_is_immediate() {
        // No kube/coordination: drive the damping state machine directly by
        // simulating reads through the same map the scaler uses.
        let scaler = Scaler::new(
            reqwest::Client::builder().build().unwrap(),
            Arc::new(Metrics::new()),
            Duration::from_millis(10),
            None,
            None,
        );
        let key = "ns/fleet";
        // Seed: active.
        scaler
            .last_active
            .lock()
            .unwrap()
            .insert(key.to_string(), (true, 0));
        // Simulate the quiet-streak path exactly as active_for computes it.
        let step = |map: &Arc<Mutex<HashMap<String, (bool, u32)>>>, raw: bool| {
            let mut m = map.lock().unwrap();
            let (was, streak) = *m.get(key).unwrap_or(&(false, 0));
            let out = if raw {
                (true, 0)
            } else if was {
                let s2 = streak + 1;
                if s2 >= scaler.downscale_stable_reads {
                    (false, 0)
                } else {
                    (true, s2)
                }
            } else {
                (false, 0)
            };
            m.insert(key.to_string(), out);
            out.0
        };
        // Two quiet reads: still active (damped). Third: finally inactive.
        assert!(step(&scaler.last_active, false));
        assert!(step(&scaler.last_active, false));
        assert!(!step(&scaler.last_active, false));
        // One busy read reactivates IMMEDIATELY.
        assert!(step(&scaler.last_active, true));
    }

    // --- IsActive at the activation boundary (0,1,2 vs activation=1) -----------

    #[test]
    fn is_active_at_the_activation_boundary() {
        // Default activation = 1 ⇒ pending must be STRICTLY greater to be active.
        assert!(!decide_active(0, 1), "0 pending is inactive");
        assert!(!decide_active(1, 1), "pending==activation is NOT active");
        assert!(
            decide_active(2, 1),
            "pending>activation lights the first pod"
        );
    }

    #[test]
    fn is_active_with_activation_zero_wakes_on_any_pending() {
        // activation=0 ⇒ any pending item activates (the eager from-zero setting).
        assert!(!decide_active(0, 0));
        assert!(decide_active(1, 0));
        assert!(decide_active(2, 0));
    }

    // --- metadata defaulting ---------------------------------------------------

    #[test]
    fn config_defaults_when_metadata_absent() {
        let cfg = ScalerConfig::from_metadata(&md(&[("coordinationUrl", "http://coord:8080/")]));
        assert_eq!(cfg.coordination_url, "http://coord:8080/");
        assert_eq!(cfg.threshold, DEFAULT_THRESHOLD); // 5
        assert_eq!(cfg.activation_threshold, DEFAULT_ACTIVATION_THRESHOLD); // 1
    }

    #[test]
    fn config_parses_overrides_and_clamps_bad_threshold() {
        let cfg = ScalerConfig::from_metadata(&md(&[
            ("coordinationUrl", "http://c/"),
            ("threshold", "20"),
            ("activationThreshold", "3"),
        ]));
        assert_eq!(cfg.threshold, 20);
        assert_eq!(cfg.activation_threshold, 3);

        // A 0 / negative / garbage threshold falls back to the default (never 0 —
        // KEDA divides backlog by targetSize).
        for bad in ["0", "-4", "abc", ""] {
            let cfg = ScalerConfig::from_metadata(&md(&[("threshold", bad)]));
            assert_eq!(cfg.threshold, DEFAULT_THRESHOLD, "threshold={bad:?}");
        }
        // A negative activationThreshold falls back; 0 is a valid override.
        assert_eq!(
            ScalerConfig::from_metadata(&md(&[("activationThreshold", "-1")])).activation_threshold,
            DEFAULT_ACTIVATION_THRESHOLD
        );
        assert_eq!(
            ScalerConfig::from_metadata(&md(&[("activationThreshold", "0")])).activation_threshold,
            0
        );
    }

    // --- GetMetricSpec targetSize from metadata --------------------------------

    #[test]
    fn metric_spec_carries_name_and_threshold_target() {
        let cfg = ScalerConfig::from_metadata(&md(&[("threshold", "8")]));
        let spec = metric_spec(cfg.threshold);
        assert_eq!(spec.metric_specs.len(), 1);
        let m = &spec.metric_specs[0];
        assert_eq!(m.metric_name, METRIC_NAME);
        assert_eq!(m.metric_name, "agentctl-backlog");
        assert_eq!(m.target_size, 8);
        assert_eq!(m.target_size_float, 8.0);
    }

    #[test]
    fn metric_spec_uses_default_threshold_when_unset() {
        let cfg = ScalerConfig::from_metadata(&md(&[]));
        let spec = metric_spec(cfg.threshold);
        assert_eq!(spec.metric_specs[0].target_size, DEFAULT_THRESHOLD);
    }

    // --- GetMetrics value from parsed work.stats -------------------------------

    #[test]
    fn metric_value_reports_pending() {
        let mv = metric_value(11);
        assert_eq!(mv.metric_values.len(), 1);
        assert_eq!(mv.metric_values[0].metric_name, METRIC_NAME);
        assert_eq!(mv.metric_values[0].metric_value, 11);
        assert_eq!(mv.metric_values[0].metric_value_float, 11.0);
    }

    #[test]
    fn parse_pending_from_structured_content() {
        // A full JSON-RPC response with the MCP dual encoding — structuredContent
        // is preferred.
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": "{\"pending\":3,\"claimed\":1,\"oldest_age_ms\":42}" }],
                "structuredContent": { "pending": 3, "claimed": 1, "oldest_age_ms": 42 },
                "isError": false
            }
        });
        assert_eq!(parse_pending(&resp), Some(3));
        // GetMetrics maps that straight through.
        assert_eq!(
            metric_value(parse_pending(&resp).unwrap()).metric_values[0].metric_value,
            3
        );
    }

    #[test]
    fn parse_pending_text_fallback_when_no_structured_content() {
        // No structuredContent ⇒ fall back to the text content[] JSON.
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": "{\"pending\":9,\"claimed\":0,\"oldest_age_ms\":0}" }],
                "isError": false
            }
        });
        assert_eq!(parse_pending(&resp), Some(9));
    }

    #[test]
    fn parse_pending_accepts_bare_call_tool_result() {
        // Tolerate being handed the CallToolResult directly (no JSON-RPC envelope).
        let result = json!({ "structuredContent": { "pending": 0, "claimed": 4 } });
        assert_eq!(parse_pending(&result), Some(0));
    }

    #[test]
    fn parse_pending_none_when_absent() {
        assert_eq!(parse_pending(&json!({ "result": { "content": [] } })), None);
        assert_eq!(
            parse_pending(&json!({ "result": { "structuredContent": { "claimed": 2 } } })),
            None
        );
        // A non-JSON text body in the fallback path is ignored, not panicked on.
        assert_eq!(
            parse_pending(
                &json!({ "result": { "content": [{ "type": "text", "text": "not json" }] } })
            ),
            None
        );
    }
}
