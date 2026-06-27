# agentctl RFC 0010: Observability & telemetry bridge

**Status:** Proposed (agentctl observability track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the telemetry plane that re-exposes a fleet of (often networkless) conformant agents to Prometheus / Loki / traces **without putting a single agent pod on the cluster network**, and aggregates fleet-wide what the agent never does

> **agentctl invents zero telemetry — it transports, relabels, aggregates, and
> authors Kubernetes policy against the frozen contract.** Every metric name,
> label key, event string, run-report field, exit code, and trace field this RFC
> consumes is **frozen by the contract** (the reference implementation's agentd RFC
> 0016 metrics/report/event schemas, agentd RFC 0010 log/health/trace, agentd RFC
> 0011 §5 exit codes). agentctl adds **no new telemetry mechanism** and **defines no
> new series**; it owns the *Kubernetes-facing* side — the scrape topology, the
> relabeling, the dashboards/alerts/recording rules, the run-outcome durability, the
> fleet rollups, and the control plane's own self-observability. Where this RFC
> names a concrete series it cites the **reference implementation** (agentd RFCs) as
> *where the contract is presently written down*, never as a dependency (P0).

> **The agent is never on the network, so nothing scrapes it directly — the
> node-agent (Tier A, agentctl RFC 0008) is the single networked telemetry bridge.**
> A conformant agent in the HARDENED or off-pod tier (agentctl RFC 0002) has no pod
> IP for Prometheus to dial and no shell for a log shipper to read. The telemetry
> bridge in node-agent Tier A scrapes each local agent's metrics and tails its event
> resource **over the discovered, attested socket** (agentctl RFC 0002 §3) and
> re-exposes them on the node-agent's own network endpoint. The agent serves *one
> instance's* telemetry; **fleet aggregation is exclusively agentctl's** (agentd RFC
> 0016 §7.3, §12 non-goals) — the agent never learns it is in a fleet, never rolls
> up, and never learns a price.

---

## 1. Problem / Context

The contract gives agentctl a complete, frozen, *self-contained* telemetry surface
per instance: a hand-written Prometheus 0.0.4 text exposition over a versioned
`metrics_schema` (agentd RFC 0016 §4), a closed JSON-lines log schema + a closed
27-name `event` vocabulary on stderr (agentd RFC 0010 §3.2/§3.3), a subscribable
`agentd://events` live-tail resource backed by a bounded lossy ring (agentd RFC 0016
§7), a machine-readable run-outcome report at `--report-file` / `agentd://run/{run_id}`
(agentd RFC 0016 §6), W3C trace-context propagated through every log line, MCP call,
LLM call, and spawn payload (agentd RFC 0010 §3.6), and a frozen exit-code table
(agentd RFC 0011 §5). All of it was designed for an operator reading **one**
instance.

A control plane reads a **fleet**, and three facts make that non-trivial in exactly
the way this RFC exists to resolve:

1. **The agent pod is not reachable.** Under the locked substrate decision (agentctl
   RFC 0002), the production multi-tenant tier (Kata-hybrid) and the portable
   off-pod variant run the agent **networkless** — no pod IP, no NIC, a `scratch`
   image with no shell. kubelet `httpGet`/`tcpSocket`/`grpc` probes cannot reach it,
   Prometheus cannot dial a `:9090`, and a sidecar log shipper has nothing to read.
   Anything that needs the agent's telemetry must cross the **discovered, attested
   socket** the node-agent already holds (agentctl RFC 0002 §3, RFC 0008) — the same
   seam every other plane uses to reach in. There is **no second path** on a
   networkless pod.

2. **The agent aggregates nothing — by contract.** agentd serves one instance's
   `/metrics`, one instance's event ring, one instance's run report (agentd RFC 0016
   §7.3: "fan-out is the subscriber's job"; agentd RFC 0014 §6 non-goals: no
   cross-instance aggregation, no fleet event bus, no long-term storage, no price
   table). A `kubectl agents top` over a 200-pod fleet, a per-tenant cost rollup, a
   fleet-wide refusal alert, a "show me every pod in this flow" trace view — **none
   of those exist until agentctl builds them.** The asymmetry is the architecture
   (agentd RFC 0014 §3): the agent exposes primitives; agentctl owns policy.

3. **Everything agentctl builds couples to an exact spelling.** A dashboard keys off
   `agentd_pending_events`, an alert off `agentd_refusals_total{reason="trifecta"}`,
   a `podFailurePolicy` off exit code `2`, a `kubectl agents results` off the run
   report's `status` field. The contract froze and versioned every one of these
   precisely so the control plane does not break silently on an agent upgrade (agentd
   RFC 0016 §1). agentctl's job is to **author against the frozen names and negotiate
   on the manifest** — never to hand-transcribe a name (the brainstorm §11.2 caught a
   real transcription bug: `agentd_reactive_backlog` does not exist in the frozen
   schema; see §5.4).

This RFC owns: the node-agent **telemetry bridge** (the metrics scrape-proxy + the
event tail + the run-outcome collector that live in Tier A); the **scrape topology**
(central service discovery, per-pod relabeling, the node-agent-as-target rule); the
**dashboards / alerts / recording rules** authored against the frozen schema; the
**events-vs-metrics-vs-logs** split (bulk logs ride container stderr, the ring is
live-tail only); **run-outcome capture** before a `once`/Job pod GCs; the
**exit-code → `podFailurePolicy`** *observability reading* (the render is agentctl
RFC 0006 §8.6); **trace correlation** across the gateway↔socket↔agent boundary;
**fleet aggregation** (rollups, cost, the `kubectl agents top/results` backends);
and the control plane's own **self-observability + SLOs**.

It does **not** own: the substrate, the descriptor, socket discovery/attestation,
or the node-agent's structure (agentctl RFC 0002 / RFC 0008 — this RFC's bridge
*lives inside* Tier A and consumes descriptors); the CRD/status schema (agentctl RFC
0003 — this RFC feeds the curated `status.lastRun`, it does not define `status`);
the reconcile loop or the `podFailurePolicy` *render* (agentctl RFC 0006); the KEDA
scaler / autoscaling-trigger detail (agentctl RFC 0011 — this RFC freezes which
*frozen* series are the scaling signals and flags the P10 defect, the scaler is
0011's); the cost-governance *enforcement* and the egress proxy's per-backend
metering (agentctl RFC 0012); the A2A gateway and its trace-root role (agentctl RFC
0013 — this RFC states the correlation contract it must honour); and the
`kubectl agents top/results` CLI rendering (agentctl RFC 0016 — this RFC is the
backend those commands read).

---

## 2. Decision — the telemetry bridge (nine principles)

1. **The node-agent (Tier A) is the single networked telemetry bridge; the agent
   pod is never a scrape or log target.** Tier A (agentctl RFC 0008) holds the
   discovered, attested socket to each local agent (agentctl RFC 0002 §3) and is the
   *only* component that re-exposes agent telemetry on the network. Prometheus,
   Loki, and the trace pipeline all target the **node-agent**, never an agent pod
   (which on the HARDENED/off-pod tiers has no network at all). This is the
   load-bearing inversion of the usual "scrape the workload" model (§4).

2. **Metrics: a per-pod relabeling scrape-proxy on the node-agent, fed by central
   service discovery.** Tier A serves `GET /proxy/<pod-uid>/metrics`, reads the
   agent's frozen Prometheus text **over the descriptor** (`surface: "metrics"` —
   a TCP dial on a networked pod, the in-socket metrics resource on a networkless
   one), and returns it **byte-identical**. An operator-maintained Prometheus
   `http_sd` endpoint emits **one target per agent pod**, with `__address__` set to
   that pod's **node-local node-agent**; Prometheus attaches pod identity from SD
   meta via relabeling (`honorLabels: false`). The proxy transports bytes; identity
   is stamped at Prometheus (§4). The proxy is **byte-identical** — it injects no
   `agentctl_*` series of its own (§4.4) — and the **caller→proxy hop is locked down**
   under hostile tenancy (NetworkPolicy + mTLS/bearer to a cluster Prometheus, §4.6),
   so it is not a cross-tenant metrics-exfiltration hole.

3. **Flag the P4 contract conflict: the in-socket metrics resource is undefined and
   is a *hard* dependency for networkless pods.** Whether the frozen metrics text is
   reachable **over the management/vsock socket** (`agentd://metrics`) is presently
   inconsistent — agentd RFC 0005/0015 do **not** list it; agentd RFC 0019 assumes
   it. On a networked pod there is a TCP `/metrics` fallback (`--health-http ADDR`);
   on a **networkless** pod there is **none**, so the in-socket resource is the only
   path and the whole scrape-proxy is blocked there until **contract ask P4** defines
   `agentd://metrics` (byte-identical Prometheus 0.0.4 text, pinned `mimeType`) and
   `agentd://capacity` (agentctl RFC 0002 §13(h); brainstorm §14). The exposition
   format is pinned: **Prometheus 0.0.4 text** (agentd RFC 0010 §3.8) (§4.4).

