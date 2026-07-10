// SPDX-License-Identifier: BUSL-1.1
//! Source-IP → pod identity **attestation** for the coordination work hub.
//!
//! The `work.*` claim surface is the single serializing point that makes
//! exactly-one-owner hold. By default a caller's identity is whatever it puts in
//! the `_meta` `agent/instance`/`agent/run_id` keys — which the caller itself sets
//! and can therefore **spoof**, so under hostile multi-tenancy one tenant could
//! ack/renew/release (settle or steal) another tenant's lease just by asserting
//! its holder string. The shared bearer token (`AGENTCTL_API_TOKEN`) gates *access*
//! to the surface but does not distinguish *which* tenant is calling.
//!
//! Attested mode (gated by `COORDINATION_ATTEST_IDENTITY`, default OFF) closes that
//! hole for the claim lifecycle: the caller's identity is derived from the
//! **kernel-set source IP** of the TCP connection (the pod IP, unforgeable by the
//! caller), resolved to the real pod via the kube API. The pod's namespace is the
//! authoritative tenant; the agent name comes from the operator-set
//! `agentctl.dev/agent` label (fallback: the pod name). The lease HOLDER recorded
//! on `work.claim` is this attested `namespace/agent` — NOT the self-asserted
//! `_meta` agent — and `work.ack`/`work.renew`/`work.release` are allowed only when
//! the caller's attested identity equals the recorded holder. A source IP that
//! attests to no pod **fails closed**: the claim-lifecycle call is rejected (we
//! never fall back to the spoofable self-asserted holder in attested mode).
//!
//! This module holds the **pure** logic — the env gate, pod→identity derivation,
//! IP→pod matching, the TTL cache, and the per-call holder-binding/holder-check
//! decisions — so it is unit-testable without a cluster. The kube lookups live in
//! `main.rs` as I/O glue.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;

/// The operator-set label carrying an agent's name on its pod (set by the
/// operator on every rendered agent pod).
pub const AGENT_LABEL: &str = "agentctl.dev/agent";

/// Default TTL for cached `ip → identity` resolutions: short, so a pod that is
/// deleted and its IP recycled is re-attested quickly, while a burst of claim
/// calls from one pod still avoids hammering the kube API.
pub const DEFAULT_TTL: Duration = Duration::from_secs(10);

/// An attested caller identity, derived authoritatively from the source pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The pod's namespace — the authoritative tenant for the work hub.
    pub namespace: String,
    /// The agent name: the `agentctl.dev/agent` label, else the pod name.
    pub agent: String,
}

impl Identity {
    /// The opaque holder string recorded on a lease and compared on the lifecycle
    /// calls: `namespace/agent`. Stable for every pod of the same logical agent, so
    /// a restarted replica of the same tenant agent can still settle its own lease,
    /// while a DIFFERENT tenant (different namespace/agent) can never match.
    pub fn holder(&self) -> String {
        format!("{}/{}", self.namespace, self.agent)
    }
}

/// Whether attested-identity mode is enabled, read from the environment.
///
/// Enabled when `COORDINATION_ATTEST_IDENTITY` is set to a truthy value
/// (`1`/`true`/`on`/`yes`, case-insensitive). Unset/empty/anything else → disabled
/// (the default: the self-asserted `_meta` holder is authoritative).
pub fn attest_enabled_from_env() -> bool {
    std::env::var("COORDINATION_ATTEST_IDENTITY")
        .map(|v| is_truthy(&v))
        .unwrap_or(false)
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
/// The namespace is taken from the pod's metadata (required — without it we cannot
/// attest a tenant, so `None`). The agent name is the `agentctl.dev/agent` label
/// when present and non-empty, else the pod name, else `"unknown"`.
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
/// `status.podIP` and the `status.podIPs` list (dual-stack), since the kube field
/// selector is advisory and we re-verify the match locally.
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

/// The attested caller identity for one request, computed by the I/O glue in
/// `main.rs` and threaded into the pure wire layer ([`crate::mcp`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallerIdentity {
    /// Attestation is OFF (default): the self-asserted `_meta` holder is
    /// authoritative and lifecycle calls are not identity-verified.
    Disabled,
    /// Attestation is ON and the caller's source IP resolved to this holder string
    /// (`namespace/agent`).
    Attested(String),
    /// Attestation is ON but the caller's source IP owned no attestable pod (or the
    /// kube lookup failed). Claim-lifecycle calls MUST fail closed.
    Unresolved,
}

