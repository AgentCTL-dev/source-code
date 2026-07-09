# agentctl RFC 0023: AAuth agent identity — provisioning the house

**Status:** Proposed (identity/delegation track; extends 0021's "identity is the boundary" beyond the cluster; composes with 0019; companions: 0024 delegation, 0025 budgets)
**Author:** Andrii Tsok
**Date:** 2026-07-09

**Part of:** the agentctl control plane — makes agentctl the **house-provisioner**: for every
`Agent` that opts in, the operator automatically provisions a portable cryptographic identity
(`aauth:local@domain`) at an **Agent Provider** and manages its full lifecycle (key → enrollment
→ identity → revocation), so the agent can authenticate itself to any AAuth-verifying resource
on the internet — the foundation RFC 0024 builds delegation on.

> **Contract-first, not agent-first (P0).** AAuth is an open protocol
> (`draft-hardt-oauth-aauth`); the Agent Provider (`apd`, repo `agentprovider`) and the client
> implementation (agentd `--features aauth`, agentd RFC 0023) are reference implementations,
> not dependencies. agentctl feature-detects the client capability via the manifest surface
> `surfaces.aauth` and renders the flags *where the contract is presently written down*
> (agentd's `--aauth-*` family). A second conformant agent that implements the same surface is
> provisioned unchanged. The Agent Control Contract change in this RFC is one **additive**
> manifest surface (§9).

> ⚠️ **Experimental tier.** The AAuth drafts are unreleased I-Ds; `apd` runs in explicit
> demo mode (v0.1.x) and agentd ships the client behind a compile-time feature flagged
> `draft: true`. Everything in this RFC is **default-off**, gated behind
> `spec.identity.aauth` + operator/Helm configuration, and pinned per §8.5. Nothing in core
> provisioning may depend on it.

---

## 1. Summary

agentctl's identity model (RFC 0021) is **perimeter-shaped**: an agent *is* its network
position — a source IP the gateways resolve to a pod, a namespace that is the tenant. That
identity is unforgeable-enough inside the cluster and **worthless outside it**: no remote MCP
server, no external LLM endpoint, no partner organization can verify "this request came from
`default/support-agent`". Today the gateways bridge that gap by *holding shared credentials
and injecting them* — which means every upstream sees one API key for a whole namespace, the
gateway is a secret vault, and cross-org access requires provisioning static tokens by hand.

AAuth inverts this: the agent holds an Ed25519 key, an **Agent Provider (AP)** attests
`key ↔ aauth:local@domain` with short-lived (≤1h) proof-of-possession tokens
(`aa-agent+jwt`, `cnf.jwk`), and every HTTP request is signed (RFC 9421). Any resource
verifies **without contacting the AP** — one cached JWKS fetch from
`{iss}/.well-known/aauth-agent.json`. Identity becomes per-agent, portable, revocable,
attributable.

This RFC specifies:

1. **The house** — how the AP (`apd`) is deployed or referenced (§4): a chart-managed
   optional component or an external provider, with the issuer-URL rules that decide whether
   identities are verifiable inside the cluster only or by the whole internet.
2. **Automatic enrollment** (§5) — the operator provisions a per-Agent key and pre-registers
   its thumbprint over apd's admin API (**allowlist enrollment**); the agent self-enrolls at
   startup with no secret ever injected. §5.1 records precisely why the stronger
   *federated/assertion* path is not yet reachable (a one-field agentd gap) and how this
   design upgrades to it without churn.
3. **The API** (§6) — `spec.identity.aauth`, `status.identity`, admission rules, and
   manifest feature-detection.
4. **Lifecycle** (§7) — identity learning, labeling for reverse lookup, revocation on
   delete, orphan GC.

**§5 (enrollment) and §6 (the CRD) are the review artifacts.**

---

## 2. Background — two identity models, three postures

| Concern | Perimeter (today: 0021 gateways) | AAuth |
|---|---|---|
| Identity | source IP → pod → namespace | per-agent key + AP-attested token |
| Valid where | inside the cluster | anywhere HTTPS reaches, cross-org |
| Upstream sees | one shared injected credential per pool/server | the individual agent (`sub`), stable across key rotations |
| Secrets at rest | gateways hold every provider/MCP credential | none shared; per-agent key; leak = one identity ≤1h |
| Metering | **modelgateway atomic ledger — real** | **none — the AP never sees traffic** |
| Legacy resources | works today (injection) | requires resource-side adoption |
| Revocation | instant (in-path) | ≤ token TTL (default 1h) |
| Attribution | gateway logs | non-repudiable signed requests + AP audit stream |

Neither subsumes the other. Adoption comes in three postures, in dependency order:

- **P-house (this RFC):** provision identities automatically. Prerequisite for everything.
- **P-delegate (RFC 0024):** remote MCP servers verify our agents directly (the gateway
  drops out of that path — a rewriting proxy cannot preserve RFC 9421 signatures); the
  intelligence path *keeps* the modelgateway (budgets require being on the data path) and
  later gains AAuth as its *inbound* authentication.
- **P-budget (RFC 0025):** budget becomes a property of the **agent** (harness-tracked,
  contract-declared), so paths without a metering gateway remain bounded.

---

## 3. Design principles

1. **P0** — feature-detect (`surfaces.aauth`), render contract-shaped flags, never link the
   reference implementations.
2. **Default-off, experimental** — consistent with every agentctl gate. No `identity` block
   ⇒ byte-identical rendering to today.
3. **Identity ≠ authorization.** Enrollment gates *existence* of an identity; resource-side
   ACLs (and RFC 0024's egress wiring) gate *access*. Both layers stay (apd's own guidance).
4. **Secret-custody stance.** The agent pod carries exactly one piece of sensitive material:
   **its own identity key** — self-scoped, proof-of-possession-bound, revocable ≤1h, never a
   borrowed/shared credential. This *narrows* the secret-free principle rather than breaking
   it: the model-driven part of a conformant agent has **no execution-environment access —
   its only surface is MCP tools** — so the key file sits with the harness, outside the
   model's blast radius, and the manifest never carries key or token material (verified:
   agentd surfaces only `{draft, provider, agent}`).
5. **The gateways' secret vault shrinks.** Every server that moves to AAuth (RFC 0024) is a
   credential the mcpgateway no longer stores.

---

## 4. The house — deploying or referencing the Agent Provider

The operator learns the house from configuration (all optional; absence disables the
feature):

| Config | Env | Meaning |
|---|---|---|
| `identity.aauth.provider` (Helm) | `AGENTCTL_AAUTH_PROVIDER` | issuer URL of the AP, e.g. `https://ap.example.com` |
| `identity.aauth.adminTokenSecretRef` | `AGENTCTL_AAUTH_ADMIN_TOKEN_FILE` | apd admin bearer (mounted Secret) — used ONLY by the operator, never rendered into agents |

Two deployment shapes:

**4.1 Chart-managed apd (optional sub-component).** The agentctl chart gains an optional
`apd` component (image `ghcr.io/agentprovider/apd`, its own chart exists and is reusable):
`enrollment.methods: ["allowlist"]`, redis backend + shared signing keys for HA (apd's chart
enforces this at render), `admin_token` from a chart-generated Secret shared with the
operator. **TLS nuance:** apd serves plain HTTP and expects TLS termination in front. Until
the upstream native-TLS ask lands (see the asks list), the in-cluster shape is HTTP behind
the operator-rendered NetworkPolicies with `insecure_dev_mode` acknowledged — acceptable for
the experimental tier, *not* for a public issuer. The clean end state is apd serving rustls
with a cert from `agentctl-ca`, exactly like every agentctl component.

**4.2 External provider.** Point `AGENTCTL_AAUTH_PROVIDER` at an apd the org already runs
(e.g. one public `ap.example.com` shared by several clusters). The operator only needs the
admin token; nothing else is deployed.

**4.3 Issuer-reachability rule (load-bearing).** The issuer URL is the verification anchor:
every *resource* that verifies our agents fetches `{iss}/.well-known/aauth-agent.json`.

- **Internal issuer** (`https://apd.agentctl-system.svc.cluster.local`) ⇒ identities are
  verifiable **in-cluster only** (sufficient for the modelgateway-inbound posture, RFC 0024
  §7).
- **Public issuer** (`https://ap.example.com`) ⇒ identities are verifiable by the internet —
  required for remote-MCP delegation (RFC 0024). Only the well-knowns need public exposure;
  enrollment/token/admin endpoints can be restricted. In-cluster agents must reach the *same
  origin* (signatures cover `@authority`) — split-horizon DNS or hairpin.

**4.4 Per-tenant houses (future).** apd is single-issuer per deployment. Strong tenant
isolation at the identity layer = one apd per tenant (`aauth:x@team-a.ap.example`), letting
external resources trust issuers per tenant. Deferred until the multi-tenant claims story
resolves (§10.3); v1 runs one house per cluster.

---

## 5. Enrollment — why allowlist, and the exact mechanics

### 5.1 The anchor decision (recorded constraints)

apd offers four enrollment gates. What stock agentd can actually *present* today decides:

| Gate | Requires from the agent | Stock agentd (`aauth` build) | Verdict |
|---|---|---|---|
| `open` | nothing | works | unacceptable outside dev (any key enrolls) |
| `token` | one-time secret in env | works (`--aauth-enroll-token`) | **rejected**: injects a secret (violates the stance), and restart semantics are brittle unless the key is durable anyway — at which point allowlist is strictly better |
| `federated` (K8s SA token / operator-minted cnf-bound JWS / x5c) | `enrollment_assertion` in the enroll body | **cannot** — agentd's enroll body is `{platform, enrollment_token?, ps?}`; no assertion field | **the end-state**, blocked on one agentd field (upstream Ask 2) |
| `allowlist` | nothing but the key — operator pre-registers the key **thumbprint** over the admin API | works | **chosen** — the only secret-free path available today |

Two dead ends recorded for posterity: **(a)** born-on-pod keys with operator pre-registration
need the pod to communicate its thumbprint to the operator *before* enrollment — every
authenticated channel for that is source-IP again (circular); **(b)** `x5c` assertions built
from the existing serving certs fail on three facts — the certs carry
`usages: ["server auth"]` (apd requires client-auth), DNS-only shared-shape SANs, and agentd
cannot construct assertions anyway.

**Consequence:** near-term, the **operator generates the durable key**. This bends "key born
on the device" (the seed transits etcd as a per-Agent Secret) — accepted as transitional
(§8.2), and it buys a real property: **stable identity across pod restarts** (enrollment is
idempotent by key thumbprint), which is what a singleton `Agent` wants for audit continuity.
The upgrade path (§10.1) removes the bend without API churn.

### 5.2 The flow

```
reconcile Agent with spec.identity.aauth:
 1. ensure Secret {name}-aauth-key           # 32-byte Ed25519 seed, base64url-unpadded,
    (owner-ref'd to the Agent)               # exactly the file format agentd's key.rs reads
 2. compute jkt = RFC 7638 thumbprint of the public JWK {"crv","kty","x"}
 3. POST {provider}/admin/allowed-keys {jkt, label: "{ns}/{name}", ttl}
    (idempotent per reconcile; registrations are consumed on first enrollment,
     re-enrollment of an already-enrolled key is idempotent server-side)
 4. render the pod:
      --aauth-provider {provider}
      --aauth-key-file /etc/agentctl/aauth/agent.key    # Secret mount, read-only
 5. pod starts → agentd loads the key (load-if-present path), self-enrolls
    ({platform:"workload"}, no token), fetches its first agent token,
    logs aauth.ready, and signs every MCP request from then on
 6. operator learns the identity (§7.2) → status.identity.aauth
```

Failure semantics map onto the existing exit-code contract: malformed AAuth config = exit 2
(terminal, `EXIT_USAGE`); apd unreachable / registration missing at prime = exit 4
(`INTEL_UNAVAILABLE`, retriable) — Jobs back off, Deployments restart; the allowed-key `ttl`
must therefore comfortably exceed worst-case scheduling latency (default: 24h, reconciler
re-registers on every pass until enrolled).

### 5.3 Fleets (near-term caveat)

Fleet members share one pod template ⇒ one mounted key ⇒ **one AAuth identity per fleet**
(each replica independently enrolls/refreshes the same key; apd treats that as the same
agent). That is semantically aligned with RFC 0022's "the fleet is one addressable agent"
front door, and honestly weaker for per-replica attribution. Per-pod identities arrive with
assertion enrollment (§10.1): each replica generates its own key and enrolls with its
pod-scoped projected SA token. The CRD shape (§6) is already per-template and needs no
change for that upgrade.

---

## 6. API

### 6.1 `Agent` / fleet `template`

```yaml
spec:
  identity:
    aauth: {}                     # opt-in; empty object = use operator defaults
    # aauth:
    #   provider: https://ap.example.com   # override the operator default
    #   personServer: https://ps.example   # RESERVED (Case C, user-scoped); not rendered in v1
status:
  identity:
    aauth:
      agent: "aauth:k7q3p9n2@ap.example"   # learned per §7.2
      provider: "https://ap.example.com"
      enrolledAt: "2026-07-09T12:00:00Z"
```

`identity` is a grouped block (per the api-design grouping convention) so future identity
systems (SPIFFE consumption, x509) slot beside `aauth` rather than flattening.

### 6.2 Admission (webhook rungs, RFC 0007 ladder)

- `identity.aauth` present but no provider resolvable (spec override absent AND operator
  default unset) → **deny** with a pointed message (config error, catch at admission not at
  pod crash).
- `identity.aauth.provider` / `personServer` must be `https://` URLs (mirrors the OIDC
  well-formedness rung).
- Trifecta interaction: an AAuth identity alone adds **no** capability leg (it is identity,
  not egress) — but RFC 0024 couples `auth.mode: aauth` servers to `capabilities.egress`.

### 6.3 Feature detection (RFC 0006 capability model)

The operator renders `--aauth-*` flags **only** when `spec.identity.aauth` is set. Whether
the *image* can honor them is a manifest fact: the capability probe checks
`surfaces.aauth` post-start; a configured-but-absent surface (stock non-aauth build) sets
`Ready=False` reason `IdentityUnavailable` — the same pattern as any requested-but-missing
surface. While the surface carries `draft: true`, the operator treats the capability as
experimental and never hard-requires it for core readiness beyond this explicit opt-in.

---

## 7. Operator mechanics

### 7.1 The admin client

A small typed client for apd's admin API (bearer from `AGENTCTL_AAUTH_ADMIN_TOKEN_FILE`,
constant-time handled server-side): `POST /admin/allowed-keys`, `GET /admin/agents`,
`POST /admin/agents/{local}/revoke`. All calls are operator-side only; the admin token never
appears in any rendered pod.

