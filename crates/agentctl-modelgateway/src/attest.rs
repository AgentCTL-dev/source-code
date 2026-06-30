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
//! A conformant agent is networkless and cannot itself reach the gateway: its
//! request is carried by the on-node bridge (the **node-agent**), which has
//! already SO_PEERCRED-attested the real caller. Attested mode therefore treats
//! the node-agent as a **trusted forwarder**: when the source IP resolves to a
//! node-agent pod, the real caller's identity is taken from the node-agent-asserted
//! `X-Agent-Pod-Uid` (resolved to the owning pod) rather than from the source IP —
//! which is the node-agent's, not the agent's. Only the node-agent is trusted this
//! way: a direct agent pod that sets `X-Agent-Pod-Uid` is ignored (it cannot
//! impersonate another tenant), and its own source IP remains authoritative.
//!
//! Under **hostile multi-tenancy** the forwarder anchor must be **unforgeable**.
//! A self-settable label (`app.kubernetes.io/name`) is *not* enough — a tenant
//! could paint that label onto its own pod and impersonate the forwarder. The
//! anchor is therefore pinned to two things a tenant **cannot** forge: the pod
//! lives in the **control-plane namespace** (a tenant cannot create pods there)
//! AND runs as the **`agentctl-node-agent` ServiceAccount** (a tenant cannot run
//! as that SA). The label check is kept as defense-in-depth; all three must hold.
//!
//! This module holds the **pure** logic — env gate, pod→identity derivation,
//! IP/UID→pod matching, node-agent detection, the source-pod classification + its
//! decision, the TTL cache, and the attested-vs-header reconciliation — so it is
//! unit-testable without a cluster. The kube lookups live in `main.rs` as I/O
//! glue.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;

/// The operator-set label carrying an agent's name on its pod.
pub const AGENT_LABEL: &str = "agentctl.dev/agent";

/// The standard `app.kubernetes.io/name` label key.
pub const APP_NAME_LABEL: &str = "app.kubernetes.io/name";

/// The `app.kubernetes.io/name` value worn by the on-node bridge (node-agent)
/// pods. Kept as **defense-in-depth** in the forwarder check — it is necessary but
/// NOT sufficient on its own, because a tenant can set this label on its own pod.
pub const NODE_AGENT_APP_NAME: &str = "agentctl-node-agent";

/// The ServiceAccount the on-node bridge (node-agent) pods run as. Together with
/// the **control-plane namespace**, this is the **unforgeable** anchor of the
/// trusted forwarder (RFC 0015): a tenant cannot create a pod in the control-plane
/// namespace, nor run a pod as this ServiceAccount, so it cannot impersonate the
/// node-agent and forward another tenant's identity in `X-Agent-Pod-Uid`.
pub const NODE_AGENT_SA: &str = "agentctl-node-agent";

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

/// Whether a pod is the **trusted agentctl node-agent forwarder** (the on-node
/// bridge). The node-agent is the only pod trusted to forward another agent's
/// identity (via the `X-Agent-Pod-Uid` header) to the gateway; every other pod is
/// a direct caller whose identity is its own source IP.
///
/// Under hostile multi-tenancy the anchor must be **unforgeable** by a tenant, so
/// ALL of the following must hold:
///
/// 1. the pod lives in the **control-plane namespace** (`control_plane_ns`) — a
///    tenant cannot create a pod there;
/// 2. the pod runs as the **`agentctl-node-agent` ServiceAccount**
///    (`spec.serviceAccountName`) — a tenant cannot run a pod as that SA;
/// 3. the pod still wears the `app.kubernetes.io/name == "agentctl-node-agent"`
///    label — kept as **defense-in-depth** (a tenant *can* set this label, so it
///    is necessary but not sufficient on its own).
///
/// **Fail closed:** an empty `control_plane_ns` (the ModelGateway's own namespace
/// is unknown — `POD_NAMESPACE` unset/empty) trusts **NO** forwarder and returns
/// `false`, so a weakly-anchored forwarder is never trusted.
pub fn is_node_agent_pod(pod: &Pod, control_plane_ns: &str) -> bool {
    // Fail closed: without a known control-plane namespace the anchor cannot be
    // verified, so trust no forwarder.
    if control_plane_ns.is_empty() {
        return false;
    }
    let in_control_plane_ns = pod
        .metadata
        .namespace
        .as_deref()
        .is_some_and(|ns| ns == control_plane_ns);
    let runs_as_node_agent_sa = pod
        .spec
        .as_ref()
        .and_then(|s| s.service_account_name.as_deref())
        .is_some_and(|sa| sa == NODE_AGENT_SA);
    let wears_node_agent_label = pod
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(APP_NAME_LABEL))
        .is_some_and(|v| v == NODE_AGENT_APP_NAME);
    in_control_plane_ns && runs_as_node_agent_sa && wears_node_agent_label
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

