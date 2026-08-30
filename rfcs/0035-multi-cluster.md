# RFC 0035 — multi-cluster: hub and spokes

- **Status:** **Draft** (deliberately behind single-cluster GA; PLAN phase M)
- **Date:** 2026-08-30
- **Depends on:** everything in 0026–0034 landing single-cluster first

## 1. Motivation

Req. 9 + 34: fleets that span clusters **and clouds** — for residency,
blast-radius, capacity, per-tenant dedicated clusters (ADR-0010's
regulated-tenant profile), and the hybrid posture where one company runs
spokes on several providers (or several agentctl installations entirely). The
design must not distort the single-cluster architecture; it federates it.

## 2. Shape: hub and spokes

- **Hub** (one, HA): apiserver (tenancy + registry source of truth), identity
  (custody/exchange/JWKS — one trust domain), gateway (global A2A + hooks
  entry), observability aggregation.
- **Spoke** (per cluster): operator, admission, scaler, system mcpg
  (state/artifacts/work local to its fleets), tenant mcpg gateways for the orgs
  placed there, the agents. A spoke runs autonomously if the hub is unreachable
  (agents keep working; management/conversation degrade).
- **`Cluster` CR** (hub): registration, credentials (bootstrap token → spoke
  agent cert), capacity/labels, placement taints.

Sync: hub→spoke registry/CR replication (server-side apply through a spoke
agent — a small Rust syncer, not a third-party dependency), spoke→hub status.
Placement: `Organization.spec.placement` and `AgentFleet.spec.placement`
(cluster selectors); fleets may span spokes with per-spoke partitions (static/
workqueue strategies compose; a queue per spoke or a hub-reachable fabric).

## 3. Identity and networking across clusters

- One AAuth trust domain: identity (hub) enrolls agents from every spoke
  (projected SA tokens validated per-cluster); JWKS shared; per-(user, agent)
  principal minting unchanged.
- A2A: hub gateway routes to spoke gateways over mTLS (gateway federation,
  RFC 0029 §4); east–west across spokes is always gateway↔gateway (no flat
  network assumption).
- Webhooks: `hooks.<domain>` at the hub; spoke-local ingress optional per org.

## 4. State, migration, DR

- State stays spoke-local (latency; the reactor's synchronous store I/O makes
  cross-cluster state a non-starter). Cross-cluster **migration = backup →
  restore** (RFC 0033 verbs) orchestrated by the hub: drain on source, snapshot
  to object store, restore on target, route flip at the gateway, retire source.
- DR: scheduled backups to hub-visible object storage + `Cluster` failover
  runbooks (restore fleets onto a standby spoke); RTO/RPO documented per store
  class.

## 5. Sharding across clusters

`AgentFleet` static partitions gain a cluster dimension (`partitions:
{cluster-a: [0,1], cluster-b: [2,3]}`); workqueue strategy needs a fabric
reachable from all member spokes (hub-adjacent fabric or per-spoke queues with
upstream partitioning) — the tradeoffs table lives here when this leaves Draft.

## 6. Supervisor federation (multi-cloud front door)

The @mention machinery (RFC 0027 §7.1) extends across installations: a user's
**front supervisor** holds *remote supervisors* — per-cloud spokes of the same
org, or entirely separate agentctl installations — as registered A2A peers
(`MCPService`-style peer entries with `kind: peer`, gateway↔gateway mTLS +
principal mapping). An estate question ("what's running, what's over budget,
ask @eu-cluster's supervisors") fans out exactly like a mention gather, with
the same typed `ask` command, the same per-peer timeouts, and the same
`args.hops` recursion ceiling — which matters doubly here, since hop depth
does not propagate across instances. That reset is not a gap to route around:
**agentd owns no cross-instance coordination at all, by design** (confirmed
upstream) — so any cap that must hold *across* instances (hops, federated
quotas, cross-installation fan-out) is the federation layer's to carry,
explicitly, in the contract the peers exchange. Federation is peer-registered
and org-admin-approved, never ambient; each side authorizes the other through
its own accessPolicies.

## 7. Explicitly deferred decisions

Virtual-cluster tenancy (vcluster) as an isolation tier; hub HA topology
(multi-region hub); cross-cluster fleet autoscaling; registry conflict rules
under split-brain; spoke-local identity caches for exchange during hub outage
(likely required — sketch: short-TTL delegated signing keys per spoke).

*This RFC intentionally stays Draft until P0–P6 are Done; it exists now so
single-cluster decisions (identity trust domain, gateway federation seams,
backup format) are made multi-cluster-compatible.*
