# agentctl

**A Kubernetes-native platform for running fleets of AI agents as a multi-tenant
cloud.** agentctl provisions, configures, secures, scales, governs, and exposes
agents — declaratively, through Custom Resources — and gives you the seams to
build *your own* managed agent product on top, the way Svix is a backbone for
webhooks. It is implemented entirely in Rust.

> ### Principle P0 — depend on the *contract*, never on a specific agent
>
> agentctl manages any binary that satisfies the **Agent Control Contract**
> (`contract/`): a capabilities manifest, a management profile, a frozen
> metrics + exit-code table, a config schema, an A2A method registry, and a
> downward-API env convention. **`agentd` is the reference implementation — the
> first agent to satisfy the contract — not a dependency.** Swap in any
> conformant agent and the control plane manages it unchanged.

---

## Table of contents

- [What it is and who it's for](#what-it-is-and-who-its-for)
- [The platform at a glance](#the-platform-at-a-glance) — the capability planes
- [Architecture](#architecture) — components and how they fit
- [The custom-resource family](#the-custom-resource-family) — the 8 CRDs
- [How you use it](#how-you-use-it) — postures, the supervisor journey, quickstart
- [The security model](#the-security-model)
- [Nuances worth knowing](#nuances-worth-knowing)
- [Documentation map](#documentation-map)
- [Licensing](#licensing)

---

## What it is and who it's for

An **agent** is a long- or short-lived process that reasons with a model, calls
tools (over the [Model Context Protocol](https://modelcontextprotocol.io)), talks
to other agents (over [A2A](https://a2a-protocol.org)), and does work on
someone's behalf. Running *one* agent is easy. Running **a fleet of them, for
many users, safely, with an audit trail and a bill at the end** is a platform
problem — and that platform is what agentctl is.

You reach for agentctl when you want to offer any of these and not build the
substrate yourself:

| You want to offer… | agentctl gives you |
|---|---|
| "Sign up, get an agent" | An `Organization` CR → a per-org namespace, a tenant gateway, and a personal **supervisor** agent per user |
| "Bring your own login" | OIDC federation — device flow for the CLI, auth-code for the web; you map IdP claims → orgs and groups |
| "Let my agent use my Zendesk" | **Connections**: one-time consent, credentials held in custody, per-user tokens injected upstream via RFC 8693 exchange |
| "Run agent-authored code safely" | The **sandbox cell** — single-use, network-denied, capability-stripped pods |
| "Give agents durable state and files" | Managed **checkpoints** (`state.*`, survives `kill -9`) and an S3-backed **artifacts** store, both org-fenced with quotas |
| "Humans approve risky actions" | **HITL gates** answered under the right identity, fanned out to your channel |
| "Charge for it" | Billing-ready **metering** — attributed events with an export API |
| "Prove what happened" | One **queryable audit trail** across every plane |
| "Scale to zero" | Webhook/claim scale-from-zero and idle supervisor park/wake — automatic |

The three audiences:

- **Platform teams** standing up an internal agent service for their company.
- **Product builders** shipping a managed agent product to end users (agentctl
  is the invisible backbone).
- **Operators** who just want a hardened, declarative way to run a few agents on
  their own cluster — the single-tenant defaults are safe out of the box.

---

## The platform at a glance

agentctl is organized as **capability planes**. You turn on the planes your
product needs; each is a small set of components plus the CRD surface that drives
it. This is also the shape of the build: the [implementation
register](docs/v2/PLAN.md) tracks every plane to a live end-to-end test.

| Plane | What it does | Key surfaces |
|---|---|---|
| **Substrate** | Renders CRs into hardened Kubernetes workloads against the Agent Control Contract | `operator`, admission, the contract |
| **Identity** | OIDC federation, credential custody, RFC 8693 token exchange, the AAuth agent-identity provider | `identity`, `Organization.identity` |
| **Tenancy** | Orgs → managed namespaces, quotas, claims-to-roles access policies, the CRD family + projection | `Organization`, `AgentClass`, `MCPService` |
| **State / durability** | Managed checkpoints (seq-CAS, tenant-fenced), store classes, an S3 artifacts façade, lifecycle verbs | `state.*`, `artifacts.*`, `Agent.store` |
| **Control** | Per-user **supervisors**, on-behalf-of tool calls, approval gates, `@mention` orchestration | `Supervisor`, the control MCP |
| **Capability** | The per-org governance **MCP gateway** (mcpg): federates tools, injects per-user credentials, sandbox, HITL, work fabric | tenant `mcpg`, `MCPService` |
| **Fleets / scaling** | Elastic claim fleets, fixed shard fleets, dispatcher fan-out, per-fleet budgets | `AgentFleet`, `coordination`, `scaler` |
| **GA surfaces** | Webhook hooks ingress, dashboards + alerts, the audit pipeline, metering export, hardening | `gateway` hooks, `audit`, `metering` |

**Status:** every plane is complete and live-verified against real `agentd`, a
blessed `mcpg`, and a bundled MinIO — see the
[GA checklist](docs/v2/GA-checklist.md).

---

## Architecture

The **control plane** is a set of Rust Deployments in the release namespace. The
**data plane** is the agent pods themselves plus, per org, a governance MCP
gateway. Agents never hold platform secrets they don't need: a governed tool call
goes through the org's gateway, which injects the right per-user credential; a
directly-dialed provider either uses a mounted key or the agent's own AAuth
identity.

```mermaid
flowchart TB
  subgraph cp["Control plane (agentctl-system)"]
    operator["operator<br/>reconcile · PKI · NetworkPolicies · KEDA"]
    apiserver["apiserver<br/>management + lifecycle verbs · metering/audit query"]
    admission["admission<br/>validating + mutating + conversion webhooks"]
    identity["identity<br/>OIDC · custody · RFC 8693 exchange · AAuth provider"]
    gateway["gateway<br/>A2A · org routes · hooks · HITL · supervisors"]
    control["control<br/>the control.* MCP (agents manage agents)"]
    coordination["coordination<br/>work.* claim hub"]
    scaler["scaler<br/>KEDA external scaler"]
    sandbox["sandbox<br/>sandbox.run cell"]
    artifacts["artifacts<br/>artifacts.* over S3"]
    state["state<br/>state.* checkpointer (mcpg + Postgres)"]
    audit["audit<br/>one queryable trail"]
    pg[("Postgres")]
  end

  subgraph org["Per-org data plane (org-acme)"]
    supervisor["supervisor agent<br/>(one per user)"]
    agents["agent pods"]
    tmcpg["tenant mcpg<br/>governance gateway"]
  end

  user["end user"] -->|OIDC bearer| gateway
  gateway -->|per-(user,agent) principal, mTLS| agents
  gateway -->|per-(user,agent) principal, mTLS| supervisor
  supervisor -->|control.* OBO| control
  agents -->|governed tool calls| tmcpg
  tmcpg -->|per-user token via exchange| ext["providers · MCP servers"]
  agents -->|checkpoints| state
  agents -->|blobs| artifacts
  agents -->|work.*| coordination
```

### Components

Each is one container image (all distroless, non-root, read-only rootfs), except
the two data-plane services built on the Apache-licensed **mcpg** gateway image.

| Component | Role |
|---|---|
| **operator** | Reconciles the whole CRD family into workloads; renders the agent's config directory; owns per-workload PKI (issues serving certs, distributes the CA), per-namespace NetworkPolicies, KEDA wiring, tenant-namespace creation, and the guarded shard-resize choreography. |
| **apiserver** | A Kubernetes *aggregated* API (`management.agentctl.dev`) for the human/programmatic surface: runtime verbs (drain/lame-duck/pause/resume/cancel), the state-plane lifecycle verbs (backup/restore/reset/stop/start/migrate), and the metering-export + audit-query read paths. Every call is authorized by `SubjectAccessReview`. |
| **admission** | Validating + mutating + **conversion** webhooks: the image allow-list, the lethal-trifecta gate, class floors, handle uniqueness, webhook-exposure-needs-a-trigger, a dangling-Secret rung, and secure defaults — all evaluated against the storage version (`v1alpha2`). |
| **identity** | The crown jewel: OIDC federation (issuer-pinned + JWKS), RFC 8628 device flow, an AES-GCM sealed credential custody, the RFC 8693 token-exchange endpoint, and the **AAuth agent-identity provider** (enroll + agent-token over RFC 9421 signatures). |
| **gateway** | The tenant-scoped data-plane front door: org routes (`/orgs/{org}/…`), Agent Cards + `message/send`/`message/stream`, the external **hooks** ingress, **HITL** channel fan-out, supervisor auto-ensure + idle park, and inbound OIDC introspection that stamps the caller's per-(user,agent) principal upstream. |
| **control** | The `control.*` MCP server — how a supervisor (or any agent) lists, inspects, creates, and delegates to other agents in its own namespace, authenticated by AAuth and scoped server-side. |
| **coordination** | The work-distribution backbone: an MCP server exposing `work.*` (submit/claim/renew/ack/release/result/deadletter) with exactly-one-owner leasing, dead-lettering, and an in-memory or durable-Postgres store. Its backlog is the scale-from-zero signal. |
| **scaler** | A KEDA external scaler reading the coordination backlog (and per-fleet inbox metrics) so claim fleets scale elastically, including from zero. |
| **sandbox** | The `sandbox.run` MCP backend: agent-authored code runs in single-use, network-denied, capability-stripped pods (optional Kata/gVisor runtime class). |
| **artifacts** | The `artifacts.put/get/list` MCP backend over an S3-compatible content store (bundled MinIO), org-fenced with per-org byte quotas. |
| **state** | The `state.*` seq-CAS checkpointer for `store.class: managed` agents — a governed MCP binding on the **mcpg** gateway over Postgres, with a server-side tenant fence. |
| **audit** | The `audit/v1` record vocabulary + a Postgres sink + a query API; an `audit-shipper` sidecar tails per-org gateway records into the one trail. |
| **tenant mcpg** | *(per org, provisioned by the operator)* The governance **capability plane**: a proxy-only [mcpg](https://github.com/mcpg-dev) gateway that federates the org's `MCPService` registry to its agents, governed — filtering tools, injecting per-user credentials, and enforcing the verified-caller tier. |

Supporting libraries and tools: **agent-api** (the CRD types + pure policy
engines), **agent-config** (the config projection), **agent-contract-client**
(the contract manifest reader), **agentctl-metering** / **agentctl-audit**
(billing + audit vocabularies), **agentctl-telemetry** (OTLP), **agentctl-crdgen**
(CRD YAML generation), and the **`agentctl`** CLI.

---

## The custom-resource family

CRDs are in the API group **`agentctl.dev/v1alpha2`** (storage version); the
management API is **`management.agentctl.dev/v1alpha1`**. Field-by-field
reference: [docs/reference.md](docs/reference.md).

| Kind | Short | Scope | Purpose |
|---|---|---|---|
| **Organization** | `org` | cluster | A tenant: managed namespace(s), quotas, IdP binding, claims-to-roles access policies, supervisor mode. |
| **Agent** | `agent` | namespaced | One agent: an instruction + typed **triggers** + scoped bindings, rendered to the workload its **`shape`** dictates (`daemon`/`job`/`cron`). |
| **AgentFleet** | `afleet` | namespaced | A replicated worker set — elastic **claim** fleets or fixed **shard** partitions, optionally fronted by a coordinator, with a per-fleet budget. |
| **AgentClass** | — | namespaced | The scoped **defaults + security floors** an agent resolves through (system → org → group). Lower scopes may only *narrow* a floor. |
| **AgentTemplate** | — | namespaced | An instantiable agent spec with typed `{{params.*}}` holes — the CLI's and control MCP's `create --from-template` source. |
| **MCPService** | — | namespaced | A capability-registry entry: an MCP/peer/HTTP service with capability **tags** (floors), allow/exclude tool ceilings, and the **auth mode** agents reach it with (`service`/`obo`/`passthrough`). |
| **ModelPool** | `mp` | namespaced | A direct-dial model endpoint registry (provider, endpoint, optional key, models) — not on the data path, so no broker and no meter. |
| **Supervisor** | — | namespaced | A user's personal agent (owner-only principal), owner-ref'd to a rendered `Agent`; the on-behalf-of anchor the control plane binds against. |

### The Agent, in one glance

An `Agent` is **an instruction, some triggers, and scoped bindings**:

```yaml
apiVersion: agentctl.dev/v1alpha2
kind: Agent
metadata: { name: triage, namespace: org-acme }
spec:
  class: support                     # inherit defaults + floors from an AgentClass
  instruction: { text: "Triage inbound tickets; escalate anything urgent." }
  shape: daemon                      # inferred if omitted: any long-lived trigger ⇒ daemon
  triggers:
    - webhook: { path: /zendesk, methods: [POST] }   # wake on an external delivery
    - schedule: { cron: "0 * * * *" }                # and hourly
  intelligence: { pool: anthropic, model: claude-sonnet-5 }
  services:                          # GRANT capability-registry entries (narrow, never widen)
    - name: zendesk
  store: { class: managed }          # durable checkpoints on the state service
  expose: { a2a: true }              # reachable as an A2A endpoint through the gateway
  approval: { policy: ask }          # risky actions pause for a human
```

The renderer compiles each trigger into a generated workflow, resolves defaults
and floors through the class chain, projects the config directory, mounts exactly
the secrets the bindings need, and reconciles the workload. **Ten trigger kinds**
cover the wake sources: `once`, `manual`, `loop`, `schedule`, `webhook`,
`subscribe` (an MCP resource changes), `stream`, `signal`, `event` (an agentd
runtime event), and `a2aCommand`.

---

## How you use it

### Four deployment postures, one chart

The same Helm chart serves all of them (see [ARCHITECTURE §3.7](docs/v2/ARCHITECTURE.md)):

- **Self-hosted enterprise** — one org, your own IdP, tenant isolation for free.
- **Managed multi-tenant service** — many orgs, hostile-tenant assumptions, every
  hardening box checked (see the [build-on guide](docs/v2/build-on-agentctl.md)).
- **Customer-embedded** — your product ships agentctl as its agent backbone; your
  users never see the substrate.
- **Cross-cluster federation** — gateway↔gateway mTLS; hooks and supervisors
  terminate at a hub.

### The supervisor journey (the multi-tenant experience)

1. A user signs in through your IdP; the gateway introspects the token and
   resolves their org + groups.
2. The org's policy provisions a **supervisor** — the user's personal agent, with
   an owner-only principal nothing else can assert.
3. The user chats with the supervisor. To act, it uses the `control.*` MCP
   **on-behalf-of** the user — creating sub-agents, delegating to teammates by
   `@handle`, and reaching the user's connected tools with *their* credentials.
4. Anything risky hits an **approval gate** that only the owner can answer;
   everything is **metered** and lands in the **audit trail**.

### Quickstart (local `kind`)

**cert-manager** is the only hard prerequisite (it issues every certificate):

```console
kubectl apply -f https://github.com/cert-manager/cert-manager/releases/latest/download/cert-manager.yaml
kubectl -n cert-manager rollout status deploy/cert-manager-webhook
```

Install the control plane (pre-create the namespace — Helm can't own its own):

```console
kubectl create namespace agentctl-system
kubectl label namespace agentctl-system \
  pod-security.kubernetes.io/enforce=baseline pod-security.kubernetes.io/warn=baseline
helm install agentctl ./charts/agentctl -n agentctl-system
kubectl -n agentctl-system get pods           # components Running
kubectl get apiservice v1alpha1.management.agentctl.dev   # AVAILABLE=True
```

A default install brings up the core planes (substrate + identity + gateway +
admission + bundled Postgres). Turn on the planes your product needs — fleets +
scaling (`coordination.enabled`, `scaler.enabled`, needs KEDA), the state service
(`state.enabled`), artifacts (`artifacts.enabled`), the sandbox
(`sandbox.enabled`), per-org gateways (`tenantMcpg.enabled`). Then apply an
`Organization`, and let the platform provision the rest.

Drive agents from the CLI (`agentctl login`, `agentctl chat <org>/<agent>`,
`agentctl create agent …`, `agentctl connect <provider>`, the lifecycle verbs) or
apply CRs from your product backend. Chart values and production notes:
[`charts/agentctl/README.md`](charts/agentctl/README.md). Worked, apply-ready
examples: [`deploy/examples/`](deploy/examples/).

---

## The security model

Identity is cryptographic and attribution is unforgeable:

- **Fail-closed by construction** — identity admin surfaces refuse with no token,
  the exchange refuses a user-less subject, a gate with no channel refuses at
  compile, and admission catches a dangling `{{secret:…}}` before a pod
  crash-loops.
- **Attribution can't be forged** — org membership is stamped server-side by the
  gateway (a supervisor cannot assert its own groups), the audit shipper's org is
  forced from its token, and OBO acting-user comes from a signed claim the
  workload cannot mint.
- **Server-side tenant fences** — managed state and artifacts key every row/object
  under the caller's *host-asserted* subject; a conforming caller physically
  cannot reach another tenant's data.
- **Secret-free where it can be** — a governed tool call gets its per-user
  credential injected at the org gateway via RFC 8693 exchange (custody holds the
  refresh grant, never the agent); an AAuth agent signs each request itself.
- **Hardened pods + tenant isolation** — non-root, caps dropped, read-only
  rootfs, no auto-mounted SA token; default-deny NetworkPolicies per namespace
  (on a policy-capable CNI); the sandbox cell is deny-all.
- **The lethal-trifecta gate** — `exec` + `egress` + `secrets` together require an
  explicit admission opt-in.
- **PKI + RBAC** — all control-plane TLS from cert-manager; management access
  RBAC-gated via `SubjectAccessReview`.

Full model: [docs/security.md](docs/security.md) and the
[sandbox threat model](docs/v2/sandbox-threat-model.md).

---

## Nuances worth knowing

The details that bite if you don't know them (the full set, with rationale, is in
[docs/reference.md](docs/reference.md#5-nuances)):

- **Config reload is real but scoped.** The operator rolls a pod only on a
  *restart-required* change (image, mounts, `store`, listeners/TLS, `security`,
  `a2a.principals`, `webhooks`); persona, workflows, model bindings and MCP
  servers hot-reload with no roll. One subtlety: an embedded `:::workflow`
  directive in a *templated* instruction is a workflow, so an innocuous prose edit
  can retire it — keep real workflows in `spec.workflows[]`.
- **A grant narrows, never widens.** An agent's `services[]`, `limits`, and
  capabilities can only *shrink* what its `AgentClass` floor allows; widening is
  an admission error that names the floor.
- **Managed state and artifacts are org-fenced by identity you can't spoof.** Keys
  live under the caller's asserted subject prefix; there is no argument that lets
  a caller reach another org's data.
- **`migrate` needs a managed store.** Ephemeral/local state is node-local and
  won't move; `agentctl migrate` refuses it and only reschedules a `managed` agent
  (whose checkpoint lives in central Postgres).
- **NetworkPolicies need a real CNI.** The chart renders every policy, but kindnet
  ignores them — the sandbox's network isolation and cross-tenant boundary are
  inert without Calico/Cilium. This is the single most important production
  prerequisite.
- **The state/artifacts services are internal.** They sit behind the netpol
  perimeter and the subject-prefix fence, so the state gateway disables the
  per-IP anonymous rate limiter that would otherwise throttle a real header-
  asserted agent — keep that limiter armed on any gateway facing untrusted
  callers.

---

## Documentation map

| Doc | What's in it |
|---|---|
| [docs/v2/ARCHITECTURE.md](docs/v2/ARCHITECTURE.md) | **The canonical architecture** — vision, every component, the tenancy + identity model, the supervisor experience, agent anatomy, fleets, security, and the four postures. |
| [docs/reference.md](docs/reference.md) | **The exhaustive element reference** — every component, every CRD field, config/env conventions, and the nuances. |
| [docs/v2/PLAN.md](docs/v2/PLAN.md) | The implementation register — every plane traced to a live e2e scenario. |
| [docs/v2/GA-checklist.md](docs/v2/GA-checklist.md) | The honest GA state and what's deferred as polish. |
| [docs/v2/build-on-agentctl.md](docs/v2/build-on-agentctl.md) | The integrator/operator guide: the four boundaries you build against + the multi-tenant hardening checklist. |
| [docs/v2/sandbox-threat-model.md](docs/v2/sandbox-threat-model.md) | The sandbox cell's containment layers. |
| [docs/security.md](docs/security.md) | The identity, isolation, trifecta, and PKI model *(v1-era; the model carries into v2)*. |
| [docs/operations.md](docs/operations.md) | Day-2: management verbs, upgrades, tuning *(v1-era)*. |
| [docs/benchmarks.md](docs/benchmarks.md) | Measured throughput, latency, density, and checkpoint capacity. |
| [docs/use-cases.md](docs/use-cases.md) | Worked examples with apply-ready manifests *(v1-era)*. |
| [contract/README.md](contract/README.md) | The Agent Control Contract — how any agent conforms. |
| [charts/agentctl/README.md](charts/agentctl/README.md) | Helm values, install options, and production notes. |
| ADRs: [docs/v2/adr/](docs/v2/adr/) | The load-bearing design decisions. |

---

## Licensing

This repository is dual-licensed **by component**; [`LICENSE`](LICENSE) is the
authoritative map.

- **Apache-2.0** — the contract (`contract/`), the SDK/libraries, and the client
  tooling. The standard and SDK are open so any agent vendor can implement and
  build on them (P0).
- **Business Source License 1.1** — the runnable control plane. Source-available:
  free for non-production and internal non-commercial use; commercial production
  or managed-service use requires a commercial license until the Change Date,
  when each version converts to Apache-2.0. See [`LICENSE-BUSL`](LICENSE-BUSL).

The reference agent (`agentd`) and the governance gateway (`mcpg`) are separate
projects with their own licenses. Commercial licensing: andrii@tsok.org.
Contributions are under the [CLA](CLA.md).
