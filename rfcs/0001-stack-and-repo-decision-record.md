# agentctl RFC 0001: Stack & repo decision record

> 📌 **Decision update (contract 2.0, [RFC 0021](0021-contract-2.0-network-substrate-pivot.md)).**
> **D2 (Rust, all components), the repo shape, and the anti-drift/P0 principle stand
> unchanged.** **D1 (substrate) is narrowed:** the *transport* half — "unix-socket-over-hostPath
> as the primary substrate, converging on 'open a discovered socket'" — is **superseded**. In v2
> the network **is** the substrate: agents serve mTLS HTTPS and dial the gateways keyless, and
> the node-agent is retired. The *Kata tenancy-hardening* half of D1 survives (a pod may be a
> Kata VM, now reached over the network). See RFC 0021 and the D1 note in the brainstorm §0.6.

**Status:** Proposed (agentctl foundational track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the foundational decision record that fixes language and repo shape for all five components, and the anti-drift principle every later RFC builds on

> **P0 (locked 2026-06-27).** agentctl depends on the **contract**, never on a
> specific agent. The data plane is *any* agent that conforms to the published,
> language-neutral contract — the capabilities manifest, the management MCP
> profile, the frozen metrics + exit-code contract, the config schema, A2A over
> the substrate, and the downward-API env convention. The reference
> implementation (the agent whose contract is currently authored in agentd RFCs
> 0014–0020) is the **first** conformant agent, **not a dependency**. This RFC
> chooses agentctl's *own* implementation stack; it deliberately does **not**
> choose, constrain, or couple to the data plane's stack.

> **This is a decision record, not a design.** It exists so the stack question is
> answered once and not relitigated. It records the *context*, the *decision*, the
> *consequences agentctl must absorb*, and the *exact triggers* under which the
> decision should be reopened. The honest analysis recommended Go; the decision is
> Rust; the record below makes both the call and its cost legible.

---

## 1. Problem / Context

agentctl is **five Kubernetes-shaped components**, not one program:

1. **operator** — leader-elected controllers for `Agent` / `AgentFleet` (and the
   ops-owned `AgentClass` / `IntelligenceService` / `MCPServerSet`), rendering
   workloads and reflecting a curated status (the reconcile/render loop is agentctl
   RFC 0006; the CRD schema + status contract it reflects is agentctl RFC 0003).
2. **node-agent** — the on-node bridge (DaemonSet) that holds the management
   connection to each local conformant-agent pod over a discovered socket
   (agentctl RFC 0002 / RFC 0008).
3. **A2A gateway** — the HTTP/SSE/auth/webhook policy-enforcement point that
   bridges the public A2A surface to the agent's A2A-over-the-substrate surface
   (agentctl RFC 0013).
4. **CLI / kubectl-plugin** — `agentctl` standalone plus the `kubectl agent[s]`
   plugin faces (agentctl RFC 0016).
5. **KEDA external scaler** — a gRPC service implementing KEDA's `ExternalScaler`
   contract for the scaling plane (agentctl RFC 0011).

Two questions must be answered **once**, before any component RFC, because every
later decision inherits them: **(a) what language** are these written in, and
**(b) what repository shape** holds them. They are foundational because reversing
either after components exist is a rewrite, not a refactor.

The honest difficulty is that the two questions pull in different directions:

- **Ecosystem fit points at Go.** The Kubernetes control-plane ecosystem is Go's
  home turf. `controller-runtime`/`kubebuilder` is the *reference* operator
  framework; `controller-runtime` ships a webhook server (with a `CertWatcher`
  that hot-reloads serving certs from disk) and leader election, and `kubebuilder`
  scaffolds conversion webhooks, drives `controller-gen` CRD generation, and wires
  in **cert-manager** for webhook-cert provisioning — much of it near-free. (Note
  the precise mechanics, since §3 leans on them: automatic cert *minting,
  rotation, and `caBundle` injection* come from cert-manager + ca-injector, **not**
  from `controller-runtime` core, which only reloads a cert from disk.) `client-go`
  + `cli-runtime` + the Krew idiom own the kubectl-plugin
  story. KEDA's reference external-scaler is Go. An honest greenfield analysis
  (brainstorm §1, D2) recommended **Go for all five** and found *no component
  Rust wins on ecosystem*.
- **The team and a single control-plane ecosystem point at Rust.** One language
  for one control plane, with the team's existing Rust + `kube-rs`/`tokio`
  fluency, removes a second toolchain, a second idiom set, and a second hiring
  surface from the project.

Historically there was a *third* argument for Rust — "share a typed wire crate
with the data-plane agent so the two repos cannot drift." **P0 deletes that
argument.** Importing a crate from a specific agent binary is precisely the
forbidden coupling: it would make agentctl depend on *one* agent rather than on
the contract, and would make a second-vendor conformant agent unmanageable. So
the stack decision must be made **without** the shared-crate rationale, and the
anti-drift mechanism must be redesigned to be language- and vendor-neutral
(§4). That redesign is the load-bearing part of this RFC.

This RFC records the decision (Rust), the gaps it opens versus the Go path
(§3), the contract-conformance strategy that replaces the void shared-crate
argument (§4), the concrete monorepo layout (§5), and the precise revisit
triggers (§6).

---

## 2. Decision — Rust for all five, one Cargo workspace (the record)

| Field | Value |
|---|---|
| **Decision** | All five components in **Rust**, on the **`kube-rs` ecosystem**, in **one Cargo workspace monorepo**. |
| **Status** | Accepted (overrides the analysis's Go recommendation; brainstorm D2, locked 2026-06-27). |
| **Context** | Five control-plane components; team Rust/`kube-rs` fluency; a single-control-plane-ecosystem goal; **P0** voiding the only contract-level pro-Rust argument. |
| **Rejected alternatives** | Go-for-all-five (the honest ecosystem recommendation); a Rust-node-agent-only hybrid; "shared wire crate with the agent" (forbidden by P0). |
| **Consequences** | agentctl **owns** the `controller-runtime` conveniences `kube-rs` lacks (§3) and **owns** a from-scratch, vendor-neutral anti-drift mechanism (§4). |
| **Revisit triggers** | §6 — a closed set; absent a trigger, the decision is not reopened. |

### 2.1 The decision, stated plainly

agentctl is written in **Rust** across the operator, node-agent, A2A gateway,
CLI/kubectl-plugin, and KEDA external scaler, on the **`kube-rs`** ecosystem
(`kube`, `kube::runtime` — `Controller` + `watcher` + `reflector`/`Store`,
`kube::CustomResource`, `schemars`), `tonic` for gRPC, `axum`/`hyper` for the
HTTP and webhook servers, and `clap` for the CLIs. All five live in **one Cargo
workspace** (§5). agentctl MUST NOT pull a second primary toolchain into the
control plane except via the single sanctioned hybrid escape hatch recorded in
§6.

### 2.2 Why Rust wins *here* (the basis of the override)

The decision rests on two grounds, and **only** these two — the shared-crate
ground is explicitly disclaimed (§2.4):

1. **Team fit.** The team has production `kube-rs` + `tokio` fluency. The Go
   recommendation in the brainstorm is correct *for a team without that
   fluency* — it is the common case, and it is recorded honestly as the
   counter-recommendation (§2.3). Where the fluency exists, the largest cost of
   the Rust path (the kube-rs gaps, §3) is a cost the team can actually pay.
2. **One control-plane ecosystem.** agentctl is itself a coherent system of five
   cooperating binaries that share types (the generated contract client of §4,
   the CRD types, the internal mTLS and telemetry plumbing). One language, one
   build, one dependency graph, one test harness, one release pipeline. The
   *data plane* is a separate concern in a separate repo with a separate
   lifecycle (and, under P0, possibly a separate vendor and a separate
   language) — agentctl's internal cohesion is unrelated to the agent's stack.

Note what is **not** a ground: "agent is Rust." The reference agent being Rust
is a coincidence, not a cause. If the reference agent were Go and a second
conformant agent were written in Zig, agentctl's stack decision would be
unchanged, because agentctl talks to **the contract** (§4), not to a binary's
source.

### 2.3 Why not Go (the counter-recommendation, recorded so it is not relitigated)

The honest greenfield call is Go, and the record states it without hedging so
nobody re-derives it later under the impression it was overlooked:

- **`controller-runtime` is the reference operator framework**; `kube-rs` is a
  CNCF Sandbox project — capable and active, but less mature and with a smaller
  contributor pool and ecosystem.
- **Go gives for free** much of what agentctl must build: a webhook server and
  leader election from `controller-runtime`, conversion-webhook scaffolding +
  cert-manager wiring from `kubebuilder`, and `controller-gen` CRD emission.
  (Automatic webhook-cert provisioning/rotation/`caBundle` injection itself comes
  from **cert-manager** + ca-injector, the *same* cert-manager agentctl uses on
  the Rust side — it is not a `controller-runtime` freebie; §3 enumerates each gap
  and its closure.)
- **The kubectl-plugin and KEDA-scaler stories are Go idioms** (`cli-runtime`,
  the Krew authoring conventions, KEDA's reference scaler).
- **An aggregated APIServer**, if the human management path needs one (D5 /
  agentctl RFC 0009), is *Go-native* (`k8s.io/apiserver`) and a substantial lift
  in Rust — this is the single largest latent Go advantage and is preserved as
  the sanctioned hybrid escape hatch in §6.

The override is sound **only** because team fit (§2.2) is real and because the
gaps (§3) are bounded, one-time engineering rather than recurring tax. If team
fit evaporates, §6 fires.

### 2.4 The "shared wire crate" argument is void under P0 (the crucial point)

The strongest *historical* pro-Rust argument was: "agentctl and the agent both
`use agent_wire::Manifest`, so the two literally cannot drift." **This RFC
explicitly rejects that argument**, on two independent grounds:

1. **It violates P0.** Importing a Rust crate published by a *specific* data-plane
   binary makes agentctl depend on that agent, not on the contract. A
   second-vendor conformant agent — possibly not even written in Rust — could
   then never be managed, because agentctl would be welded to one binary's types.
   The dependency arrow MUST point at the contract, never at an agent.
2. **It is illusory even within one agent** (verified against the reference impl,
   2026-06-27):
   - The reference manifest is built `serde_json::json!` → `Value`
     (`crates/agent/src/capabilities.rs`), **not** a `#[derive(Serialize)]`
     struct. This is **deliberate, not laziness**: the reference impl's `Secret`
     newtype has no `Serialize`, "so it cannot reach this builder" (source
     comment; RFC 0012 §3.7). There is no struct to share, and the no-struct
     property is a *security invariant*, not an oversight.
   - The contract is **runtime-negotiated and additive-by-minor by design** (RFC
     0014 §6.3): a consumer MUST tolerate unknown additive fields/tools/metrics
     and refuse only on an unknown **major**. A shared crate with
     `deny_unknown_fields` would *break* that contract; making it lenient
     dissolves the very "cannot drift" guarantee that motivated sharing it.
   - The decision-critical `surfaces{}` block is JSON sum types
     (`bool | string`, `bool | object`) that defeat typed codegen in *any*
     language and need hand-written deserializers regardless of stack.

Because the shared-crate argument is both forbidden and false, the Rust decision
stands purely on §2.2, and the anti-drift mechanism is rebuilt as
**conformance to a published, language-neutral contract + a behavioral
conformance suite** (§4) — which is exactly what makes the contract portable to
other agents, the thing P0 requires.

### 2.5 Consequences (the cost ledger agentctl signs up for)

| Consequence | What agentctl absorbs |
|---|---|
| **Own the kube-rs gaps** | Build/wire webhook serving + the cert-manager-absent cert fallback, conversion handlers, leader election, the KEDA scaler, and CRD emission that `controller-runtime`/`kubebuilder` give for free (§3). One-time, bounded. (Cert provisioning/rotation proper is cert-manager on both stacks.) |
| **Own a vendor-neutral anti-drift mechanism** | Generate the contract client from published schemas (`agent-contract-client`) and maintain a black-box behavioral conformance suite that drives *any* conformant agent (§4). |
| **Depend on the CC precondition** | The contract must be published as language-neutral, machine-readable schemas (§4.5). Today the reference manifest is untyped `Value` and no versioned schema corpus is published — this is real **contract** work (agent ask P6 / P3b), not agent-coupling work. |
| **Accept the latent aggregated-APIServer cost** | If D5 needs an aggregated APIServer, it is the one place Rust is materially harder; it is the sanctioned hybrid seam (§6). |
| **Gain single-ecosystem cohesion** | One toolchain/build/test/release; shared internal crates; the team's existing fluency applies end-to-end. |

---

## 3. The kube-rs gaps and how agentctl closes them

`controller-runtime` bundles conveniences `kube-rs` leaves to the application.
This is the concrete bill for §2.5. Each row is honest about the gap and names
the mitigation; none is a blocker.

| Capability | `controller-runtime` (Go) | `kube-rs` (Rust) | agentctl's closure | Cost |
|---|---|---|---|---|
| **Controller loop** | `Manager` + `Controller` + shared informer caches | `kube::runtime::Controller` + `watcher` + `reflector`/`Store` | Adopt directly — near-parity; level-triggered reconcile with `Store`-backed caches (agentctl RFC 0006). | Low |
| **Server-side apply / field manager** | `client-go` SSA | `kube::api::Patch::Apply` with a `fieldManager` | Adopt directly; SSA is first-class in `kube`. | Low |
| **Leader election** | Built into `Manager` | Not built in | Wire a `coordination.k8s.io` `Lease` elector via **`kube-leader-election`** or **`kubert`**. | Low |
| **CRD type → OpenAPI v3 schema** | `controller-gen` markers | `#[derive(CustomResource, JsonSchema)]` + **`schemars`** | Derive types; emit CRD YAML from an `xtask` (§5). CEL validation rules authored on the schema (agentctl RFC 0003). | Low |
| **Admission webhook server** | `controller-runtime` provides the webhook server | None — bring your own HTTP server | Run an **`axum`/`hyper`** TLS server in the operator; decode `AdmissionReview` by hand against the generated types. | Medium |
| **Webhook TLS cert provisioning + rotation** | cert-manager (+ ca-injector) mints/rotates the cert and patches `caBundle`; `controller-runtime` only hot-reloads it from disk (`CertWatcher`) | cert-manager does the same; `kube-rs` has no built-in disk-reload watcher | **Default (both stacks): cert-manager** `Certificate` + CA injection — **identical on Go and Rust**, so this is *not* a Go freebie agentctl forfeits. **The real Rust-only delta is the cert-manager-absent fallback:** a small in-repo cert controller that mints a self-signed CA, rotates the serving cert, patches the webhook `caBundle`, and hot-reloads on disk (the `CertWatcher` equivalent). The honest gap is that fallback, materially smaller than "Go gives cert rotation free." | Medium |
| **Conversion webhooks** | Scaffolded by `kubebuilder` | None | Hand-write the conversion HTTP handler — **minimized by policy**: agentctl RFC 0005 mandates a single served CRD version + `StorageVersionMigration`, so conversions are rare by construction, shrinking this gap. | Medium |
| **KEDA external scaler** | Go reference impl | n/a | Implement KEDA's `externalscaler.proto` (`IsActive` / `StreamIsActive` / `GetMetricSpec` / `GetMetrics`) as a **`tonic`** gRPC service in `crates/scaler` (agentctl RFC 0011). | Low–Medium |
| **kubectl plugin + Krew** | `cli-runtime` + Krew idiom | None idiomatic | A **`clap`** binary installed as `kubectl-agent` / `kubectl-agents`; Krew is language-agnostic (it only requires a `kubectl-<name>` binary on `PATH`) — ship Krew manifests in `deploy/`. `kube` handles kubeconfig, so no `client-go` is needed. | Medium |
| **Aggregated APIServer** (if D5 needs it) | `k8s.io/apiserver` (Go-native) | No mature Rust equivalent | **Deferred**; if required, the sanctioned hybrid seam (§6) — a Go component out-of-workspace. v1 may instead ship the `pods/proxy` admin stopgap (agentctl RFC 0009). | High *(if needed)* |

The arithmetic: most rows are Low; the genuinely new engineering is the
**webhook serving + the cert-manager-absent cert fallback** pair and the
**Krew/kubectl-plugin packaging**. Note that cert *provisioning/rotation* is
cert-manager on **both** stacks, so the Rust-only cost there is just the fallback
for clusters without cert-manager — not "rebuild what Go gave free." Both
remaining items are one-time and well-trodden in the `kube-rs` community. The
only High-cost row is conditional and is the explicit hybrid escape hatch, not a
v1 commitment.

---

## 4. Anti-drift: conformance to the contract, not a shared crate (P0)

This section replaces the void shared-crate argument with the mechanism P0
actually requires. The objective is unchanged — agentctl and conformant agents
must not silently drift — but the mechanism is now **language- and
vendor-neutral**: agentctl couples to a *published contract*, generates its own
client from it, and proves any agent against it behaviorally.

```
   THE CONTRACT  (language-neutral, machine-readable; agentctl RFC 0018)
   ┌──────────────────────────────────────────────────────────────────┐
   │  manifest JSON Schema   config JSON Schema   metrics + exit-code   │
   │  (RFC 0014 §5/0015 §5.2)  (RFC 0017)           registry (RFC 0016)  │
   │  management MCP profile (RFC 0015 §4/§5)   A2A methods (0020, P2*)  │
   └───────────────┬───────────────────────────────────┬───────────────┘
        codegen     │ (a) consumed as DATA               │ validated against
                    ▼                                     ▼
   ┌────────────────────────────┐        ┌──────────────────────────────────┐
   │ crates/contract-client      │        │ crates/conformance (black-box)    │
   │ `agent-contract-client`     │        │ drives ANY conformant agent binary│
   │ GENERATED Rust types        │        │ over the substrate transport;     │
   │ (b) — never links an agent  │        │ asserts the contract behaviorally │
   └──────────────┬─────────────┘        └──────────────┬───────────────────┘
                  │ used by all 5 components             │ first subject = the
                  ▼                                      ▼ reference agent (one of many)
            agentctl components ──── negotiate on contract_version (d) ────►  a conformant agent
                                     degrade gracefully off surfaces{} (d)
```

### 4.1 (a) The contract is consumed as published, language-neutral schemas

agentctl treats the contract as **data**, never as a linked binary. The
machine-readable artifacts it consumes:

- the **capabilities-manifest** JSON Schema (the shared spine; RFC 0014 §5,
  RFC 0015 §5.2);
- the **config** JSON Schema (the config file the operator renders to a
  ConfigMap; RFC 0017 `--config-schema`);
- the **frozen metrics schema + exit-code table** (RFC 0016 — the names/labels
  agentctl scrapes and the codes it compiles into `podFailurePolicy`);
- the **management MCP profile** — the operator tool and resource names (RFC
  0015 §4/§5: `drain` / `lame-duck` / `cancel`, `agent://capabilities` /
  `inventory` / `status` / `events`);
- the **A2A method set** and the Task/Message/Part shapes (agentd RFC 0020) —
  **flagged as a *pending* artifact, not a ready one.** The contract has not yet
  frozen the A2A wire-method strings (the reference impl still cites the 0.2.x
  spelling `SendMessage`/`GetTask`/…, while A2A v1.0 renames them
  `message/send`/`tasks/get`/…), and `surfaces.a2a` is not in the frozen manifest
  schema. This surface is a contract **ask (P2)**, not something agentctl can
  generate against today (§4.5); it is listed here for completeness, marked
  pending.

These are JSON Schema documents plus a small method/metric registry — neutral by
construction. agentctl pins a `contract_version` **range** and reads the
artifacts; it does not link, vendor, or compile any agent's source. (The A2A
entry above is the one not-yet-frozen member of this set — see §4.5.)

### 4.2 (b) agentctl generates its own typed client: `agent-contract-client`

A build step (an `xtask` over a schema-to-Rust generator such as `typify`, plus
hand-written deserializers where needed) turns the published schemas into a
**generated** crate, `crates/contract-client` (`agent-contract-client`):

- typed `Manifest`, `Surfaces`, `Identity`, `Limits`, config types, the
  run-report type, the metrics-name registry as constants, the exit-code table,
  and the A2A `Task`/`Message`/`Part` types (plus the A2A method-name constants
  **once the contract freezes the wire strings** — P2; see §4.5);
- the **single home** of the wire shape inside agentctl — every component depends
  on this crate and nothing else for contract types;
- **never** a link against any agent binary's source. The forbidden coupling is
  *structurally impossible*: there is no agent crate in the dependency graph.

Two honesty notes carried from the analysis (brainstorm §11.2): the
`surfaces{}` sum types (`bool|string`, `bool|object`) defeat codegen and get
**hand-written** deserializers scoped to those fields; and because negotiation is
additive (§4.4), the generated types deserialize **leniently** (unknown fields
tolerated, surfaced via an additive-drift report), never `deny_unknown_fields`.

### 4.3 (c) A behavioral, black-box conformance suite

`crates/conformance` is agentctl's executable definition of "a conformant agent."
It mirrors the pattern the reference impl already ships in its own
`agent-conformance` crate, which is **black-box by charter**
(`crates/agent-conformance/src/lib.rs`, verbatim): *"Nothing here links the
agent library: conformance is judged against the MCP / JSON-RPC spec and the
documented exit-code table, not against agent's own types."* It drives the real
binary and so catches real protocol/behaviour regressions instead of agreeing
with the implementation's own types.

agentctl's suite does the same against **any** agent binary, over the substrate
transports (the unix-socket dev fallback is the contract-clean local loop;
agentctl RFC 0002):

- drives `--capabilities` and `agent://capabilities` and validates them against
  the published manifest schema;
- asserts the management profile *behaves* — `drain` ≡ SIGTERM ≡ clean exit 0
  (RFC 0015 §4.1), `lame-duck` flips readiness without exiting (§4.2), `cancel`
  by handle works (§4.4);
- scrapes `/metrics` (or `agent://metrics`) and asserts the frozen names are
  present (the registry of §4.1, never a hand-transcribed list — that
  transcription error is exactly what the suite exists to catch);
- asserts the exit-code table and the A2A method set (once P2 freezes its wire
  strings, §4.5).

**The reference agent is simply the first subject.** Passing this suite is what
makes *any* agent — from any vendor, in any language — manageable by agentctl
unchanged. The suite is the operational meaning of P0.

**Honest scope of "any vendor."** "Conformant agent" today presupposes honouring
the contract surfaces *as the reference implementation currently brands them*: the
`--capabilities` CLI entrypoint, the `agent://` URI scheme
(`agent://capabilities`/`metrics`/…), and the `agent_`-prefixed metric names
(RFC 0016). A second-vendor agent must expose those agent-branded surfaces
verbatim to pass the suite. Depending on them is depending on the *contract* (they
are contract-normative — RFC 0014 §3, RFC 0015 §5.2, RFC 0016 §4), so this is P0-clean;
but **true, brand-free vendor portability requires the neutral-contract extraction**
this RFC defers to Open Q #1 (§9), which would rename those surfaces to a
vendor-neutral spelling. Until then, the suite is portable across *implementations
of the agent-branded contract*, not across arbitrary brandings.

### 4.4 (d) Version negotiation + graceful degradation

Drift is handled at runtime, language-independently, exactly as the contract
prescribes:

- **Negotiation on `contract_version`** (RFC 0014 §6.3): agentctl refuses an
  instance whose **major** it does not know; it reads the **minor** plus the
  independently-versioned sub-schemas (`metrics_schema`, `report_schema`,
  `config_schema`) to branch.
- **Graceful degradation off `surfaces{}`** (RFC 0014 §6.2 / §8): `surfaces{}` is
  the *single* discovery point. A surface absent ⇒ unbuilt/off ⇒ agentctl drives
  only what is declared (it manages liveness + exit codes + logs even against an
  agent that advertises nothing else).
- **Additive tolerance:** unknown additive fields/tools/metrics are tolerated,
  not rejected — and reported in an **additive-drift report** (capabilities seen
  but not yet driven), because the conformance suite catches *regressive* drift
  but not *additive* drift. The two together are the full anti-drift picture.

This is why the mechanism cannot be a static shared type: the contract *requires*
lenient, negotiated consumption, which a `deny_unknown_fields` crate would
violate.

### 4.5 The CC precondition is real contract work (and it is the agent-portable kind)

The Rust decision has one hard precondition, named **CC** in the brainstorm
(§0.6): **the contract must be published as language-neutral, machine-readable
schemas.** This precondition is the RFC's hard gate, so the ledger of what
remains must be complete — under-scoping it would overstate codegen/conformance
readiness and let downstream RFCs (0011 scaling, 0013 A2A, 0018 codegen) inherit
an optimistic baseline. The full set of **not-yet-stable contract surfaces**,
each tagged with its brainstorm ask ID:

- **The JSON-Schema corpus is not published (P6 / P3b-schemas).** The reference
  manifest is built `json!` → `Value`, with no `derive(Serialize)` struct and no
  published schema document — and, as established in §2.4, the no-struct property
  is a deliberate secret-safety invariant (`Secret` has no `Serialize`). The
  surface advertises `config_schema` / `config_validate` flags, but a
  **published, versioned JSON-Schema corpus** (manifest + config + report) as a
  stable contract artifact, and the `--config-schema` emitter as its dependable
  output, is the work that remains (agent asks **P6**, **P3b**).
- **The A2A wire strings + `surfaces.a2a` are uncommitted (P2).** The A2A
  method-name spelling is unreconciled between the reference impl's 0.2.x
  (`SendMessage`/`GetTask`/…) and A2A v1.0 (`message/send`/`tasks/get`/…), and
  `surfaces.a2a` is absent from the frozen manifest schema. So `a2a.methods.json`
  (§5 tree) is **pending freeze**, not published; the A2A constants in
  `agent-contract-client` (§4.2) cannot be generated faithfully until P2 lands.
- **The autoscaling metric names are unreconciled (P10).** RFC 0016 (frozen
  metrics) and RFC 0019 (scaling signals) drift — e.g. `agent_reactive_backlog`
  is cited by the scaling RFC but is **not** in the frozen metrics schema. So
  `metrics.registry.json` (§5 tree) is **derived-by-scraping / pending freeze**,
  not a stable published artifact. (The brainstorm calls this transcription
  hazard "exactly what codegen is meant to prevent," §11.2 — which is precisely
  why the registry, not a hand list, is the source.)
- **The versioned `--capabilities` golden corpus is not published (P3b).** The
  conformance suite (§4.3) and `test/fixtures` (§5) need a per-feature-set golden
  `--capabilities` corpus, versioned by `(contract major.minor + digest)`, as a
  stable contract output rather than a scraped snapshot.

The cleanest shape under P0: the **contract** publishes the JSON Schemas; the
reference agent (and any future agent) **maps its internal structs onto the same
schema** (with `Secret` structurally absent on the reference side); agentctl
**generates its client from the schema** (§4.2). Crucially, this is **contract
work, not agent-coupling work** — it is precisely what makes the contract
portable to other agents, which is what P0 demands. Until extraction (see §9 Open
questions), agentctl vendors the schemas under `contract/` pinned by version;
the codegen and conformance pipeline are owned by agentctl RFC 0018.

---

## 5. Monorepo layout

One Cargo workspace. Five component binaries, plus the generated contract client,
the behavioral conformance suite, and shared internal crates. The data plane is
**not** here — it is a separate repo (and possibly a separate vendor); only the
**contract** (schemas) is vendored, under `contract/`.

```
agentctl/                              # one repo, one Cargo workspace
├── Cargo.toml                         # [workspace] members = crates/*
├── rust-toolchain.toml                # pinned channel/edition (Open Q #6)
├── crates/
│   ├── operator/                      # COMPONENT 1: controllers + admission +
│   │                                  #   conversion webhook server (axum/hyper),
│   │                                  #   leader election, CRD-emit xtask target
│   ├── node-agent/                    # COMPONENT 2: on-node bridge (DaemonSet);
│   │                                  #   "open a discovered socket" (RFC 0002/0008)
│   ├── gateway/                       # COMPONENT 3: A2A HTTP/SSE/auth PEP +
│   │                                  #   substrate bridge (RFC 0013)
│   ├── cli/                           # COMPONENT 4: the CLI/plugin faces —
│   │   ├── bin/agentctl.rs            #   standalone
│   │   ├── bin/kubectl-agent.rs       #   singular kubectl plugin face
│   │   └── bin/kubectl-agents.rs      #   plural kubectl plugin face (Krew needs
│   │                                  #   distinct on-PATH names; RFC 0016)
│   ├── scaler/                        # COMPONENT 5: KEDA external scaler (tonic)
│   │
│   ├── contract-client/              # `agent-contract-client` — GENERATED from
│   │                                  #   contract/ schemas; the ONLY home of the
│   │                                  #   wire shape; links NO agent (§4.2)
│   ├── conformance/                   # black-box behavioral suite; drives ANY
│   │                                  #   conformant agent binary (§4.3)
│   ├── crds/                          # CustomResource types + schemars/CEL (RFC 0003)
│   └── common/                        # shared: kube client setup, internal mTLS,
│                                      #   telemetry, error types
├── contract/                          # VENDORED language-neutral contract schemas,
│   │                                  #   pinned by (contract major.minor + digest).
│   │                                  #   Interim home until neutral extraction
│   ├── manifest.schema.json           #   (§9 Open questions). NOT an agent binary.
│   ├── config.schema.json             #   pending publication (P6/P3b)
│   ├── report.schema.json             #   pending publication (P6)
│   ├── metrics.registry.json          #   DERIVED by scraping; pending name freeze (P10)
│   └── a2a.methods.json               #   PENDING: A2A wire strings uncommitted (P2)
├── xtask/                             # codegen (schemas→contract-client), CRD YAML
│                                      #   emit, Krew manifest gen (cargo xtask ...)
├── deploy/
│   ├── helm/                          # operator, node-agent DaemonSet, gateway,
│   │                                  #   scaler, RBAC, CRDs, cert-manager wiring
│   ├── kustomize/
│   └── krew/                          # kubectl-agent + kubectl-agents Krew manifests
└── test/
    ├── e2e/                           # kind + Kata lanes (exercise the substrate
    │                                  #   crossing, not just the unix fast-loop)
    └── fixtures/                      # golden --capabilities corpus per feature-set
```

Layout invariants:

- **`contract-client` is generated and the sole wire-type home.** No component
  hand-rolls manifest/metrics/a2a types; all import `agent-contract-client`.
- **`contract/` holds schemas, never a binary.** The dependency arrow points at
  the contract. There is no path by which an agent's source enters the graph.
- **The CLI ships three binaries, one crate.** Krew resolves plugins by on-`PATH`
  binary name per top-level token, so `kubectl-agent` and `kubectl-agents` MUST
  be distinct installed names (brainstorm §8.1); they share `crates/cli`'s lib.
- **`xtask` owns codegen + CRD-YAML + Krew-manifest generation** — the
  `controller-gen`/`kubebuilder` replacements live in one reproducible place.
- **The workspace can host an out-of-workspace Go component** (its own `go.mod`,
  e.g. `apiserver/`) *without* reopening the Rust default for the other four —
  this is the mechanical expression of the §6 hybrid escape hatch.

---

## 6. Revisit triggers — when (and only when) to reopen this decision

Recorded so the decision is **not** relitigated casually. Absent one of these,
the stack is settled.

**Reopen the all-Rust decision only if ANY of:**

1. **Team fit collapses.** The team loses its `kube-rs`/`tokio` fluency (attrition
   or reassignment) *and* a kube-rs gap (§3 — most likely webhook cert rotation
   or conversion) becomes a recurring source of production incidents. Team fit is
   the override's load-bearing premise (§2.2); if it fails, the premise fails.
2. **The ecosystem regresses.** `kube-rs` (CNCF Sandbox) is abandoned, or a
   critical CVE class in the runtime/Controller cannot be patched on the
   project's timeline. (controller-runtime's maturity is the standing risk this
   trigger watches.)
3. **An aggregated APIServer becomes v1-blocking.** If the human management path
   (D5 / agentctl RFC 0009) requires an aggregated APIServer, that component is
   materially harder in Rust (`k8s.io/apiserver` is Go-native). This does **not**
   reopen the whole decision — it activates the **hybrid escape hatch** below for
   *that component only*.
4. **A pillar dependency drops its Rust-viable path** — KEDA's external-scaler
   gRPC contract, or Krew's language-agnostic packaging, changes such that Rust
   is no longer first-class.

**The single sanctioned hybrid escape hatch.** If trigger #3 fires, add **one**
Go component (the aggregated APIServer) **out-of-workspace** (its own `go.mod`
under `apiserver/`), keeping the other four Rust. This is the *only* pre-approved
deviation from all-Rust; any other hybrid requires reopening the decision in full.

**Conditions under which Go would have won (recorded so the call is legible):**

- A greenfield team **without** `kube-rs` fluency — the common case, and the
  brainstorm's honest recommendation (§2.3). `controller-runtime`'s free webhook
  serving + leader election, `kubebuilder`'s conversion scaffolding + cert-manager
  wiring, the Go-native kubectl-plugin/Krew and KEDA stories, and the Go-native
  aggregated APIServer collectively outweigh single-language cohesion when the
  team cannot pay the kube-rs gap cost. (Cert provisioning/rotation is cert-manager
  on either stack, so it is *not* among the Go advantages.)
- A world where the shared-wire-crate argument were *real and allowed* — i.e.
  **not** P0, **and** the agent committed to a published Rust wire crate, **and**
  CRDs were single-version-forever. P0 makes this counterfactual moot, but it is
  recorded as the historical hinge: P0 is *why* the strongest Rust argument
  evaporated, and yet Rust still wins on §2.2.

**Conditions under which a hybrid (beyond the #3 hatch) would have won:** if the
node-agent's frame/transport handling had been the dominant complexity *and* a Go
vsock library decisively beat the Rust path. It does **not**: under D1 the
node-agent's job is "open a discovered **unix** socket" (per-VM uds on the
hardened Kata tier), for which `tokio`'s UDS support is fully adequate. A
node-agent-only hybrid is therefore **rejected**, not deferred.

---

## 7. Non-goals

- **Choosing the data plane's language.** The contract is language-neutral; the
  reference agent's Rust is incidental and is *not* a reason agentctl is Rust
  (§2.2/§2.4). A second-vendor conformant agent in any language is in scope to
  manage, by construction (§4).
- **Specifying the CRD schema, substrate, admission ladder, scaling plane, or
  CLI grammar.** Those are agentctl RFCs 0003 / 0002 / 0007 / 0011 / 0016. This
  RFC fixes only language, repo shape, and the anti-drift *principle*.
- **Specifying the codegen pipeline or conversion/versioning policy in detail.**
  Codegen + conformance mechanics are agentctl RFC 0018; CRD versioning +
  conversion is agentctl RFC 0005. This RFC fixes that they exist and where they
  live.
- **Committing to the aggregated APIServer (Go) now.** It is deferred to D5 /
  agentctl RFC 0009 and recorded only as the sanctioned hybrid seam (§6).
- **Mandating a build system beyond Cargo + `xtask`.**

## 8. Rollout & compatibility

- **Greenfield; no migration.** This RFC predates any code.
- **Bootstrapping order:** (1) workspace skeleton (§5); (2) vendor the contract
  schemas under `contract/` (hand-authored interim schemas where the published
  corpus does not yet exist — the CC precondition, §4.5) and generate
  `agent-contract-client`; (3) stand up `crates/conformance` driving a reference
  agent over the unix socket (the contract-clean dev loop); (4) operator +
  node-agent on the unix-hostPath primary substrate; (5) gateway + scaler as
  their planes land. This matches the brainstorm's Phase-0 cut (§16).
- **Pinning:** codegen is pinned by **(contract major.minor + schema digest)**,
  *not* by an agent SHA — the reference impl's manifest/surfaces/metrics are
  build-conditional (`cfg!`), so a SHA alone is not a stable contract identity
  (brainstorm §11.1).
- **Reversibility is per-component and trigger-gated** (§6). The workspace shape
  admits an out-of-workspace Go component without reopening the Rust default for
  the other four.

## 9. Open questions

1. **Where the canonical contract spec should live (CC home).** **Recommended:
   extract the contract into a neutral "Agent Control Contract" spec** — its own
   home with published, versioned JSON Schemas — so neither agentctl nor any
   agent owns the other. The contract is *currently* authored inside the
   reference agent's RFCs 0014–0020 but is implementation-neutral; extraction is
   the clean P0 resolution. Until then, agentctl vendors the schemas under
   `contract/` pinned by version (agentctl RFC 0018; brainstorm §0.6 P0).
2. **Generated `agent-contract-client`: checked-in vs build-time-generated.**
   Checked-in aids review and reproducibility; build-time avoids stale artifacts.
   (Leaning checked-in + a CI drift check.)
3. **CLI packaging:** three thin binaries over one shared lib (the §5 layout) vs
   a single multi-call binary with an `argv[0]` switch. Krew's distinct-name
   requirement pushes toward distinct binaries (agentctl RFC 0016).
4. **`xtask` vs `cargo-make` vs `make`** for codegen + CRD-YAML + Krew-manifest
   generation. (Leaning `xtask` — pure Rust, no extra toolchain.)
5. **The hybrid escape hatch (§6):** confirm a future Go aggregated APIServer
   stays out-of-workspace, or reaffirm Rust-only for v1 and accept the
   `pods/proxy` admin stopgap (D5 / agentctl RFC 0009).
6. **MSRV / edition pin** and whether to track the reference impl's toolchain
   (edition 2024 / rust 1.88) or pin independently. (Independent; pick one and
   record it in `rust-toolchain.toml`.)

## 10. References

- agentctl RFC 0002 — Substrate & transport abstraction (the "open a discovered
  socket" code path the node-agent shares across tiers; the unix dev loop)
- agentctl RFC 0003 — `Agent` & `AgentFleet` CRD schema + status contract
- agentctl RFC 0005 — CRD versioning & conversion policy (minimizes the §3
  conversion-webhook gap)
- agentctl RFC 0006 — Operator reconcile & manifest-driven capability model
- agentctl RFC 0007 — Admission validation ladder (the webhook server of §3)
- agentctl RFC 0008 — node-agent architecture (the on-node management bridge; the
  "open a discovered socket" discovery loop the §1 component roster names)
- agentctl RFC 0009 — Management access path & RBAC (the aggregated-APIServer / D5
  question behind the §6 hybrid escape hatch)
- agentctl RFC 0011 — Scaling plane (the KEDA external scaler / `tonic`)
- agentctl RFC 0013 — A2A gateway & task store (the public A2A surface of §1; the
  consumer of the pending A2A method-set artifact, §4.1/§4.5)
- agentctl RFC 0016 — CLI & kubectl-plugin grammar (the three CLI faces / Krew)
- agentctl RFC 0018 — Codegen & contract conformance (owns the §4 pipeline and the
  CC schema corpus)
- agentd RFC 0014 (the reference impl's contract umbrella) — manifest spine §5,
  `surfaces{}` discovery §6.2, versioning/negotiation §6.3, downward-API env
  §6.4, graceful degradation §8
- agentd RFC 0015 (the reference impl's contract spec) — management & control
  surface; manifest schema §5.2, operator tools §4
- agentd RFC 0016 (the reference impl's contract spec) — frozen metrics schema +
  exit-code contract
- agentd RFC 0017 (the reference impl's contract spec) — declarative config,
  `--validate-config` / `--config-schema` (the CC precondition surface)
- agentd RFC 0018 (the reference impl's contract spec) — intelligence transport
  resilience
- agentd RFC 0020 (the reference impl's contract spec) — A2A over the substrate
  (the A2A method set; wire strings not yet frozen — contract ask P2, §4.5)
- agentd RFC 0012 §3.7 (the reference impl) — the `Secret`-has-no-`Serialize`
  invariant that makes the manifest deliberately untyped (§2.4 / §4.5)
- `crates/agent-conformance` (the reference impl) — the black-box,
  never-link-the-library conformance pattern agentctl's `crates/conformance`
  mirrors for *any* agent (§4.3)
- agentctl architecture brainstorm — §0.6 (P0 + locked decisions), §1/D2
  (stack), §11 (stack/repo/codegen)