### 7.2 Identity learning

agentd exposes the resolved identity in its manifest: `surfaces.aauth.agent`
(`aauth:…@domain`, `null` until primed). The operator already fetches manifests over the
management mTLS hop (capability probing); when the surface reports a non-null `agent`, the
operator writes `status.identity.aauth` and stamps the label
`agentctl.dev/aauth-local: {local}` — the **reverse-lookup index** gateways use in RFC 0024
§7 (map a verified token `sub` → the Agent CR without touching apd). Fallback (agent not yet
Ready): `GET /admin/agents` filtered by the `label` we registered (`{ns}/{name}`).

### 7.3 Revocation & GC

- **Delete:** finalizer `agentctl.dev/aauth-revoke` on opted-in Agents — on deletion the
  operator calls `POST /admin/agents/{local}/revoke` (local from status/label; fallback
  admin list by registration label), then removes the finalizer. The key Secret dies by
  owner-ref. Existing tokens age out ≤ TTL (default 1h); new issuance refuses immediately.
  apd unreachable at finalize ⇒ requeue with backoff, never wedge deletion forever
  (bounded retries, then event + release — documented operator posture).
- **Orphan sweep:** a periodic reconciler lists `GET /admin/agents` and revokes records
  whose `{ns}/{name}` label no longer matches a live opted-in Agent (covers finalizer-skips
  and, later, per-pod fleet identities).

