# agentctl RFC 0014: Agent mesh identity

**Status:** Proposed (agentctl A2A track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the mesh-facing identity layer: how a fleet of conformant agents presents as **one** verifiable agent in the A2A mesh, and how that identity is signed, published, and discovered

> **One fleet, one identity (the load-bearing rule of this RFC).** An `AgentFleet`
> is N replicas but **one logical agent** (agentctl RFC 0003 §4). The mesh must
> never see N cards, N URLs, or a replica's pod identity — it sees exactly **one
> canonical Agent Card per fleet**, served at **one** stable fleet URL, with the
> replica fan-out hidden behind the cluster A2A ingress (agentctl RFC 0008 Tier B /
> RFC 0013). The card is assembled from the fleet's **contract capabilities**, not
> from any live replica, so it is stable across replica churn and **exists even at
> `replicas: 0`** (scale-from-zero fleets, agentctl RFC 0003 §4.3).

> **Contract-first, no new primitive (P0).** A fleet's mesh identity is **pure
> control-plane policy.** The card's *fields* project the capabilities manifest —
> the same manifest any conformant agent emits (`agentd --capabilities`, the
> reference impl's agentd RFC 0015 §5.2) and the same projection agentd RFC 0020 §5
> defines. Signing, provenance, naming, publication, and discovery are layered **on
> top** by agentctl and require **zero** agent cooperation: a conformant agent never
> learns it is in a fleet, never signs anything, and never learns its mesh URL
> (agentd RFC 0014 §6 non-goals). This RFC adds **no** agentd ask of its own; it
> inherits the A2A asks (P2, P-meta) the gateway already carries (agentctl RFC 0013).

---

## 1. Problem / Context

agentctl turns a declarative `Agent`/`AgentFleet` into a running, observable,
manageable workload (agentctl RFC 0003/0006) and — when A2A is enabled — into a
**first-class agent in the mesh** reachable by other agents over A2A (agentd RFC
0020; the gateway and per-card serving mechanics are agentctl RFC 0013). The A2A
model is *Agent Card → Task → methods*: a consumer first fetches an **Agent Card**
(capability + endpoint discovery), then drives the agent through it.

The identity problem is the one A2A's own model does not solve for a Kubernetes
fleet, because A2A assumes **one agent = one card = one URL**:

1. **An `AgentFleet` is N pods but ONE agent.** The fleet exists precisely so that
   work scales horizontally across fungible replicas (agentctl RFC 0003 §4, RFC
   0011). Those replicas come and go under KEDA; at `replicas: 0` none exist at
   all. The mesh must see a single, stable agent — not N cards, not a card that
   disappears when the fleet scales to zero, not a card pinned to a pod that is
   about to be evicted. **Whose manifest is the card, and what URL does it
   advertise, when there is no canonical replica?**

2. **Identity must be publishable.** A consumer needs to *fetch* the card from a
   well-known location, and an operator needs to *find* every agent available in
   the cluster. There is no in-cluster catalog of agents today.

3. **Identity must carry provenance.** A2A is moving toward cross-org meshes where
   a card asserts "I am the `triage` agent operated by Acme." A self-asserted
   `provider` field is unverifiable. A consumer — eventually a consumer in another
   org — must be able to **cryptographically verify** that a card genuinely
   originated from this control plane and was not forged or tampered with in
   transit. The brainstorm red-team is explicit (§7.2): sign cards **centrally**,
   never with a per-node key, because a per-node JWS key lets any single node
   compromise forge cross-org cards.

4. **Identity must be tenancy-scoped.** Under hostile multi-tenancy (the locked v1
   posture, brainstorm §0.6), one tenant's fleet card must not leak to, or be
   discoverable by, another tenant — and the mesh `name`/URL must not collide
   across tenants.

This RFC owns the **fleet identity layer**: the one-card-per-fleet projection
(§3), central card signing and provenance (§4), publication and the in-cluster
discovery registry (§5), the CRD-identity ↔ mesh-identity model (§6), and the
explicit **federation seam** for the post-v1 cross-cluster/cross-org case (§7).

### 1.1 The ownership seam with agentctl RFC 0013 (read this first)

RFC 0013 and RFC 0014 split the A2A surface cleanly; everything below depends on
the split.

| Concern | Owner | What it covers |
|---|---|---|
| **Per-card field mapping** (manifest → A2A Agent Card schema) | **RFC 0013** | the projection of one capabilities manifest into A2A's card schema (`name`/`url`/`capabilities`/`skills`/…), the honesty rules (`capabilities.streaming` = status-level only; truthful `input-required`/`auth-required`, agentd RFC 0020 §6), the version pin, the `agent/getAuthenticatedExtendedCard` method. |
| **Card *serving*** (the HTTP endpoint) | **RFC 0013** | the gateway serving a (pre-assembled, pre-signed) card at the well-known endpoint and the extended-card method; TLS/auth/PEP. |
| **Whose manifest, and emit exactly ONE** | **RFC 0014** (this) | feeding RFC 0013's projection from the operator's **resolved fleet capability facts** (not a live pod), collapsing N replicas to one canonical card. |
| **Signing / provenance / key lifecycle** | **RFC 0014** | central JWS signing, the JWKS trust anchor, rotation/revocation. |
| **Publication topology + in-cluster registry + discovery** | **RFC 0014** | the cluster catalog index, tenancy-scoped discovery, how external clients find a fleet's URL. |
| **CRD↔mesh naming + tenancy scoping** | **RFC 0014** | the deterministic mesh name/URL derived from CRD identity. |
| **Federation (cross-cluster/cross-org)** | **RFC 0014** | the deferred seam (§7). |

In one line: **RFC 0013 maps a manifest into a card and serves it; RFC 0014
decides *whose* manifest, *emits exactly one*, *signs* it, *names* it, and
*publishes/indexes* it.**

---

## 2. Decision

1. **One canonical Agent Card per `AgentFleet` (and per A2A-enabled singleton
   `Agent`).** The operator assembles **exactly one** card per logical agent and
   collapses the N-replica fan-out behind the cluster A2A ingress URL (§3). The
   card never names a pod, a node, or a replica count.

2. **The card is projected from contract capabilities, not from a live replica.**
   Its inputs are the operator's already-resolved **STATIC capability facts**
   (digest-keyed `CapabilityProbe`, agentctl RFC 0006 §4 two-path model / §5 cache) plus the **rendered
   config** (model, MCP servers, surfaces, limits — agentctl RFC 0003/0006) plus
   the **fleet identity** (§6). This is why the card is stable across churn and
   present at `replicas: 0` (§3.2).

3. **Cards are signed centrally** by a single control-plane signing identity, using
   A2A's JWS `signatures` mechanism over a canonical serialization, with the public
   verification key published as a **JWKS trust anchor**. The signing key **never
   leaves the control plane and is never placed on a node or in a gateway** (§4).

4. **One in-cluster registry of cards.** The operator maintains a tenancy-scoped
   **catalog** of every A2A-enabled agent's signed card — the discovery index for
   in-cluster consumers and operators (`kubectl agents catalog`, the CLI grammar is
   agentctl RFC 0016) (§5).

5. **External A2A clients fetch a fleet's card at its published well-known URL** on
   the cluster A2A ingress (served by RFC 0013's gateway). v1 in-cluster discovery
   only; out-of-band URL distribution for external consumers (§5.3).

6. **Mesh identity derives deterministically from CRD identity** — the namespaced
   `(namespace, name)` of the `Agent`/`AgentFleet`, scoped by tenancy, producing a
   collision-free, federation-ready mesh name and a single fleet URL (§6).

7. **Federation is post-v1.** Cross-cluster / cross-org mesh federation is
   explicitly deferred; this RFC fixes the **seam** (stable signing identity +
   globally-namespaceable names + a published trust root) that makes it additive
   later, and nothing in v1 forecloses it (§7).

---

## 3. Fleet-level Agent Card projection

### 3.1 The N-replicas-one-identity problem

A2A discovery is "fetch the card, then drive the agent." For a singleton that is
trivial. For an `AgentFleet` it is not: the fleet is a set of fungible replicas
(claim or shard regime, agentctl RFC 0003 §4.2) sized continuously by KEDA, each
with its **own** instance-local manifest (`identity.run_id`, `identity.instance`,
`identity.uid`, live counts — agentd RFC 0015 §5.2/§5.4). If the mesh saw replica
manifests it would see:

- **N cards** that differ only in volatile identity fields — meaningless to a
  consumer that wants *the agent*, not *a pod*.
- **A flapping card** that changes on every scale event and **vanishes at
  `replicas: 0`** — fatal for a scale-from-zero fleet, which must present a stable,
  discoverable card *while no pod runs* so consumers can resolve it and the gateway
  can **accept and queue the task into the durable store** (agentctl RFC 0013 §6)
  even at `replicas: 0`. (The card's presence is for **discovery stability** and
  **task acceptance**, not the scale trigger: scale-from-zero is driven by the
  **off-pod backlog** a KEDA external scaler reads, claim-mode only — agentctl RFC
  0003 §4.3 / RFC 0011 §5.3 — *not* by an inbound A2A request hitting the ingress.
  If inbound-A2A-driven wake is ever wanted, it is a **new activation seam**
  (A2A-task-queue-depth → the external scaler) specified in RFC 0011/0013, not
  asserted here — see §3.2.)
- **Leaked topology** — pod UID, node name, replica count — which the mesh has no
  business knowing and which is an information-disclosure vector under hostile
  tenancy.

So the fleet card cannot be "a replica's card." It must be a **fleet-level
projection** that is replica-independent by construction.

### 3.2 The card is assembled from contract capabilities, not a live pod

The operator already holds everything the card needs **without talking to any
replica**, because it resolves capability facts at admission/render time:

```
            ┌──────────────── operator (agentctl RFC 0006) ───────────────┐
            │                                                              │
 image digest ──> CapabilityProbe cache (STATIC, digest-keyed, RFC 0006 §4/§5)
            │        └─ build_features, contract_version, surfaces{} key set,
            │           operator_tools, metrics_schema, exit_codes  ───────┐ │
 AgentFleet.spec ──> renderConfig() (RFC 0003/0006)                        │ │
            │        └─ model, mcp_servers (+ trifecta tags), limits,      │ │
            │           surfaces values, instruction-derived metadata  ────┤ │
 fleet identity (§6) ──> mesh name + fleet URL + provider + tenancy  ──────┤ │
            │                                                              ▼ │
            │                       ASSEMBLE canonical fleet card (unsigned) │
            └──────────────────────────────────────────────┬───────────────┘
                                                            │
                       card-signer (central key, §4) ──► SIGN (JWS)
                                                            │
                          publish artifact (per-fleet ConfigMap + registry, §5)
                                                            │
                         gateway (RFC 0013) READS + SERVES at the well-known URL
```

Crucially, **the projection's field mapping is RFC 0013's** (manifest → A2A schema).
RFC 0014 changes only the **input**: instead of a live pod's manifest, RFC 0013's
projection is fed the operator's **resolved fleet facts** — the STATIC
digest-keyed capability set (identical across replicas by construction, since they
share an image and `AgentClass`) merged with the rendered config (identical across
replicas by construction, since they share the `template`). The volatile manifest
fields (`identity.*`, live counts, `intelligence.healthy`) are **dropped** — they
are per-replica and have no fleet-level meaning. The result is one card that does
not depend on, or name, any replica.

This directly resolves the `replicas: 0` case: the facts are digest-and-config
derived, so the card is fully assemblable with **zero pods running** — exactly when
a scale-from-zero fleet most needs a live card at its URL (agentctl RFC 0003 §4.3,
brainstorm P9).

### 3.3 What feeds the projection

| Card field (A2A) | Source | Notes |
|---|---|---|
| `name` | mesh identity (§6) | derived from CRD `(namespace, name)`; **not** a pod name. |
| `description` | `spec` operator-supplied metadata | optional CRD field; instruction text is **never** copied (it may be sensitive / large). |
| `url`, `preferredTransport` | the **fleet** A2A ingress (§3.4) | one stable URL; never a pod IP/node. |
| `provider` (`{organization, url}`) | control-plane install config | the self-asserted org; **provenance is the signature (§4)**, not this field. |
| `protocolVersion`, `version` | A2A version pin (RFC 0013) + the resolved `contract_version` | card version bumps when the resolved facts change (§5.4). |
| `capabilities` (`{streaming, pushNotifications, …}`) | `surfaces{}` + RFC 0013 honesty rules | `streaming` = **status-level only** (distillate-only, agentd RFC 0020 §6 / agentd RFC 0009 §8); declared truthfully by RFC 0013. |
| `skills` | operator-declared / derived | the derivation rule is RFC 0013's per-card mechanic; the fleet card carries the **operator-declared** set (the manifest has no skills concept) — see OQ #4. |
| `defaultInputModes`/`defaultOutputModes` | RFC 0013 projection | from the contract surface. |
| `securitySchemes`/`security` | the gateway PEP (RFC 0013) | the auth the **gateway** enforces, not the agent (agentd is auth-free, agentd RFC 0012 §3.8). |
| `signatures` | the card-signer (§4) | the central JWS provenance. |

The card carries **no secrets** (it is a public document) — the same invariant the
manifest already guarantees (no tokens, no resolved `{{secret:…}}`, agentd RFC
0015 §5.2 / RFC 0012 §3.7) — and **no per-replica identity**.

### 3.4 Hiding the replica fan-out

The card's `url` is the **fleet** endpoint: the cluster A2A `Service` that fronts
the per-node gateways (agentctl RFC 0008 Tier B / RFC 0013), **not** any pod. A
request to that URL lands on any replicated stateless gateway; the gateway routes
to the owning pod (live `taskId` → owner via the shared task store, RFC 0013 / D4)
or answers from the store (terminal tasks). All of that is RFC 0013's task-routing
concern; from the card's point of view there is **one URL** and the fan-out is
invisible. The card never advertises a replica count, a shard count, or
`additionalInterfaces` that resolve to individual pods.

### 3.5 The seam in one sentence

RFC 0013 owns the projection function `manifest → card` and the serving endpoint;
RFC 0014 calls that function **once per fleet** with the operator's resolved facts,
strips the per-replica fields, and hands RFC 0013 a single signed artifact to
serve.

---

## 4. Card signing & provenance

### 4.1 Central signing, never per-node

Provenance answers "did this card really come from this control plane, unmodified?"
The brainstorm red-team (§7.2) fixes the mechanism: **sign centrally.** A single
control-plane **signing identity** holds the private key; every fleet card is
signed by it. The key is held in a tightly-RBAC'd `Secret` (ideally KMS/HSM-backed,
or issued by a cert-manager `Issuer`), owned by a dedicated **card-signer**
component co-resident with the operator (or the aggregated APIServer, agentctl RFC
0009). It is **never** distributed to nodes, node-agents, or gateways.

This is the decisive reason the gateway serves a **pre-signed** artifact (§3.2):
the **data-path components** — the node-pinned relay and the replicated, untrusted-
input-facing A2A gateway — are less trusted than the control plane (agentctl RFC
0008 §2/§4/§6; the gateway is a *replicated stateless Deployment*, **not** node-
pinned — that is the relay — RFC 0008 §2.3). Putting a signing key on any of them
would mean a single compromise (a node, or a gateway replica) could forge a card
for **any** fleet, including a cross-org one (brainstorm §7.2/§10.2). By keeping
signing in the control plane and shipping the gateway only a finished signed
document, such a compromise can at worst **deny** service (refuse to serve) or
serve a **stale** card — it cannot **forge** one.

### 4.2 The JWS mechanism

A2A Agent Cards (the version RFC 0013 pins — see §5.1; the contract reference impl
currently serves the `0.2.x` `/.well-known/agent.json` path, agentd RFC 0020 §5)
carry an optional `signatures` array of detached JWS objects. The card-signer:

1. Takes the unsigned canonical card (§3.2), serialized with a **deterministic
   canonicalization** (JCS / RFC 8785) so that re-serialization by the gateway or a
   consumer does not invalidate the signature.
2. Produces a JWS (recommended `EdDSA`/Ed25519 or `ES256`) over that canonical
   payload, with a protected header carrying the **`kid`** (key id). It MAY also
   carry a **`jku`** (JWKS URL), but `jku` is **advisory only** — a verifier MUST
   root trust in a JWKS pinned out-of-band (§4.4), **never** in a URL taken from the
   signature it is trying to verify (the classic JWS `jku`-injection pitfall).
3. Attaches the JWS to `card.signatures` and emits the signed card as the published
   artifact (§5).

```jsonc
// signed fleet Agent Card (illustrative; field mapping owned by RFC 0013, §3.3)
{
  "protocolVersion": "0.3.0",            // pinned by RFC 0013; exact A2A version is its call
  "name": "triage",                       // mesh name from CRD identity (§6)
  "description": "Inbox triage agent",
  "url": "https://a2a.cluster.example/agents/tenant-acme/triage/",  // the FLEET ingress (§3.4)
  "preferredTransport": "JSONRPC",
  "version": "2026-06-27.4",              // re-derived on facts change (§5.4)
  "provider": { "organization": "Acme", "url": "https://acme.example" },
  "capabilities": { "streaming": true, "pushNotifications": true },  // streaming = STATUS-level (RFC 0013)
  "skills": [ /* operator-declared (§3.3) */ ],
  "securitySchemes": { /* enforced by the gateway PEP, RFC 0013 */ },
  "signatures": [
    { "protected": "eyJhbGciOiJFZERTQSIsImtpZCI6ImNwLTIwMjYtMDYiLCJqa3UiOiJodHRwczovL2EyYS5jbHVzdGVyLmV4YW1wbGUvLndlbGwta25vd24vandrcy5qc29uIn0",
      "signature": "…detached JWS signature over the JCS-canonical card…" }
  ]
}
```

### 4.3 Key lifecycle

| Phase | Action |
|---|---|
| **Generation** | At install: generate a keypair (or have cert-manager/KMS issue one). The private key lands in the card-signer's `Secret`/KMS; the public key is published to the JWKS (§4.4). One active signing `kid` at a time. |
| **Rotation** | Generate a new `kid`; **publish both public keys in the JWKS** (overlap window); switch the signer to the new `kid`; **re-sign every card** (the operator re-emits each fleet's signed artifact — low-churn, §5.4); after all cards carry the new `kid` and the overlap window passes, **retire** the old public key from the JWKS. Consumers fetching a card during the window verify against whichever `kid` it names. |
| **Revocation** | On suspected key compromise: remove the public key from the JWKS immediately (verification fails for cards signed by it), rotate to a fresh `kid`, re-sign all cards. Revocation is a JWKS edit + a re-sign sweep — no consumer-side state. |
| **Custody** | Private key never on a node/gateway (§4.1); RBAC on the `Secret` restricted to the card-signer SA; audit every signing operation alongside the management-action audit sink (agentctl RFC 0010 §10). KMS/HSM custody is recommended and is the federation-grade posture (§7). |

### 4.4 What a consumer verifies

A consumer fetches the card, reads `signatures[].protected` for the `kid`, looks the
key up in a **pinned, out-of-band-configured JWKS** (the cluster trust root —
published at a stable control-plane URL, e.g. `/.well-known/jwks.json` on the cluster
A2A ingress, but **configured into the verifier out of band**, not discovered from the
card), selects the key by `kid`, and verifies the JWS over the JCS-canonical card body.

> **Security invariant (normative).** The JWKS / trust-anchor URL is **pinned and
> configured out-of-band**; the verifier MUST **ignore any `jku` carried inside the
> card's own signature** (or accept it only if it exactly matches the pinned anchor).
> Following an in-signature `jku` would let an attacker present a forged card whose
> `jku` points at a JWKS they control and self-sign it — defeating the entire
> central-signing provenance guarantee (§4.1). Trust is rooted in the pinned anchor,
> never in the document under verification.

Success means: this card was signed by **this control plane's signing identity** and
has not been modified. The self-asserted `provider` field is thereby **bound to the
cryptographic identity**. For v1 in-cluster consumers the pinned trust anchor is the
cluster's JWKS; the cross-org trust-anchor **exchange/anchoring** (still out-of-band,
never via in-card `jku`) is the federation seam (§7).

---

## 5. Publication & discovery

### 5.1 The well-known endpoint (the external face)

A fleet's card is served at its **well-known Agent Card endpoint** on the cluster
A2A ingress, by RFC 0013's gateway. The exact path is pinned by the A2A version
RFC 0013 commits to (`/.well-known/agent-card.json` in current A2A; the older
`/.well-known/agent.json` in 0.2.x, which the contract reference impl serves today —
the version pin and the `agent/getAuthenticatedExtendedCard` extended-card method are
RFC 0013's (P2), and the brainstorm flags the version choice as open, §17 #9). The gateway serves the
**pre-signed artifact** RFC 0014 produced (§3.2/§4) — it does not assemble or sign.

### 5.2 The in-cluster registry / catalog

The operator maintains a cluster-wide **catalog** of every A2A-enabled agent's
signed card — the discovery index for in-cluster consumers and operators. Storage
and surfacing:

- **Per-fleet signed-card artifact:** the operator writes each signed card to a
  per-fleet `ConfigMap` (the card is public; the signature is public) labelled
  `agents.x-k8s.io/managed=true` and `agents.x-k8s.io/card=<fleet>`, in the fleet's
  namespace. This is the artifact the gateway reads (§5.1) and the registry indexes.
- **Curated `.status` summary, not the full card:** `.status` carries a small,
  stable card summary — `mesh.name`, `mesh.url`, `card.signatureDigest`,
  `card.lastSigned`, `card.kid` — and **not** the full card body (keeping churny /
  large data out of `.status`, agentctl RFC 0003 §2.2 / brainstorm §2.2). The card
  changes rarely (§5.4), so the summary + digest is watchable and GitOps-visible
  without write-amplification. **Status-schema ownership:** these `mesh`/`card`
  fields **extend the RFC 0003 `.status` projection contract** — RFC 0003 owns the
  Agent/AgentFleet `.status` schema and the single-`DeepEqual`-guarded writer
  discipline, and this `mesh`/`card` block is added to that schema, written **only**
  by the operator (the single status writer, agentctl RFC 0006 §8.4), never a second
  writer. (If RFC 0003's status contract instead prefers to keep this out of
  `.status`, the alternative home is the per-fleet card ConfigMap above — see OQ #3;
  either way there is **one** owner of the schema, RFC 0003.)
- **The catalog read API:** the set of card ConfigMaps + status summaries **is**
  the registry; it is surfaced as a cold read (reusing kubeconfig RBAC) via
  `kubectl agents catalog` (the CLI grammar — three faces, cold/live paths — is
  agentctl RFC 0016; this RFC fixes only the read model). An optional read-only
  in-cluster discovery endpoint MAY be exposed on the gateway/aggregated APIServer
  for non-kubectl in-cluster consumers; it serves the **same** signed cards.

The registry is **low-churn** (cards change only on §5.4 triggers), so — unlike the
A2A *task* store, which is a high-churn durable store deliberately kept out of etcd
(brainstorm §D4) — the card index is correctly a Kubernetes-native object set.

### 5.3 How external A2A clients find a fleet's card

- **In-cluster consumers** discover via the catalog (§5.2), tenancy-scoped (§6.2),
  then fetch+verify the signed card and drive the fleet at its `url`.
- **External consumers (v1)** are handed the fleet's well-known URL **out of band**
  (documentation, a tenant portal, DNS) and fetch+verify it directly at the cluster
  A2A ingress (§5.1). v1 ships **no** cross-cluster discovery service; a federated
  registry is post-v1 (§7).

### 5.4 Freshness — when the card is re-assembled and re-signed

The card is re-derived (and re-signed) only when one of its **inputs** changes:

1. the resolved STATIC capability facts change (new image digest → new
   `CapabilityProbe` result, agentctl RFC 0006);
2. the rendered config changes (a `spec`/`template` edit that bumps the resolved
   model / MCP set / surfaces / limits, i.e. an observed-generation change);
3. operator-declared card metadata (description, skills, provider) changes;
4. the signing key rotates or is revoked (§4.3).

It is **not** re-derived on scale events, replica churn, pod reschedule, or live
health flaps — those touch only per-replica fields the fleet card does not carry.
The signed artifact is cached and served as-is; signing is not per-request.

---

## 6. Identity model

### 6.1 CRD identity ↔ mesh identity

| Layer | Identifier | Owner |
|---|---|---|
| **CRD identity** | `(group=agents.x-k8s.io, kind=AgentFleet\|Agent, namespace, name)` | agentctl RFC 0003 |
| **Mesh name** | A2A card `name` — the human-facing agent name | this RFC |
| **Mesh URL** | A2A card `url` — the single fleet A2A endpoint | this RFC (value), RFC 0013/0008 (the ingress) |
| **Mesh provenance** | the signing `kid` + JWKS trust anchor (§4) | this RFC |

The mapping is **deterministic** so the mesh identity is stable, reproducible, and
collision-free:

- **Mesh `name`** defaults to the CRD `metadata.name` (an operator-supplied display
  name MAY override the human-facing label, but the canonical identifier stays
  CRD-derived). It is **not** a pod name and never changes under scaling.
- **Canonical fleet URL** is rooted at the cluster A2A ingress with a per-fleet,
  tenancy-qualified path, e.g.
  `https://<a2a-ingress>/agents/<namespace>/<name>/`. The well-known card path
  derives from it (§5.1). Exactly one URL per fleet (§3.4).
- **Collision-freedom** is guaranteed in-cluster by the `(namespace, name)` tuple
  (Kubernetes already enforces its uniqueness). The path encodes the namespace, so
  two tenants may each have a `triage` fleet without collision.

### 6.2 Tenancy scoping

Mesh identity inherits the cluster's tenancy posture (effective tenancy =
`max(namespace label agents.x-k8s.io/tenancy, AgentClass.substrate.tenancy)`,
agentctl RFC 0004 §3.3):

- A fleet's card artifact (§5.2) lives in the **tenant's namespace**; the catalog
  read (§5.2) is **RBAC-scoped** — a caller sees only cards in namespaces it may
  read. Cross-tenant card discovery is therefore gated by standard Kubernetes RBAC,
  not a bespoke ACL.
- The mesh URL path encodes the namespace (§6.1), so cards are namespaced in the
  URL space too; the gateway PEP (RFC 0013) treats the `tenant` as an
  authorization predicate (row-level, brainstorm §7.2), not a descriptive field.
- The descriptive caller/tenant identity a gateway passes inward to the agent is
  the contract's `_meta` convention (P-meta, agentd RFC 0020 §5) — never
  re-verified by the agent (the gateway already did). Mesh identity (this RFC) is
  the **callee** side of that same tenancy story.

### 6.3 Singleton `Agent` vs `AgentFleet` — one uniform rule

The rule is uniform: **every A2A-enabled logical agent has exactly one mesh
identity.** For an `AgentFleet` it is the fleet card (§3); for a singleton `Agent`
(`reactive`/`loop`) it is the one instance's card, assembled the same way (resolved
facts, not necessarily a live pod). For an ephemeral `once`/`schedule` `Agent` the
card MAY be projected but its `url` is only live while a run exists; such agents are
normally **not** mesh-published (a fire-and-exit Job is not a discoverable mesh
endpoint) — gating A2A publication on the `AgentClass`/`surfaces.a2a` being enabled
and the mode being long-lived. Whether a `once` agent is ever cataloged is OQ #5.

---

## 7. Federation deferred — the seam

Cross-cluster and cross-org mesh federation — a consumer in cluster/org **B**
discovering and verifying a fleet card from cluster/org **A** — is **explicitly
post-v1.** v1 ships **in-cluster** identity, signing, publication, and discovery
only.

What v1 deliberately builds so federation is **additive, not a rewrite**:

- **A stable, central signing identity** (§4) — the unit a remote org establishes
  trust in. Federation = exchanging/anchoring **trust roots** (the JWKS, or a CA
  that issues signing keys), exactly the cross-org provenance the central-signing
  decision (§4.1) was chosen to enable. A per-node key would have foreclosed this.
- **Globally-namespaceable names** (§6.1) — the v1 cluster-local `(namespace,
  name)` extends cleanly to a federation-unique form
  `<org>/<cluster>/<namespace>/<name>` (or an SPIFFE-/DNS-rooted identifier). v1
  reserves the higher-order scope; it does not use or collide with it.
- **A published trust anchor (JWKS) at a stable URL** (§4.4) — the thing a remote
  verifier fetches. v1 publishes it cluster-locally; federation makes it
  externally reachable + anchored.

What is **out of v1 scope** (the seam, not the build): a cross-cluster card
**registry/federation service**; cross-org **trust-root exchange / anchoring**
protocol; mesh-wide **name resolution** (resolving `<org>/<cluster>/…` to a URL);
cross-org **discovery** and the policy for who may discover whom. Nothing in v1
(central key, deterministic names, JWKS anchor, RBAC-scoped catalog) must change to
add these; they layer on top.

---

## 8. Non-goals

- **The gateway, the task store, per-card field mapping, and card *serving*.**
  agentctl RFC 0013 (the transport bridge + PEP, the durable task store, the
  manifest→card projection function, the well-known endpoint + extended-card
  method, the A2A version pin). This RFC feeds and signs what that gateway serves.
- **The A2A method surface and task lifecycle.** agentd RFC 0020 / agentctl RFC
  0013 (`SendMessage`/`GetTask`/`CancelTask`, Task states, streaming semantics).
  This RFC is about *identity*, not *work*.
- **The cluster A2A ingress / Service and the per-node relay topology.** agentctl
  RFC 0008 (Tier B) / RFC 0013. This RFC consumes "one stable fleet URL."
- **The capabilities manifest schema and any agent-side behavior.** agentd RFC
  0015 §5.2 (the reference impl's manifest). agentctl invents no agent surface and
  asks for no new primitive here (the load-bearing P0 callout).
- **The internal mTLS/PKI for control-plane components.** agentctl RFC 0015. The
  card-signing key lifecycle here is the *mesh-provenance* key, distinct from the
  internal component PKI (though both may share cert-manager/KMS plumbing).
- **The CLI grammar (`kubectl agents catalog`, `kubectl agent <name> card`).**
  agentctl RFC 0016 owns the three-faces packaging, cold/live paths, and output
  contract; this RFC fixes only the catalog **read model** it serves.
- **Cross-org / cross-cluster federation** (§7) — deferred; only the seam is in v1.
- **Authn/authz for A2A requests.** The gateway PEP (agentctl RFC 0013/0015). A
  signed card asserts *who the callee is*; it does not authenticate *callers*.

---

## 9. Open questions

1. **Card-signing key custody and the federation-grade posture.** v1 baseline is a
   tightly-RBAC'd `Secret` (or cert-manager `Issuer`); the recommendation is
   KMS/HSM. Is KMS/HSM a v1 requirement (it is the only posture that is genuinely
   sound for the cross-org case §7), or is a `Secret`-backed key acceptable for v1
   in-cluster provenance with KMS as a federation prerequisite? (brainstorm §7.3:
   "card-signing key custody" — open.)
2. **One signing identity per cluster vs per tenant.** A single cluster key is
   simplest and matches "this control plane signed it." A per-tenant signing
   identity would let a tenant's cards be verified against *its* trust root
   (stronger isolation, cleaner federation per-tenant), at the cost of N keys to
   rotate. Settle before the federation seam (§7) is built on.
3. **Catalog realization: ConfigMap+status (§5.2) vs an aggregated `cards`
   resource.** The ConfigMap+status index reuses kube RBAC and is GitOps-visible;
   an aggregated read-only `cards` GroupVersion (alongside the RFC 0009 aggregated
   APIServer) would give a first-class, server-side-filtered discovery API. Which
   is the v1 catalog, and is the optional in-cluster discovery endpoint worth
   shipping in v1 or deferred to the federation work?
4. **`skills` derivation.** The capabilities manifest has no `skills` concept, yet
   A2A cards advertise skills for discovery. v1 carries an **operator-declared**
   skill set on the CRD (§3.3). Should skills instead/also be **derived** (from the
   instruction, the MCP tool set, or a future contract `skills` surface — a
   possible agentd ask), and is the derivation RFC 0013's per-card mechanic or a
   CRD field this RFC must define? Resolve before publishing cross-org cards.
5. **Whether ephemeral (`once`/`schedule`) agents are ever cataloged (§6.3).** A
   fire-and-exit Job is not a stable mesh endpoint, but its card *is* projectable.
   Default: not mesh-published. Confirm, or define a "transient card" with a live
   `url` only while a run exists.
6. **Canonicalization choice and A2A signature interop.** JCS / RFC 8785 is the
   recommendation (§4.2); confirm it against the A2A (the version RFC 0013 pins) `signatures`
   canonicalization expectation and real A2A client verifiers, and reconcile with
   the A2A version RFC 0013 pins (brainstorm §17 #9) — a version change can move the
   well-known path and the card schema the signature covers.
7. **Stale-card bound under gateway/node compromise.** A compromised node can serve
   a *stale* (validly-signed) card (§4.1). Is a freshness bound needed (a card
   `expires`/`notAfter` the signer sets, forcing periodic re-sign), or is the
   §5.4 re-sign-on-change cadence + revocation sufficient for v1?

---

## 10. References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & repo decision record: §6 the single sanctioned
  Go hybrid seam (the aggregated APIServer the card-signer / catalog API may
  co-reside with); the P0 contract-as-schema discipline this RFC's "no new
  primitive" callout rests on.
- **agentctl RFC 0003** — Agent & AgentFleet CRDs: §4 the `AgentFleet` =
  N replicas / one logical agent (the premise of §3); §4.3 the `replicas: 0`
  scale-from-zero case (§3.2); §2.2 the keep-churn-out-of-`.status` discipline the
  card summary obeys (§5.2); the `agents.x-k8s.io` group + identity fields.
- **agentctl RFC 0004** — AgentClass, IntelligenceService, MCPServerSet: §3.3 the
  effective-tenancy `max()` rule and the `agents.x-k8s.io/tenancy` label that scope
  mesh identity (§6.2); the `AgentClass` home of the substrate/A2A posture.
- **agentctl RFC 0006** — operator reconcile & capability model: §4 the two-path
  STATIC(digest)/LIVE capability model — the **STATIC** facts feed the card (§3.2);
  §5 the digest-keyed `CapabilityProbe` cache; the `agents.x-k8s.io/managed` label.
- **agentctl RFC 0008** — node-agent architecture (two tiers): Tier B the A2A data
  path + the cluster A2A ingress fronting per-node gateways — the single fleet URL
  (§3.4) resolves to it.
- **agentctl RFC 0009** — management access path & RBAC: the aggregated APIServer
  the catalog read API / card-signer may co-reside with (§5.2/§4.1); the per-verb
  RBAC the tenancy-scoped catalog read reuses.
- **agentctl RFC 0010** — observability & telemetry bridge: §10 the
  management-action audit sink the signing operations log to (§4.3);
  control-plane self-observability for the card-signer.
- **agentctl RFC 0013** — A2A gateway & task store (**the per-card owner**): the
  manifest→card projection function, the honesty rules, the well-known endpoint +
  `agent/getAuthenticatedExtendedCard`, the A2A version pin, the task store and
  `taskId`→owner routing behind the one fleet URL. RFC 0014 feeds, signs, names,
  publishes, and indexes what RFC 0013 serves (§1.1).
- **agentctl RFC 0015** — security & multi-tenancy: the two-PEP trust model, the
  internal mTLS/PKI (distinct from the mesh-signing key §4), the hostile-tenancy
  posture the tenancy scoping (§6.2) and central-signing decision (§4.1) answer.
- **agentctl RFC 0016** — CLI & kubectl-plugin grammar: `kubectl agents catalog`
  (the discovery face) and `kubectl agent <name> card` (the cold read); the
  cold/live path split this RFC's catalog read model plugs into.

**Contract (the reference implementation's spec — where the contract is presently
written down, not a dependency, P0)**

- **agentd RFC 0015 §5.2** — the capabilities manifest = the Agent Card source; the
  no-secrets / `Secret`-unserializable invariant the card inherits (§3.3).
- **agentd RFC 0020** — A2A interop: §5 the manifest→Agent-Card projection (the
  card *is* the manifest, projected); §6 streaming = status-level honesty and the
  descriptive `_meta` caller/tenant convention (P-meta, §6.2).
- **agentd RFC 0014 §3/§6** — primitives-not-policy; the agent never learns it is in
  a fleet, never signs, never learns its mesh URL (the §3.2 / P0 premise).
- **agentd RFC 0012 §3.8** — the transport-is-the-boundary / auth-free agent posture
  (provenance and caller-auth are agentctl's, not the agent's).

**Contract asks inherited (the cross-repo critical path, brainstorm §14)** — this
RFC raises **no new** ask; it depends on the A2A asks RFC 0013 already carries:

- **P2** — `surfaces.a2a` (served A2A version + address, or false) + agentd's `a2a`
  feature committing to specific wire strings — gates whether a fleet is
  mesh-publishable at all and which well-known path/schema the signature covers.
- **P-meta** — the descriptive caller/tenant `_meta` convention (the caller side of
  the tenancy story whose callee side is mesh identity, §6.2).

**External**

- [A2A specification](https://a2a-protocol.org/latest/specification/) — Agent Card,
  `signatures` (JWS), the well-known endpoint, transports.
- RFC 8785 (JSON Canonicalization Scheme), RFC 7515 (JWS) — the signing mechanism
  (§4.2).
