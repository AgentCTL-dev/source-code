// SPDX-License-Identifier: BUSL-1.1
//! Prometheus text-exposition (format 0.0.4) scraping + parsing.
//!
//! Every control-plane component serves `GET /metrics` in the hand-rendered text
//! format. This module turns that into queryable samples so a
//! scenario can assert an oracle like "`agentctl_modelgateway_tokens_total` rose by
//! 100" or "`agentctl_apiserver_verb_denied_total{} == 1`".
//!
//! Two scrape paths — via the kube apiserver proxy or a port-forward:
//!   * [`scrape_proxy`] — `kubectl get --raw` of the apiserver Service proxy path
//!     (no extra port to open; works through the same kubeconfig as everything else).
//!   * [`scrape_url`]   — a direct `reqwest` GET of a `kubectl port-forward`ed URL.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};

use crate::shell;

/// One parsed metric series sample: `name{labels} value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// The metric name (e.g. `agentctl_coordination_claims_granted_total`).
    pub name: String,
    /// The label set, key-sorted for stable matching.
    pub labels: BTreeMap<String, String>,
    /// The sample value.
    pub value: f64,
}

/// A parsed `/metrics` exposition: a flat list of [`Sample`]s with query helpers.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    /// Every parsed sample, in file order.
    pub samples: Vec<Sample>,
}

impl Metrics {
    /// Parse a Prometheus 0.0.4 text exposition. `# HELP`/`# TYPE`/blank lines are
    /// ignored; a trailing scrape timestamp (Prometheus allows one) is dropped.
    pub fn parse(text: &str) -> Self {
        let mut samples = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(s) = parse_line(line) {
                samples.push(s);
            }
        }
        Metrics { samples }
    }

    /// All samples of a metric `name`, regardless of labels.
    pub fn series<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Sample> + 'a {
        self.samples.iter().filter(move |s| s.name == name)
    }

    /// Sum of every sample of `name` (handy for label-partitioned counters where the
    /// scenario only cares about the total, e.g. tokens across `token_type`).
    pub fn sum(&self, name: &str) -> f64 {
        self.series(name).map(|s| s.value).sum()
    }

    /// The value of `name` with EXACTLY the given labels (a subset match: every
    /// requested label must be present and equal). `None` if no such series.
    pub fn get(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.series(name)
            .find(|s| {
                labels
                    .iter()
                    .all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v))
            })
            .map(|s| s.value)
    }

    /// The value of `name` with no labels (or its single series), else `None`.
    pub fn scalar(&self, name: &str) -> Option<f64> {
        self.series(name).map(|s| s.value).next()
    }

    /// The distinct metric names present (the conformance scenario diffs this against
    /// the contract metrics registry).
    pub fn names(&self) -> BTreeSet<String> {
        self.samples.iter().map(|s| s.name.clone()).collect()
    }

    /// Flatten to a `HashMap<canonical-key, value>`, keyed by a stable
    /// `name{k="v",…}` string (labels key-sorted).
    pub fn to_map(&self) -> HashMap<String, f64> {
        self.samples
            .iter()
            .map(|s| (canonical_key(&s.name, &s.labels), s.value))
            .collect()
    }
}

/// Render a stable `name{k="v",…}` key for a sample (labels already key-sorted in a
/// `BTreeMap`).
fn canonical_key(name: &str, labels: &BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        return name.to_string();
    }
    let inner = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("{name}{{{inner}}}")
}

/// Parse one exposition line: `name[{labels}] value [timestamp]`.
fn parse_line(line: &str) -> Option<Sample> {
    let (head, rest) = match line.find('{') {
        Some(brace) => {
            let name = line[..brace].trim().to_string();
            let close = line[brace + 1..].find('}')? + brace + 1;
            let labels = parse_labels(&line[brace + 1..close]);
            let value_part = line[close + 1..].trim();
            return finish(name, labels, value_part);
        }
        None => {
            // No labels: `name value [timestamp]`.
            let mut it = line.split_whitespace();
            let name = it.next()?.to_string();
            (name, it.next()?)
        }
    };
    let value = rest.parse::<f64>().ok()?;
    Some(Sample {
        name: head,
        labels: BTreeMap::new(),
        value,
    })
}