4. **Events vs logs: bulk logs ride container stderr → Loki; `agentd://events` is
   live-tail only.** Container stdout/stderr is captured by the kubelet/CRI **locally,
   independent of pod networking** (the "stderr assumes a network" premise is false —
   brainstorm §9.2). So the **bulk** event stream is the normal stderr → node log
   agent → Loki path, which already works on a networkless pod and never drops lines
   under load. The lossy bounded `agentd://events` ring (agentd RFC 0016 §7) is
   reserved for **live tail** (`kubectl agent logs -f`), tailed by Tier A over the
   socket. Routing bulk through the ring would be redundant and strictly worse (§6).

5. **Run-outcome capture is a first-class Tier A responsibility, not an afterthought
   — and it races pod GC.** A `once`/Job pod is gone seconds after it exits, taking
   `agentd://run/{run_id}` with it. Tier A **subscribes the run resource at pod-up**,
   **reads on the terminal transition while the process is still alive**, persists
   the report to a durable store, and feeds the operator a curated
   `Agent.status.lastRun`; `--report-file` on an emptyDir/PVC is the backstop. This
   needs **contract ask P5** (a read-before-exit guarantee / short post-terminal
   linger + re-read by handle) — agentd delivers the distillate **once** and a
   `once`-mode agent exits immediately, so a blinking collector loses the result (§7).

6. **The exit-code → `podFailurePolicy` mapping is read from the contract; the
   *render* is the operator's, the *alerting* is this RFC's.** agentd RFC 0016 §5.2
   freezes the per-code control-plane intent; agentctl RFC 0006 §8.6 compiles the
   `onExitCodes` rules at render time. This RFC owns the **observability** half:
   surfacing the rich report behind the coarse code, and the **infra-code alerts**
   (`137` OOM → raise memory, `143` forced drain → grace too tight) that no
   `podFailurePolicy` action expresses. All gated on the `surfaces.exit_codes` major
   (§7.3).

7. **Trace correlation is W3C trace-context, rooted at the A2A gateway, stitched by
   `trace_id` — never by `agent_path`.** The gateway (agentctl RFC 0013) is the trace
   root: it sets `_meta.traceparent` on the inbound frame (or the operator sets
   `AGENTD_TRACEPARENT` at pod start), and agentd adopts-or-mints and carries
   `trace_id` through logs, events, the report, every MCP/LLM call, and the spawn
   payload (agentd RFC 0010 §3.6). Cross-pod correlation is **`trace_id` + the span
   tree**; `agent_path` resets to `0` per pod and is valid **only within one pod's
   tree** (§8). Gateway traceparent ingest is **contract ask P-trace**.

8. **Fleet aggregation is exclusively agentctl's, and the agent never does it.**
   Rollups (recording rules `sum by (namespace/agentclass/fleet/tenant)`), cost
   (`tokens × a price table agentctl owns` — agentd emits tokens, never currency),
   the fleet event bus (Loki), the fleet trace view (by `trace_id`), the run-outcome
   history, and the `kubectl agents top/results` backends (agentctl RFC 0016) all
   live here. The agent serves one instance and is told nothing (§9).

9. **The control plane observes itself, on a path that does not depend on the data
   plane.** The operator, both node-agent tiers, the gateway, the KEDA scaler, the
   coordination server, and the webhook each emit `agentctl_*` metrics/traces/logs
   (a distinct namespace from `agentd_*`), scraped **directly** (these components are
   networked). The node-agent-bridge SPOF, reconcile health, webhook latency, and
   scrape-proxy success are first-class SLOs; the management-action audit
   (`mgmt.invoked`, P-meta/P-audit) is part of self-observability (§10).

These nine are final for the telemetry surface. Each defers to its owning sibling
RFC where it touches another plane (noted inline), and **degrades gracefully** when
a surface is absent or a contract ask is unmet (§11).

---

## 3. What this RFC owns vs reuses, and the end-to-end path

### 3.1 The boundary table

| Concern | Owner | This RFC's role |
|---|---|---|
| Prometheus exposition mechanism (0.0.4 text, hand-written) | **agentd RFC 0010 §3.8** | reuse; scrape it via the bridge, re-expose byte-identical |
| Frozen metric name/label set + `metrics_schema` version | **agentd RFC 0016 §4** | author dashboards/alerts/recording rules against it; branch on the major |
| JSON-lines log schema + closed `event` vocabulary | **agentd RFC 0010 §3.2/§3.3** | reuse verbatim; ship stderr→Loki; tail the ring projection live |
| `agentd://events` live-tail ring + `events_schema` | **agentd RFC 0016 §7** | tail for `logs -f`; **not** the bulk path (§6) |
| Run-outcome report + `--report-file` + `report_schema` | **agentd RFC 0016 §6** | **capture before GC**, persist, feed `status.lastRun` (§7) |
| Exit-code table + `podFailurePolicy` intent + `exit_codes` | **agentd RFC 0011 §5 / RFC 0016 §5** | observability reading + infra alerts; render is agentctl RFC 0006 §8.6 |
| W3C trace-context propagation | **agentd RFC 0010 §3.6** | stitch a multi-pod flow by `trace_id`; gateway is the root (§8) |
| Health surface (`/healthz`/`/readyz`/`--health-file`/exec verb) | **agentd RFC 0010 §3.7 / agentctl RFC 0002 §8 (P1)** | wire probes per tier; surface `up`/`ready` + reachability (§4.5) |
| Substrate, descriptor, discovery, attestation | **agentctl RFC 0002 / RFC 0008** | consume descriptors; the bridge lives in Tier A |
| CRD `.status` schema | **agentctl RFC 0003** | feed curated `status.lastRun`; do not redefine `.status` |
| `podFailurePolicy` render; reconcile | **agentctl RFC 0006** | feed; do not duplicate |
| KEDA scaler / autoscaling triggers | **agentctl RFC 0011** | name the frozen scaling signals; flag P10 (§5.4) |
| Cost-governance enforcement; proxy per-backend metering | **agentctl RFC 0012** | rollup + price table; enforcement is 0012's |
| `kubectl agents top/results` rendering | **agentctl RFC 0016** | the backend those commands read |

If a row says "reuse," this RFC must not redefine it. The new artifacts this RFC
*introduces* are entirely Kubernetes-shaped: the Tier A scrape-proxy + `http_sd`
contract (§4), the dashboard/alert/recording-rule corpus authored against the
frozen schema (§5), the run-outcome durability path (§7), the fleet-rollup recording
rules + price table (§9), and the `agentctl_*` self-observability surface (§10).

### 3.2 The telemetry path (networkless pod → Tier A → Prometheus / Loki / traces)

```
        ┌──────────────────────────── networkless agent pod (Kata microVM / off-pod) ─────────────┐
        │  conformant agent (reference impl: agentd) — NO pod IP, NO shell, scratch image          │
        │                                                                                          │
        │   stderr (NDJSON, 27-name closed vocab) ──────────────────┐  (CRI captures locally,     │
        │   agentd://metrics  (Prom 0.0.4 text)        [P4]          │   independent of pod net)   │
        │   agentd://events   (bounded lossy ring)     [live tail]   │                             │
        │   agentd://run/{run_id} (report, once-mode, ONE delivery)  │                             │
        │   --report-file PATH  (emptyDir/PVC backstop)              │                             │
        │   --health-file / exec-health verb           [P1]          │                             │
        └───────────┬───────────────────────────────────────────────┼─────────────────────────────┘
                    │ discovered + ATTESTED socket (agentctl RFC 0002 §3/§7)  │ container stderr
                    ▼  (vsock→per-VM uds  |  hostPath uds  |  emptyDir uds)    ▼
   ┌──────────────── node-agent Tier A (DaemonSet, the ONLY networked bridge) ─────────────────────┐
   │  scrape-proxy   : GET /proxy/<uid>/metrics → read(metrics surface) → BYTE-IDENTICAL text       │
   │  events tail    : subscribe agentd://events → fan to `kubectl agent logs -f` (live only)       │
   │  run collector  : subscribe agentd://run/{uid} @pod-up → read @terminal → persist (P5)      │
   │  reachability   : synth up / bridge-reachable series (disambiguate dead-pod vs gapped-bridge)  │
   └───────┬───────────────────────────┬───────────────────────────────┬───────────────────────────┘
           │ /proxy/<uid>/metrics       │ container stderr (kubelet)     │ captured run reports
           ▼  (http_sd → node-agent)    ▼                                ▼
   ┌───────────────┐           ┌───────────────┐              ┌──────────────────────────┐
   │  PROMETHEUS   │           │  node log     │              │  durable run-report store │
   │  (central SD, │           │  agent → LOKI │              │  + operator → status.lastRun│
   │   relabel)    │           │  (bulk events)│              │  (kubectl agents results) │
   └──────┬────────┘           └──────┬────────┘              └──────────────────────────┘
          │ recording/alert rules     │ LogQL                  trace_id stitches all three +
          ▼ (frozen names)            ▼                        the control-plane spans (§8/§10)
   ┌──────────────────────────────────────────────────────────────────────────────────────────────┐
   │  FLEET VIEWS (agentctl only): rollups · cost = tokens×price · top · results · trace view        │
   │  CONTROL-PLANE SELF-OBS (agentctl_*): operator · node-agent · gateway · scaler · webhook        │
   └──────────────────────────────────────────────────────────────────────────────────────────────┘
```