impl CallerIdentity {
    /// Whether the caller was successfully attested (drives the `attest_ok` metric).
    pub fn is_attested(&self) -> bool {
        matches!(self, CallerIdentity::Attested(_))
    }
}

/// The holder to RECORD on `work.claim`. `Some(holder)` ⇒ proceed and bind the
/// lease to this holder; `None` ⇒ reject the claim (fail closed: attested mode
/// could not attest the caller).
///
/// - [`CallerIdentity::Disabled`] ⇒ the self-asserted holder.
/// - [`CallerIdentity::Attested`] ⇒ the ATTESTED identity (authoritative; it
///   overrides the self-asserted `_meta` agent so a tenant cannot bill the lease to
///   someone else).
/// - [`CallerIdentity::Unresolved`] ⇒ `None` (reject).
pub fn claim_holder(caller: &CallerIdentity, self_asserted: &str) -> Option<String> {
    match caller {
        CallerIdentity::Disabled => Some(self_asserted.to_string()),
        CallerIdentity::Attested(id) => Some(id.clone()),
        CallerIdentity::Unresolved => None,
    }
}

/// The `expected_holder` predicate for a verifying lifecycle op
/// (`work.ack`/`work.renew`/`work.release`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HolderCheck {
    /// No constraint (attest disabled) — the store enforces nothing.
    Unconstrained,
    /// Constrain the op to this attested holder (the store enforces `holder == h`
    /// atomically — a tenant cannot settle or steal another tenant's lease).
    MustMatch(String),
    /// Reject before touching the store (unattestable caller in attested mode —
    /// fail closed).
    Reject,
}

/// Decide the holder predicate for a verifying lifecycle op from the attested
/// caller. See [`HolderCheck`].
pub fn holder_check(caller: &CallerIdentity) -> HolderCheck {
    match caller {
        CallerIdentity::Disabled => HolderCheck::Unconstrained,
        CallerIdentity::Attested(id) => HolderCheck::MustMatch(id.clone()),
        CallerIdentity::Unresolved => HolderCheck::Reject,
    }
}

/// A small TTL cache of `source IP → attested identity`, so a burst of claim
/// calls from one pod does not hammer the kube API. Entries expire after `ttl`.
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

    // --- identity derivation + holder string -------------------------------

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

    #[test]
    fn holder_string_is_namespace_slash_agent() {
        assert_eq!(id("team-a", "checkout").holder(), "team-a/checkout");
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

    // --- claim-holder binding (attested wins; fail closed) -----------------

    #[test]
    fn claim_holder_disabled_uses_self_asserted() {
        assert_eq!(
            claim_holder(&CallerIdentity::Disabled, "pod-1"),
            Some("pod-1".to_string())
        );
    }

    #[test]
    fn claim_holder_attested_overrides_self_asserted() {
        // Attested identity is authoritative — the self-asserted holder is ignored,
        // so a tenant cannot bill the lease to another identity.
        assert_eq!(
            claim_holder(
                &CallerIdentity::Attested("team-a/checkout".into()),
                "victim"
            ),
            Some("team-a/checkout".to_string())
        );
    }

    #[test]
    fn claim_holder_unresolved_rejects() {
        assert_eq!(claim_holder(&CallerIdentity::Unresolved, "pod-1"), None);
    }

    // --- holder check (verify ops) -----------------------------------------

    #[test]
    fn holder_check_disabled_is_unconstrained() {
        assert_eq!(
            holder_check(&CallerIdentity::Disabled),
            HolderCheck::Unconstrained
        );
    }

    #[test]
    fn holder_check_attested_must_match() {
        assert_eq!(
            holder_check(&CallerIdentity::Attested("team-a/checkout".into())),
            HolderCheck::MustMatch("team-a/checkout".to_string())
        );
    }

    #[test]
    fn holder_check_unresolved_rejects() {
        assert_eq!(
            holder_check(&CallerIdentity::Unresolved),
            HolderCheck::Reject
        );
    }

    #[test]
    fn is_attested_only_for_attested_variant() {
        assert!(CallerIdentity::Attested("x".into()).is_attested());
        assert!(!CallerIdentity::Disabled.is_attested());
        assert!(!CallerIdentity::Unresolved.is_attested());
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
        assert_eq!(
            cache.get_at(&ip, t0 + Duration::from_secs(9)),
            Some(id("team-a", "checkout"))
        );
        assert_eq!(cache.get_at(&ip, t0 + Duration::from_secs(10)), None);
        assert_eq!(cache.get_at(&ip, t0 + Duration::from_secs(11)), None);
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
