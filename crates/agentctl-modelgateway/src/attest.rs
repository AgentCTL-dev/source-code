// SPDX-License-Identifier: BUSL-1.1
//! Source-IP → pod identity **attestation** for the ModelGateway.
//!
//! A conformant agent is networkless and asserts only its *identity* to the
//! gateway. In the default (back-compat) mode that identity is carried in the
//! `X-Agent-Namespace`/`X-Agent-Name` headers — which the agent itself sets, and
//! can therefore **spoof** to bill/route as another tenant.
//!
//! Attested mode (gated by `IDENTITY_ATTEST`/`ATTEST_IDENTITY`) closes that hole:
//! the caller's identity is derived from the **kernel-set source IP** of the TCP
//! connection (the pod IP, unforgeable by the agent), resolved to the real pod
//! via the kube API. The pod's namespace is the authoritative tenant; the agent
//! name comes from the operator-set `agentctl.dev/agent` label (fallback: pod
//! name). If the request *also* carries `X-Agent-Namespace` and it disagrees
//! with the attested namespace, the attested one **always wins** and the
//! disagreement is recorded as a spoof attempt.
//!
//! This module holds the **pure** logic — env gate, pod→identity derivation,
//! IP→pod matching, the TTL cache, and the attested-vs-header reconciliation —
//! so it is unit-testable without a cluster. The kube lookup itself lives in
//! `main.rs` as I/O glue.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;

/// The operator-set label carrying an agent's name on its pod.
pub const AGENT_LABEL: &str = "agentctl.dev/agent";

/// Default TTL for cached `ip → identity` resolutions: short, so a pod that is
/// deleted and its IP recycled is re-attested quickly, while a burst of requests
/// from one pod still avoids hammering the kube API.
pub const DEFAULT_TTL: Duration = Duration::from_secs(10);

/// An attested caller identity, derived authoritatively from the source pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The pod's namespace — the authoritative tenant for routing + metering.
    pub namespace: String,
    /// The agent name: the `agentctl.dev/agent` label, else the pod name.
    pub agent: String,
}

/// Whether attested-identity mode is enabled, read from the environment.
///
/// Enabled when either `IDENTITY_ATTEST` or `ATTEST_IDENTITY` is set to a truthy
/// value (`1`/`true`/`on`/`yes`, case-insensitive). Unset/empty/anything else →
/// disabled (the default; pure header behaviour, back-compat).
pub fn attest_enabled_from_env() -> bool {
    ["IDENTITY_ATTEST", "ATTEST_IDENTITY"]
        .iter()
        .any(|k| std::env::var(k).map(|v| is_truthy(&v)).unwrap_or(false))
}

/// Whether an env value reads as truthy.
fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Derive the attested identity from a resolved pod.
///
/// The namespace is taken from the pod's metadata (required — without it we
/// cannot attest a tenant, so `None`). The agent name is the `agentctl.dev/agent`
/// label when present and non-empty, else the pod name, else `"unknown"`.
pub fn identity_from_pod(pod: &Pod) -> Option<Identity> {
    let namespace = pod.metadata.namespace.clone().filter(|s| !s.is_empty())?;
    let agent = pod
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(AGENT_LABEL))
        .filter(|s| !s.is_empty())
        .cloned()
        .or_else(|| pod.metadata.name.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".to_string());
    Some(Identity { namespace, agent })
}

/// Whether a pod's status reports the given source IP. Checks both the singular
/// `status.podIP` and the `status.podIPs` list (dual-stack), since the kube
/// field selector is advisory and we re-verify the match locally.
pub fn pod_matches_ip(pod: &Pod, ip: &str) -> bool {
    let Some(status) = pod.status.as_ref() else {
        return false;
    };
    if status.pod_ip.as_deref() == Some(ip) {
        return true;
    }
    status
        .pod_ips
        .as_ref()
        .is_some_and(|ips| ips.iter().any(|p| p.ip == ip))
}