/// Finish a labelled line: take the first whitespace token after `}` as the value.
fn finish(name: String, labels: BTreeMap<String, String>, value_part: &str) -> Option<Sample> {
    let value = value_part.split_whitespace().next()?.parse::<f64>().ok()?;
    Some(Sample {
        name,
        labels,
        value,
    })
}

/// Parse a `k1="v1",k2="v2"` label body, honoring `\"` and `\\` escapes in values.
fn parse_labels(body: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip separators/whitespace.
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        // Key up to '='.
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key = body[key_start..i].trim().to_string();
        i += 1; // consume '='
        if i >= bytes.len() || bytes[i] != b'"' {
            break;
        }
        i += 1; // consume opening quote
        let mut val = String::new();
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if i + 1 < bytes.len() => {
                    let next = bytes[i + 1];
                    val.push(match next {
                        b'n' => '\n',
                        b't' => '\t',
                        other => other as char,
                    });
                    i += 2;
                }
                b'"' => {
                    i += 1;
                    break;
                }
                b => {
                    val.push(b as char);
                    i += 1;
                }
            }
        }
        if !key.is_empty() {
            out.insert(key, val);
        }
    }
    out
}

/// Scrape a Service's `/metrics` through the kube-apiserver Service proxy, via
/// `kubectl get --raw`. `scheme` is `"http"` or `"https"` (the proxy path encodes
/// it as `https:<svc>:<port>` for TLS backends). `path` is the metrics path,
/// usually `/metrics`.
pub fn scrape_proxy(ns: &str, svc: &str, port: u16, scheme: &str, path: &str) -> Result<Metrics> {
    let svc_seg = if scheme.eq_ignore_ascii_case("https") {
        format!("https:{svc}:{port}")
    } else {
        format!("{svc}:{port}")
    };
    let raw = format!("/api/v1/namespaces/{ns}/services/{svc_seg}/proxy{path}");
    let text = shell::kubectl(&["get", "--raw", &raw])
        .with_context(|| format!("scrape via apiserver proxy {raw}"))?;
    Ok(Metrics::parse(&text))
}

/// Scrape `/metrics` directly from a URL (typically a `kubectl port-forward`ed
/// `http://127.0.0.1:<lp>/metrics`).
pub async fn scrape_url(http: &reqwest::Client, url: &str) -> Result<Metrics> {
    let text = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("status of {url}"))?
        .text()
        .await
        .with_context(|| format!("body of {url}"))?;
    Ok(Metrics::parse(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# HELP agentctl_coordination_claims_granted_total Claims granted.
# TYPE agentctl_coordination_claims_granted_total counter
agentctl_coordination_claims_granted_total 7
agentctl_modelgateway_tokens_total{token_type="in"} 90
agentctl_modelgateway_tokens_total{token_type="out"} 10
agentctl_apiserver_verb_denied_total{verb="drain"} 1 1700000000000
"#;

    #[test]
    fn parses_labelled_and_unlabelled() {
        let m = Metrics::parse(SAMPLE);
        assert_eq!(
            m.scalar("agentctl_coordination_claims_granted_total"),
            Some(7.0)
        );
        // label-partitioned sum
        assert_eq!(m.sum("agentctl_modelgateway_tokens_total"), 100.0);
        assert_eq!(
            m.get(
                "agentctl_modelgateway_tokens_total",
                &[("token_type", "out")]
            ),
            Some(10.0)
        );
        // trailing timestamp is dropped
        assert_eq!(
            m.get("agentctl_apiserver_verb_denied_total", &[("verb", "drain")]),
            Some(1.0)
        );
    }

    #[test]
    fn names_and_map_are_canonical() {
        let m = Metrics::parse(SAMPLE);
        let names = m.names();
        assert!(names.contains("agentctl_modelgateway_tokens_total"));
        let map = m.to_map();
        assert_eq!(
            map.get("agentctl_modelgateway_tokens_total{token_type=\"in\"}"),
            Some(&90.0)
        );
    }

    #[test]
    fn escaped_label_values() {
        let m = Metrics::parse(r#"x{path="a\"b",msg="l\nr"} 1"#);
        let s = &m.samples[0];
        assert_eq!(s.labels.get("path").unwrap(), "a\"b");
        assert_eq!(s.labels.get("msg").unwrap(), "l\nr");
    }
}