The path is uniform across tiers because the *descriptor* (agentctl RFC 0002 §3)
hides the substrate: on a networked pod the metrics read is a TCP dial, on a
networkless pod it is the in-socket resource (P4), but the proxy, the SD, and every
downstream consumer are substrate-blind. Bulk logs always ride stderr (CRI-captured
regardless of pod networking); the ring is only ever a live convenience.

---

## 4. The metrics path — Tier A scrape-proxy + central service discovery

### 4.1 Why the node-agent is the scrape target (not the pod)

A networkless agent pod has nothing for Prometheus to dial. Even a *networked*
stock-unix pod is routed through the bridge for uniformity: one scrape topology
covers all three tiers, attestation (agentctl RFC 0002 §7) is enforced on the read,
and the node-agent stays the single networked component (the isolation posture the
whole design exists to preserve, agentctl RFC 0002 §10). So:

> **Normative.** Prometheus scrapes the **node-agent**, never an agent pod. A
> `PodMonitor`/`ServiceMonitor` (Prometheus-Operator) MUST select the node-agent
> DaemonSet's Service, **never** the agent workload; the preferred discovery is
> `http_sd` (§4.2) because the relationship is *many agent pods behind one
> node-agent per node*, which a per-pod `PodMonitor` cannot express.

### 4.2 Central service discovery (`http_sd`), one target per agent pod

An operator-maintained `http_sd` endpoint returns the live target list. Each target
is **one agent pod**, but its `__address__` is that pod's **node-local node-agent**
proxy, and its `__metrics_path__` carries the pod UID:

```jsonc
// GET http://<sd>/targets  →  Prometheus http_sd_config response (one entry per agent pod)
[
  {
    "targets": ["10.0.3.7:9910"],                 // the node-agent on the pod's node (NOT the pod)
    "labels": {
      "__metrics_path__": "/proxy/f3c1…/metrics",  // pod UID — the proxy routes to this agent's socket
      "__meta_agentctl_namespace": "agents",
      "__meta_agentctl_agent":     "triage",       // Agent/AgentFleet name
      "__meta_agentctl_pod":       "triage-abc",
      "__meta_agentctl_uid":       "f3c1…",        // the descriptor join key (agentctl RFC 0002 §3)
      "__meta_agentctl_node":      "node-3",
      "__meta_agentctl_class":     "standard-substrate",
      "__meta_agentctl_tier":      "kata-hybrid",  // stock-unix | kata-hybrid | sidecar-emptydir
      "__meta_agentctl_mode":      "reactive",
      "__meta_agentctl_tenant":    "team-a"
    }
  }
]
```

Prometheus relabels `__meta_agentctl_*` → final labels (`namespace`, `agent`, `pod`,
`node`, `agentclass`, `tier`, `mode`, `tenant`) via `relabel_configs`, and
`metricRelabelings` apply the cardinality labeldrop (§4.3). The SD source of truth
is the set of managed pods the operator already watches (agentctl RFC 0006 §8.2,
the labelled-pod edge); whether the `http_sd` is **served by the operator or by a
small SD service decoupled from operator liveness** is an open question (§13) — a
fleet's scrape target list must not vanish during an operator rollout.

### 4.3 Relabeling and cardinality discipline (binding)

- **`honorLabels: false`.** SD-derived identity MUST win over any label in the
  scraped body. The brainstorm caught `honorLabels: true` as inverted (§9.2): it
  lets a scraped (tenant-influenced) label overwrite SD identity — a cross-tenant
  spoofing hole. Identity comes from SD, never from the wire.
- **`metricRelabelings` labeldrop the forbidden cardinality keys** —
  `run_id`, `agent_id`, `agent_path`, `call_id`, `session_id`, resource `uri`. The
  contract already forbids these as metric labels (agentd RFC 0016 §4.2), so this is
  **defence in depth** against a non-conformant or second-vendor agent that violates
  the discipline; per-run granularity lives in the run report (§7) and traces (§8),
  never in a series.
- **Do NOT stamp `model` or `metrics_schema` as a per-target label.** `model` is
  already a *bounded series label* agentd emits (agentd RFC 0016 §4.3), and a
  multi-endpoint pod serves multiple models — a per-target `model` collides and
  churns. Version hints (`metrics_schema`, `contract_version`, agent build) ride a
  dedicated `agentd_build_info{...} 1`-style series, never the target identity.
- **Per-pod identity on `once`/Job metrics is a cardinality blowup.** A short-lived
  Job churns `pod` label values endlessly; for fleets, scrape **aggregate** series
  and read **per-run cost/outcome from the run report** (§7), not from a per-pod
  metric.

### 4.4 The proxy is byte-identical; the format is pinned