---

## 8. Security considerations

1. **Key custody.** The key file is mounted read-only for the harness. A conformant agent's
   model surface is MCP tools only — the key is not inside the model's reach; the manifest
   deliberately never exposes key or token. Threat delta vs today: a pod-adjacent compromise
   today steals the *source-IP identity* (same pod = same IP); post-AAuth it steals a
   **revocable, fully-audited, ≤1h-leash identity**. Net: comparable or better, with
   attribution.
2. **Key-in-etcd (transitional).** Mitigations: per-Agent Secrets (no shared blast radius),
   namespace RBAC, revocation on any suspicion (revoke = identity dead ≤1h), and the §10.1
   upgrade that removes control-plane key custody entirely. Note honestly: apd has **no
   re-key operation** today — rotating a durable key means revoke + fresh enrollment = a new
   identity (upstream ask filed).
3. **The AP signing key is the fleet trust anchor.** Compromise = mint-anything. It lives in
   a Secret (chart `keys.existingSecret`; never chart-generated), supports online `kid`
   rotation, KMS custody is future work. Same rigor class as the A2A card-signing seed
   (RFC 0014).
4. **Availability.** apd lands on the signed-request path *indirectly*: agents cache tokens
   (refresh ~60s early), so an apd outage ≤ token TTL degrades nothing; longer outages fail
   MCP calls (agentd exit/retry semantics apply). HA = redis backend + shared keys.
