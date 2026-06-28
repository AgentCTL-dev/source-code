# agentctl RFC 0015: Security & multi-tenancy

**Status:** Proposed (agentctl security track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the cross-cutting security & multi-tenancy capstone: the one place the platform's trust model, its policy-enforcement points, its PKI/secret lifecycle, its egress posture, its supply chain, and its threat model are stated whole, and the home that **resolves** the security items the earlier RFCs deferred to it

> **The collapse point (the load-bearing fact of this RFC).** A conformant agent
> **re-verifies nothing.** Its management/A2A transport is gated by *reachability*,
> not a credential (the reference impl's agentd RFC 0015 §7: the operator profile is
> selected by `PeerOrigin`, which is reachability; agentd RFC 0012 §3.8: the transport
> *is* the boundary). Therefore **reachability of the transport == full authority over
> the agent**, and **agentctl owns 100% of authn/authz** at a small set of
> policy-enforcement points (PEPs). A forwarded request crosses the substrate to the
> agent with **no in-band auth**; the authority was decided *before* the crossing. This
> single asymmetry — the agent exposes primitives, agentctl owns policy (agentd RFC
> 0014 §3) — is what makes the security model tractable and is what every section below
> elaborates.

> **Capstone, not a new mechanism (P0).** This RFC introduces almost **no new agent
> surface**; it composes the per-RFC controls into one model and **resolves** the
> cross-cutting items earlier RFCs parked here: the **internal mTLS PKI** (deferred from
> agentctl RFC 0014 §8), the **management-action audit vocabulary** (referenced by
> agentctl RFC 0007/0008/0009/0010 as "the agentctl RFC 0015 audit vocabulary"), the
> **egress allow-list** (the "home is agentctl RFC 0015" pointer in agentctl RFC
> 0013 §4.4/§8 and RFC 0012 §5.4), the **data-at-rest envelope-key hierarchy** (the
> "KMS/operator key, agentctl RFC 0015" pointer in agentctl RFC 0013 §4.4), the
> **guest→host vsock egress restriction** (deferred from agentctl RFC 0002 §10
> correction 2), and the **namespace tenancy-label ownership** (RFC 0004 OQ4). The
> agent's *own* security mechanisms — Rule-of-Two at the spawn chokepoint, the
> reader/actor distillate firewall, SSRF guards, gated `exec`, the `Secret`-unserializable
> invariant — live in the contract (the reference impl's agentd RFC 0012) and agentctl
> **surfaces and depends on** them; it does not re-implement them. The single conditional
> agent ask this posture *wants* is **P-attach-gate** (the no-puppeting gate, §4.4),
> already carried by agentctl RFC 0009; this RFC is its requirement-home.

---

## 1. Problem / Context

agentctl runs a fleet of **conformant agents** for **mutually-untrusted tenants on a
shared cluster** — hostile multi-tenancy is a locked v1 requirement (brainstorm §0.6).
That posture is unforgiving: a single missing check is not a bug, it is a cross-tenant
authority grant. The earlier RFCs each solved one slice of the problem and, by design,
deferred the cross-cutting synthesis and several whole concerns to *this* RFC. The
completeness analysis named the gaps explicitly (brainstorm §12: "Secret/PKI lifecycle —
cert-manager is scoped only to the webhook today"; the security plane "elevated into v1"
by the hostile-tenancy decision, §0.6). This RFC is that synthesis.

### 1.1 The trust boundaries, stated once

The platform has exactly four kinds of boundary, and confusing them is the source of
every red-team finding the brainstorm folded in:

1. **The isolation boundary — the microVM kernel (Kata-hybrid).** The only boundary
   that survives an *untrusted neighbour-tenant* compromise. NetworkPolicy is **not** an
   isolation boundary (it is IP-layer, CNI-dependent, and silent on `AF_VSOCK`); a shared
   host kernel is **not** an isolation boundary against a container escape. agentctl RFC
   0002 §5 makes Kata mandatory for tenant workloads precisely because nothing weaker is
   a boundary we can stand behind (§4.2).

2. **The authority boundary — the transport.** Reachability of the management/A2A
   transport == authority over the agent (the collapse point above). The transport is
   crossed with no in-band auth, so the authority boundary is **the set of PEPs that
   decide who may reach it** (§3), plus the **attestation** that proves the thing on the
   other end of the socket is the pod we meant (agentctl RFC 0002 §7).

3. **The identity boundary — the control-plane PKI.** Component-to-component traffic
   (operator ↔ node-agent ↔ gateway ↔ aggregated APIServer) is authenticated by **mTLS
   from one internal CA**; the agent's *callers* (humans, peers) are authenticated at the
   PEPs by Kubernetes RBAC (management) and the Agent Card `securitySchemes` (A2A). These
   are different identity systems for different traffic and this RFC keeps them distinct
   (§5).

4. **The injection boundary — the reader/actor distillate firewall.** Prompt injection
   is unsolved and not patchable (agentd RFC 0012 §1). The only structural defense is the
   agent's CaMeL-style split: an untrusted-content reader with *no* sensitive/egress tools
   returns a low-bandwidth distillate to an actor that never sees the raw content. This
   boundary lives **inside the agent** (agentd RFC 0012 §3.3); agentctl's job is to *not
   defeat it* (do not co-locate the trifecta) and to *surface* it (the CRD tags + advisory
   admission, §8).

### 1.2 What this RFC resolves (the deferred-items ledger)

| Deferred item | Parked by | Resolved in |
|---|---|---|
| Internal mTLS CA + rotation across **all** components (not just the webhook) | brainstorm §12; agentctl RFC 0014 §8 | §5.2 |
| The **management-action audit vocabulary** (the closed `mgmt.invoked`-class event set + override audit annotations + security events) | agentctl RFC 0007 §3.3, RFC 0008 §7.3, RFC 0009 §5.3, RFC 0010 §10.2 | §9 |
| The **egress allow-list** posture (one implementation, two callers: webhook delivery + delegation-out) | agentctl RFC 0013 §4.4/§8, RFC 0012 §5.4 | §6 |
| The **data-at-rest envelope-key** hierarchy (webhook creds, task-store fields) | agentctl RFC 0013 §4.4 | §5.5 |
| The **guest→host vsock egress restriction** (host-side, only provisioned ports) | agentctl RFC 0002 §10 correction 2 | §6.3 |
| **Who owns the namespace tenancy label** (`agents.x-k8s.io/tenancy`) | agentctl RFC 0004 §3.3 OQ4 | §4.5 |
| **Supply chain** — image signing/verification, SBOM, contract-client provenance | brainstorm §10.1/§11.2/§12 | §7 |
| The **internal CA / SPIFFE IDs** the node-agent pins (its two known clients) | agentctl RFC 0009 §5.3 | §5.2 |
| The **mesh CA** for east-west A2A mTLS | agentctl RFC 0013 §4.1, RFC 0014 | §5.4 |

This RFC owns the **model and these resolutions**; it does **not** restate the mechanics
the owning RFCs already specify (the admission ladder, the gateway PEP internals, the
access-path routing, the card field-mapping). It cross-references them and fills the seams.

---

## 2. Decision — the cross-cutting security model (nine principles)

1. **The transport is the boundary; agentctl owns all authz at the PEPs.** The agent
   re-verifies nothing (agentd RFC 0012 §3.8). There are exactly **four caller→agent
   authority PEPs** (§3): admission (agentctl RFC 0007), the management access path (RFC
   0009 + the node-agent internal chokepoint RFC 0008), and the A2A gateway (RFC 0013). A
   forwarded request crosses with no in-band auth. One further enforcement point governs
   the *opposite* direction — the agent's **outbound** model/egress traffic — the
   intelligence/egress proxy (§5.6, §6.2), which decides `peer→Agent→allowed-pools` on the
   attested peer; it is a distinct **egress-authority** point (agent→pool, not caller→agent)
   and is enumerated, hardened, and audited under the **same** exhaustiveness discipline
   (§3.2.1).

2. **Isolation is a substrate decision, not a NetworkPolicy decision.** Under hostile
   tenancy, **Kata-hybrid is mandatory** for tenant workloads and `stock-unix` is
   forbidden (agentctl RFC 0002 §5); NetworkPolicy is a defense-in-depth IP-layer control,
   never the tenant boundary (§4.2).

3. **Attest the socket before speaking authority over it.** A discovered socket is
   bound to a proven `pod_uid` before the first `tools/call` (agentctl RFC 0002 §7);
   reachability == authority makes a mis-attested socket a cross-tenant authority grant,
   so attestation is **v1-blocking**, not deferred (§4.3).

4. **One internal CA, one identity per component, mTLS everywhere east-west.** Every
   control-plane↔control-plane hop authenticates by mTLS from a single internal CA;
   cert-manager is the default, the in-repo fallback (agentctl RFC 0001 §3) extends from
   webhook-only to all internal certs (§5.2).

5. **Keep every secret off the agent pod; where it must be in-pod, it is a mounted-Secret
   ref, never the config file.** Provider creds resolve at the proxy (agentctl RFC 0012
   §5); A2A/webhook creds live in the gateway (RFC 0013); the agent's
   `Secret`-unserializable invariant (agentd RFC 0012 §3.7) guarantees no secret reaches
   the manifest, `.status`, a log, or a trace (§5.1).

6. **The intelligence/model channel is the one irreducible egress; everything else is
   default-deny.** Egress is restricted at two layers — IP-layer NetworkPolicy
   default-deny for the agent pod, and a host-side **guest→host vsock egress allow-list**
   (only provisioned ports) under the HARDENED tier — with identity-based egress authz at
   the proxy (§6). "No network" never meant "nothing to exfiltrate" (agentctl RFC 0002 §10).

7. **Verify what runs: signed images at admission, signed schemas at codegen.** The agent
   image is cosign-verified at admission against the `AgentClass.image` digest; the
   contract schemas the generated client is built from are signed/hash-pinned (§7). MCP
   servers are processes *inside* the verified image, not separate pods, so they are
   covered by image verification, not by a separate policy (§7.3).

8. **The lethal-trifecta defense is the agent's per-spawn Rule-of-Two; agentctl's layers
   are declaration + advice + override-gating, never a blocking union.** Tags are declared
   on the CRD (RFC 0003/0004), the union is detected *advisorily* at admission, and the
   *override* (`allowTrifecta`) is gated by elevated RBAC + audit (RFC 0007 §3). The
   reader/actor distillate firewall is the structural control (§8).

9. **Every destructive act is doubly audited under one closed vocabulary.** The front
   (apiserver/aggregated layer) records *who asked*; the PEP (node-agent/gateway) records
   *what was executed against the attested target*. Both use the `mgmt.invoked`-class
   vocabulary this RFC owns (§9), emitted to the self-observability sink (RFC 0010 §10.2).

---

## 3. The trust & PEP model

### 3.1 The four PEPs

**Caller→agent** authority is decided at exactly four enforcement points (PEP-1..4 below).
One further enforcement point governs the **opposite direction** — the agent's *outbound*
egress — the intelligence/egress proxy (§5.6, §6.2); it is called out separately because it
decides `peer→Agent→allowed-pools` (agent→pool authority), not caller→agent authority, but
it is enumerated, hardened, and audited under the same exhaustiveness discipline (§3.2.1).
What is **not** an enforcement point: **the agent itself** (it has no auth model — agent
RFC 0012 §2/§3.8), and **NetworkPolicy** (it gates IP reachability, never caller identity —
agentctl RFC 0002 §10 correction 2, RFC 0008 §7.4).

```
   AUTHORITY DECIDED HERE (the PEPs)                          NO AUTH HERE (the boundary crossed)
   ┌─────────────────────────────────────────┐
   │ PEP-1  ADMISSION (agentctl RFC 0007)      │   what is even allowed to exist:
   │   CEL → webhook → config-schema →         │   tier×tenancy, trifecta-override, image
   │   init-container ground truth             │   signature, secret-ref scoping
   ├─────────────────────────────────────────┤
   │ PEP-2  MANAGEMENT ACCESS (RFC 0009)       │            ──mTLS──▶  node-agent
   │   humans → aggregated APIServer (per-verb │                       │ attested socket (RFC 0002 §7)
   │   RBAC + SAR + forwarded identity);       │   PEP-3 ──────────────┤  ▼  NO in-band auth
   │   operator → direct mTLS (autonomous)     │   internal chokepoint │  a conformant agent
   ├─────────────────────────────────────────┤   (RFC 0008 §7.2):     │  (management profile,
   │ PEP-3  NODE-AGENT INTERNAL AUTHZ (0008)   │   per-target-ns       │   PeerOrigin=reachability)
   │   two known mTLS clients; per-target-ns   │   re-check + audit ───┘
   │   re-check; descriptive _meta; audit      │
   ├─────────────────────────────────────────┤
   │ PEP-4  A2A GATEWAY (agentctl RFC 0013)    │            ──vsock──▶  relay ──▶ agent
   │   securitySchemes authn; tenant=row-level │   (off-cluster, untrusted peers; the agent
   │   predicate; rate-limit; webhook SSRF     │    serves the live A2A core, gateway owns 100% authz)
   └─────────────────────────────────────────┘
```

| PEP | Owning RFC | Faces | Decides | Identity it trusts |
|---|---|---|---|---|
| **PEP-1 Admission** | agentctl RFC 0007 | the apiserver write path | whether an `Agent`/`AgentClass` may exist at all (tier×tenancy §4.2, trifecta override §8, image signature §7, secret-ref scope §5.1) | `AdmissionReview.request.userInfo` + SAR |
| **PEP-2 Management access** | agentctl RFC 0009 | humans + the operator | per-verb authority over a *named* agent (`drain`/`cancel`/`attach`/observe) | end-user identity via the aggregated APIServer; the operator's single mTLS identity |
| **PEP-3 Node-agent chokepoint** | agentctl RFC 0008 §7.2 | the management transport | that *this* request, for *this* attested `pod_uid` in *this* namespace, may execute (the anti-confused-deputy core) | exactly two pinned mTLS clients (operator + aggregated APIServer), from the internal CA (§5.2) |
| **PEP-4 A2A gateway** | agentctl RFC 0013 §4 | off-cluster untrusted A2A peers | authn per the card `securitySchemes`, tenant-scoped authz (row-level), rate-limit, multi-turn/steering gate | OIDC/mTLS/apiKey per tenant; the mesh CA (§5.4) for east-west |

### 3.2 The "no in-band auth across the substrate" principle (and what it forces)

Once a PEP authorizes, the request crosses the substrate to the agent **with no further
credential** — the agent will execute whatever the management/A2A profile exposes to a
reachable peer. Three consequences are normative and load-bearing:

1. **The enforcement-point set must be exhaustive — in both directions.** If *any*
   unauthenticated party can reach the management transport, it has full authority. This is
   why PEP-3 pins **exactly two** mTLS clients (agentctl RFC 0009 §5.3.1) and a NetworkPolicy
   additionally restricts *reachability* of the listener to the operator/apiserver pods (RFC
   0008 §7.4) — belt and braces, because the transport grants authority on reach alone.
   Exhaustiveness spans **both** the four caller→agent PEPs **and** the egress-authority point
   (§5.6/§6.2, which decides which pools an attested agent may reach): both classes must be
   enumerated for the model to be tractable. There is no enforcement point outside these two
   classes.

2. **The same primitive reachable through two PEPs must be gated at both.** Warm-session
   steering (`subagent.send`, the reference impl's agentd RFC 0015 §4.5) is reachable via
   the management transport (PEP-2/3, as `attach`) **and** via A2A multi-turn (PEP-4). A
   "no-puppeting" guarantee enforced at only one is no guarantee (agentctl RFC 0009 §6.1,
   RFC 0013 §4.2). §4.4 resolves this with the contract gate.

3. **Descriptive identity, never re-verified.** The caller identity a PEP forwards to the
   agent (`_meta`, the contract's P-meta convention) is **descriptive only** — the agent
   *records* who asked but never makes an authz decision on it (agentd RFC 0015 §6, RFC
   0012 §3.8; agentctl RFC 0008 §7.3). Authority lives entirely in the PEPs; the agent's
   echo of it exists for audit (§9), not enforcement.

---

## 4. The hostile-multi-tenancy mandate

### 4.1 The threat: a tenant against the platform and against its neighbours

The adversary is a **tenant who controls their own agent's instruction, config, MCP
servers, and (via prompt injection) the runtime behaviour of their loop**, and who wants
to (a) reach a *neighbour's* agent (steal its authority, exfiltrate its data), (b) reach
the *platform* (the node-agent's god-mode, the CRI socket, another tenant's secrets), or
(c) abuse shared resources (another tenant's model budget, the gateway's egress). Every
mechanism below answers one of these.

### 4.2 Kata-hybrid is mandatory for tenant workloads

The microVM kernel boundary is the **only** isolation boundary that survives a
neighbour-tenant compromise (§1.1, agentctl RFC 0002 §5). The binding rule (restated from
RFC 0002 §5, owned there, enforced by PEP-1):

> On a cluster with **hostile (untrusted) tenants**, the default and required tier for
> tenant workloads is **kata-hybrid**; `stock-unix` is **forbidden** for untrusted tenant
> agents (it has no kernel boundary — a container escape or node-root compromise crosses
> "reachability," which is authority). `sidecar-emptydir` is permitted for untrusted
> tenants **only** where Kata is unavailable and **only** with an explicit, audited
> acknowledgement that its isolation is netns-shared with no kernel boundary (agentctl
> RFC 0002 §4.3/§5).

This is enforced at PEP-1 by the effective-tenancy rule (agentctl RFC 0004 §3.3, RFC 0007
B3): `effective = max(namespace label, AgentClass.substrate.tenancy)`, `hostile`
dominating; a `hostile` effective tenancy with `tier == stock-unix` is **rejected**, and
`kata-hybrid` requires `runtimeClassName`. The substrate compatibility matrix (RFC 0002 §9)
is the honest consequence: **only AKS Pod Sandboxing and self-managed-Kata clusters offer a
real microVM boundary**; on every other managed tier the honest posture is single-tenant
(stock-unix) or netns-shared (sidecar) — hostile multi-tenancy is **not deployable** there,
and admission says so rather than pretending.

### 4.3 Attested pod→socket — reachability is only authority over the *right* pod

Because reachability == authority, the node-agent MUST prove the socket it dials is owned
by the `pod_uid` it intends to manage **before** the first side-effecting call, or a
socket-squatting tenant inherits a victim's `drain`/`cancel`/steer authority (agentctl RFC
0002 §7, AT, v1-blocking). The mechanism is per-tier and owned by RFC 0002 §7:

- **stock-unix** (`so-peercred-cgroup`): `SO_PEERCRED` → peer `(pid,uid,gid)` →
  `/proc/<pid>/cgroup` → pod UID via CRI/cgroup-v2 slice; **require** it equals the
  expected `pod_uid`. The bare UID is insufficient (two tenant pods may share a UID) — the
  **cgroup→pod-UID** mapping is the load-bearing check. (stock-unix is single-tenant-only,
  so this is the dev/single-tenant attestation; it defends squatting, not node-root.)
- **kata-hybrid** (`kata-sandbox-uds`): the runtime created the per-VM uds and bound it to
  the sandbox; the guest cannot forge a host uds. The `pod-UID → sandbox → uds` map *is*
  the access-control table — isolation is natural, there is no squattable shared path.
- **sidecar-emptydir** (`pod-local-sidecar`): one emptyDir, one pod; attestation is the
  pod boundary itself.

Normative for this RFC's model: attestation failure MUST set `attestation.verified=false`,
**withhold the descriptor from all management-capable consumers**, emit the
`security.attestation_failed` audit event (§9), and **never silently degrade** (agentctl
RFC 0002 §7). The node-agent re-attests on every reconnect (the reconnect = clean re-read
correlated by `identity.uid`, agentd RFC 0015 §8).

### 4.4 The no-puppeting gate (P-attach-gate)

Under hostile tenancy a caller must be able to `drain`/`cancel`/observe a neighbour (a
shared operational PEP) **without** being able to *steer* it (live puppeting). Two layers,
because RBAC alone is insufficient (agentctl RFC 0009 §6, RFC 0013 §4.2):

- **Layer 1 — per-verb RBAC (human path).** `agents/attach` is its own aggregated connect
  subresource with its own `create` verb (RFC 0009 §5.1/§5.4): the `agent-operator` role
  omits it; only `agent-steerer` (break-glass, every use audited) holds it. Necessary, and
  the *primary* human-path control.
- **Layer 2 — the contract per-tool gate (P-attach-gate).** RBAC at the front is not
  sufficient alone: `subagent.send` is a **work** tool listed to both Stdio and Management
  peers (not gated by `PeerOrigin` the way `drain` is), and the same primitive is reachable
  via A2A multi-turn at PEP-4. So agentctl asks the contract for a per-tool gate **within
  the Management profile** to omit `inject`/`subagent.send` from the management transport
  **without dropping** `drain`/`cancel`/observe. With it, "no live-puppeting for tenant X"
  is **structural** — the surface simply does not present steering, so even a front-side
  RBAC bug cannot reach it (capability-absence-not-error, agentd RFC 0015 §2.5).

> **This RFC is the requirement-home of P-attach-gate** (agentctl RFC 0009 §6.1.2, RFC
> 0013 §4.2 both name it and point here). Until it lands, the honest v1 posture is: the
> human-path gate is real and enforced, and the residual ("reachability of the management
> transport == ability to call `subagent.send`") reduces to **trust the two PEPs** — the
> two-known-clients rule (PEP-3) means only the operator and the aggregated APIServer reach
> the transport, and the aggregated APIServer *does* gate `agents/attach` per-verb;
> symmetrically, PEP-4 gates A2A multi-turn per-tenant. The structural form is deferred to
> the contract, the dependency is recorded, and the gap is documented rather than papered.

### 4.5 Tenant-scoped RBAC + namespaces — and **who owns the tenancy label** (RESOLVED)

Tenancy maps onto Kubernetes namespaces: a tenant's agents, RBAC, Secrets, and (default)
per-tenant gateway (§4.6) live in the tenant's namespace(s). Cold reads reuse kubeconfig
RBAC on the CRD group (`agents.x-k8s.io`); runtime verbs use the aggregated management
group (`management.agents.x-k8s.io`) with per-verb RBAC (agentctl RFC 0009 §5). The
effective tenancy of a namespace (§4.2) gates whether `stock-unix` is even selectable
there.

That makes the **`agents.x-k8s.io/tenancy` namespace label a security-critical input** —
it decides whether the microVM boundary is mandatory. RFC 0004 §3.3 left its authoritative
owner open (OQ4). **This RFC resolves it:**

> **Resolution (binding).** The `agents.x-k8s.io/tenancy` namespace label is owned by
> **platform / cluster policy, never by the tenant.** Concretely:
>
> 1. **Tenants MUST NOT have RBAC to write namespace labels** (in practice they do not have
>    `update` on `namespaces` at all in a hostile cluster; if they do for unrelated reasons,
>    a Kyverno/validating-admission policy MUST forbid mutating the `agents.x-k8s.io/tenancy`
>    key by non-platform identities). A tenant cannot relabel their namespace `single` to
>    escape Kata — that is the whole point.
> 2. **The fail-safe default is `hostile`, unconditionally.** A namespace with **no**
>    `agents.x-k8s.io/tenancy` label is treated as `hostile` by the effective-tenancy `max()`
>    (an absent label is the *most* restrictive, not the least) — with **no** weakening
>    cluster-wide default that could relax an unlabelled namespace. A single-tenant/dev
>    cluster opts a namespace *down* to `single` by **explicitly labelling it `single` at
>    provisioning** (by platform authority, point 3); the absence of an explicit downgrade
>    never grants the weaker posture. There is deliberately **no** install-time cluster
>    default that silently makes an unlabelled namespace `single` — that would re-introduce
>    the exact privilege-escalation-by-relabel this rule closes (T13). (This corrects the
>    earlier "namespace label = cluster default" phrasing in agentctl RFC 0007 §B3 and is the
>    binding reading of the `max()` defined in agentctl RFC 0004 §3.3.)
> 3. **The label is set at namespace provisioning** by the tenant-onboarding/platform
>    automation (the same actor that creates the namespace, its ResourceQuota, its NetworkPolicy
>    baseline, and its RBAC). It is a **platform-policy field**, peer to those, not a
>    self-service knob.
>
> The reasoning is the collapse point: if a tenant could weaken the tenancy posture of the
> namespace their workload runs in, they could opt out of the only real isolation boundary
> — a privilege-escalation-by-relabel. Ownership therefore **must** sit with the actor a
> tenant cannot impersonate. (Open: a per-namespace immutable annotation vs a
> cluster-policy CRD as the *carrier*, and whether a downgrade requires a second approval —
> OQ #1.)

### 4.6 Per-tenant gateways + central signing (the blast-radius rule)

Under hostile tenancy the **default is one A2A gateway Deployment per tenant** (agentctl
RFC 0013 §4.5), each with a tenant-scoped store credential (a DB role that can touch only
that tenant's rows, §5.5), tenant-scoped auth config, and an independent failure domain. A
**shared** multi-tenant gateway (row-level isolation only) is acceptable **only** for
single-tenant/trusted clusters — the same tier framing as the substrate. And **card
signing is central, never per-gateway/per-node**: the gateway serves a *pre-signed*
artifact and never holds the signing key, so a compromised gateway can at worst deny or
serve a stale card, never **forge** one (agentctl RFC 0014 §4.1, RFC 0013 §4.5; §5.4 here).

---

## 5. Secret & PKI lifecycle

There are **five distinct key/secret classes**, each with a different trust root, custody,
and rotation story. Conflating them is a recurring red-team error (the "per-node JWS key"
forge hole, the "plaintext push tokens in the store" hole). This section is the single
home for all five.

```
   ┌── (A) INTERNAL mTLS CA ─────────────────────────────────────────────┐
   │   trust root: one cluster-internal CA (cert-manager Issuer | fallback)│
   │   issues:  operator, node-agent(TierA+relay), A2A gateway, KEDA       │
   │            scaler, coordination server, aggregated APIServer,         │
   │            webhook serving cert, card-signer (transport identity)     │
   │   names:   SPIFFE-style  spiffe://<cluster>/ns/<ns>/sa/<component>     │
   └─────────────────────────────────────────────────────────────────────┘
   ┌── (B) MESH-PROVENANCE KEY (card signing) ───────────────────────────┐
   │   trust root: a JWKS pinned OUT-OF-BAND  (agentctl RFC 0014 §4)       │
   │   custody:  control-plane card-signer ONLY; never on node/gateway     │
   └─────────────────────────────────────────────────────────────────────┘
   ┌── (C) A2A INBOUND-AUTH TRUST (per tenant) ──────────────────────────┐
   │   OIDC issuer(s) / mTLS trust bundle (mesh CA) / apiKey secret        │
   │   custody:  per-tenant gateway (agentctl RFC 0013 §4.1/§4.5)          │
   └─────────────────────────────────────────────────────────────────────┘
   ┌── (D) DATA-AT-REST ENVELOPE KEY ────────────────────────────────────┐
   │   root: KMS/operator key  →  wraps per-tenant DEKs                    │
   │   protects: webhook push creds, sensitive task-store columns          │
   └─────────────────────────────────────────────────────────────────────┘
   ┌── (E) PROVIDER / WORKLOAD CREDENTIALS ──────────────────────────────┐
   │   provider model key  (agentctl RFC 0012 §5 — at the PROXY, off-pod)  │
   │   in-pod refs (only topology:none / single-tenant): mounted Secret    │
   └─────────────────────────────────────────────────────────────────────┘
```

### 5.1 The invariant under all five: no secret on the agent pod, no secret in the config file

The agent guarantees secrets are env/file-only behind a `resolve()` front door, **never
read from the config file**, and that the credential carrier is **structurally
unserializable** (no `Serialize`, so it cannot reach the manifest, `.status`, a log, or a
trace — the reference impl's agentd RFC 0006 §6 / RFC 0012 §3.7). agentctl preserves and
exploits this: in the default proxy-fronted topologies the operator injects **no** provider
token env into the agent pod (agentctl RFC 0012 §5.2), and admission (PEP-1) **rejects** a
cross-namespace `Secret`/pool reference without an explicit grant and rejects the in-pod
`topology: none` on a hostile class (agentctl RFC 0012 §5.3, RFC 0007). Where a token *must*
be in-pod (single-tenant `topology: none`), it is an env/`_FILE` ref to a mounted Secret,
never an inline value and never the config file.

### 5.2 (A) The internal mTLS CA — issuance, rotation, revocation

This is the resolution of the brainstorm §12 gap ("cert-manager is scoped only to the
webhook today") and of agentctl RFC 0009 §5.3's "internal CA" / agentctl RFC 0014 §8's
deferral.

- **Trust root.** One cluster-internal CA, scoped to the `agentctl-system` install. It
  authenticates **only** control-plane↔control-plane traffic; it is **not** a tenant-facing
  CA and is **not** the mesh CA (§5.4). Default realization: a cert-manager `Issuer`/
  `ClusterIssuer` (CA or, better, an intermediate from a KMS-backed root). **cert-manager-absent
  fallback:** the in-repo cert controller agentctl RFC 0001 §3 already specifies for the
  webhook serving cert is **generalized here** to mint/rotate every component's
  leaf cert and hot-reload it on disk (the `CertWatcher` equivalent). The honest cost is
  exactly that fallback, not "Go gives cert rotation free" (RFC 0001 §3).
- **Identities.** Each component gets a leaf cert whose identity is a **SPIFFE-style ID**
  (`spiffe://<cluster-trust-domain>/ns/agentctl-system/sa/<component>`) or, equivalently,
  a pinned client cert keyed to the component ServiceAccount. PEP-3 pins exactly two of
  these (operator + aggregated APIServer) as its only acceptable management-API clients
  (agentctl RFC 0009 §5.3.1); any other peer is refused at the TLS layer.
- **Issuance.** At install/scale-up, cert-manager (or the fallback) issues a leaf bound to
  the component's SA; SAN/URI = the SPIFFE ID. The webhook serving cert additionally drives
  `caBundle` injection (agentctl RFC 0007 §4.4).
- **Rotation.** Leaf certs rotate on a short clock (cert-manager renewal window or the
  fallback's timer) with **hot reload, no restart** (CertWatcher). The CA rotates rarely;
  CA rotation publishes the new CA into every component's trust bundle **before** switching
  signing (overlap window), so an in-flight skew never fails mTLS — the multi-component skew
  matrix this implies is the lifecycle RFC's (agentctl RFC 0017) but the **dual-trust
  overlap** is mandated here (OQ #2).
- **Revocation.** Component compromise → revoke its leaf (cert-manager `Certificate`
  deletion + re-issue with a fresh key; the fallback drops the leaf from its issued set);
  because leaves are short-lived, the practical revocation is rotation + a trust-bundle
  edit, not a CRL the components must poll. A node-agent whose host is compromised is
  revoked by cordoning/draining the node and rotating that node-agent's leaf.
- **Storage.** Private keys in cert-manager-managed `Secret`s (or the fallback's per-pod
  in-memory + disk material), RBAC-restricted to the owning component's SA; never copied
  cross-component.

### 5.3 The webhook serving cert (a leaf of A, called out because it bootstraps)

The admission webhook's serving cert is one leaf of the internal CA, but it is special:
a `failurePolicy: Fail` webhook is on the apply critical path, so its cert **must** be
healthy before `Agent` writes succeed, and the operator's own finalizer writes must not be
gated on it (the operator-SA `matchCondition` exemption, agentctl RFC 0007 §4.2). This RFC
notes only that the webhook cert is part of the internal PKI hierarchy and inherits its
rotation; the bootstrap-deadlock handling is RFC 0007's.

### 5.4 (B) The mesh-provenance (card-signing) key + (C) inbound-auth trust

- **(B) Card signing** is owned by agentctl RFC 0014 §4 and is **distinct from the internal
  mTLS CA**: it is the cross-org *provenance* key, the unit a remote org establishes trust
  in. Custody rules this RFC ratifies: the private key lives **only** in the control-plane
  card-signer (KMS/HSM recommended, a tightly-RBAC'd `Secret` the v1 baseline); it is
  **never** placed on a node, node-agent, or gateway; the public key is published as a
  **JWKS trust anchor pinned out-of-band** (a verifier MUST ignore any in-card `jku`,
  agentctl RFC 0014 §4.4 — the JWS `jku`-injection pitfall). Rotation = publish both `kid`s
  (overlap), switch signer, re-sign cards, retire the old key; revocation = remove the key
  from the JWKS + rotate + re-sign. A compromised gateway/node can serve a **stale** validly-
  signed card but **cannot forge** one — the whole reason signing is central (agentctl RFC
  0014 §4.1).
- **(C) Inbound A2A auth trust** is owned by the gateway PEP (agentctl RFC 0013 §4.1):
  per-tenant OIDC issuers (JWKS), per-tenant mTLS trust bundles (the **mesh CA** — a
  separate CA from the internal one, for east-west/intra-mesh peer certs), or per-tenant
  apiKey secrets. Credentials are Secret-referenced, never inlined; rotation is a Secret/
  issuer-config edit the gateway re-reads. This RFC's contribution: the mesh CA is a
  **tenant-facing** trust root and MUST NOT be the internal mTLS CA (§5.2) — a tenant's
  peer cert must never be a valid control-plane component identity.

### 5.5 (D) Data-at-rest envelope key — webhook creds & sensitive store columns

This resolves the "KMS/operator key, agentctl RFC 0015" pointer in agentctl RFC 0013 §4.4.

- **Webhook push creds MUST NOT be stored in plaintext** (the same reason etcd was rejected
  for the task store — brainstorm D4). They are **envelope-encrypted**: a **per-tenant data
  encryption key (DEK)** wrapped by a **root key** (a KMS key, or an operator-held root
  Secret where KMS is absent). The DEK decrypts only in the gateway's delivery worker at
  POST time (agentctl RFC 0013 §4.4); the wrapped DEK + ciphertext live in the store, the
  root key never does.
- **Per-tenant DEKs** mean a compromise of one tenant's gateway (which holds only its
  tenant-scoped DB role, §4.6) cannot decrypt another tenant's webhook creds even if it
  could read the rows.
- **Rotation** of the root key re-wraps DEKs (no re-encryption of the bulk data);
  per-tenant DEK rotation re-encrypts that tenant's creds — both background, no client
  impact. **Revocation** of a leaked DEK forces tenants to re-register webhook creds.
- **Sensitive task-store columns.** Whether the distillate/payload columns themselves are
  envelope-encrypted at rest (they may carry tenant-sensitive content) is OQ #4; the
  webhook-cred encryption is mandatory regardless.

### 5.6 (E) Provider & workload credentials (cross-reference)

Provider model credentials are owned by the intelligence plane (agentctl RFC 0012 §5):
resolved **at the proxy**, mounted as a **file** (rotation = file replacement, no proxy
restart, agent never involved), namespaced with the `IntelligenceService`, and gated by the
proxy's `peer → Agent → allowed-pools` authz map (RFC 0012 §5.4) — whose trust roots this
RFC owns (§5.2 attested peer identity feeds it). The security property — **zero secret in
the agent pod** in the default topologies — is the plane's load-bearing one (§5.1).

---

## 6. Guest→host egress restriction — the one irreducible egress

### 6.1 "No network" never closed egress (the honesty correction, carried forward)

The intelligence/model channel is an **irreducible egress leg**: the reasoning loop blocks
on an LLM call that *must* leave the trust boundary (agentd RFC 0006), and in the
lethal-trifecta model that channel is the *dangerous* one (agentd RFC 0012 §1). Removing the
cluster netns (the microVM) removes **IP-layer reachability**; it does **not** remove
egress — guest→host vsock is itself a live egress channel a compromised agent can write out
over (agentctl RFC 0002 §10 correction 1). The egress posture must therefore be explicit,
not assumed-closed.

### 6.2 Two layers, because they govern different things

| Layer | Governs | Mechanism | Owner |
|---|---|---|---|
| **IP-layer** | which *hosts/IPs* the pod may reach | NetworkPolicy **default-deny egress** on the agent pod, allowing only DNS + the egress proxy endpoint (and, off-pod tiers, nothing) | this RFC's posture; the CNI is the cluster's |
| **vsock-layer (HARDENED)** | which *guest→host vsock ports* the VM may dial | host-side **allow-list: only provisioned ports** (the intelligence dial, the management/A2A serve ports) | the node-agent (host control), agentctl RFC 0008 |
| **identity-layer** | which *pools/peers* a given agent may use | the proxy's `peer → Agent → allowed-pools` authz map (attested peer, §5.6) | agentctl RFC 0012 §5.4 |

**NetworkPolicy is IP-layer, never identity, and silent on `AF_VSOCK`** (agentctl RFC 0002
§10 correction 2, RFC 0008 §7.4): it cannot select by caller and does nothing against
guest→host vsock. So the vsock allow-list is a **host control**, not a NetworkPolicy, and
the identity decision is the proxy's. This RFC's resolution of the deferred §10 correction:

> **Binding (guest→host vsock egress restriction).** Under the HARDENED (Kata-hybrid) tier,
> the host MUST restrict guest→host vsock egress to **only the provisioned ports** — the
> intelligence dial port and the management/A2A serve port(s) the operator wired for that
> pod. This is a **deployment control the node-agent owns** at the hypervisor/host-vsock
> layer (Firecracker/Cloud-Hypervisor vsock device config), not an agent feature and not a
> NetworkPolicy. A guest attempting any other vsock port is refused at the host. The
> minimum-privilege shape of this host control (which host capability the node-agent needs)
> is a node-agent concern (agentctl RFC 0008) and a known open item.

### 6.3 The egress allow-list — one implementation, two callers

agentctl RFC 0013 §4.4 (webhook delivery) and §8 (A2A delegation-out) both point here as
"the egress allow-list home." This RFC defines **one** egress-policy implementation with two
callers:

- **Block** the cloud metadata endpoint (`169.254.169.254`), link-local, loopback, and (by
  default) RFC-1918 ranges; **resolve-then-pin** the destination IP to defeat DNS rebinding;
  enforce a **per-tenant allow-list of destination hosts**. These are exactly the SSRF
  guards the agent's own HTTP client applies (agentd RFC 0012 §3.5); agentctl applies the
  *same* guards at its two egress points that POST to client-supplied/peer URLs:
  1. **Webhook delivery** — the gateway POSTs task-status notifications to client-supplied
     callback URLs (agentctl RFC 0013 §4.4): the one component that POSTs to client-supplied
     URLs from inside the cluster.
  2. **Delegation-out** — the agent dials A2A *out* to a peer over the substrate (agentctl
     RFC 0013 §8, the P-a2a-out grammar): the gateway-mediated outbound leg.
- Both callers share one allow-list config (per tenant) and one resolver/pinner. A dial to a
  non-allow-listed host is refused and counted (`agentctl_a2a_*` / a `security.egress_denied`
  audit event, §9).

The honest residual: the **model channel itself** is allow-listed to the provider/proxy
endpoint but is, by construction, an egress leg an injected loop can attempt to abuse — the
real defense there is the **reader/actor distillate firewall** (§8), not the egress ACL
(agentctl RFC 0002 §10 correction 3). agentctl markets the firewall + the kernel boundary,
not "nothing to exfiltrate."

---

## 7. Supply chain

### 7.1 Signed agent images + admission image policy

The agent image is the most security-critical artifact agentctl runs untrusted-tenant work
in. **PEP-1 verifies it:**

- The `AgentClass.image` (the ops-RBAC'd, single home of the image + contract pin, agentctl
  RFC 0004 §3.4) is resolved to a **digest** and **cosign/sigstore-verified** against a
  configured trust policy (a `sigstore-policy-controller`/Kyverno `verifyImages` rule, or an
  equivalent check the operator performs before render). An unsigned or untrusted image is
  **rejected at admission** (PEP-1) — it never schedules. Verifying at the `AgentClass`
  (cluster-scoped, ops-owned) means the trust decision is made once, by ops, and a tenant
  cannot point an `Agent` at an unverified image (a classless `Agent` naming its own
  `spec.image` is subject to the same cluster image policy).
- Because the image is pinned by **digest**, the verified bytes are exactly the bytes that
  run — the same digest the `CapabilityProbe` cache keys on (agentctl RFC 0006 §5) and the
  init-container ground-truth uses (agentctl RFC 0007 §2.4).

### 7.2 SBOM + provenance

- Every agentctl component image (operator, node-agent, gateway, scaler, coordination
  server, aggregated APIServer) ships an **SBOM** and **SLSA provenance attestation**
  (cosign attestations), verified in the platform's own deploy pipeline. agentctl's
  components are ordinary Rust services (agentctl RFC 0001) and are signed/attested like any
  other supply-chain-controlled workload.
- The **agent** image is expected to ship the same (SBOM + provenance); the trust policy
  (§7.1) MAY require a provenance predicate (builder identity) in addition to a signature.

### 7.3 The MCP servers are *inside* the verified image — not separate pods

The red-team correction is normative (brainstorm §10.2): the MCP servers an agent runs are
**processes inside the agent image** (spawned per the operator-config server set), **not**
separate Kubernetes pods. Therefore `verifyImages`-style admission policy **cannot** verify
them independently — they are covered **only** because they are baked into the verified
agent image (§7.1). Two consequences:

- **Bake MCP servers into the verified agent image** (or pin them by digest within it); do
  not fetch server binaries at runtime from an unverified source (the agent already forbids
  the *model* from naming a launch command — agentd RFC 0012 §3.4 — but the *operator* must
  also not introduce an unverified server binary).
- **Wire the rug-pull detector to alerting.** A tool whose description hash changes between
  connections logs `mcp.tool.description_changed` at `warn` (agentd RFC 0012 §3.4,
  TOCTOU/tool-poisoning ASI01); agentctl routes this into the management-action/security
  alerting path (agentctl RFC 0010), because the MCP server surface is the real ASI01 attack
  surface and image verification alone does not catch a post-deploy description swap.

### 7.4 The contract-pinned generated client

The generated `agent-contract-client` (agentctl RFC 0018, RFC 0001 §4.2) is itself a
supply-chain input:

- It is generated from the **published contract schemas**, not from any agent binary's
  source (P0) — so there is no data-plane crate to trust.
- The agent binary fetched **to emit those schemas** in the codegen pipeline MUST be
  **signed/hash-pinned** (brainstorm §11.2) — the codegen input is supply-chain-controlled
  exactly like a runtime image.
- The client is pinned by `(contract major.minor + feature-set)` and **refuses an unknown
  contract major** (agentd RFC 0014 §6.3, agentctl RFC 0001 §4.4) — version-negotiation is
  itself a supply-chain integrity control: an agent advertising a major agentctl does not
  understand is degraded to liveness+exit-code management, never spoken to with assumptions.

> **P0 / contract-neutralization note.** The agent-branded contract surfaces this RFC
> cites in passing — `--capabilities`, the `agent://` scheme, the `agent_` metric prefix,
> `AGENT_*` env, the `a2a.*` strings — are the **reference impl's** spellings of
> contract-normative surfaces, flagged for neutralization under the P0 contract-extraction
> open question (owned by agentctl RFC 0018 / RFC 0001 §9). A second-vendor conformant agent
> with neutralized surfaces is verified, managed, and secured by the **same** PEPs and the
> same supply-chain controls unchanged — none of §7 names a binary.

### 7.5 Isolating control-plane execution of the tenant image

Two control-plane steps execute the **tenant's own image** *before/around* the agent serves
traffic: the per-digest **`CapabilityProbe` Job** that runs `--capabilities` (agentctl RFC
0006 §5) and the **`agent --validate-config` init-container** ground-truth rung (agentctl
RFC 0007 §3.3 / §2.4). Running an untrusted tenant binary inside the control plane is itself a
neighbour→platform attack surface (the brainstorm §3.3 warning against "running tenant
binaries in the control plane"), and §7.1's image-verification story is undercut if the
verified-but-untrusted bytes then run unisolated in `agentctl-system`. Normative for this
RFC's model: **any control-plane execution of a tenant image MUST run under the same
substrate rules the workload itself would get** —

- under **Kata** wherever the consuming namespace's effective tenancy is `hostile` (the probe
  Job and the validate-config init-container inherit the resolved `runtimeClassName` — never a
  bare `stock-unix` runtime for a hostile-namespace image);
- under a **dedicated, minimally-privileged ServiceAccount** (distinct from the
  operator/node-agent SA), with **no** provider, internal-CA, or signing secrets mounted;
- with **default-deny egress** (NetworkPolicy + the §6 vsock allow-list) — `--capabilities`
  and `--validate-config` need no network, so the probe/init paths are network-closed.

This keeps "we verified the bytes" (§7.1) from being silently paired with "and then we ran
those bytes with control-plane privilege." The minimum-privilege shape of the probe Job's
runtime is an operator concern (agentctl RFC 0006 §5) and tracked at OQ #8.

---

## 8. The granted-MCP-subset / Rule-of-Two trust budget (defense-in-depth)

### 8.1 The agent owns the real control; agentctl declares, advises, and gates the override

The lethal-trifecta defense (an agent that simultaneously reads untrusted content, holds
sensitive data/tools, and can egress is a one-injected-prompt exfiltration tool — agent
RFC 0012 §1) is enforced **inside the agent, per spawn**: the supervisor evaluates the tag
union over each child's **narrowed grant** at the `subagent.spawn` chokepoint and refuses
any *single* subagent that would hold all three legs without `--allow-trifecta` (agentd RFC
0012 §3.2). This is the real, unforgeable, correctly-grained control (one isolation unit =
one process). agentctl adds **three composing layers**, none of which is a blocking union:

1. **Declaration (CRD).** MCP servers carry operator-declared trifecta **tags**
   (`untrusted_input`/`sensitive`/`egress`) on the `Agent`/`MCPServerSet` (agentctl RFC
   0003/0004) — the operator-declared tags the agent's check consumes, never server-declared
   metadata (agentd RFC 0012 §3.4).
2. **Advisory detection (PEP-1).** Admission computes the **Agent-level union** across
   inline servers + all `serverSetRefs` and, when it spans the full trifecta with
   `allowTrifecta: false`, **admits with a warning** + a standalone `TrifectaUnionObserved`
   condition (agentctl RFC 0007 §3.2). It is **observational, not blocking** — because the
   canonical *safe* pattern (a reader/actor split) *declares* a full-trifecta union yet no
   single spawn holds all three; a blocking union would refuse the safe shape and train
   operators to flip the override routinely (agentctl RFC 0007 §3.1).
3. **Override gating (PEP-1).** The dangerous act is turning the per-spawn guard *off*.
   `allowTrifecta: true` (which renders the contract's `--allow-trifecta`, flipping the
   spawn chokepoint from `Refuse` to `Warn`, and is **process-global** today — agentd RFC
   0012 §6 open item) is **rejected** unless **both** an explicit override annotation
   (justification/ticket) **and** an elevated-RBAC `override-trifecta` SAR pass; on admit,
   the override is written to the apiserver audit log via `auditAnnotations` (agentctl RFC
   0007 §3.3).

### 8.2 The structural injection defense is the firewall — agentctl's job is not to defeat it

The load-bearing structural control is the **reader/actor distillate firewall** (agentd RFC
0012 §3.3): untrusted content is quarantined in a no-sensitive/no-egress reader that returns
a ~1–2k-token distillate; the actor that holds sensitive/egress tools never ingests the raw
bytes. agentctl's contribution is **to not co-locate the trifecta** (the advisory union
nudges toward the split) and **to not market "no network = safe"** (the firewall + the
kernel boundary are the real wins, §6.3, agentctl RFC 0002 §10 correction 3). agentctl
**does not** add a policy engine, a content classifier, or a "is this injection?" model call
— the agent consciously rejected those (agentd RFC 0012 §2/§5) and agentctl does not
reintroduce them (Non-goals).

---

## 9. The management-action audit vocabulary

Multiple RFCs emit to "the agentctl RFC 0015 audit vocabulary" (agentctl RFC 0007 §3.3, RFC
0008 §7.3, RFC 0009 §5.3, RFC 0010 §10.2). **This RFC owns the vocabulary; agentctl RFC
0010 §10.2 owns the sink** (the `agentctl_*` self-observability path, scraped/logged on a
path independent of the data plane). The vocabulary is **closed** (a fixed set, mirroring
the contract's closed event vocabulary discipline, agentd RFC 0010) so audit queries are
stable across versions and vendors.

| Event | Emitted by | Fields | Pairs with |
|---|---|---|---|
| `mgmt.invoked` | the node-agent PEP-3 on every `drain`/`lame-duck`/`cancel`/steer (and `pause`/`resume` once **P-pause** lands). This is the **agentctl-side** record; once the contract's **P-audit** `mgmt.invoked{tool,caller?}` lands a conformant *agent* additionally echoes its own contract event — a distinct, third record | `{tool, target(ns/name/uid), caller, forwarded_user?, trace_id}` (the agentctl spelling uses `tool`, matching the P-audit / RFC 0008/0009/0010 vocabulary) | the kube/aggregated-APIServer audit "who asked" record (agentctl RFC 0009 §5.2) — two independent records of one act |
| `mgmt.denied` | PEP-2/PEP-3 on an authz failure | `{verb, target, caller, reason}` | the per-target-namespace re-check (agentctl RFC 0009 §5.3) |
| `attach.inject` | the node-agent per inject (durable, independent of the lossy events ring — the only complete steering record until P-inject) | `{target, caller, session, digest}` | agentctl RFC 0008 §7.3 |
| `trifecta.override_admitted` | PEP-1 on an `allowTrifecta` admit (the apiserver writes the `auditAnnotations`) | `{user, namespace, name, annotation, legs}` | agent's runtime `scope.trifecta_grant` warn (agentd RFC 0012 §3.2) — admission boundary + runtime boundary |
| `image.verify_failed` | PEP-1 on an image-signature reject (§7.1) | `{class, image, digest, policy}` | the supply-chain trust policy |
| `security.attestation_failed` | the node-agent on pod→socket attestation failure (§4.3) | `{node, pod_uid, expected_uid, tier, method}` | agentctl RFC 0002 §7 |
| `security.egress_denied` | the gateway/proxy on an SSRF/allow-list refusal (§6.3) | `{caller_tenant, dest_host, reason}` | agentctl RFC 0013 §4.4/§8 |
| `intel.authz_denied` | the proxy on a pool-authz refusal (§5.6) | `{peer, agent, requested_pool, reason}` | agentctl RFC 0012 §5.4 |
| `card.signed` | the card-signer on each (re)sign (§5.4) | `{fleet, kid, signature_digest}` | agentctl RFC 0014 §4.3 |

The **double-audit invariant** is normative for the **human path**: every *destructive*
human-initiated act (`drain`/`cancel`/steer/override) is recorded **once at the front** (who
asked — the apiserver/aggregated audit log, PEP-2 via the aggregated APIServer, and PEP-1
for the override) and **once at the PEP** (what was executed against the attested target —
the `mgmt.invoked`-class event). Neither is sufficient alone; together they survive a
routing or templating bug in the front (the PEP records the *actual* attested target) and a
PEP compromise (the front records the *request*). For the **autonomous operator path**
(PEP-2's direct-mTLS half, which does **not** traverse the aggregated APIServer), there is
no aggregated "who asked" record; the **front** is instead the **apiserver write that
triggered the reconcile** (e.g. the pod-delete the operator turns into `drain == SIGTERM`,
agentctl RFC 0017 §3.1) plus the operator controller's own audit, paired with the node-agent
PEP record. The resilience claim holds for both halves only with this distinction made
explicit — a compromised node-agent's PEP record is corroborated by the apiserver-side
reconcile trigger, not by an aggregated-APIServer entry that the autonomous path never
writes.

---

## 10. The threat model

Threats × the mitigation that defeats each × the PEP/RFC that owns the mitigation. This is
the consolidated index; each mitigation is specified in its owning RFC and cross-referenced
above.

| # | Threat | Primary mitigation | Owning PEP / RFC |
|---|---|---|---|
| T1 | **Socket-squatting** — a tenant places a socket where a victim's is expected to inherit `drain`/steer authority | pod→socket attestation (`SO_PEERCRED`→cgroup→pod-UID; per-VM uds; per-pod subPath layout) | node-agent / agentctl RFC 0002 §7, §4.3 |
| T2 | **Neighbour-tenant compromise / container escape** reaching another tenant's agent | the microVM kernel boundary (Kata-hybrid mandatory; `stock-unix` forbidden for hostile tenants) | PEP-1 / agentctl RFC 0002 §5, §4.2 |
| T3 | **Confused deputy at the management PEP** — a routing/templating bug drains a pod in an unauthorized namespace | two pinned mTLS clients + per-target-namespace re-check + re-resolve-or-reject on stale mapping | PEP-3 / agentctl RFC 0008 §7.2, RFC 0009 §5.3 |
| T4 | **Live puppeting** of a neighbour (steer it, not just observe) | per-verb RBAC (`agents/attach`) + the P-attach-gate contract gate (both PEPs) | PEP-2/PEP-4 / agentctl RFC 0009 §6, RFC 0013 §4.2, §4.4 |
| T5 | **Cross-tenant task / webhook-cred data leak** via the shared store | tenant = row-level predicate + per-tenant DB role + per-tenant gateway | PEP-4 / agentctl RFC 0013 §4.2/§4.5, §4.6 |
| T6 | **Provider credential / model-budget theft** by a pod dialing another tenant's pool | zero-secret-in-pod + proxy `peer→Agent→allowed-pools` authz map (attested peer) | proxy / agentctl RFC 0012 §5, §5.6 |
| T7 | **Prompt-injection exfiltration** (lethal trifecta) | reader/actor distillate firewall + per-spawn Rule-of-Two + advisory tags/override-gating | agent + PEP-1 / agentd RFC 0012, agentctl RFC 0007 §3, §8 |
| T8 | **SSRF / cloud-metadata** via webhook delivery or delegation-out | one egress allow-list (block `169.254.169.254`/link-local/loopback/RFC-1918; resolve-then-pin; per-tenant host allow-list) | gateway / agentctl RFC 0013 §4.4/§8, §6.3 |
| T9 | **Data exfiltration over the model channel / guest→host vsock** | NetworkPolicy default-deny (IP) + host-side vsock port allow-list (HARDENED) + the firewall | node-agent / agentctl RFC 0002 §10, §6 |
| T10 | **Forged cross-org Agent Card** | central card signing + JWKS pinned out-of-band (ignore in-card `jku`); gateway serves pre-signed only | card-signer / agentctl RFC 0014 §4, §5.4 |
| T11 | **Tool poisoning / rug-pull (ASI01)** — a post-deploy MCP tool-description swap | MCP servers baked into the verified image + `mcp.tool.description_changed` → alerting | PEP-1 + obs / §7.3 |
| T12 | **Supply-chain image tampering** | cosign `verifyImages` at admission against the `AgentClass.image` digest; SBOM + provenance | PEP-1 / §7.1, §7.2 |
| T13 | **Privilege-escalation-by-relabel** — a tenant downgrades its namespace to `single` to escape Kata | tenancy label owned by platform/cluster-policy; tenants cannot write it; **fail-safe default `hostile`** | platform / §4.5 |
| T14 | **Node-root compromise** (CRI socket = node-root; node-agent holds node-local trust) | model CRI/node-agent as node-root (dedicated SA + audit); Kata bounds blast radius to that node's VMs; split untrusted-input A2A PEP from the privileged bridge | node-agent / agentctl RFC 0008 §2/§8, RFC 0002 §6 |
| T15 | **Plaintext / replayed creds at rest** in the task store | envelope encryption (per-tenant DEK wrapped by a KMS/operator root key) | gateway / agentctl RFC 0013 §4.4, §5.5 |
| T16 | **Compromised gateway/node forging authority** | gateway serves pre-signed cards (deny/stale, never forge); never holds the signing key or the internal CA; tenant-scoped DB role | §4.6, §5.4 |
| T17 | **Silent control gap** masking an attack (no audit) | the closed `mgmt.invoked`-class vocabulary + the double-audit invariant | §9, agentctl RFC 0010 §10.2 |
| T18 | **Untrusted tenant binary executed in the control plane** (`CapabilityProbe` Job / `validate-config` init-container running the tenant image inside `agentctl-system`) | run it under the workload's substrate rules (Kata where the namespace is hostile) + a dedicated minimal SA + no secrets mounted + default-deny egress | PEP-1 + operator / §7.5, agentctl RFC 0006 §5, RFC 0007 §2.4/§3.3 |

---

## 11. Non-goals

- **Re-implementing the agent's own security mechanisms.** Rule-of-Two at the spawn
  chokepoint, the reader/actor distillate firewall, the SSRF guards in the agent's HTTP
  client, gated `exec`, the `Secret`-unserializable invariant, self-MCP hardening — all live
  in the contract (the reference impl's agentd RFC 0012). agentctl **surfaces, composes, and
  depends on** them; it adds no in-agent security surface (the lone conditional ask is
  P-attach-gate, §4.4).
- **A policy engine / DSL in the loop, a prompt-injection classifier, or request signing on
  the agent.** The agent consciously reversed "governance is the moat" (agentd RFC 0012 §2);
  agentctl does **not** reintroduce a regorus/JWT/x509-in-the-loop layer. Authority is the
  PEPs + Kubernetes RBAC + the card `securitySchemes`, not an injectable in-loop engine.
- **The mechanics owned by the per-PEP RFCs.** The admission ladder (agentctl RFC 0007), the
  management access-path routing + aggregated APIServer (RFC 0009), the node-agent topology
  (RFC 0008), the gateway PEP internals + task-store schema (RFC 0013), the card field-
  mapping + signing mechanism (RFC 0013/0014), the substrate tiers + attestation methods
  (RFC 0002). This RFC is the **model + the resolved seams**, not a restatement.
- **Cost/budget enforcement** beyond naming the kill-switch seam (OQ #6) — owned by agentctl
  RFC 0012 §7.
- **The audit *sink*/transport and control-plane self-observability SLOs** — agentctl RFC
  0010 §10 (this RFC owns only the **vocabulary**, §9).
- **The specific CNI / NetworkPolicy controller, and air-gap / data-residency / compliance**
  — the cluster's choice and the lifecycle RFC's scope (agentctl RFC 0017; brainstorm §12).
- **External Secrets / Vault as a hard dependency.** Supported as a credential *source*
  (file-projected, agentctl RFC 0012 §5.3), not mandated; KMS/HSM is recommended for the
  root keys (§5) and tracked as an open question (OQ #3).

## 12. Open questions

1. **Tenancy-label carrier + downgrade approval (§4.5).** The ownership is resolved
   (platform/cluster-policy, fail-safe-`hostile`). Open: the *carrier* — a per-namespace
   immutable label guarded by a Kyverno/admission policy vs a cluster-policy CRD that
   projects tenancy onto namespaces — and whether a `hostile → single` downgrade requires a
   second approval (two-person rule). Leaning: label + admission guard for v1; CRD if
   per-tenant policy grows.
2. **Internal CA rotation choreography + skew matrix (§5.2).** The dual-trust overlap is
   mandated; the full multi-component upgrade/skew matrix (operator/node-agent/gateway/
   scaler/aggregated APIServer rotating leaves and the CA without an mTLS gap) is the
   lifecycle RFC's (agentctl RFC 0017). Confirm the overlap window and whether a SPIFFE
   trust-domain (workload-identity/SPIRE) is adopted vs pinned certs.
3. **KMS/HSM as a v1 requirement** for the card-signing key (§5.4 / agentctl RFC 0014 OQ1)
   and the envelope root key (§5.5). It is the only genuinely sound posture for cross-org
   provenance and for at-rest creds, but a `Secret`-backed key is the simpler v1 baseline.
   Pick one as v1-required vs federation/compliance-prerequisite.
4. **Encrypt sensitive task-store columns at rest (§5.5).** Webhook-cred encryption is
   mandatory; whether the distillate/payload columns (potentially tenant-sensitive) are
   also envelope-encrypted, and at what performance cost, is open.
5. **Revocation / kill-switch granularity (brainstorm §10.3, §12).** A per-grant kill-switch
   for a compromised `attach`/steer grant, and a per-fleet/per-tenant **kill-switch** (pair
   with the cost-governance kill-switch, agentctl RFC 0012 §7 / brainstorm §12) — what
   revokes fastest under compromise: rotating the node-agent leaf (§5.2), a NetworkPolicy
   cut, draining the tenant's pods, or a contract-level pause (P-pause)? Define the
   break-glass runbook.
6. **One signing identity per cluster vs per tenant (agentctl RFC 0014 OQ2).** A per-tenant
   signing identity (and per-tenant internal sub-CA?) strengthens isolation and cleans
   per-tenant federation, at the cost of N keys/CAs to rotate. Settle before federation
   (agentctl RFC 0014 §7) is built on.
7. **Per-route / per-spawn `allowTrifecta` override (§8 / agentd RFC 0012 §6 open).** The
   override is process-global today, so its blast radius is the whole daemon. A per-route
   override is a contract refinement; until then the admission gate (§8) + the runtime audit
   are the interim controls. Track as a contract ask candidate.
8. **node-agent CRI/host minimum privilege (§6.3, T14).** Exactly which host capability the
   node-agent needs for the guest→host vsock allow-list and the CRI-based attestation, and
   whether a dedicated minimally-privileged SA per node-agent tier is sufficient (agentctl
   RFC 0008 open). The node-agent remains a high-value node-root principal; minimizing it is
   ongoing.

## 13. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo decision record: §3 the kube-rs gaps incl. the
  cert-manager-absent cert fallback generalized here to all internal certs (§5.2); §4 the
  P0 anti-drift / contract-as-schema discipline (§7.4); §6 the single sanctioned Go hybrid
  seam (the aggregated APIServer PEP-2 may co-reside with); §9 the contract-extraction open
  question (the P0 neutralization home).
- **agentctl RFC 0002** — substrate & transport abstraction: §5 the tenancy×substrate forced
  resolution (Kata mandatory, §4.2); §7 pod→socket attestation (§4.3); §10 the "no network"
  honesty corrections this RFC carries into the egress posture (§6).
- **agentctl RFC 0003** — Agent & AgentFleet CRDs: the trifecta tags + `security.allowTrifecta`/
  `attachPolicy` surfaced at the CRD (§8); the `.status` conditions incl. `TrifectaUnionObserved`.
- **agentctl RFC 0004** — AgentClass, IntelligenceService, MCPServerSet: §3.3 the
  effective-tenancy `max()` rule and the `agents.x-k8s.io/tenancy` label this RFC's §4.5
  resolves the ownership of (OQ4); §3.4 the `AgentClass.image`/contract-pin home (§7.1);
  §4.3 zero-secret-in-pod; §5.3 the MCP tag union → admission.
- **agentctl RFC 0006** — operator reconcile & capability model: the digest-keyed
  `CapabilityProbe` cache (the verified-image digest, §7.1); the single status-writer
  discipline.
- **agentctl RFC 0007** — admission validation ladder (**PEP-1**): the ladder (CEL →
  config-schema → init-container ground truth); §3 the trifecta-override gating this RFC's §8
  composes; the image-policy hook (§7.1); the operator-SA fail-closed exemption (§5.3).
- **agentctl RFC 0008** — node-agent architecture (**PEP-3**): §7.2 the per-target-namespace
  internal authz chokepoint; §7.3 descriptive caller `_meta` + in-band audit; §7.4 transport-
  not-policy / NetworkPolicy is IP-layer; the host-side guest→host vsock control (§6.3).
- **agentctl RFC 0009** — management access path & RBAC (**PEP-2**): §5 the aggregated-
  APIServer per-verb RBAC + SAR + forwarded identity, and the operator's mTLS path; §5.3 the
  two-known-clients rule (the internal-CA identities §5.2 issues); §6 the attach/inject
  no-puppeting gate (P-attach-gate, §4.4).
- **agentctl RFC 0010** — observability & telemetry bridge: §10.2 the management-action
  audit **sink** + node-agent SPOF alerting + trace continuity (this RFC owns the **vocabulary**
  §9; RFC 0010 owns the sink).
- **agentctl RFC 0012** — intelligence plane: §5 zero-secret-in-pod + the proxy
  `peer→Agent→allowed-pools` authz map (§5.6, T6); §7 cost/budget (the kill-switch seam, OQ5).
- **agentctl RFC 0013** — A2A gateway & task store (**PEP-4**): §4 the gateway PEP (authn per
  `securitySchemes`, tenant = row-level predicate, rate-limit); §4.4 webhook delivery (SSRF,
  encrypted creds — §5.5/§6.3); §4.5 per-tenant gateways + central signing (§4.6); §8
  delegation-out (the second egress caller, §6.3).
- **agentctl RFC 0014** — agent mesh identity: §4 central card signing + the JWKS pinned
  out-of-band (§5.4, T10); §8 the internal-mTLS-PKI deferral this RFC resolves (§5.2).

**Contract (the reference implementation's spec — where the contract is presently written
down, not a dependency, P0)**

- **agentd RFC 0012 (the reference impl's contract spec)** — security posture: §1 the lethal
  trifecta; §2 no-policy-engine/no-auth-as-core (the conscious reversal this RFC honours);
  §3.1–3.2 tool tags + the per-spawn Rule-of-Two (§8); §3.3 the distilled-return injection
  firewall (§1.1/§8.2); §3.4 all-MCP-content-untrusted + the rug-pull detector (§7.3); §3.5
  SSRF guards (the egress posture mirrors, §6.3); §3.7 secrets / `Secret`-unserializable
  (§5.1); §3.8 self-MCP-over-unix = filesystem-perms, not in-band auth.
- **agentd RFC 0015 (the reference impl's contract spec)** — management & control surface:
  §3.4/§7 `PeerOrigin` = reachability (the collapse point); §4.5 `subagent.send` steering
  (the no-puppeting target, §4.4); §6 descriptive downward-API identity, never re-verified;
  §8 reconnect = clean re-read (re-attest, §4.3).
- **agentd RFC 0014 (the reference impl's contract spec)** — control-plane contract: §3
  primitives-not-policy; §6.3 contract-version negotiation / refuse-unknown-major (the
  supply-chain integrity control, §7.4).
- **agentd RFC 0020 (the reference impl's contract spec)** — A2A interop: the A2A method
  surface PEP-4 fronts; the descriptive `_meta` caller/tenant convention (P-meta).

**Contract asks (the cross-repo critical path, brainstorm §14)** — this RFC is the
requirement-home of one and depends on others:

- **P-attach-gate** (requirement-home here, §4.4) — a per-tool gate within the Management
  profile to omit `inject`/`subagent.send` without dropping `drain`/`cancel`/observe (the
  structural no-puppeting tier for hostile tenants).
- **P-audit** — the closed-vocabulary `mgmt.invoked{tool,caller?}` management-action event
  (§9).
- **P-meta** — the descriptive caller/tenant `_meta` convention the agent echoes, never
  re-verified (§3.2).
- **P1** — the exec-health verb (the networkless-tier probe; gates the HARDENED tier whose
  egress restriction §6.3 governs).

**External**

- Willison, "the lethal trifecta"; OWASP **ASI01** (tool poisoning) — the injection threat
  model (§1.1, §7.3, §8).
- **cosign / sigstore**, **SLSA** provenance, SBOM — the supply-chain controls (§7).
- **SPIFFE/SPIRE** — the workload-identity model the internal-CA SPIFFE IDs follow (§5.2).
- **RFC 8785** (JCS), **RFC 7515** (JWS) — the card-signing mechanism referenced from
  agentctl RFC 0014 (§5.4).