/// The outcome of reconciling an attested identity against the (spoofable)
/// header-supplied namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconciled {
    /// The namespace to USE — always the attested one (attested always wins).
    pub namespace: String,
    /// `true` when the request carried an `X-Agent-Namespace` that DISAGREED
    /// with the attested namespace (a spoof attempt) — the caller should bump
    /// the spoof counter and log a warning.
    pub spoofed: bool,
}

/// Reconcile the attested namespace against an optional header-supplied one.
///
/// The attested namespace **always wins**. A present, non-empty, *different*
/// header namespace flags a spoof attempt. An absent/empty or matching header is
/// not a spoof.
pub fn reconcile(attested_ns: &str, header_ns: Option<&str>) -> Reconciled {
    let spoofed = matches!(header_ns, Some(h) if !h.is_empty() && h != attested_ns);
    Reconciled {
        namespace: attested_ns.to_string(),
        spoofed,
    }
}

/// The policy decision for an attested request, given the (already performed)
/// source-IP lookup and the header namespace. Pure, so the whole attested-mode
/// policy — *attest, flag a spoof, or reject* — is unit-testable without a
/// cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Identity attested — use this `(namespace, agent)`. `spoofed` is `true`
    /// when a header namespace disagreed (bump the spoof counter + warn).
    Use { identity: Identity, spoofed: bool },
    /// The source IP resolved to no pod — reject (`403`). In attested mode we
    /// never fall back to the spoofable header.
    Reject,
}

/// Decide a request's attested identity from the resolved (or absent) pod
/// identity and the header namespace. `None` (no pod owns the source IP) ⇒
/// [`Decision::Reject`]; otherwise [`Decision::Use`] with the attested namespace
/// and a `spoofed` flag from [`reconcile`].
pub fn decide(looked_up: Option<Identity>, header_ns: Option<&str>) -> Decision {
    match looked_up {
        Some(identity) => {
            let spoofed = reconcile(&identity.namespace, header_ns).spoofed;
            Decision::Use { identity, spoofed }
        }
        None => Decision::Reject,
    }
}

/// A small TTL cache of `source IP → attested identity`, so a burst of requests
/// from one pod does not hammer the kube API. Entries expire after `ttl`.
#[derive(Debug)]
pub struct IpIdentityCache {
    ttl: Duration,
    entries: Mutex<HashMap<IpAddr, Entry>>,
}

#[derive(Debug)]
struct Entry {
    identity: Identity,
    inserted: Instant,
}

impl IpIdentityCache {
    /// A cache whose entries expire after `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// A fresh cached identity for `ip`, or `None` when absent/expired.
    pub fn get(&self, ip: &IpAddr) -> Option<Identity> {
        self.get_at(ip, Instant::now())
    }

    /// Insert/refresh the identity for `ip`.
    pub fn put(&self, ip: IpAddr, identity: Identity) {
        self.put_at(ip, identity, Instant::now());
    }

    /// [`Self::get`] at an explicit clock value (testable expiry).
    fn get_at(&self, ip: &IpAddr, now: Instant) -> Option<Identity> {
        let entries = self.entries.lock().expect("ip cache mutex");
        let entry = entries.get(ip)?;
        if now.saturating_duration_since(entry.inserted) < self.ttl {
            Some(entry.identity.clone())
        } else {
            None
        }
    }