5. **Draft-protocol drift.** The e2e matrix pins `apd` + agentd versions **together**; a
   contract-conformance lane exercises enroll → sign → verify against the pinned pair.
   `surfaces.aauth.draft: true` is the tripwire — when it drops, the experimental tier can
   graduate.
6. **Audit.** apd emits a structured audit line per decision (`enroll`, `enroll_denied`,
   `agent_token_issued`, `agent_revoked`, …) — ship it with the cluster's log pipeline;
   operator events mirror registration/revocation on the Agent.

---

## 9. Contract touchpoints (all additive)

- **`manifest.schema.json`**: new optional surface
  `surfaces.aauth: false | {draft: bool, provider: string, agent: string|null}` —
  same sum-type discipline as `surfaces.workflow` (absent/false on non-aauth builds;
  hand-written deserializer in `agent-contract-client`). Contract version stays 1.0
  (additive minor, the `surfaces.workflow` precedent).
- **Env convention:** untouched — AAuth config is restart-only flags, consistent with the
  lifecycle rules (`--aauth-*` documented alongside `--serve-*` material, not env).
- **Exit codes:** untouched — 2/4 already carry the right semantics (§5.2).
- **Conformance suite:** an optional-surface assertion set — *if* `surfaces.aauth` is served:
  enrollment idempotency, signed-MCP shape (three headers, covered components), `aauth.ready`
  before first signed dial, no key/token material in manifest or logs.

