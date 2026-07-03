# agentctl RFCs — index

> **Note (contract is vendor-neutral):** the control contract these RFCs consume is
> vendor-neutral, so **any** agent can implement it. The canonical wire forms are
> `agent_*` (metric prefix) / `agent://` (URI scheme) / `AGENT_*` (env family) — these
> are neutral, not brand names. The **reference implementation is `agentd` 2.x**,
> which speaks exactly that neutral contract (and keeps `agentd://` as a legacy alias).
> The reference implementation's repo dir is `agentd-dev`. See `contract/README.md` and
> `contract/SPEC.md` (L4).

> ⚠️ **Contract 2.0 pivot — read [RFC 0021](0021-contract-2.0-network-substrate-pivot.md) first.**
> The reference agent's v2 refactor removed every non-HTTP transport (stdio/unix/vsock)
> and the exec surface: agents now **serve mTLS HTTPS** (`POST /mcp`) and **dial the
> gateways keyless**, the **node-agent is retired**, and identity is **cryptographic** (a
> verified client cert, or an attested source IP) — *"the network is the substrate;
> identity is the boundary."* **RFC 0021 is the authoritative current design;** it
> supersedes-in-part **0002** (transport), **0008** (retired), **0009/0010/0012/0013/0015/0019**
> (amended). Each carries a banner. The RFCs below record the v1 design as built; where
> they and RFC 0021 disagree on the substrate/transport model, **0021 wins.**

This directory holds the agentctl RFC set. **agentctl is the Kubernetes control
plane for *conformant agents*** — it provisions, reaches, scales, observes, and
manages a fleet of agents, and exposes their public A2A surface. **RFCs 0001–0020
are written** (Proposed): the **0001–0018 core track** below, plus the **0019–0020
tools/identity track** — MCP server registration, identity & authentication (0019)
and instruction sourcing & live delivery (0020) — which extend the earlier CRDs
(RFC 0004 §5's `MCPServerSet`, RFC 0003's `instruction`) with the secret-free MCP
broker and the live-instruction resolver (§ "The tools/identity track", below). The
**0001–0003** RFCs are the **foundational track**:
they fix the questions every later component RFC inherits — the implementation
stack and repo shape (0001), the data-plane *reach* abstraction (0002), and the
declarative CRD API + status contract (0003). **0004–0007** build directly on them
— the ops/dev decoupling CRDs (0004), CRD versioning & conversion (0005), the
operator reconcile & capability model (0006), and the admission validation ladder
(0007). **0008–0010** are the **runtime / operations track** that turns the
declarative model into live action and observation — the on-node keystone every
plane reaches the data plane through (0008 node-agent, two tiers), the management
access path & RBAC that fronts it (0009), and the observability & telemetry bridge
(0010). **0011–0014** are the **plane track** built on all of the above — the
scaling plane (0011), the intelligence plane (0012), the A2A gateway & task store
(0013), and the agent mesh identity layer (0014). **0015–0018** are the
**cross-cutting / interface / lifecycle track** that completes the set — the security
& multi-tenancy capstone that resolves the deferred trust-model, PKI, egress, and
supply-chain seams (0015), the CLI & kubectl-plugin grammar that is the human client
(0016), the release & lifecycle engineering — rolling upgrades, skew, DR, GitOps,
air-gap (0017), and the codegen & contract-conformance machinery that operationalizes
P0 anti-drift (0018). All cross-reference one another by number rather than restating
detail.

## P0 — depend on the contract, never on a specific agent

**The single load-bearing principle of this track (locked 2026-06-27):** agentctl
depends on the **contract**, not on any one agent binary. The data plane is *any*
agent that conforms to the published, language-neutral control contract — the
capabilities manifest, the management MCP profile, the frozen metrics + exit-code
contract, the config schema, A2A over the substrate, and the downward-API env
convention. **agent is the reference / first implementation, not a dependency.**
Future agents from other vendors implementing the same contract must be manageable
by agentctl unchanged. Concretely: the anti-drift mechanism is *conformance to a
published contract + a behavioral conformance suite* (never a shared agent type);
agentctl codegens its client from the **contract schemas**, never from a data-plane
binary's source; and every RFC is written against "a conformant agent" / "the
contract," naming agent only as the reference implementation in worked examples.

