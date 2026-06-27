# agentctl — design vision

> The control plane for fleets of **agentd** instances on Kubernetes: a CLI, a
> `kubectl agent[s]` plugin, a Kubernetes operator, and an on-node bridge that
> provisions, supplies intelligence, scales, observes, and manages agentd —
> talking to each pod over **vsock**.

This document is the north-star for what agentctl is and how it is built. It is
deliberately opinionated; details will be RFC'd as we go.

---

## 1. Thesis: agentctl owns policy, agentd owns primitives

`agentd` (the sibling repo) is the **data plane** — a single static ~1.1 MB Rust
binary that runs the supervised agentic loop, MCP-native, minimal (3 deps, no
async runtime, no HTTP server, no Kubernetes coupling). It deliberately exposes
**primitives** over its existing transports and refuses to learn about clusters.

`agentctl` is the **control plane** — everything Kubernetes-shaped. The split is
the whole architecture, and it is already specified from agentd's side in the
**agentctl control-plane RFC track, agentd RFCs 0014–0020**. agentctl is the
*consumer* of those contracts:

| agentd contract (RFC) | What agentctl does with it |
|---|---|
| **0014** the umbrella + the `surfaces{}` discovery + the env convention | the boundary; reconcile only what a pod advertises |
| **0015** capabilities manifest + `--serve-mcp vsock` + operator tools (`drain`/`lame-duck`/`cancel`/`inventory`) | the backend for `kubectl agent describe/tree/drain` |
| **0016** frozen metrics schema + exit-code contract + `agentd://events` | dashboards, alerts, `podFailurePolicy`, `kubectl agents top/results` |
| **0017** declarative config + `--validate-config` + hot reload | ConfigMap-driven, admission-validated, restart-free reconfig |
| **0018** multi-endpoint intelligence + health + hot-swap | wire the vsock model service; survive its moves |
| **0019** work-claim/lease + `--shard K/N` + autoscaling signals | KEDA/HPA-driven horizontal scale |
| **0020** A2A over vsock (manifest = Agent Card; run = Task) | the **A2A gateway** + the agent mesh |

**The rule we never break:** no Kubernetes/CRD/dashboard logic ever leaks into
agentd. If agentctl wants something, it asks for a *primitive* in an agentd RFC;
the cluster-facing translation lives here.

---

## 2. Architecture

```
        ┌──────────────────── users / clients ───────────────────────────┐
        │  kubectl agent[s] …   │   agentctl CLI   │   A2A clients (mesh)  │
        └──────────┬─────────────────────┬──────────────────────┬─────────┘
                   │ kube-apiserver       │ (same gRPC/REST)     │ A2A/HTTPS
                   ▼                      ▼                      ▼
        ┌───────────────────────  agentctl control plane  ───────────────────┐
        │  Operator (Deployment, leader-elected)   CRDs: Agent / AgentFleet   │
        │    reconcile → Pods/Deployments/Jobs/StatefulSets/HPA               │
        └───────────────────────────────┬────────────────────────────────────┘
                                         │ owns workloads
                                         ▼
        ┌──────────────  node-agent (DaemonSet, one per node)  ──────────────┐
        │  • management bridge: kube-apiserver/CLI ⇄ agentd self-MCP over vsock│
        │  • A2A gateway:  HTTP/SSE/auth/webhooks ⇄ agentd A2A over vsock      │
        │  • telemetry:    scrape /metrics + agentd://events over vsock        │
        └───────────────────────────────┬────────────────────────────────────┘
                                vsock (point-to-point guest↔host)
                                         ▼
        ┌──────────────────────  agentd pods (data plane)  ──────────────────┐
        │   NO cluster network — vsock-out for intelligence, vsock-in for     │
        │   management + A2A. Static binary on scratch. Nothing to attack.    │
        └─────────────────────────────────────────────────────────────────────┘
```

The load-bearing idea is **vsock-everything**: an agentd pod can run with *no
cluster network at all*. The on-node DaemonSet is the only thing on the network;
it is the management bridge, the A2A gateway, and the telemetry collector. This
is the strongest isolation posture in the ecosystem and the thing that makes
agentctl distinctive.

---

## 3. Components

### 3.1 The operator (CRDs + reconcile)

Two custom resources, reconciled by a leader-elected Deployment:

- **`Agent`** — one logical agent: its instruction/config, intelligence binding,
  MCP servers (+ trifecta tags), mode (`once`/`loop`/`reactive`/`schedule`),
  limits, and the surfaces it should expose (management, metrics, a2a). The
  operator renders it to a Pod/Job/CronJob/Deployment depending on mode, mounts
  the config (ConfigMap), injects the **downward-API env** agentd expects
  (`AGENTD_POD_NAME/UID/NAMESPACE`, `AGENTD_NODE_NAME`, `AGENTD_POD_GRACE_SECONDS`,
  `AGENTD_SHARD` — RFC 0014 §6.4), and wires vsock for intelligence + serving.
- **`AgentFleet`** — a replicated, autoscaled set of reactive `Agent`s: replica
  bounds, the shared work source (a subscribed MCP resource), the claim/lease +
  `--shard K/N` policy (RFC 0019), and the autoscaling target (a metric from RFC
  0016). Renders to a StatefulSet (stable shard identity) + a KEDA `ScaledObject`.

Reconcile is **manifest-driven**: the operator never assumes a pod's
capabilities — it reads `agentd --capabilities` (RFC 0015) at admission and only
drives the `surfaces{}` a build actually advertises (graceful degradation). The
`Agent.status` mirrors the live manifest + inventory + health.

**Admission**: a validating webhook runs `agentd --validate-config` (RFC 0017)
against the rendered config and rejects a bad CR *before* it ever schedules.

### 3.2 The CLI + `kubectl agent[s]` plugin

One Go binary, two faces. As `kubectl-agent` it installs as a kubectl plugin so
`kubectl agent …` / `kubectl agents …` Just Work; as `agentctl` it is standalone.
Verbs map to agentd's operator surface (RFC 0015), proxied through the node-agent:

- `kubectl agents get [-o wide]` — list, columns from the manifest (model,
  build_features, surfaces, ready, in-flight).
- `kubectl agent <x> describe` — the manifest + inventory + health.
- `kubectl agent <x> tree` — render `agentd://inventory` (the live subagent tree).
- `kubectl agent <x> logs -f` — stream `agentd://events` (RFC 0016).
- `kubectl agent <x> top` — token/cost + saturation from the metrics (RFC 0016).
- `kubectl agent <x> drain | lame-duck | cancel <handle>` — the operator tools.
- `kubectl agent <x> attach` — interactive steering = `subagent.send` into a warm
  session (the genuinely novel UX: a human or an agent steering a live agent).
- `kubectl agents results` — run-outcome reports for `once`/Job agents.

### 3.3 The node-agent (DaemonSet)

The keystone. One per node, host-side, vsock-adjacent to every local agentd pod.
Three roles, all *thin*:

1. **Management bridge** — proxies CLI/operator calls to a pod's self-MCP over
   vsock (the `Management` `PeerOrigin`, RFC 0015 §3.4). It is the only thing that
   needs the per-pod vsock CID; it exposes a small authenticated API the operator
   and the `kubectl agent` plugin call (via the kube-apiserver proxy).
2. **A2A gateway** (see §3.4).
3. **Telemetry collector** — scrapes each pod's metrics + tails `agentd://events`
   over vsock and re-exposes them on the network for Prometheus/the operator
   (so the pods stay network-isolated).

### 3.4 The A2A gateway (inside the node-agent)

Because agentd serves **real A2A over vsock** (RFC 0020), the gateway is a **dumb
transport bridge**, not a protocol translator:

- Serves the public A2A surface on the network (HTTP/JSON-RPC + SSE + webhooks +
  OAuth/OIDC/mTLS) — all the heavy A2A machinery agentd deliberately lacks.
- **Projects the Agent Card** from agentd's capabilities manifest (`surfaces.a2a`,
  RFC 0015/0020) at `/.well-known/agent.json`.
- Bridges A2A JSON-RPC frames HTTP/SSE ⇄ **vsock** to the pod, emitting the exact
  `a2a.SendMessage` / `a2a.GetTask` / `a2a.CancelTask` / `a2a.ListTasks` method
  names agentd implements. Streaming: agentd writes a stream of same-`id`
  `StreamResponse` frames over vsock until `statusUpdate.final == true`; the
  gateway re-frames each to an SSE event and closes on final.
- Is the **policy-enforcement point**: it authenticates the client, enforces
  authz/tenant/rate limits, holds the **webhook registry** and the **durable task
  history** (`ListTasks`) — agentd serves only *live* tasks. A trusted forwarded
  request crosses vsock with no in-band auth (the transport is the boundary).

Symmetry worth stating: agentd dials A2A *out* (delegation, RFC 0020 §3) over
vsock to the gateway too — the gateway forwards into the mesh. Inbound and
outbound A2A share the vsock-behind-a-gateway shape, exactly like intelligence
behind a sidecar.

---

## 4. Key flows

