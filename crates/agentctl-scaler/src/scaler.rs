// SPDX-License-Identifier: BUSL-1.1
//! The `ExternalScaler` gRPC service (agentctl RFC 0011 §5.2) + its pure decision
//! logic.
//!
//! The scaler reads the reference coordination MCP server's **off-pod backlog**
//! (`work.stats` → `pending`, contract ask P9) over MCP JSON-RPC/HTTP and maps it
//! onto KEDA's four RPCs:
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

/// The per-`ScaledObject` knobs the scaler reads from `ScaledObjectRef.scalerMetadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalerConfig {
    /// The coordination MCP endpoint to POST `work.stats` to (required at runtime).
    pub coordination_url: String,
    /// Per-replica target (`targetSize`); KEDA divides backlog by this. Always ≥ 1.
    pub threshold: i64,
    /// The 0→1 gate: `IsActive` is `pending > activation_threshold`.
    pub activation_threshold: i64,
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
        Self {
            coordination_url,
            threshold,
            activation_threshold,
        }
    }
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
    /// the fleet never flaps to 0 and strands pending work.
    last_active: Arc<Mutex<HashMap<String, bool>>>,
    /// `StreamIsActive` poll cadence.
    poll_interval: Duration,
}

impl Scaler {
    /// Construct from a shared HTTP client + metrics and the stream poll cadence.
    pub fn new(http: reqwest::Client, metrics: Arc<Metrics>, poll_interval: Duration) -> Self {
        Self {
            http,
            metrics,
            last_active: Arc::new(Mutex::new(HashMap::new())),
            poll_interval,
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
        let resp = self
            .http
            .post(&cfg.coordination_url)
            .json(&body)
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

    /// Resolve the current active state for one `ScaledObject`: read the backlog
    /// and decide; on a read failure serve the LAST known value (default false on
    /// the very first read — initial state is "no pods, no known work").
    async fn active_for(&self, cfg: &ScalerConfig, key: &str) -> bool {
        match self.read_pending(cfg).await {
            Ok(pending) => {
                let active = decide_active(pending, cfg.activation_threshold);
                self.last_active
                    .lock()
                    .expect("last_active mutex")
                    .insert(key.to_string(), active);
                active
            }
            Err(e) => {
                let last = *self
                    .last_active
                    .lock()
                    .expect("last_active mutex")
                    .get(key)
                    .unwrap_or(&false);
                tracing::warn!(error = %e, key, last_active = last, "work.stats read failed; serving last known IsActive");
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
    /// `0 ↔ >0` transition (and an initial value on the first tick). Poll-based is
    /// fine for v1 (agentctl RFC 0011 §5.2). On a read failure the last emitted
    /// value is held (no transition is reported), so the fleet never flaps to 0.
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
        let pending = match self.read_pending(&cfg).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "work.stats read failed; reporting last known backlog");
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