---

## 10. Future work (dependency-gated)

1. **Assertion enrollment** (agentd Ask 2 — `enrollment_assertion` + file re-read): per-agent
   zero-RBAC ServiceAccount + projected token volume (`audience: {issuer}`), apd
   `type: oidc/jwks_file` with `required_claims.sub: system:serviceaccount:{ns}:agent-*` and
   `embed_claims: {kubernetes.io.namespace → k8s_namespace}`. Removes operator key custody;
   unlocks **per-pod fleet identities** (§5.3) and AP-attested tenancy claims. The `Agent`
   API does not change — only the operator's anchor.
2. **Tenancy claims on the near-term path** (apd ask): `embed_claims` on allowlist
   registrations would give operator-attested claims without waiting for (1).
3. **Per-tenant houses** (§4.4) once (1)/(2) settle which claims story wins.
4. **Person Server binding** (`identity.aauth.personServer` — reserved field): user-scoped,
   consent-gated access (AAuth Case C); the governance layer for *on whose authority* an
   agent acts, complementing agentctl's *what infrastructure it may use*.
5. **Per-subagent identities** via apd `POST /subagent-token` (`parent+disc`, single-level) —
   today an agentd process tree deliberately shares one identity.

---

## 11. Alternatives considered

- **Enrollment-token injection** — rejected: a secret on the pod, single-use vs restart
  brittleness, and strictly dominated by allowlist once keys are durable (§5.1).
- **`x5c` assertions from serving certs** — rejected on verified facts (§5.1); revisit only
  if the serving-cert profile gains client-auth usage + URI SANs *and* agentd learns to build
  assertions — at which point the projected-SA path is simpler anyway.
- **SPIFFE/SPIRE as the identity layer** — SVIDs are not what remote MCP servers verify
  (mTLS-terminating, not application-layer; cross-org resources behind CDNs won't join);
  AAuth is proxy-tolerant (signatures survive TLS termination) and is the direction the MCP
  ecosystem's identity work points at. apd can *consume* SPIFFE evidence as enrollment
  assertions later — the models compose rather than compete.
- **Waiting for federated enrollment before shipping anything** — rejected: the allowlist
  path is secret-free, works with stock implementations the day agentd's release binary
  carries the feature (Ask 1), and the upgrade is anchor-only (§10.1).

---

## 12. Phasing & verification

- **Phase 0 (prove it, no product code):** kind e2e — chart-deployed apd (allowlist mode) +
  an `--features aauth,tls` agentd build + a mock AAuth-verifying MCP server (reuse
  `aauth-core`'s verification path in the mock); assert: signed calls verify, unsigned calls
  401, restart keeps the identity, revoke kills access ≤ TTL.
- **Phase 1 (this RFC):** operator key/registration/render/status/finalizer + chart `apd`
  component + admission rungs + contract surface + conformance assertions. Gated default-off.
- **Exit criteria to graduate from experimental:** upstream Ask 1 landed (release binary),
  drafts stabilized (`draft: false`), assertion path (§10.1) available for fleets.
