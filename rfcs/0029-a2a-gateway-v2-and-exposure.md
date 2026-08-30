# RFC 0029 — A2A gateway v2: unified endpoint, principal routing, and exposure

- **Status:** Proposed
- **Date:** 2026-08-30
- **Supersedes:** RFC 0013 · **Decisions:** ADR-0002, ADR-0004
- **Depends on:** RFC 0028 (principals), 0033 (Agent.spec.expose)

## 1. Motivation

One address for the whole estate (req. 8, 22): every user reaches every agent
they're allowed to — supervisors by default — through a single authenticated A2A
endpoint; external systems reach agents' webhook listeners through a controlled
host (req. 15); agents converse across boundaries only through the same door
(req. 5). agentd's principal model (roles, labels, quotas, addressed gates)
works *if* someone hands each agent a real per-user principal — that is the
gateway's defining job.

## 2. Addressing and routing

```
https://a2a.<domain>/orgs/<org>/agents/<name>          # any agent
https://a2a.<domain>/orgs/<org>/supervisor             # caller's own supervisor
https://a2a.<domain>/orgs/<org>/fleets/<fleet>         # fleet A2A endpoint (dispatcher or fan-in per 0034)
   └─ /.well-known/agent-card.json under each route
https://hooks.<domain>/<org>/<agent>/<path>            # webhook exposure (§5)
```

`<name>` in a route is the agent's **org-unique handle** (`Agent.spec.handle`,
defaulting to the CR name) — the same `@handle` users type in supervisor chat
(RFC 0027 §7.1) — so a card URL and a mention are the same identifier.

Routing table = watched `Agent`/`Supervisor`/`AgentFleet` CRs (`.status.a2a`
endpoints). The gateway is stateless (HA replicas); SSE streams pin to a replica
with resume-cursor failover (A2A `fromSeq`/`Last-Event-Id` semantics preserved
end-to-end).

## 3. Authentication and principal injection

Inbound: OIDC bearer (validated via identity: JWKS/introspection) or a gateway
API key for service callers (minted per org). Authorization: the org's resolved
**accessPolicies** (IdP claims/groups/scopes → roles over label selectors,
RFC 0033 §Organization) decide *which agents and which verb classes* —
conversation, task reads, gate answers, admin verbs (admin verbs additionally
require an operator/admin role). "Engineering reaches engineering-labeled
agents; admins reach all" is enforced here for every conversation.

Upstream: mTLS (gateway client cert per org SAN) **plus the per-(user, agent)
principal bearer** from identity (RFC 0028 §6). agentd therefore sees
`principal = user:<subject>` with `labels {org, group, user}` and quotas —
giving natively: attribution (`acting_for` into every tool `_meta`), per-user
rate/budget quotas, and **addressed gates** (`to: {id: "user:…"}`) that only that
human can answer — with operator override recorded as such by agentd.

The loopback-operator trap is closed structurally: every agentd's listener gets
`principals` declared (disabling anonymous-loopback-as-operator), `interface.
origins` set to the gateway/UI origins, and NetworkPolicies admit only the
gateway (+ declared peers) to the A2A port.

## 4. What flows through

- `SendMessage`/`SendStreamingMessage` (chat + typed command DataParts),
  `GetTask`/`ListTasks`/`CancelTask`, `SubscribeToTask`, `SubscribeToEvents`
  (interface feed for CLI/UI), gate answers (message-to-task), agent cards
  (public + extended-per-principal).
- **Admin verbs** (`drain/pause/resume/cancel`) — gateway-authorized (operator
  role) and also used by the operator itself for lifecycle (its own Management
  principal, direct in-cluster path).
- **East–west**: same-org agent↔agent goes direct (operator-wired peers, mTLS) —
  not through the gateway; cross-org/cluster peers are gateway routes with
  explicit `MCPService`-style peer registry entries and org-admin approval.

## 5. Webhook and signal exposure (req. 15)

`Agent.spec.expose.webhooks[]` (validated against the agent's `webhook` start
routes) publishes `hooks.<domain>/<org>/<agent>/<path>` → agentd's webhook
listener. The gateway adds: TLS termination, per-route tenant rate limits and IP
allowlists, size caps, and audit; **agentd's own per-route auth (HMAC/bearer)
remains the authenticator** — the gateway never strips or replaces it (defense
in depth; agentd exits 2 on unauthenticated public routes anyway). Secrets:
operator provisions the HMAC into both the agent config (`{{secret:}}`) and the
caller-facing display (CLI `agentctl expose webhook … --show-secret`).
One-shot callback URLs (agentd `wait {on: webhook}`) are exposable under a
TTL'd path family. A2A `workflow.signal` remains available through the normal
A2A route for signal-style pokes.

Implementation: the gateway proxies these routes itself in v2.0 (one data plane,
one audit); emitting Gateway-API `HTTPRoute`s instead is a supported
alternative profile for clusters with an existing edge (values flag), with the
tenant limits then delegated to that edge.

## 6. Multi-client UX

The thin agentd web UI and TUI keep working — pointed at the gateway route with
the user's OIDC token; `interface.origins` on each agent includes the hosted-UI
origin (chart-provisioned). CLI `agentctl chat/inbox` are gateway A2A clients.

## 7. Operational notes

- A2A TLS on agentd does **not** hot-rotate (upstream ask U2): cert renewals
  trigger rolling restarts via operator; the gateway tolerates upstream restarts
  by task-level resume (unary `GetTask` recovery like agentd's own client).
- Per-org isolation: routing cache, quota buckets, and audit streams are
  org-keyed; a melting org cannot starve another (bounded per-org inflight).
- The gateway holds **no message bodies at rest**; the task store of RFC 0013 is
  retired — agentd owns tasks; the gateway only proxies + audits envelopes.

## 8. Alternatives considered

Envoy/existing API gateway with an authz filter (loses A2A-level semantics:
principal minting, gate routing, card shaping; adds a non-Rust data plane);
gateway-terminated tasks store (v1 design — duplicated agentd's task state and
drifted). 

## 9. Open questions

1. Streaming fan-in for `fleets/<fleet>` routes (merge worker events vs
   dispatcher-only view) — v2.0 ships dispatcher-only.
2. Per-message policy hooks at the gateway (DLP on outbound artifacts?) —
  deferred; the capability plane is the governed choke point in v2.0.