**The contract these RFCs consume is currently specified by agentd RFCs
0014–0020** (the reference implementation's control-plane track). That contract is
implementation-neutral but is *presently authored inside the agent repo*.
**Extracting it into a standalone, neutral "Agent Control Contract" spec — its own
home with published, versioned JSON Schemas, so neither agentctl nor any agent owns
the other — is the standing P0 open question** (agentctl RFC 0001 §9 / RFC 0002
open question (a)). Until extraction, agentctl vendors the schemas under
`contract/`, pinned by `(contract major.minor + digest)`, and the contract surfaces
remain agent-branded (the `--capabilities`/`AGENT_SERVE_MCP` entrypoints, the
`agent://` URI scheme, the `agent_` metric prefix, the `AGENT_*` env family) —
all flagged for neutralization.

## The binding decision record

**The binding pre-RFC decision record is
[`docs/design/agentctl-architecture-brainstorm.md`](../docs/design/agentctl-architecture-brainstorm.md).**
The four locked decisions are in **§0.6** (2026-06-27): **P0** (contract, not
agent); **D1 — substrate** (stock-unix PRIMARY + Kata-hybrid HARDENED, converging
on "open a discovered socket"); **D2 — stack** (Rust for all five components, one
`kube-rs` control-plane ecosystem, overriding the analysis's Go recommendation);
**hostile multi-tenancy in v1**; and **all planes in v1** (not a thin MVP). Where
an RFC and that record diverge, the record wins and the RFC is refined to match.
Where an RFC and an agent contract spec disagree *on the wire*, the contract wins
and the RFC files a primitive ask (the cross-repo critical path, brainstorm §14).

## The written RFCs

| RFC | Title | Status | Scope (one line) |
|---|---|---|---|
| [0001](0001-stack-and-repo-decision-record.md) | Stack & repo decision record | Proposed (foundational) | Rust for all five components on `kube-rs`, one Cargo workspace; the kube-rs gaps vs `controller-runtime`/cert-manager and their closure; the contract-as-schema (P0) anti-drift strategy (codegen + black-box conformance) that replaces the void shared-wire-crate argument; revisit triggers. |
| [0002](0002-substrate-and-transport-abstraction.md) | Substrate & transport abstraction | Proposed (foundational) | The endpoint descriptor — the one *reach* abstraction every plane programs against; three substrate tiers (stock-unix / kata-hybrid / sidecar-emptydir) that converge on "open a discovered socket"; tenancy×substrate forced resolution; pod→socket attestation; networkless-pod probes; the exec-health (P1) and contract-extraction (P0) asks. |
| [0003](0003-agent-and-agentfleet-crds.md) | Agent & AgentFleet — CRD schema & status contract | Proposed (foundational) | The `Agent` / `AgentFleet` CRDs: contract-shaped `.spec` (incl. inline `image` for a classless Agent), curated `.status` projection, mode→workload rendering (the double-processing / double-schedule traps), CEL single-object invariants vs the mandatory webhook, single-served-version + conversion + SVM posture decoupled from the contract version, downward-API env injection. |
| [0004](0004-agentclass-intelligenceservice-mcpserverset.md) | AgentClass, IntelligenceService (ModelPool) & MCPServerSet | Proposed | The ops/dev decoupling CRDs an `Agent`/`AgentFleet` points at: `AgentClass` (substrate tier + tenancy posture + contract-version pin home), `IntelligenceService` (ordered model endpoints, zero-secret-in-pod via the egress proxy), `MCPServerSet` (reusable per-tool-glob-tagged tool bundles); deterministic deep-merge-maps / replace-lists / ADD-MCP-servers override semantics. |
| [0005](0005-crd-versioning-and-conversion.md) | CRD versioning & conversion policy | Proposed | One served + stored version at steady state; `conversion: None` only there (the pruning data-loss trap); a bump = hand-written conversion webhook + transition window + `StorageVersionMigration`; alpha→beta→GA graduation; the CRD `apiVersion` clock decoupled from the agent `contract_version` clock. |
| [0006](0006-operator-reconcile-and-capability-model.md) | Operator reconcile & manifest-driven capability model | Proposed | Two controllers on one manager; the two-path STATIC(image)/LIVE(instance) capability model + the digest-keyed `CapabilityProbe` cache; manifest-driven rendering that keys off `surfaces{}` (never `build_features`); per-kind finalizer drain choreography; the single `DeepEqual`-guarded `.status` writer + the reconcile-correctness discipline. |
| [0007](0007-admission-validation-ladder.md) | Admission validation ladder | Proposed | Four rungs cheapest→most authoritative (CEL → webhook cross-object/policy → cached config-schema → init-container ground truth in the exact image); trifecta union advisory + the gated `allowTrifecta` override; fail-closed wiring with the bootstrap-deadlock + operator-SA exemptions; `auditAnnotations` dry-run-safe audit. |
| [0008](0008-node-agent-architecture.md) | node-agent architecture (two tiers) | Proposed | The on-node keystone as a *process set*: a bounce-safe Tier A (control + telemetry) and an HA Tier B A2A data path (node-pinned relay + replicated stateless gateway), intelligence-proxy-out; the discovery/connection-manager/attestation implementation of the RFC 0002 abstractions; the mTLS API *shape* + per-target-namespace authz chokepoint (policy is 0009); per-tier failure/blast-radius/upgrade. |
| [0009](0009-management-access-path-and-rbac.md) | Management access path & RBAC | Proposed | Split by caller: operator → node-agent direct mTLS; humans → an aggregated APIServer (the single sanctioned RFC 0001 §6 hybrid seam, Go out-of-workspace) so per-verb RBAC + end-user identity survive; why raw `pods/proxy` fails under hostile tenancy (admin/single-tenant stopgap only); the CRD-stays-on-kube-apiserver vs verbs-on-a-distinct-aggregated-GroupVersion split; the attach/inject no-puppeting gate. |
| [0010](0010-observability-and-telemetry-bridge.md) | Observability & telemetry bridge | Proposed | The node-agent (Tier A) as the single networked telemetry bridge for networkless agents: a byte-identical metrics scrape-proxy + central `http_sd`, stderr→Loki bulk vs `agent://events` live-tail, run-outcome capture before once/Job GC, exit-code observability (137/143 from pod status, not the report), `trace_id` correlation, fleet rollups + cost×price-table, control-plane self-observability; the caller→proxy hop locked down under hostile tenancy. |
| [0011](0011-scaling-plane.md) | Scaling plane | Proposed | The elastic plane: claim vs shard regimes (Deployment+KEDA `ScaledObject` vs StatefulSet with the shard-resize controller), the `crates/scaler` KEDA external scaler reading an off-pod backlog (scale-from-zero, P9), the reference coordination MCP server (atomic lease + `claim_key` dedupe), drain→bleed→release on SIGTERM, the claim-only `scaling.min`/`scaling.max` range vs the shard-only `scaling.shards` partition count `N`; KEDA-owns-replicas, exactly one replica-field writer. |
| [0012](0012-intelligence-plane.md) | Intelligence plane | Proposed | The egress proxy data path behind the one model endpoint (out of the node-agent, a data-PATH component): two-level resilience (proxy within-pool LB/breaker vs agent across-pool failover), zero-secret-in-pod, dialect pass-through-vs-translate read from the agent's manifest (P-dialects), the price table + chosen token source, tiered cost governance (per-run hard → fleet best-effort → fleet hard gated on P-cost). |
| [0013](0013-a2a-gateway-and-task-store.md) | A2A gateway & task store | Proposed | The replicated stateless A2A HTTP gateway (the A2A PEP: TLS/auth/SSE/webhooks/rate-limit/version-negotiation) fronting the node-pinned relay (RFC 0008); the shared durable task store (Postgres; tenant as a row-level predicate); live-vs-durable method routing; the JSON-RPC binding-string commitment (P2); SSRF-guarded encrypted webhooks; delegation-out. |
| [0014](0014-agent-mesh-identity.md) | Agent mesh identity | Proposed | One fleet = one signed Agent Card: the fleet-level card projection (from contract capabilities, stable at `replicas: 0`), central JWS signing with a pinned out-of-band JWKS trust anchor (never per-node/per-gateway), the in-cluster catalog/discovery registry, deterministic CRD↔mesh naming, the deferred federation seam. |
| [0015](0015-security-and-multi-tenancy.md) | Security & multi-tenancy | Proposed | The cross-cutting security capstone: the transport-is-the-boundary trust model and its four caller→agent PEPs + the egress-authority point; Kata-mandatory hostile multi-tenancy; attested pod→socket; the no-puppeting gate (P-attach-gate home); the five-class secret/PKI lifecycle (internal mTLS CA, card-signing key, A2A inbound trust, data-at-rest envelope keys, provider creds); the two-layer egress restriction + SSRF allow-list; signed-image admission, SBOM/provenance, isolated control-plane execution of tenant images; the closed `mgmt.invoked`-class audit vocabulary + double-audit invariant; the consolidated threat model. |
| [0016](0016-cli-and-kubectl-plugin.md) | CLI & kubectl-plugin grammar | Proposed | The human client: three faces over one `crates/cli` (`kubectl-agent`/`kubectl-agents`/`agentctl`), manifest-rendered verb *visibility* (contract-vocabulary typed + generic passthrough for novel vendor tools), the cold/live/gateway path split (four cold back-ends with distinct auth — only the kube-apiserver reuses kubeconfig), the `attach` steering UX (lease + multi-viewer, single-warm-session-bound until P-session), output/negotiation/exit-code contract; a client, never a new access path. |
| [0017](0017-release-and-lifecycle.md) | Release & lifecycle engineering | Proposed | The three-version-clock lifecycle: the contract-keyed agent-image rolling upgrade (re-probe → re-negotiate → contractVersionRange **and** requiredSurfaces gate → canary on exit-0/health → roll back as a cache hit); the agentctl component upgrade & skew matrix + the CRD-bump SVM inner loop; DR concentrated on the three irreplaceable stores; the GitOps ownership boundary (SSA, KEDA-owns-replicas, finalizer-honouring prune); air-gap as the default-clean path. |
| [0018](0018-codegen-and-contract-conformance.md) | Codegen & contract conformance | Proposed | The detailed spec of RFC 0001 §4 anti-drift: the three sources of truth (neutral schemas / generated `agent-contract-client` / black-box behavioral conformance suite) + runtime negotiation; the four codegen lanes and hand-written `surfaces{}` sum-type deserializers; pinning by `(contract major.minor + schema digest)`, never an agent SHA; the contract-extraction + brand-neutralization plan; the conformance assertions (semantic, contract-derived required/optional partitions) + the E2E/chaos + kind/Kata test matrix. |
| [0019](0019-mcp-server-registration-identity-and-authentication.md) | MCP server registration, identity & authentication | Proposed (tools/identity track) | Realizes RFC 0004 §5's `MCPServerSet` as a CRD and extends it with per-server identity/auth (`transport`/`endpoint`/`auth` union/`budget`); the **MCP broker** data path mirroring RFC 0012's ModelGateway (credential held off the pod, keyless dial, out of the node-agent); the full MCP 2025-06-18 OAuth flow (RFC 9728 PRM → 8414 → DCR/CIMD → 8707 Resource Indicators → audience binding, no token passthrough) performed by the broker; two tiers — **Tier 1 headless** (client-credentials SEP-1046 + `private_key_jwt` RFC 7523) and **Tier 2 on-behalf-of-user** (EMA/ID-JAG SEP-990, human-principal-only); attested-peer workload identity (SO_PEERCRED/vsock, SA-token/SPIFFE optional) on the RFC 0012 §5.4 authz chokepoint; the broker as the runtime PDP/PEP (OAuth/EMA stop at issuance); the stdio↔broker bridge for the stdio-only reference agent (contract ask P-mcp-egress). |
| [0021](0021-contract-2.0-network-substrate-pivot.md) | **Contract 2.0 — the network is the substrate** | **Proposed (pivot track)** | The re-architecture that realigns agentctl to agentd v2: agents serve mTLS HTTPS `POST /mcp` and dial the gateways keyless; the node-agent is **retired** (every function re-homed to a network-native path); identity is cryptographic (mTLS client cert into agents, attested source IP into gateways) — *"identity is the boundary,"* superseding transport-is-the-boundary. Supersedes-in-part 0002 (transport)/0008 (retired)/0009/0010/0012/0013/0015/0019; ratified by the contract-2.0 re-vendor (RFC 0018). Provisioning+PKI, management, intelligence, A2A, MCP tools, workflows, node-agent retirement, the consolidated identity model, and the migration/compatibility posture. |
| [0020](0020-instruction-source-and-live-delivery.md) | Instruction source & live delivery | Proposed (tools/identity track) | Generalizes `AgentSpec.instruction` into a closed `instructionSource` union (`inline`/`configMapKeyRef`/`secretKeyRef`/`file`/`url`/`mcpResource`); a **resolver** that materializes any source into the agent's instruction input (the agent never fetches — static sources render the startup instruction with no contract change; live sources refresh via `url` polling / `mcpResource` subscription); secretless ingress (the fetch/OAuth credential stays in the resolver/broker, never the pod); `url` typed+authenticated+polled+bounded; `mcpResource` subscription-driven live instruction composed with RFC 0019's broker; the sourced instruction treated as an `untrusted_input` trifecta leg with an admission allow-list + provenance; atomic, turn-boundary, change-gated reload — **hot (no-restart) once instruction is made reloadable (contract ask P-instr-file), managed roll as the interim**. |

These eighteen are P0-disciplined and mutually consistent: they share the tier names
(`stock-unix` / `kata-hybrid` / `sidecar-emptydir`), the "conformant agent / the
contract / the reference implementation" vocabulary, the contract-ask IDs (CC, P1, P2,
P3, P3b, P4, P5, P6, P7, P9, P10, P12, P-meta, P-audit, P-trace, P-hist, P-pause,
P-seq, P-a2a-out, P-cost, P-dialects, P-attach-gate, P-inject, P-session, …), the shared CRD field shapes
(the per-tool-glob MCP `tags` map, the effective-tenancy
`max(namespace-label, AgentClass.substrate.tenancy)` rule, the claim-only
`scaling.min`/`scaling.max` range vs the shard-only `scaling.shards` partition count
`N` — RFC 0003 §4.1), the two-tier node-agent model (Tier A control+telemetry / Tier
B A2A data path: a node-pinned relay + a replicated stateless gateway), the
KEDA-owns-replicas single-writer rule, the canonical status taxonomy (`Ready=False`
with reason `ManagementUnreachable`/`AttestationFailed` — RFC 0003 §6.2, *not* a
separate `ManagementReachable` condition), and the locked decisions above. 0009
fronts the 0008 management API with access policy; 0010's bridge lives inside 0008
Tier A; 0011 reads the 0003 scaling fields and drives KEDA; 0012's egress proxy is
the data-PATH component kept out of the 0008 node-agent; 0013's gateway fronts the
0008 relay and is signed by 0014's central card-signer. The capstone four close the
loop: 0015 names the four caller→agent PEPs (admission 0007, management 0009 + the
node-agent chokepoint 0008, the A2A gateway 0013) plus the egress-authority point
(the 0012 proxy), resolves the deferred PKI/egress/secret seams, and is the home of
`P-attach-gate`; 0016 is the kubectl-native client of 0009's access path; 0017
sequences the machinery 0005/0006/0008/0011/0013/0014 own into rolling/skew/DR/GitOps/
air-gap choreography; and 0018 is the detailed spec of 0001 §4 anti-drift. All keep
agent as the reference implementation only (P0).

## The tools/identity track (0019–0020)

Two RFCs extend the core track with the **tool-facing identity plane** the earlier
CRDs deferred. **0019** realizes RFC 0004 §5's `MCPServerSet` as a real CRD and adds
the per-server **identity & authentication** 0004 §5 left open — applying the exact
0004-owns-the-schema / 0012-owns-the-runtime split to tools: a secret-free **MCP
broker** (the RFC 0012 ModelGateway pattern, one plane over) holds the credential
off the hostile-tenant pod, performs the MCP 2025-06-18 OAuth flow, and exposes each
server keyless; **Tier 1** (headless, SEP-1046 client-credentials + `private_key_jwt`)
covers autonomous agents and **Tier 2** (EMA/ID-JAG, SEP-990) covers on-behalf-of-a-
human enterprise access; the agent→broker hop reuses the RFC 0012 §5.4 attested-peer
authz chokepoint, and the broker is the runtime PDP/PEP because OAuth/EMA stop at
issuance. **0020** generalizes RFC 0003's inline `instruction` into an
`instructionSource` union (inline / ConfigMap / Secret / file / typed-authenticated-
polled `url` / subscription-driven `mcpResource`) delivered into the agent's instruction
input by a **resolver** that keeps live sources fresh and holds any fetch credential off
the pod (static sources need no contract change; *hot* no-restart live reload needs the
instruction made reloadable — contract ask P-instr-file — with a managed roll as the
interim) — with the sourced text
treated as an `untrusted_input` trifecta leg, admission-allow-listed, and
provenance-recorded. The two compose: 0020's `mcpResource` instruction is
authenticated by 0019's broker (one MCP-identity model, two consumers). Both preserve
every core-track invariant — **P0**, secret-free pod, out-of-node-agent egress,
attested identity, hostile-multitenancy isolation — and neither gates an MVP
milestone (both are additive, inline/no-MCP agents run untouched). The design research
behind them is persisted, fully sourced, in
[`docs/design/mcp-auth-research.md`](../docs/design/mcp-auth-research.md).

## The full proposed track

The **core** agentctl RFC track — **agentctl RFC 0001–0018** — is enumerated in the
brainstorm **[§15](../docs/design/agentctl-architecture-brainstorm.md)** (ordered
roughly by dependency: foundational stack/substrate/CRD first, then the ops/dev
CRDs, versioning, operator reconcile, admission, node-agent, management access,
observability, scaling, intelligence, A2A gateway + mesh identity, security &
multi-tenancy, CLI/kubectl-plugin, release engineering, and codegen & conformance).
The **0019–0020 tools/identity track** extends it beyond that original brainstorm
enumeration (prompted by the MCP-authentication + instruction-sourcing work; design
research in [`docs/design/mcp-auth-research.md`](../docs/design/mcp-auth-research.md)).
The phased build roadmap with the explicit MVP cut line is **§16**, and the top
human-decision open questions are **§17**.

**Track complete (drafted), then pivoted.** All twenty core RFCs (**0001–0020**) were
written (Proposed), and **[RFC 0021](0021-contract-2.0-network-substrate-pivot.md)** then
re-architected the substrate/transport model for **contract 2.0** (the reference agent's
v2 realignment — see the banner above; it supersedes-in-part 0002/0008/0009/0010/0012/0013/0015/0019).
The core track (Proposed): the foundational stack/substrate/CRD track (0001–0003), the ops/dev CRDs
+ versioning + reconcile + admission (0004–0007), the runtime/operations track
(0008–0010), the plane track (0011–0014), the cross-cutting security / interface
/ lifecycle / conformance capstone (0015–0018), and the tools/identity track
(0019–0020: MCP registration/identity/auth + instruction sourcing). The forward
references the plane RFCs made to agentctl RFC 0015 (the cross-cutting trust model,
internal PKI, the egress allow-list, per-tenant isolation, and `P-attach-gate`) are
now **resolved** in the authored 0015; 0016/0017/0018 likewise fill the CLI, lifecycle,
and conformance seams the earlier RFCs deferred; and 0019/0020 realize the `MCPServerSet`
(RFC 0004 §5) and `instructionSource` (RFC 0003) seams the CRD track reserved. **The
next gates are execution, not more RFCs:** the phased
build roadmap (brainstorm **§16**, with the explicit MVP cut line) and the **cross-repo
critical path** — the ~12 agent contract primitives/fixes v1 depends on (brainstorm
**§14**), since agent repo work must lead agentctl work for each plane (brainstorm
§0.6).

## Supporting material (non-normative)

In [`docs/design/`](../docs/design/): `agentctl-architecture-brainstorm.md` (the
binding pre-RFC record synthesizing ten per-dimension designs, their red-teams, and
four cross-cutting analyses) and `ideas.md` (the original vision; superseded where
the brainstorm revises it, brainstorm §13). The contract track these RFCs consume
lives in the sibling **agent** repository's `rfcs/` directory (the reference
implementation's RFCs 0014–0020).