- **Provision.** Apply an `Agent` CR → admission validates the config → operator
  renders the workload, mounts config, injects downward-API env + vsock → pod
  comes up → node-agent reads the manifest → `Agent.status` reflects it.
- **Scale (reactive fleet).** `AgentFleet` → StatefulSet (shards) + KEDA
  `ScaledObject` on `agentd_pending_events` (RFC 0016/0019). Replicas claim work
  via the lease (no double-processing); scale-down sends `drain` so in-flight work
  bleeds off and held claims release.
- **Rolling update / node drain.** `kubectl agent <x> lame-duck` → `/readyz` 503,
  in-flight bleeds off (watch `agentd://inventory`) → `drain` → clean exit 0 (not
  143) → the operator rolls the next pod. No `kill`, honest dashboards.
- **Observe.** `kubectl agent <x> tree`/`logs`/`top` read inventory/events/metrics
  over vsock; the operator aggregates fleet-wide views (agentd never does).
- **Mesh.** An external A2A client (any vendor) discovers a fleet's Agent Card,
  `SendMessage`s a Task → gateway → vsock → agentd run → distillate streamed back.
  An agentd agent delegates *out* to a peer via `--a2a-peer` → gateway → mesh.

---

## 5. Recommended stack

- **Language: Go** for the operator, CLI/kubectl-plugin, node-agent, and A2A
  gateway — it is the native ecosystem (controller-runtime/kubebuilder, client-go,
  the kubectl-plugin and KEDA-scaler SDKs, mature vsock + HTTP/SSE libs). agentd
  stays Rust (minimalism); agentctl optimizes for cluster-ecosystem fit, not
  binary size. *(Alternative: `kube-rs` in Rust for symmetry — viable, more
  friction; revisit only if a single-language story becomes a hard requirement.)*
- **Layout (monorepo):** `cmd/agentctl`, `cmd/kubectl-agent`, `cmd/node-agent`,
  `operator/` (api/v1 CRDs + controllers), `gateway/` (A2A bridge),
  `pkg/agentd/` (the typed client for the manifest/operator-tools/A2A vsock wire —
  generated from the agentd RFC schemas so the two repos can't drift), `deploy/`
  (Helm/Kustomize: CRDs, operator, DaemonSet, RBAC).
- **The contract is generated, not hand-copied.** `pkg/agentd/` is the single typed
  client for: the capabilities manifest (RFC 0015 §5.2), the operator tool/resource
  set, the metrics schema (RFC 0016), the config schema (`agentd --config-schema`,
  RFC 0017), and the A2A wire (the `a2a.*` methods + Task/Message/Part). Pin it to a
  contract version and negotiate (RFC 0014 §6.3).

---

## 6. Build roadmap

1. **MVP — manage what exists.** `pkg/agentd` vsock client for the **management**
   surface (RFC 0015) → the node-agent management bridge → `kubectl agent
   get/describe/tree/logs/drain/lame-duck/cancel`. The `Agent` CRD + operator
   rendering `once`/`reactive` pods with vsock + downward-API env. This is usable
   on day one against the shipped agentd v2.2.0.
2. **Observe.** Telemetry collector + dashboards/alerts off the frozen metrics
   schema (RFC 0016); `top`/`results`.
3. **Config & reconfigure.** ConfigMap-driven config + the admission webhook
   (`--validate-config`) + hot reload (RFC 0017).
4. **Scale.** `AgentFleet` + the claim/lease + shard + KEDA scaler (RFC 0019).
5. **A2A gateway.** The HTTP↔vsock A2A bridge + Agent Card + auth/webhooks (RFC
   0020) — agentd's A2A surface (server + streaming + client) already shipped, so
   this is gateway-only work.
6. **Intelligence ops.** Multi-endpoint/health/hot-swap wiring (RFC 0018).

---

## 7. Open questions (to RFC on the agentctl side)

- **vsock CID allocation** per pod on a node (the node-agent's scheme; agentd just
  uses what it's given — RFC 0015 open item).
- **node-agent ↔ operator API**: do CLI calls go operator → node-agent, or
  kube-apiserver-proxy → node-agent directly? (Latency vs RBAC surface.)
- **Multi-cluster / fleet federation** for the A2A mesh — out of v1.
- **Durable task store** location for the gateway (`ListTasks`/webhook history):
  per-node vs a shared backing store.
- **Agent identity in the mesh** — how an `AgentFleet`'s Agent Card is published
  and discovered cluster-wide and cross-org.

---

*See the agentd repo's `rfcs/0014`–`0020` for the binding contracts this design
consumes. Where this doc and an agentd RFC disagree on the wire, the RFC wins and
this doc is corrected.*