/// Whether a pod's `metadata.uid` equals `uid`. Used to resolve the
/// node-agent-asserted `X-Agent-Pod-Uid` back to the real agent pod — the uid is
/// not a kube field selector, so the match is performed locally over a pod list.
pub fn pod_matches_uid(pod: &Pod, uid: &str) -> bool {
    pod.metadata.uid.as_deref() == Some(uid)
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

/// What the kernel-set source IP of a request resolved to, in attested mode.
///
/// The source IP is unforgeable by the agent, so this classification is the root
/// of trust: a request either comes straight from an agent pod (its own
/// identity), from the trusted node-agent forwarder (the real caller's identity
/// asserted out of band in `X-Agent-Pod-Uid`), or from an IP that owns no
/// attestable pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourcePod {
    /// No pod owns the source IP (or it has no namespace) — cannot attest.
    Unresolved,
    /// A direct agent pod with an attestable identity — use it (source-IP attested).
    Direct(Identity),
    /// The trusted node-agent forwarder. The real caller's identity is NOT this
    /// pod's; it is asserted in `X-Agent-Pod-Uid` and resolved separately.
    Forwarder,
}

/// The policy decision for an attested request, given the classified source pod,
/// the (already performed) forwarder UID lookup, and the header namespace. Pure,
/// so the whole attested-mode policy — *attest, forward, flag a spoof, or reject*
/// — is unit-testable without a cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Identity attested from the caller's own source IP — use this
    /// `(namespace, agent)`. `spoofed` is `true` when a header namespace
    /// disagreed (bump the spoof counter + warn).
    Use { identity: Identity, spoofed: bool },
    /// Identity attested **via the trusted node-agent forwarder** — the real
    /// caller's `(namespace, agent)`, resolved from the forwarder-asserted
    /// `X-Agent-Pod-Uid`. Bump the forwarded counter.
    Forwarded { identity: Identity },
    /// Reject (`403`): the source IP owns no pod, or the trusted forwarder did
    /// not assert a resolvable caller. In attested mode we never fall back to the
    /// spoofable header.
    Reject,
}