`GET /proxy/<uid>/metrics` reads the agent's exposition over the descriptor and
returns it **unchanged** — same `# HELP`/`# TYPE` lines, same `name{labels} value`,
**Prometheus 0.0.4 text** (agentd RFC 0010 §3.8, agentd RFC 0016 §4.1). The proxy
does no parsing, no rewriting, no aggregation, and **injects no `agentctl_*` series
of its own** (relabeling is Prometheus's job, §4.2); keeping it byte-identical means a
future `metrics_schema` minor (a new additive series) flows through with **zero proxy
change**. *Byte-identical means byte-identical:* any agentctl-minted series — the
bridge-reachability signal (§4.5) and the additive-drift counter (§5.6) — lives on
the **node-agent's own `/metrics`** (the §10.1 self-obs target), **never** spliced
into `/proxy/<uid>/metrics`. The upstream read is descriptor-typed:

| Tier | Metrics descriptor `dial` | Hard dependency |
|---|---|---|
| stock-unix (networked) | TCP `:9090` (`--health-http ADDR`, agentd RFC 0010 §3.7) | none |
| stock-unix / Kata / off-pod (networkless) | in-socket metrics resource over the management socket | **P4** (undefined today) |
| sidecar-emptydir | the sidecar dials the emptyDir socket pod-locally, re-exposes | none (sidecar is on the netns, agentctl RFC 0002 §4.3) |

Until P4 lands, networkless metrics are **not shippable**; the day-one path is the
networked stock-unix tier scraping a TCP `/metrics` through the proxy (agentctl RFC
0001 roadmap Phase 1, agentctl RFC 0002 §13(h)).

### 4.5 `up`, readiness, and dead-pod vs gapped-bridge disambiguation

Prometheus's synthetic `up` measures *the scrape of the node-agent*, not the agent.
The bridge MUST disambiguate the two failure modes a single `up` conflates — because
a networkless pod whose node-agent bounced is **still alive**, only its bridge reach
gapped (agentctl RFC 0002 §8):

- `up == 1` (the node-agent scraped) and `/proxy/<uid>/metrics` returns the agent's
  series ⇒ agent healthy and reachable.
- `up == 1` but the node-agent could not reach a pod's socket ⇒ the node-agent emits
  `agentctl_bridge_reachable{uid} 0` (+ a `reason`) **on its own `/metrics`** (§10.1),
  distinct from "pod dead." This is *not* injected into `/proxy/<uid>/metrics` (which
  stays byte-identical, §4.4); a pod whose socket is unreachable simply yields an empty
  proxy body. Liveness/readiness still derive from the contract health surface (the
  `--health-file`/exec-health verb, agentd RFC 0010 §3.7, agentctl RFC 0002 §8 P1) —
  a dropped management/scrape connection is **not** a liveness signal (agentd RFC
  0015 §8).
- `up == 0` ⇒ the **node-agent** is unscrapeable (its own SPOF, §10), not a verdict
  on any agent.

This split is what lets an alert say "agent X is OOM-dead" vs "node-3's bridge is
gapped" instead of paging on every node-agent rollout. The exact synthetic-series
shape (and whether `agentctl_bridge_reachable` is emitted by the node-agent or derived
in Prometheus from a staleness rule) is an open question (§13).

### 4.6 The scrape endpoint is locked down (caller→proxy authz, hostile tenancy)

Under the locked hostile-multi-tenancy decision (brainstorm §0.6) the scrape-proxy is
the observability analogue of RFC 0009's management path: it multiplexes **every
tenant's** metrics on one node-agent endpoint, and the node-agent runs
`hostNetwork: false`, so **any pod on the cluster network could reach its IP** unless
locked down. The RFC carefully attests the proxy→agent *read* (§4.1) but that says
nothing about the **caller→proxy** hop — an ungated multiplexing proxy is a
cross-tenant metrics-exfiltration hole (per-tenant token counts via
`agentd_tokens_total`, refusal reasons, model names, backlog all carry tenant-sensitive
signal). So the caller→proxy hop is **normatively gated**, two layers (matching
RFC 0002 §10's IP-layer-vs-identity distinction):

> **Normative.** (1) A **NetworkPolicy** restricts *reachability* of
> `GET /proxy/<uid>/metrics` (the node-agent's scrape port, e.g. `:9910`) to the
> **Prometheus identity's** pods — IP-layer reachability, not identity (RFC 0002 §10
> correction 2). (2) Prometheus authenticates with **mTLS or a bearer token**
> (`tls_config`/`authorization` in the scrape config); the node-agent **refuses an
> unauthenticated scrape**. NetworkPolicy alone is necessary-not-sufficient (it is
> IP-layer); identity is enforced by the mTLS/bearer check.

**Per-namespace isolation.** v1 scopes scraping to a **cluster-level (admin/platform)
Prometheus** — the SD (§4.2) and the node-agent enforce that only the platform
Prometheus identity may scrape, and tenant-facing dashboards read the **relabeled,
identity-scoped** series through Grafana/datasource RBAC, not by scraping the proxy
directly. Whether a **per-tenant** Prometheus may be granted scrape of only its own
namespace's UIDs (the node-agent checking the caller identity's namespace against the
target descriptor's namespace, the §4.1 attestation extended to the *caller* side) is
an open question (§13) — but the default is "no tenant scrapes the shared proxy."
The **live-tail / `logs -f` / `top -w`** caller hop is a *different* endpoint: it
flows through the node-agent's §7 management API (agentctl RFC 0008) and is governed by
the RFC 0009 access path (per-verb RBAC + the two-known-clients rule), not by this
scrape gate.

---

## 5. The frozen metrics schema → dashboards, alerts, recording rules

agentctl authors all three artifact classes against the **exact frozen spelling**
(agentd RFC 0016 §4.3) and branches on `surfaces.metrics_schema` major (§11). It
never mints a name. This section enumerates what agentctl builds; the series
themselves are the contract's.

### 5.1 Dashboards (read the frozen series, group by SD identity)

The standard fleet dashboard panels, each a query over frozen names grouped by the
SD-relabeled identity (`namespace`/`agentclass`/`tier`/`tenant`):

- **Terminal-status histogram** — `sum by (status) (agentd_runs_total)` (`status`
  is the agentd RFC 0007 §3.4 closed set: `completed`/`refused`/`exhausted_steps`/…).
- **Token throughput / cost** — `sum by (model,type) (rate(agentd_tokens_total[5m]))`,
  joined to the price table (§9.2) for currency.
- **Safety** — `sum by (reason) (rate(agentd_refusals_total[5m]))` and
  `agentd_limit_exceeded_total{limit}` (the headline safety panels).
- **Reliability** — `agentd_subagent_stuck_kills_total{signal}`,
  `agentd_subagent_restarts_total{reason}`, `agentd_reactor_stalls_total`.
- **Dependency health** — `agentd_intel_up`, `agentd_intel_errors_total{reason}`,
  `agentd_mcp_up{server}`, `agentd_mcp_connect_failures_total{server}`.
- **Lifecycle** — `agentd_drains_total{phase}` (clean `completed` vs `forced`),
  `agentd_restarts_total`.
- **Reactive backlog** — `agentd_pending_events`, `agentd_reaction_lag_ms`,
  `agentd_inflight_reactions`, `agentd_subscriptions_active` (§5.4).

### 5.2 Alert rules (off the frozen names + the exit-code/drain distinction)

| Alert | Expression (frozen names) | Why |
|---|---|---|
| `AgentTrifectaRefusals` | `increase(agentd_refusals_total{reason="trifecta"}[15m]) > 0` | a Rule-of-Two/trifecta scope refusal fired (agentd RFC 0012) |
| `AgentForcedDrain` | `increase(agentd_drains_total{phase="forced"}[10m]) > 0` | SIGTERM forced past the drain budget ⇒ `terminationGracePeriodSeconds` too tight vs `AGENTD_DRAIN_TIMEOUT` (a **config** fix, not a retry) — pairs with exit `143` (§7) |
| `AgentReactorWedged` | `increase(agentd_reactor_stalls_total[5m]) > 0` | the supervisor reactor is wedged (a live PID is not a live agent, agentd RFC 0016 §10) |
| `AgentIntelDown` | `agentd_intel_up == 0` for 5m | model endpoint unreachable (agentd RFC 0018) |
| `AgentMCPDown` | `agentd_mcp_up == 0` for 5m | a declared MCP dependency is down (agentd RFC 0004) |
| `AgentStuckKills` | `increase(agentd_subagent_stuck_kills_total[15m]) > 0` | the reliability headline — a subagent had to be killed |
| `AgentOOMKilled` | `kube_pod_container_status_last_terminated_reason{reason="OOMKilled"} == 1` (kube-state-metrics; §7.4) | OOM kill ⇒ raise `resources.limits.memory` |
| `AgentForcedKill` | `kube_pod_container_status_last_terminated_reason{reason="Error"}` + terminated `exitCode==143`, or the `DisruptionTarget` pod condition (§7.4) | SIGTERM forced past grace ⇒ grace too tight / eviction |
| `BridgeGapped` | `agentctl_bridge_reachable == 0` for 2m | a node-agent cannot reach a live pod's socket (§4.5) — distinct from pod-dead |

`137`/`143` are **OS-set**, never returned by agentd (agentd RFC 0011 §5.1), and a
process killed by signal **never writes a run report** — so they are read from
**Kubernetes pod/container status** (kube-state-metrics + the `DisruptionTarget`
condition, brainstorm §3.2), **not** from `exit_code` in the run report. They have
**no `podFailurePolicy` "alert-only" action**, so they are surfaced as alerts here
precisely because the workload layer cannot express them (§7.3/§7.4).

### 5.3 Recording rules (fleet rollups — the aggregation the agent never does)

agentd serves per-instance counters; agentctl pre-aggregates fleet rollups as
recording rules so dashboards and `kubectl agents top` read a cheap series:

```yaml
groups:
  - name: agentctl-fleet-rollups
    rules:
      - record: agentctl:runs:rate5m
        expr: sum by (namespace, agentclass, agent) (rate(agentd_runs_total[5m]))
      - record: agentctl:tokens:rate5m
        expr: sum by (namespace, tenant, model, type) (rate(agentd_tokens_total[5m]))
      - record: agentctl:refusals:rate15m
        expr: sum by (namespace, tenant, reason) (rate(agentd_refusals_total[15m]))
      - record: agentctl:fleet_backlog
        expr: sum by (namespace, agent) (agentd_pending_events)   # fleet sum of a per-pod gauge
```

These rollups are the **policy** layer; the per-pod gauges are the contract's
**primitives** (agentd RFC 0016 §4.3). The agent is never aware its series are being
summed across a fleet (agentd RFC 0014 §3).

### 5.4 The autoscaling signal set — and the P10 metric-name defect

The reactive-backlog gauges are the autoscaling inputs the scaling plane (agentctl
RFC 0011) consumes. **Only the agentd RFC 0016 §4.3 frozen set is real:**

- **Frozen (author against these):** `agentd_pending_events`,
  `agentd_reaction_lag_ms`, `agentd_inflight_reactions`, `agentd_subscriptions_active`.
- **NOT frozen (do not author against these):** `agentd_reactive_backlog`,
  `agentd_saturation`, `agentd_tokens_per_sec`, `agentd_claims_lost_total` — agentd
  RFC 0019 §5 names these and falsely calls them frozen; they are **not** in the
  schema. This is **contract ask P10**: reconcile the two name sets into one and add
  `agentd_saturation` to the frozen schema if it is to be a scaling signal.

> **Binding for this RFC and agentctl RFC 0011.** Dashboards, alerts, and KEDA
> triggers MUST be authored against the **frozen** names only; `saturation`/`backlog`
> are treated as not-real until P10 reconciles them. This RFC raises P10; the KEDA
> external scaler that consumes the frozen signals is agentctl RFC 0011's. Note that
> **scale-from-zero cannot read any of these** (a per-replica gauge emits nothing at
> replica 0) — that signal comes from the coordination server's queue depth (P9),
> owned by agentctl RFC 0011, out of scope here.

### 5.5 Histograms, quantiles, and SLOs — blocked on P-hist

The `*_duration_ms` histograms (`agentd_run_duration_ms`,
`agentd_intel_call_duration_ms`, `agentd_tool_call_duration_ms`) have **two
problems** agentctl must not paper over: the histogram name set **conflicts** between
agentd RFC 0010 §3.8 (otel-only `gen_ai.*`/`*_duration_ms`) and agentd RFC 0016 §4.3
(frozen `*_duration_ms`), and the **bucket boundaries are unspecified**. A
quantile/SLO dashboard authored on unspecified buckets is meaningless across agent
versions. This is **contract ask P-hist** (reconcile the name set + freeze the
bucket boundaries).

> **Binding.** Until P-hist lands, agentctl authors latency SLOs as **counter
> ratios** (`success / total`, e.g. `agentd_runs_total{status="completed"} / sum`),
> **not** histogram quantiles, and ships no `histogram_quantile(...)` panel against
> the unspecified buckets.

### 5.6 Additive-drift reporting (lenient, but observed)

Conformance (agentctl RFC 0001 §4 / RFC 0018) catches *regressive* drift (a frozen
series gone). It does **not** catch *additive* drift (a new series an agent emits
that agentctl does not recognise). Detecting additive drift requires **parsing and
schema-comparing** the metrics body — which the byte-identical scrape-proxy MUST NOT
do (§4.4). So the comparison is done **outside the proxy**: a small analyzer in the
node-agent (scraping the same upstream the proxy reads, independently of the
flow-through path) emits `agentctl_unknown_series_total{agentclass}` **on the
node-agent's own `/metrics`** (§10.1) when an upstream body carries a series outside
the known `metrics_schema` major — so an operator sees "this agent build advertises
more than we drive" rather than silently ignoring it (brainstorm §11.2). The
flow-through proxy stays dumb; only this side analyzer parses, and it never rewrites
what Prometheus scrapes from `/proxy/<uid>/metrics`. This is observe-only; agentctl
never errors on additive drift (agentd RFC 0016 §8.3).

---

## 6. The events pipeline — stderr→Loki (bulk) + `agentd://events` (live tail)

### 6.1 The split, and why bulk is stderr (not the ring)

The contract gives two views of the **same** closed-vocabulary event stream (agentd
RFC 0016 §7.1): the durable source of truth on **stderr** (NDJSON, agentd RFC 0010
§3.2), and a live **`agentd://events`** projection backed by a bounded, **lossy**
in-memory ring (agentd RFC 0016 §7.2). agentctl uses each for what it is good at:

| View | Path | Use | Loss model |
|---|---|---|---|
| **Bulk** (durable, searchable, alerting source) | container **stderr** → node log agent → **Loki** | history, LogQL, security/limit-line alerting | **lossless** (CRI captures every line) |
| **Live tail** (interactive) | `agentd://events?after=<seq>` over the socket → Tier A → `kubectl agent logs -f` | `logs -f`, the live `tree -w` event feed | **lossy** (ring drops oldest, bumps `dropped`) |

The brainstorm correction is load-bearing (§9.2): **the "stderr assumes a network"
premise is false.** Container stdout/stderr is captured by the kubelet/CRI
**locally, independent of pod networking** — so it works on a networkless pod with
no changes, and it never drops a line under load. Routing the **bulk** stream
through the lossy vsock ring would be redundant with a path that already works and
**strictly worse** (it would drop exactly the security/limit lines you most want
under load). So:

> **Normative.** Bulk events = container stderr → node log agent → Loki.
> `agentd://events` is **live-tail only** (the `kubectl agent logs -f` / `tree -w`
> convenience). agentctl MUST NOT use the ring as the durable/alerting event store.

### 6.2 Live tail over the socket (Tier A)

For interactive `logs -f` / `tree -w`, the ring is the right surface even though the
bulk stream is stderr (§6.1): it gives a cursor, a `dropped` signal, and cheap
server-side prefix filtering that re-deriving a live view from Loki cannot. Tier A
subscribes `agentd://events` (notify-then-read, agentd RFC
0005 §3.3): on each `notifications/resources/updated{uri:"agentd://events"}` it
`resources/read("agentd://events?after=<seq>")`, advances the cursor, honours the
`dropped` counter (surfacing a "tail fell behind, N lines dropped" marker to the
client), and applies the contract's cheap server-side prefix filter
(`?level=warn`, `?event=security.,limit.,subagent.`). The fan-out to multiple
viewers and the cursor bookkeeping are Tier A's; the agent serves one ring (agentd
RFC 0016 §7.3). The `kubectl agent logs -f` CLI surface that consumes this is
agentctl RFC 0016.

### 6.3 Loki labels mirror the SD identity

The node log agent labels Loki streams with the **same** SD-derived identity the
metrics path uses (`namespace`/`agent`/`pod`/`node`/`agentclass`/`tier`/`tenant`),
so a LogQL query joins to a metrics panel and a trace by shared identity + `trace_id`
(§8). The raw NDJSON line schema (agentd RFC 0010 §3.2) is preserved verbatim into
Loki — agentctl does not reshape it — so `agent_path` prefix subtree queries work in
LogQL exactly as agentd intended (within one pod's tree; §8).

---

## 7. Run-outcome capture & the exit-code → `podFailurePolicy` reading

### 7.1 The race: a once/Job result outlives its pod by seconds

`kubectl agents results` needs the **full** terminal outcome — which terminal status
(agentd RFC 0007 §3.4), tokens/steps, duration, a pointer to the distillate, the
per-run refusal roll-up, the `trace_id` (agentd RFC 0016 §6.2) — not the coarse exit
code. But a `once`/Job pod is **gone seconds after it exits**, taking
`agentd://run/{run_id}` (served only while the process is alive, agentd RFC 0016
§6.3) with it. So the outcome MUST be captured to a durable place **at exit**, never
inferred from a vanished pod.

### 7.2 Tier A is the run-outcome collector (subscribe at pod-up, read at terminal)

```
run-outcome capture (node-agent Tier A, per local once/Job pod):
  1. @pod-up      : subscribe agentd://run/{run_id}     (notify-then-read, agentd RFC 0005 §3.3)
  2. @terminal    : on the terminal notifications/resources/updated, resources/read the report
                    WHILE THE PROCESS IS STILL ALIVE  (the P5 window)
  3. persist      : write the report (report_schema, agentd RFC 0016 §6.2) to the durable store
  4. feed status  : hand the curated outcome to the operator → Agent.status.lastRun
                    (the node-agent NEVER writes Agent.status — agentctl RFC 0006 §2.3;
                     single DeepEqual-guarded writer — §2.6)
  backstop        : --report-file PATH on an emptyDir/PVC, read after the pod terminates
```

`Agent.status.lastRun` is a **curated, low-churn** projection (one write per terminal
run — terminal `status`, `exit_code`, `started_at`/`ended_at`, `distillate_ref`,
`trace_id`), consistent with agentctl RFC 0003's rule that **churny** telemetry
(token counters, live counts) stays out of `.status` and is served via metrics/the
report. The full report history (per-run cost, refusals, usage) lives in the durable
store, not in `.status`; `kubectl agents results` reads the store (agentctl RFC 0016),
and "works when the pod is gone" is scoped to **the store**, not to a vanished
`--report-file`.

### 7.3 The P5 window — the result is delivered exactly once

agentd delivers the distillate **exactly once** as a status notification and serves
`agentd://run/{run_id}` only for a **live** run (agentd RFC 0016 §6.3, agentd RFC
0020 §6). A `once`-mode agent **exits immediately** on completion. So if the
collector is not draining at the terminal transition — a Tier A bounce, a slow read
— the final artifact is **gone**. This is a must-not-miss consumer, and it needs a
contract primitive:

> **Contract ask (P5).** A **read-before-exit guarantee** (or a short
> post-terminal *linger* with a read-ack) for `agentd://run/{run_id}`, **and** a way
> to **re-read a terminal distillate by run handle** after the linger — so a
> networkless `once`-mode result is not lost if the collector blinks. Absent P5,
> the `--report-file` emptyDir/PVC backstop is the only durable copy, and the
> lost-window contract degrades to **`status: lost` + idempotent re-drive** (with the
> re-drive caveats the A2A durability design carries, brainstorm §D4 — re-drive is
> gated on an explicit idempotency assertion). The persistence target for the
> captured history (object store vs bounded CRs) is an open question (§13).

### 7.4 Exit code → `podFailurePolicy`: the observability reading

The exit-code table (agentd RFC 0011 §5) and its `podFailurePolicy` intent (agentd
RFC 0016 §5.2) are frozen and versioned (`surfaces.exit_codes`). The **render** of
`onExitCodes` rules is agentctl RFC 0006 §8.6; this RFC owns the **reading**. Two
distinct sources, and the split is load-bearing: the codes agentd itself **returns**
(`0,1,2,3,4,5,6,7,124`) are read from the **run report's `exit_code` field** (§7.2);
the OS-set signal-kill codes (`137`/`143`) are read from **Kubernetes pod/container
status**, because a signal-killed process never writes a report:

| Code | Name | Source | Observability action (this RFC) |
|---|---|---|---|
| `0` | `EXIT_OK` | run report | success (incl. clean SIGTERM drain → `0`, not `143`); pairs with `agentd_drains_total{phase="completed"}` |
| `2`,`5` | `EXIT_USAGE`/`EXIT_SEMANTIC` | run report | deterministic failure; no retry alert |
| `1`,`4`,`6` | failure/intel/mcp | run report | retriable; dependency-health panels (§5.1) |
| `3`,`7` | partial/budget | run report | budget panel; respects `--budget-exit-code` remap |
| `124` | `EXIT_TIMEOUT` | run report | deadline panel |
| `137` | `128+SIGKILL` (OS) | **pod status** (kube-state-metrics `kube_pod_container_status_last_terminated_reason="OOMKilled"` / terminated `exitCode`; pair with `onPodConditions: DisruptionTarget`) | **`AgentOOMKilled` alert** → raise memory; no `podFailurePolicy` action exists |
| `143` | `128+SIGTERM` (OS) | **pod status** (terminated `exitCode==143` / `DisruptionTarget` condition) | **`AgentForcedKill` alert** → grace too tight; pairs with `drains_total{phase="forced"}` |

> **Why not the report for `137`/`143`.** brainstorm §3.2 records that OOM/eviction
> exit-code matching is unreliable and `137` must be paired with
> `onPodConditions: DisruptionTarget`; the report-write is best-effort and a SIGKILL
> at the wrong instant leaves no report at all. So a frozen alert keyed on
> `run-report exit_code == 137` could **never fire** — it is sourced from pod status
> instead. The clean drain still returns `0` in the report (not `143`), so a `143`
> is always an *involuntary* signal kill observed at the pod layer.

This RFC does **not** reproduce or re-derive the table; it reads it. All gated on the
`surfaces.exit_codes` major — agentctl refuses to *interpret* an exit-code report
from a major it does not understand (it still records the raw code), exactly as it
refuses to *render* a `podFailurePolicy` for one (agentctl RFC 0006 §8.6, agentd RFC
0016 §5.1).

---

## 8. Trace correlation across the gateway↔socket↔agent boundary

### 8.1 The gateway is the trace root; agentd carries the trace through

W3C trace-context propagation is **on by default** in the contract and free (a few
JSON/header fields; export is the only heavy, gated part — agentd RFC 0010 §3.6/§3.9).
agentctl invents nothing; it sets the root and stitches by `trace_id`:

- **Root.** The A2A gateway (agentctl RFC 0013) is the trace root for an inbound
  flow: it adopts the caller's `traceparent` (or mints one) and sets
  `_meta.traceparent` on the frame it forwards over the socket to the agent. For an
  operator-initiated run, the operator sets `AGENTD_TRACEPARENT` at pod start (agentd
  RFC 0010 §3.6). The agent **adopts-or-mints** and from then on carries `trace_id`
  through every log line, event-stream entry, the run report, every outbound MCP/LLM
  call, and the spawn payload (agentd RFC 0010 §3.6) — so a multi-pod, multi-hop flow
  is one trace with **no agentd change**.
- **The boundary primitive that is missing.** Whether the agent **ingests** an
  inbound `traceparent` *on the A2A method surface* (vsock frame) is unspecified —
  agentd RFC 0020 / RFC 0010 §3.6 define ingest on the self-MCP request and via the
  env var, but not on the A2A surface the gateway uses. This is **contract ask
  P-trace**; until it lands, a gateway-rooted trace cannot be *claimed*, only the
  self-MCP/env-rooted one.

### 8.2 The correlation tuple, and why not `agent_path`

The fleet correlation key is the tuple **`{trace_id, run_id, pod uid, span tree}`**:

- **`trace_id`** → "all pods in this flow." This is the **only** valid cross-pod key.
- **`run_id`** → "all telemetry for this unit of work" (stable across one pod's tree,
  agentd RFC 0010 §3.2).
- **pod `uid`** → the descriptor join key (agentctl RFC 0002 §3) tying logs, metrics
  identity, and the captured report to one instance.
- **span tree** (`span_id`/`parent_span_id`) → the within- and cross-pod call shape.

> **Normative.** Cross-pod correlation MUST be `trace_id` + the span tree, **NOT**
> `agent_path` prefix. `agent_path` resets to `0` in each pod's own process tree
> (agentd RFC 0010 §3.2/§3.5: depth/path are supervisor-minted *per pod*), so it is
> valid **only within a single pod's subtree**. Using `agent_path` across pods
> silently mis-joins unrelated subtrees. Within one pod, `agent_path` prefix remains
> the cheap no-join subtree query (in Loki and in `kubectl agent tree`).

### 8.3 The control-plane hops are spans too

The gateway, the node-agent bridge, and the operator reconcile are **control-plane
spans** that join the same trace by `trace_id` (§10) — so a
`kubectl agent … → gateway → socket → agent → MCP backing service → Job pod` flow
renders as one span tree spanning the control and data planes. Span **export** stays
gated behind the `otel` feature on the agent side (agentd RFC 0010 §3.9); in the
default build agentctl correlates **logs + reports + events by `trace_id` alone**,
with no OTLP collector required. Provisioning the collector/backend is infra
agentctl deploys, not an agent feature (§12).

---

## 9. Fleet aggregation — what the operator rolls up (the agent never does)

The agent serves one instance and is told nothing about the fleet (agentd RFC 0016
§7.3, agentd RFC 0014 §6). Every fleet view is agentctl's:

### 9.1 The rollups

- **Metric rollups** — recording rules `sum by (namespace/agentclass/fleet/tenant)`
  (§5.3); these back the fleet dashboard and the `kubectl agents top` aggregate
  columns (agentctl RFC 0016).
- **Event aggregation** — Loki is the fleet event bus (§6); cross-pod search and
  fleet-wide security/limit alerting are LogQL over the labelled streams.
- **Trace aggregation** — the fleet trace view is "all spans for `trace_id`" (§8),
  spanning every pod a flow touched.
- **Run-outcome history** — the durable run-report store (§7) is the fleet results
  ledger `kubectl agents results` reads, collapsing retried Jobs by a stable
  `run_id` (agentd RFC 0016 §6.3).

### 9.2 Cost is a rollup × a price table agentctl owns

agentd emits **tokens, never currency** (agentd RFC 0016 §4.3: "cost = tokens × a
price table agentctl owns; agentd never learns a price"). agentctl computes cost
**only** at the rollup layer:

- **Key the price by `{model, type}`** — input vs output pricing differ — and account
  for cache-read/cache-write tiers where the source provides them (brainstorm §9.2).
- **Pick ONE authoritative token source.** The two candidates — `agentd_tokens_total`
  (the agent's own counter) and the egress proxy's per-backend metering (agentctl RFC
  0012) — MUST NOT be double-counted. In the keyless-dial tier the proxy is the
  egress chokepoint and may be the cleaner meter; this RFC names the choice as a
  cross-RFC contract (the price-table + chosen-source ownership is agentctl RFC
  0012's), and forbids summing both.
- **Per-run cost from the report, fleet cost from aggregate series.** A `once`/Job
  run's cost comes from its captured report (§7); a fleet's running cost is the
  `agentctl:tokens:rate5m` recording rule × price (§5.3). Never a per-pod cost label
  on a churny Job series (§4.3).

Cost **governance/enforcement** (budgets, kill-switch, chargeback) is agentctl RFC
0012 and needs the `EXIT_BUDGET`/back-pressure signal (P-cost); this RFC provides the
**observability** substrate enforcement reads. (`agentd_intel_*` health metrics in
the proxy tier measure the **pod↔proxy hop**, not the model — the proxy re-exports
per-backend health on its own series, agentctl RFC 0012; dashboards must label which
is which, and intel endpoint metric labels survive list-reorder only with stable
operator-assigned names, **contract ask P7**.)

---

## 10. Control-plane self-observability + SLOs

The data-plane telemetry path (§4–§8) observes the **agents**. The control plane must
observe **itself**, on a path that does **not** depend on the data plane (so a
data-plane outage is still visible).

### 10.1 Every component emits `agentctl_*` (a distinct namespace), scraped directly

The operator, both node-agent tiers, the A2A gateway, the KEDA external scaler, the
reference coordination server, and the admission webhook are ordinary networked Rust
services (the `kube-rs`/`tonic` stack, agentctl RFC 0001). Each exposes its own
`/metrics`, structured logs, and traces under the **`agentctl_*`** namespace
(deliberately distinct from `agentd_*`, so a query never confuses control-plane and
data-plane series), and is scraped **directly** (no bridge — these components have
network). The headline SLO series:

| Component | Key `agentctl_*` series | SLO |
|---|---|---|
| operator | `agentctl_reconcile_duration_seconds`, `agentctl_reconcile_errors_total`, `agentctl_status_writes_total` | reconcile p99 latency; error rate; status-write rate (hot-loop guard, agentctl RFC 0006 §8.1) |
| node-agent (Tier A) | `agentctl_bridge_connections`, `agentctl_bridge_reachable`, `agentctl_scrape_proxy_errors_total`, `agentctl_unknown_series_total`, `agentctl_run_reports_captured_total`, `agentctl_run_report_capture_failures_total` (all on the node-agent's **own** `/metrics`, never spliced into `/proxy/<uid>/metrics` — §4.4) | bridge reachability per node (the **SPOF**, §10.2); scrape-proxy success; additive-drift (§5.6); run-capture success |
| A2A gateway | `agentctl_a2a_requests_total{method,code}`, `agentctl_a2a_request_duration_seconds` | request latency/error rate (agentctl RFC 0013) |
| KEDA scaler | `agentctl_scaler_decisions_total`, `agentctl_scaler_query_errors_total` | scaling-decision latency; signal-read errors (agentctl RFC 0011) |
| webhook | `agentctl_admission_duration_seconds`, `agentctl_admission_rejections_total{reason}` | admission p99 (fail-closed latency budget); rejection causes (agentctl RFC 0007) |

### 10.2 The node-agent SPOF, the management-action audit, and trace continuity

- **Node-agent SPOF alerting.** A Tier A crash costs a **control + telemetry gap on
  one node** and zero data-plane impact (the bounce-safe invariant is scoped to Tier A,
  agentctl RFC 0008 §3.3) — but a *silent, prolonged* gap is dangerous (stale status, missed
  run-report capture, no live tail). `agentctl_bridge_reachable == 0` per node, and a
  DaemonSet-not-ready-on-node alert, page on it. This is the §4.5 disambiguation
  surfaced as a self-obs SLO, distinct from any agent verdict.
- **Management-action audit.** Every operator/human management verb (`drain`,
  `lame-duck`, `cancel`, steer/inject) the node-agent invokes over the socket is
  audited — the **`mgmt.invoked{tool,caller?}`** closed-vocabulary event (**contract
  asks P-audit/P-meta**: a descriptive caller/tenant `_meta` the agent echoes into
  its event log, never re-verified, like the downward-API identity). This audit is
  part of self-observability, not data-plane telemetry: it records *what the control
  plane did to an agent*, attributed to the caller identity the access path
  established (agentctl RFC 0009).
- **Trace continuity.** The control-plane spans (§8.3) carry the same `trace_id` as
  the data-plane flow, so a `kubectl agent attach …` or a gateway request renders as
  one trace from the human/caller through the control plane into the agent tree.

---

## 11. Failure semantics & versioning

### 11.1 Telemetry never blocks the data plane (inherited + restated)

The contract guarantees telemetry never takes down the agent (agentd RFC 0016 §8.4):
stderr write errors are swallowed, the events ring is lossy/bounded and never
back-pressures, the report write is best-effort-but-loud (the **exit code is the
floor**, never gated on the report landing), and `/metrics`/`events`/`--report-file`
are side-effect-free w.r.t. the run. agentctl preserves this on its side: a
scrape-proxy failure degrades to `up`/`agentctl_bridge_reachable` signals (§4.5), a
run-capture miss degrades to the `--report-file` backstop then `status: lost` (§7.3),
and **no agentctl telemetry component is ever in the agent's data path** (the bridge
is read-only; a Tier A bounce is a control/telemetry gap, not a data-plane outage).

### 11.2 Versioning — branch on the manifest, degrade on absence (P0)

agentctl reads the manifest **first** on every instance and branches per surface
(agentd RFC 0016 §8.3), exactly as the operator does (agentctl RFC 0006 §6.1):

| Surface | Manifest key | Major match | Major newer | Surface absent |
|---|---|---|---|---|
| metrics | `surfaces.metrics_schema` | scrape + author dashboards/alerts; tolerate unknown additive series (report as drift, §5.6) | scrape recognised series; skip unknown | no scrape target; `top` columns empty (manage by liveness+logs) |
| events envelope | `surfaces.events_schema` | tail the ring; honour `dropped` | tail recognised fields | no live tail; bulk stderr→Loki still works |
| run report | `surfaces.report_schema` | capture + parse | capture; skip unknown fields | no report; outcome = exit code + terminal log line |
| exit codes | `surfaces.exit_codes` | interpret + alert | record raw code; **do not interpret** | exit code is opaque; alert only on `137`/`143` (OS-set, version-free) |

A surface absent is **never an error** — the bridge re-exposes what the agent can
serve and manages the rest by liveness + exit code + logs (agentd RFC 0014 §8). The
in-socket metrics path additionally gates on **P4**, live tail on the `events`
feature, and networkless probes on the exec-health verb (**P1**, agentctl RFC 0002
§8). **A second-vendor conformant agent is observed unchanged** the moment it passes
the conformance suite (agentctl RFC 0001 §4.3): rendering keys off `surfaces{}`,
scraping off the frozen schema, correlation off `trace_id` — none of it names a
binary. That is the telemetry-plane expression of P0.

### 11.3 Rollout

- **Phase 1 — Observe** (agentctl RFC 0001 roadmap): the Tier A scrape-proxy +
  central `http_sd` + relabeling; stderr→Loki bulk events; the dashboard/alert/
  recording-rule corpus against the frozen schema; run-outcome capture + the
  `results` backend; `kubectl agents top`. **Blocking contract asks: P4** (define
  `agentd://metrics`/`agentd://capacity`) for networkless metrics; **P5** for
  robust `once`-mode results. Networked stock-unix metrics (TCP `/metrics` through the
  proxy) ship without P4.
- **Later phases:** P-trace gates gateway-rooted traces (lands with the A2A gateway,
  agentctl RFC 0013); P-hist gates quantile/SLO histogram dashboards; P10 gates the
  saturation/backlog scaling signals (with agentctl RFC 0011); P7 gates intel-endpoint
  metric-label stability; P-cost gates cost *enforcement* (agentctl RFC 0012).
- **Self-observability ships from day one** and independently of the data-plane path
  (§10) — the control plane must be observable even before the bridge is.

---

## 12. Non-goals (these live in other planes, in the agent, or in infra)

- **Any new telemetry mechanism or series.** agentctl defines no metric, event,
  report field, or trace field — it consumes the frozen contract (agentd RFC
  0010/0011/0016). Adding a series is the contract's job.
- **The metrics exposition format / the agent's `/metrics` implementation.** The
  hand-written Prometheus 0.0.4 text is the agent's (agentd RFC 0010 §3.8); the proxy
  re-exposes it byte-identical.
- **The substrate, descriptor, discovery, attestation, node-agent structure.**
  agentctl RFC 0002 / RFC 0008. The bridge lives inside Tier A and consumes
  descriptors; it does not produce them.
- **The CRD `.status` schema and the reconcile loop.** agentctl RFC 0003 / RFC 0006.
  This RFC feeds the curated `status.lastRun`; the operator writes it (single writer).
- **The `podFailurePolicy` render.** agentctl RFC 0006 §8.6. This RFC reads the
  exit-code contract and authors the infra alerts the policy cannot express.
- **The KEDA scaler, autoscaling triggers, scale-from-zero signal.** agentctl RFC
  0011. This RFC names which *frozen* series are the scaling signals and flags P10.
- **Cost governance / budget enforcement / kill-switch / chargeback, and the proxy's
  per-backend metering.** agentctl RFC 0012 (+ P-cost). This RFC owns the rollup +
  price-table observability substrate, not enforcement.
- **The A2A gateway and its trace-root implementation.** agentctl RFC 0013. This RFC
  states the correlation contract the gateway must honour (P-trace).
- **The `kubectl agents top/results/logs` CLI rendering.** agentctl RFC 0016. This
  RFC is the backend those commands read.
- **Provisioning Prometheus / Loki / Grafana / the OTLP collector.** Infra agentctl
  deploys (Helm/Kustomize, agentctl RFC 0001); not in scope as a design.
- **Long-term storage / retention / DR for metrics/logs/traces/run-reports.**
  agentctl RFC 0017 (release & lifecycle); this RFC names the run-report store as a
  durability requirement (§7) and an open question (§13), not a DR design.

---

## 13. Open questions

1. **Networkless metrics (P4) — the single hard blocker.** `agentd://metrics` must be
   defined as byte-identical Prometheus 0.0.4 text with a pinned `mimeType` and
   confirmed reachable over the management socket (agentd RFC 0005/0015 omit it, RFC
   0019 assumes it). Plus `agentd://capacity` (frozen schema) for victim selection.
   Until P4, networkless metrics are unshippable (§4.4). (agentctl RFC 0002 §13(h).)
2. **Run-report durability (P5) + the history store.** The read-before-exit
   guarantee / post-terminal linger + re-read-by-handle (§7.3), **and** the
   persistence target: an object store vs bounded CRs vs the A2A durable store
   (agentctl RFC 0013 / brainstorm D4). RPO for a lost `once`-result and the
   `status: lost` + idempotent-re-drive fallback contract.
3. **SD durability decoupled from operator liveness.** The `http_sd` target list must
   not vanish during an operator rollout (§4.2). Served by the operator, a small
   replicated SD service, or the node-agents collectively? This gates whether scraping
   survives a control-plane bounce.
4. **`up` vs bridge-gapped disambiguation — the exact synthetic series.** The shape of
   `agentctl_bridge_reachable` / a synthetic per-target `up` so an alert separates
   "agent dead" from "node-agent bounced" (§4.5), and whether it is emitted by the
   node-agent's own `/metrics` or derived in Prometheus from a staleness rule.
   `agentctl_bridge_reachable` is never spliced into `/proxy/<uid>/metrics` (§4.4) —
   only the realization (node-agent series vs Prometheus staleness rule) is open.
4a. **Scrape-endpoint authz — cluster Prometheus vs per-tenant scrape (§4.6).** v1
   locks the scrape proxy to a cluster/platform Prometheus identity (NetworkPolicy +
   mTLS/bearer). Open: whether a **per-tenant** Prometheus may scrape only its own
   namespace's UIDs (the node-agent checking caller-identity namespace against the
   target descriptor's namespace — the §4.1 attestation extended to the caller side),
   and how the SD (§4.2) would scope a per-tenant target list. Until settled, the
   default is "no tenant scrapes the shared proxy."
5. **Sub-scrape-interval Jobs.** A `once`/Job that runs and exits **faster than one
   Prometheus scrape interval** is never scraped — its only telemetry is the run
   report (§7) and stderr→Loki (§6). Confirm `kubectl agents top` for `once` fleets
   reads the report/aggregate, never a per-pod scrape, and that no metric is expected
   to exist for a sub-interval Job.
6. **Histogram buckets + name reconciliation (P-hist).** Until the bucket boundaries
   are frozen and the RFC 0010-§3.8-vs-RFC-0016-§4.3 name conflict is resolved, SLO
   dashboards are counter-ratios, not quantiles (§5.5). Which histograms become frozen
   (non-`otel`) series?
7. **Authoritative token source for cost.** `agentd_tokens_total` vs the egress
   proxy's per-backend metering (§9.2) — pick one per deployment to avoid
   double-counting; owned jointly with agentctl RFC 0012. Price-table freshness and
   ownership (cache-tier pricing) ride here too.
8. **Trace ingest on the A2A surface (P-trace).** Gateway-rooted traces cannot be
   claimed until the agent ingests `traceparent` on the A2A method surface, not only
   the self-MCP request / `AGENTD_TRACEPARENT` (§8.1). Owned with agentctl RFC 0013.
9. **Loki vs CRI-only for bulk on a no-stdout substrate.** §6 assumes the kubelet/CRI
   captures stderr on every tier; confirm this holds for the off-pod/sidecar variants
   (agentctl RFC 0002 §4.3) where the agent's stderr routing may differ, and define
   the fallback (ring-as-bulk is forbidden, §6.1) if a substrate does not surface
   stderr to the node log agent.

---

## 14. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — Stack & repo decision record: the `kube-rs`/`tonic`
  runtime the `agentctl_*` self-obs (§10) is emitted from, the codegen + black-box
  conformance (P0) the schema-branching (§11.2) and additive-drift report (§5.6) rest
  on, the Phase-1 "Observe" roadmap, the Prometheus/Loki/collector deploy charts.
- **agentctl RFC 0002** — Substrate & transport abstraction: the endpoint descriptor
  (`surface: metrics|events`) the bridge reads (§4), the networkless-pod reality and
  the TCP-vs-in-socket metrics dial (§4.4), the P4 conflict (§13(h)), the exec-health
  verb (P1) the probes (§4.5) need, attestation gating the read.
- **agentctl RFC 0003** — Agent & AgentFleet CRD schema & status contract: the curated
  `status.lastRun` this RFC feeds (§7.2), the rule that churny telemetry stays out of
  `.status`, the `Ready`/`Degraded` conditions (reasons
  `ManagementUnreachable`/`AttestationFailed`) the bridge surfaces.
- **agentctl RFC 0006** — Operator reconcile & capability model: the single
  `.status` writer (the node-agent never writes status, §7.2), the `podFailurePolicy`
  **render** (§8.6) this RFC reads exit codes alongside, the managed-pod label edge
  the `http_sd` SD draws from (§4.2), the manifest-driven surface gating (§11.2).
- **agentctl RFC 0007** — Admission validation ladder: the webhook whose
  `agentctl_admission_*` SLOs (§10.1) this RFC observes.
- **agentctl RFC 0008** — node-agent architecture (two tiers): **Tier A is where this
  RFC's telemetry bridge lives** — the scrape-proxy, the events tail, the
  run-outcome collector; the bounce-safe invariant scoped to Tier A (§10.2), the
  observed-snapshot transport.
- **agentctl RFC 0009** — Management access path & RBAC: the caller identity the
  management-action audit (§10.2, P-meta/P-audit) attributes verbs to.
- **agentctl RFC 0011** — Scaling plane: consumes the frozen scaling signals (§5.4),
  owns the KEDA external scaler + the P10 reconciliation + the scale-from-zero (P9)
  signal this RFC defers to it.
- **agentctl RFC 0012** — Intelligence plane: owns the price table + the proxy's
  per-backend metering + the chosen token source (§9.2) + cost enforcement (P-cost);
  the intel-endpoint metric-label stability (P7).
- **agentctl RFC 0013** — A2A gateway & task store: the **trace root** (§8.1, P-trace),
  the durable store option for run-report history (§13.2), the gateway SLOs (§10.1).
- **agentctl RFC 0016** — CLI & kubectl-plugin grammar: `kubectl agents top` (reads
  metrics/rollups), `results` (reads the captured run-report store, §7), `logs -f`
  (consumes the `agentd://events` live tail, §6.2) — the consumers of this backend.
- **agentctl RFC 0017** — Release & lifecycle engineering: retention/DR for the
  telemetry stores (§12 non-goal, §13.2 open question).

**Contract spec (the reference implementation, agentd RFCs)**

- **agentd RFC 0016 (the reference impl's contract spec)** — telemetry & lifecycle
  contract: the **frozen metrics schema** (§4) this RFC authors against, the
  exit-code→`podFailurePolicy` intent (§5), the **run-outcome report** + `--report-file`
  (§6) this RFC captures, the **`agentd://events`** ring (§7) tailed for live, the
  version-surfacing this RFC branches on (§8), the cost-is-tokens×price rule (§4.3).
- **agentd RFC 0010 (the reference impl's contract spec)** — observability, health &
  telemetry: the **JSON-lines log schema + closed event vocabulary** (§3.2/§3.3) shipped
  to Loki, the **Prometheus 0.0.4 exposition** (§3.8) the proxy re-exposes, the **W3C
  trace propagation** (§3.6) this RFC stitches by `trace_id`, the **health surface**
  (§3.7) the probes wire, `agent_path` validity scope (§3.5).
- **agentd RFC 0011 (the reference impl's contract spec)** §5 — the **exit-code table**
  (§7.4) and the clean-drain-returns-`0`-not-`143` distinction the forced-drain alert
  keys off; §4.2 the drain choreography.
- **agentd RFC 0007 (the reference impl's contract spec)** §3.4 — the closed
  `TerminalStatus` vocabulary used as `report.status` and the `agentd_runs_total{status}`
  label domain.
- **agentd RFC 0005 (the reference impl's contract spec)** — the `agentd://` scheme,
  notify-then-read, and `agentd://run/{run_id}` (§7.2) the run collector subscribes;
  the in-socket metrics-resource ambiguity (P4, §4.4).
- **agentd RFC 0014 (the reference impl's contract spec)** §3 — primitives-not-policy
  (the agent serves one instance, agentctl aggregates the fleet, §9); §5/§6.2/§8 —
  the manifest + `surfaces{}` + graceful degradation the §11.2 branching keys off.
- **agentd RFC 0015 (the reference impl's contract spec)** — the management profile
  the bridge multiplexes alongside telemetry; §8 reconnect = clean re-read (a dropped
  connection is not a liveness signal, §4.5).
- **agentd RFC 0018 (the reference impl's contract spec)** — the intelligence-health
  metrics (`agentd_intel_up`/`_errors_total`) and the per-endpoint model/name stability
  (P7) the cost/health panels (§9.2) read.
- **agentd RFC 0019 (the reference impl's contract spec)** — horizontal scaling: the
  source of the P10 metric-name defect (§5.4) and the `agentd://capacity` (P4) ask.
- **agentd RFC 0020 (the reference impl's contract spec)** — A2A over the substrate:
  the surface P-trace adds traceparent ingest to (§8.1) and the exactly-once
  distillate delivery that forces P5 (§7.3).

**Contract asks raised or cited by this RFC** (agentctl brainstorm §14): **P4**
(define `agentd://metrics`/`agentd://capacity` — §3/§4.4, the networkless blocker),
**P5** (run-report read-before-exit/linger + re-read by handle — §7.3), **P-trace**
(traceparent ingest on the A2A surface — §8.1), **P-hist** (reconcile histogram names +
freeze buckets — §5.5), **P10** (reconcile the autoscaling metric names — §5.4),
**P7** (stable intel-endpoint names so metric labels survive reorder — §9.2),
**P-meta/P-audit** (caller `_meta` + the `mgmt.invoked` audit event — §10.2),
**P1** (exec-health verb — networkless probes, §4.5, agentctl RFC 0002 §8),
**P-cost** (budget-exhausted signal for cost enforcement — §9.2, agentctl RFC 0012).