    /// [`Self::put`] at an explicit clock value (testable expiry).
    fn put_at(&self, ip: IpAddr, identity: Identity, now: Instant) {
        let mut entries = self.entries.lock().expect("ip cache mutex");
        entries.insert(
            ip,
            Entry {
                identity,
                inserted: now,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PodIP, PodStatus};
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;

    fn id(ns: &str, agent: &str) -> Identity {
        Identity {
            namespace: ns.to_string(),
            agent: agent.to_string(),
        }
    }

    fn pod_with(
        ns: Option<&str>,
        name: Option<&str>,
        label: Option<&str>,
        ip: Option<&str>,
    ) -> Pod {
        let mut pod = Pod::default();
        pod.metadata.namespace = ns.map(str::to_string);
        pod.metadata.name = name.map(str::to_string);
        if let Some(l) = label {
            let mut labels = BTreeMap::new();
            labels.insert(AGENT_LABEL.to_string(), l.to_string());
            pod.metadata.labels = Some(labels);
        }
        if let Some(ip) = ip {
            pod.status = Some(PodStatus {
                pod_ip: Some(ip.to_string()),
                ..Default::default()
            });
        }
        pod
    }

    // --- env gate ----------------------------------------------------------

    #[test]
    fn is_truthy_accepts_common_forms() {
        for v in ["1", "true", "TRUE", "On", "yes", "  yes  "] {
            assert!(is_truthy(v), "{v:?} should be truthy");
        }
        for v in ["0", "false", "off", "no", "", "maybe"] {
            assert!(!is_truthy(v), "{v:?} should be falsy");
        }
    }

    // --- identity derivation ----------------------------------------------

    #[test]
    fn identity_prefers_agent_label() {
        let pod = pod_with(
            Some("team-a"),
            Some("agent-xyz-123"),
            Some("checkout"),
            None,
        );
        assert_eq!(identity_from_pod(&pod), Some(id("team-a", "checkout")));
    }

    #[test]
    fn identity_falls_back_to_pod_name_without_label() {
        let pod = pod_with(Some("team-a"), Some("agent-xyz-123"), None, None);
        assert_eq!(identity_from_pod(&pod), Some(id("team-a", "agent-xyz-123")));
    }

    #[test]
    fn identity_falls_back_to_unknown_without_label_or_name() {
        let pod = pod_with(Some("team-a"), None, None, None);
        assert_eq!(identity_from_pod(&pod), Some(id("team-a", "unknown")));
    }

    #[test]
    fn identity_empty_label_falls_back_to_name() {
        let pod = pod_with(Some("team-a"), Some("p1"), Some(""), None);
        assert_eq!(identity_from_pod(&pod), Some(id("team-a", "p1")));
    }

    #[test]
    fn identity_requires_a_namespace() {
        assert_eq!(
            identity_from_pod(&pod_with(None, Some("p1"), None, None)),
            None
        );
        assert_eq!(
            identity_from_pod(&pod_with(Some(""), Some("p1"), None, None)),
            None
        );
    }

    // --- ip matching -------------------------------------------------------

    #[test]
    fn pod_matches_singular_pod_ip() {
        let pod = pod_with(Some("ns"), Some("p"), None, Some("10.1.2.3"));
        assert!(pod_matches_ip(&pod, "10.1.2.3"));
        assert!(!pod_matches_ip(&pod, "10.1.2.4"));
    }

    #[test]
    fn pod_matches_dual_stack_pod_ips() {
        let mut pod = pod_with(Some("ns"), Some("p"), None, Some("10.1.2.3"));
        pod.status = Some(PodStatus {
            pod_ip: Some("10.1.2.3".to_string()),
            pod_ips: Some(vec![
                PodIP {
                    ip: "10.1.2.3".to_string(),
                },
                PodIP {
                    ip: "fd00::1".to_string(),
                },
            ]),
            ..Default::default()
        });
        assert!(pod_matches_ip(&pod, "fd00::1"));
        assert!(pod_matches_ip(&pod, "10.1.2.3"));
        assert!(!pod_matches_ip(&pod, "fd00::2"));
    }

    #[test]
    fn pod_without_status_matches_nothing() {
        let pod = pod_with(Some("ns"), Some("p"), None, None);
        assert!(!pod_matches_ip(&pod, "10.1.2.3"));
    }

    // --- reconciliation (attested always wins) -----------------------------

    #[test]
    fn reconcile_no_header_is_not_a_spoof() {
        let r = reconcile("team-a", None);
        assert_eq!(r.namespace, "team-a");
        assert!(!r.spoofed);
    }

    #[test]
    fn reconcile_empty_header_is_not_a_spoof() {
        let r = reconcile("team-a", Some(""));
        assert_eq!(r.namespace, "team-a");
        assert!(!r.spoofed);
    }

    #[test]
    fn reconcile_matching_header_is_not_a_spoof() {
        let r = reconcile("team-a", Some("team-a"));
        assert_eq!(r.namespace, "team-a");
        assert!(!r.spoofed);
    }

    #[test]
    fn reconcile_disagreeing_header_is_a_spoof_and_attested_wins() {
        let r = reconcile("team-a", Some("team-b"));
        assert_eq!(r.namespace, "team-a", "attested namespace must win");
        assert!(r.spoofed, "a disagreeing header is a spoof attempt");
    }

    // --- decision policy (attest / spoof / reject) -------------------------

    #[test]
    fn decide_no_pod_rejects() {
        assert_eq!(decide(None, None), Decision::Reject);
        // Even a header that names a namespace cannot rescue an unattestable IP.
        assert_eq!(decide(None, Some("team-a")), Decision::Reject);
    }

    #[test]
    fn decide_resolved_pod_attests_without_header() {
        assert_eq!(
            decide(Some(id("team-a", "checkout")), None),
            Decision::Use {
                identity: id("team-a", "checkout"),
                spoofed: false,
            }
        );
    }

    #[test]
    fn decide_matching_header_is_not_a_spoof() {
        assert_eq!(
            decide(Some(id("team-a", "checkout")), Some("team-a")),
            Decision::Use {
                identity: id("team-a", "checkout"),
                spoofed: false,
            }
        );
    }

    #[test]
    fn decide_disagreeing_header_flags_spoof_and_attested_wins() {
        // Header claims team-b; attested pod is team-a → use team-a, flag spoof.
        assert_eq!(
            decide(Some(id("team-a", "checkout")), Some("team-b")),
            Decision::Use {
                identity: id("team-a", "checkout"),
                spoofed: true,
            }
        );
    }

    // --- TTL cache ---------------------------------------------------------

    #[test]
    fn cache_miss_then_hit() {
        let cache = IpIdentityCache::new(DEFAULT_TTL);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!(cache.get(&ip), None);
        cache.put(ip, id("team-a", "checkout"));
        assert_eq!(cache.get(&ip), Some(id("team-a", "checkout")));
    }

    #[test]
    fn cache_entry_expires_after_ttl() {
        let cache = IpIdentityCache::new(Duration::from_secs(10));
        let ip = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        let t0 = Instant::now();
        cache.put_at(ip, id("team-a", "checkout"), t0);
        // Fresh just before the TTL boundary.
        assert_eq!(
            cache.get_at(&ip, t0 + Duration::from_secs(9)),
            Some(id("team-a", "checkout"))
        );
        // Expired at/after the TTL boundary.
        assert_eq!(cache.get_at(&ip, t0 + Duration::from_secs(10)), None);
        assert_eq!(cache.get_at(&ip, t0 + Duration::from_secs(11)), None);
    }

    #[test]
    fn cache_put_refreshes_identity_and_clock() {
        let cache = IpIdentityCache::new(Duration::from_secs(10));
        let ip = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        let t0 = Instant::now();
        cache.put_at(ip, id("team-a", "old"), t0);
        // Re-put later with a new identity → fresh window from the new instant.
        cache.put_at(ip, id("team-a", "new"), t0 + Duration::from_secs(8));
        assert_eq!(
            cache.get_at(&ip, t0 + Duration::from_secs(15)),
            Some(id("team-a", "new"))
        );
    }

    #[test]
    fn cache_isolates_distinct_ips() {
        let cache = IpIdentityCache::new(DEFAULT_TTL);
        let a = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let b = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        cache.put(a, id("team-a", "a"));
        assert_eq!(cache.get(&a), Some(id("team-a", "a")));
        assert_eq!(cache.get(&b), None);
    }
}