/// Decide a request's attested identity from the classified source pod, the
/// (optional) forwarder-resolved identity, and the header namespace.
///
/// - [`SourcePod::Unresolved`] ⇒ [`Decision::Reject`].
/// - [`SourcePod::Direct`] ⇒ [`Decision::Use`] with the pod's own source-IP
///   identity and a `spoofed` flag from [`reconcile`]. `forwarded` is **ignored**
///   here: only the node-agent is a trusted forwarder, so a direct pod can never
///   substitute another identity via `X-Agent-Pod-Uid`.
/// - [`SourcePod::Forwarder`] ⇒ [`Decision::Forwarded`] with `forwarded` when it
///   resolved to a pod, else [`Decision::Reject`] (the forwarder must attest a
///   resolvable caller).
pub fn decide(source: SourcePod, forwarded: Option<Identity>, header_ns: Option<&str>) -> Decision {
    match source {
        SourcePod::Unresolved => Decision::Reject,
        SourcePod::Direct(identity) => {
            let spoofed = reconcile(&identity.namespace, header_ns).spoofed;
            Decision::Use { identity, spoofed }
        }
        SourcePod::Forwarder => match forwarded {
            Some(identity) => Decision::Forwarded { identity },
            None => Decision::Reject,
        },
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
    use k8s_openapi::api::core::v1::{PodIP, PodSpec, PodStatus};
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;

    /// A representative control-plane namespace for the forwarder-anchor tests.
    const CP_NS: &str = "agentctl-system";

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

    /// A pod for node-agent (forwarder) detection: an optional namespace, an
    /// optional `app.kubernetes.io/name` label, and an optional
    /// `spec.serviceAccountName`.
    fn node_agent_pod(ns: Option<&str>, app: Option<&str>, sa: Option<&str>) -> Pod {
        let mut pod = Pod::default();
        pod.metadata.namespace = ns.map(str::to_string);
        if let Some(a) = app {
            let mut labels = BTreeMap::new();
            labels.insert(APP_NAME_LABEL.to_string(), a.to_string());
            pod.metadata.labels = Some(labels);
        }
        if let Some(sa) = sa {
            pod.spec = Some(PodSpec {
                service_account_name: Some(sa.to_string()),
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

    // --- node-agent (trusted forwarder) detection: unforgeable anchor -------

    #[test]
    fn is_node_agent_pod_true_only_with_ns_sa_and_label() {
        // The genuine forwarder: control-plane namespace + agentctl-node-agent SA
        // + app-name label. All three hold → trusted.
        let pod = node_agent_pod(Some(CP_NS), Some(NODE_AGENT_APP_NAME), Some(NODE_AGENT_SA));
        assert!(is_node_agent_pod(&pod, CP_NS));
    }

    #[test]
    fn is_node_agent_pod_false_for_label_alone_in_tenant_ns_spoof() {
        // The SPOOF case: a tenant paints app.kubernetes.io/name=agentctl-node-agent
        // on its OWN pod (and even names its SA the same) in its OWN namespace. It
        // is NOT in the control-plane namespace, so it is NOT trusted as forwarder.
        let pod = node_agent_pod(
            Some("tenant-a"),
            Some(NODE_AGENT_APP_NAME),
            Some(NODE_AGENT_SA),
        );
        assert!(!is_node_agent_pod(&pod, CP_NS));
    }

    #[test]
    fn is_node_agent_pod_false_for_label_only_pod_in_tenant_ns() {
        // A label-only tenant pod (wrong namespace, no matching SA) is NOT a
        // forwarder — the spoofable label cannot stand on its own.
        let labelled = node_agent_pod(Some("tenant-a"), Some(NODE_AGENT_APP_NAME), None);
        assert!(!is_node_agent_pod(&labelled, CP_NS));
        let with_tenant_sa = node_agent_pod(
            Some("tenant-a"),
            Some(NODE_AGENT_APP_NAME),
            Some("tenant-sa"),
        );
        assert!(!is_node_agent_pod(&with_tenant_sa, CP_NS));
    }

    #[test]
    fn is_node_agent_pod_false_for_right_ns_but_tenant_sa() {
        // Right namespace + label, but a tenant ServiceAccount → not the forwarder.
        let pod = node_agent_pod(Some(CP_NS), Some(NODE_AGENT_APP_NAME), Some("tenant-sa"));
        assert!(!is_node_agent_pod(&pod, CP_NS));
        // ...and with no SA set at all.
        let no_sa = node_agent_pod(Some(CP_NS), Some(NODE_AGENT_APP_NAME), None);
        assert!(!is_node_agent_pod(&no_sa, CP_NS));
    }

    #[test]
    fn is_node_agent_pod_false_for_right_ns_sa_but_wrong_or_no_label() {
        // Defense-in-depth: the app-name label must still be present (right ns + SA
        // is not enough on its own).
        let no_label = node_agent_pod(Some(CP_NS), None, Some(NODE_AGENT_SA));
        assert!(!is_node_agent_pod(&no_label, CP_NS));
        let wrong_label = node_agent_pod(Some(CP_NS), Some("agent"), Some(NODE_AGENT_SA));
        assert!(!is_node_agent_pod(&wrong_label, CP_NS));
    }

    #[test]
    fn is_node_agent_pod_false_when_control_plane_ns_empty_fail_closed() {
        // Fail closed: POD_NAMESPACE unknown (empty) trusts NO forwarder, not even
        // a genuine node-agent pod.
        let genuine = node_agent_pod(Some(CP_NS), Some(NODE_AGENT_APP_NAME), Some(NODE_AGENT_SA));
        assert!(!is_node_agent_pod(&genuine, ""));
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

    // --- uid matching → identity (forwarder uid resolution) ----------------

    #[test]
    fn pod_matches_uid_compares_metadata_uid() {
        let mut pod = pod_with(Some("team-a"), Some("p1"), Some("checkout"), None);
        pod.metadata.uid = Some("uid-123".to_string());
        assert!(pod_matches_uid(&pod, "uid-123"));
        assert!(!pod_matches_uid(&pod, "uid-999"));
    }

    #[test]
    fn pod_without_uid_matches_nothing() {
        let pod = pod_with(Some("team-a"), Some("p1"), None, None);
        assert!(!pod_matches_uid(&pod, "uid-123"));
    }

    #[test]
    fn uid_match_then_identity_derivation() {
        // The forwarder asserts a uid; the matched pod's namespace + agent label
        // become the attested identity (uid → identity mapping).
        let mut pod = pod_with(
            Some("team-a"),
            Some("agent-xyz-123"),
            Some("checkout"),
            None,
        );
        pod.metadata.uid = Some("uid-123".to_string());
        assert!(pod_matches_uid(&pod, "uid-123"));
        assert_eq!(identity_from_pod(&pod), Some(id("team-a", "checkout")));
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

    // --- decision policy (attest / forward / spoof / reject) ---------------

    #[test]
    fn decide_no_pod_rejects() {
        assert_eq!(decide(SourcePod::Unresolved, None, None), Decision::Reject);
        // Even a header that names a namespace cannot rescue an unattestable IP.
        assert_eq!(
            decide(SourcePod::Unresolved, None, Some("team-a")),
            Decision::Reject
        );
    }

    #[test]
    fn decide_direct_pod_attests_without_header() {
        assert_eq!(
            decide(SourcePod::Direct(id("team-a", "checkout")), None, None),
            Decision::Use {
                identity: id("team-a", "checkout"),
                spoofed: false,
            }
        );
    }

    #[test]
    fn decide_matching_header_is_not_a_spoof() {
        assert_eq!(
            decide(
                SourcePod::Direct(id("team-a", "checkout")),
                None,
                Some("team-a")
            ),
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
            decide(
                SourcePod::Direct(id("team-a", "checkout")),
                None,
                Some("team-b")
            ),
            Decision::Use {
                identity: id("team-a", "checkout"),
                spoofed: true,
            }
        );
    }

    // --- forwarder trust (node-agent) + anti-spoof -------------------------

    #[test]
    fn decide_node_agent_with_valid_uid_uses_forwarded_identity() {
        // Source is the node-agent; the forwarder-resolved identity is used.
        assert_eq!(
            decide(SourcePod::Forwarder, Some(id("team-a", "checkout")), None,),
            Decision::Forwarded {
                identity: id("team-a", "checkout"),
            }
        );
    }

    #[test]
    fn decide_node_agent_without_uid_rejects() {
        // Node-agent source but X-Agent-Pod-Uid missing/unresolvable (forwarded
        // is None) → reject: the trusted forwarder MUST attest a real caller.
        assert_eq!(decide(SourcePod::Forwarder, None, None), Decision::Reject);
        // A header namespace cannot rescue a forwarder that asserted no caller.
        assert_eq!(
            decide(SourcePod::Forwarder, None, Some("team-a")),
            Decision::Reject
        );
    }

    #[test]
    fn decide_direct_pod_ignores_forwarded_identity_anti_spoof() {
        // Anti-spoof: a NON-node-agent (direct) pod's own source-IP identity is
        // used even if a forwarded identity is supplied — a random pod cannot
        // substitute another tenant via X-Agent-Pod-Uid. (The glue never even
        // resolves the header for a direct source; here we prove the pure policy
        // discards it too.)
        assert_eq!(
            decide(
                SourcePod::Direct(id("team-a", "checkout")),
                Some(id("victim", "secret-agent")),
                None,
            ),
            Decision::Use {
                identity: id("team-a", "checkout"),
                spoofed: false,
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
