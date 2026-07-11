# agentctl RFC 0024: AAuth delegation — direct-dial remote MCP, and the intelligence posture

> ⚠️ **Update (2026-07-10) — partially realized; the modelgateway is gone entirely.** This RFC's **direct-dial `auth.mode: aauth` MCP** design (agents dial remote MCP servers directly, no facade) shipped. But its *intelligence posture* — "the modelgateway stays for budgets, gaining AAuth inbound" — did **not**: the **ModelGateway was removed**, so agents dial the model provider directly too, and budgets are harness-tracked ([RFC 0025](0025-harness-tracked-budgets.md)), not gateway-metered. References below to the modelgateway/mcpgateway "facade" describe the pre-removal design.

**Status:** Proposed (identity/delegation track; requires 0023; composes with 0019 — the
third auth mode beside its broker tiers; amends 0021 §MCP-egress for aauth-mode servers;
budget coupling via 0025)
**Author:** Andrii Tsok
**Date:** 2026-07-09

**Part of:** the agentctl control plane — lets a provisioned agent (RFC 0023) **use** its
identity: remote MCP servers verify and authorize the *individual agent* directly
(per-agent ACLs, rate limits, audit at the resource — no static token ever provisioned),
and the intelligence plane gains a posture for AAuth both inbound (replacing source-IP
attestation at the modelgateway) and outbound (AAuth-native inference endpoints, gated on
RFC 0025 budgets).

> **Contract-first (P0).** The delegated dial is contract-shaped: the agent signs because
> its manifest says it can (`surfaces.aauth`), the server verifies against a public JWKS,
> and agentctl's job is *wiring* (render the right endpoint, open the right egress, couple
> the right admission checks). No agentd- or apd-specific behavior is load-bearing beyond
> the flags named as "where the contract is presently written down."

> ⚠️ Experimental tier, default-off, same pinning discipline as RFC 0023.

---

## 1. Summary

Today every MCP tool call leaves the pod **keyless** toward the mcpgateway facade
(`--mcp name={gw}/s/name`), which attests the caller by source IP, checks the binding,
injects a per-server static token, and forwards (RFC 0021; RFC 0019 defines the broker's
richer OAuth tiers). This is exactly right for credential-bearing servers — and structurally
wrong for AAuth servers, for one wire-level reason:

**Theorem (rewriting proxy × RFC 9421).** An AAuth signature covers `@authority` and
`@path`. The facade rewrites both (`{gw}/s/name` → upstream URL), so an agent's signature
can never verify upstream of the mcpgateway. The two available dodges both fail the point
of the exercise: *call-chaining* (the gateway re-signs as its own AAuth identity) collapses
every agent into one upstream principal — destroying the per-agent attribution AAuth
exists to provide; a *CONNECT tunnel* preserves signatures by making the gateway a blind
byte relay — keeping a daemon in the path that can no longer do anything a NetworkPolicy
doesn't do cheaper.

**Consequence: AAuth-mode MCP is direct-dial.** The agent signs for the true upstream
authority; the mcpgateway stays in the path only for servers that still need held
credentials. Nothing of value is lost on the moved servers — measured against what the
mcpgateway actually enforces today: per-server `budget` is parsed-but-unenforced, there is
no metering surface (`/metrics` absent), and binding scoping is *already* enforced at
render time (the operator only renders `--mcp` for bound sets). What the gateway loses is
what the remote gains, upgraded: identity-based per-agent authorization instead of one
shared token.

This RFC specifies: the `auth.mode: aauth` server union member (§3), the egress wiring
that finally gives `capabilities.egress` teeth (§4), the governance shift table (§5), the
public-issuer requirement (§6), the intelligence posture (§7), the A2A future (§8).

**§3–§4 are the review artifacts.**

---

## 2. Scope boundary with RFC 0019

RFC 0019's broker tiers answer "how does an agent use a server that demands *credentials*"
(static token today; OAuth client-credentials / EMA as designed tiers). This RFC adds the
third answer: "the server verifies *identity* — no credential exists to hold." The three
are per-server union members of one `auth` field, chosen by what the remote accepts:

| `auth.mode` | Who authenticates | Data path | Secret at rest |
|---|---|---|---|
| `none` | nobody | facade | — |
| `staticToken` | the gateway, injecting | facade | per-server Secret |
| **`aauth`** | **the agent itself, signing** | **direct** | **none** |

---

## 3. API & rendering

### 3.1 `MCPServerSet`

```yaml
spec:
  servers:
  - name: github
    endpoint: https://mcp.github.example/mcp     # the REAL remote endpoint
    auth:
      mode: aauth          # none | staticToken | aauth
    tags: ["egress"]       # trifecta legs as today
```

CEL/webhook: `mode: aauth` forbids `tokenSecretRef` (nothing to inject); endpoint must be
`https://` (no plaintext direct dial, ever — stricter than the facade path).

### 3.2 Render rules (operator)

For each bound server with `auth.mode: aauth`:

- render `--mcp {name}={endpoint}` **verbatim** (no `/s/` facade). In-cluster endpoints are
  absolutized exactly as the mcpgateway does (trailing-dot FQDN) to kill search-domain
  hijack; external endpoints pass through.
- require the Agent to be identity-provisioned: `spec.identity.aauth` must be set
  (admission, §3.3) — a direct AAuth dial from an identity-less agent would just 401.
- signing behavior is the contract's: an identity-installed agent signs **all** MCP
  servers unless a server opts out. Signed headers reaching a *legacy* facade server are
  harmless-by-design (non-AAuth servers ignore them; the token is proof-of-possession —
  useless without the key) but do disclose `{identity, provider}` to that server. §9.3
  treats this; a per-server opt-out arg form is filed upstream (agentd polish ask — the
  config-file `aauth: false` knob has no CLI-arg equivalent agentctl can render today).

### 3.3 Admission coupling (the trifecta becomes real)

`capabilities.egress` has been **admission-only** since the grouping pass — declared, gating
the trifecta, wiring nothing. This RFC gives it its first enforcement semantics:

- An Agent binding any `auth.mode: aauth` server **must** declare `capabilities.egress: true`
  (webhook cross-object check, same rung as the ModelPool-existence check).
- `capabilities.egress: true` + aauth servers ⇒ the operator renders the egress policy of
  §4. No egress capability ⇒ deny at admission, not a black-holed dial at runtime.

---

## 4. Egress wiring

Today's rendered posture (gated, RFC 0021): default-deny + egress **only** to
control-plane gateways + DNS. Direct dial needs precise new holes. Honesty first: **vanilla
NetworkPolicy cannot express "egress to `mcp.github.example` only"** — L3/L4 only, no FQDN.
Three tiers, rendered per what the cluster can enforce:

1. **Baseline (vanilla, always renderable):** a per-namespace
   `agent-allow-aauth-egress` policy for identity-provisioned agents with aauth servers:
   egress TCP/443 to `0.0.0.0/0` **except** RFC 1918 + link-local + the cluster CIDRs —
   "internet-only egress". Lateral movement stays blocked; the internet-facing hole is
   honest and visible. Plus egress to the AP (token refresh) for all identity-provisioned
   agents.
2. **FQDN tier (Cilium/Calico detected/configured):** documented recipes rendering
   DNS-aware policies scoped to the exact declared endpoints (the operator knows every
   endpoint — they are CRD fields). Not implemented in v1; the values knob reserves the
   tier name.
3. **No-CNI-enforcement:** policies render inert (existing documented caveat) — the
   admission gate (§3.3) still forces the *declaration*, so intent is at least auditable.

The design consequence worth stating: **egress moves from "implicit via gateway" to
"declared intent, rendered as policy"** — the model `capabilities.egress` always promised.

---

## 5. Governance shift

| Concern | Facade path (today) | AAuth direct (this RFC) |
|---|---|---|
| May agent use server X? | render-time binding + gateway 403 | render-time binding + **remote ACL on `sub`/claims** |
| Who does upstream see? | one injected token per server | the individual agent, stable id |
| Rate limits / tool ACLs | per-namespace at best (attested ns) | **per-agent at the resource** |
| Metering | none (facade has no meter) | none centrally; per-agent at the resource; budgets via RFC 0025 |
| Revocation | delete the Secret / binding (instant) | unbind (render) + netpol (instant) **and** identity revoke (≤1h) |
| Audit | gateway logs | **non-repudiable signed requests** at the resource + AP audit stream |
| Secret custody | mcpgateway vault grows per server | **nothing stored** for aauth servers |

Partner-side guidance (for teams operating the remote MCP servers): verification is a
middleware — parse the three signature headers, verify the `aa-agent+jwt` against the AP's
published JWKS (cache, egress-admit), check `cnf.jwk` signed the request, key ACLs off
`sub`. The reference `aauth-core` crate implements the whole path and is dependency-light;
trust in our AP is a per-`iss` allow decision, exactly like trusting an API-key issuer.

---

## 6. The public house requirement

Remote verification means the AP issuer must be **reachable by the resource** (RFC 0023
§4.3). For this RFC's headline use, that means a public issuer domain with split-horizon
DNS for in-cluster agents. Only `/.well-known/aauth-agent.json` + JWKS need public
exposure; ceremony and admin endpoints can stay restricted. Clusters running an internal
issuer can still use everything in §7 (in-cluster verification) but not remote delegation.

---

## 7. The intelligence posture

**Theorem 2 (metering needs the data path).** agentctl's token budgets are enforced by an
atomic ledger *on* the request path (modelgateway). The AP is never on the path — AAuth
brings identity, not metering. Consequence: **the modelgateway stays** wherever agentctl
budgets matter. AAuth changes its faces in two steps:

### 7.1 Inbound: `modelgateway.attestIdentity: aauth` (gated on agentd signing the intel dial)

Replace source-IP attestation with signature verification: verify `aa-agent+jwt` (JWKS from
the AP, cached), check the request signature against `cnf.jwk`, resolve the principal
`sub` → Agent CR via the `agentctl.dev/aauth-local` label index (RFC 0023 §7.2) — namespace,
pool binding, and budgets exactly as today. Wins: no kube-API pod lookups on the hot path,
no IP-reuse races, NAT/mesh/hostNetwork indifference, the node-agent trusted-forwarder
vestige retires, and — the strategic one — **out-of-cluster agents can use budgeted
intelligence** (identity is no longer a pod IP). Blocked on: agentd Ask 3 (the intel client
is bearer-only today; the signer is consulted exclusively in the MCP transport).

### 7.2 Outbound: `ModelPool.auth.mode: aauth` (gated on 7.1 + RFC 0025)

For AAuth-native inference endpoints (org-internal routers first; agent-native providers
eventually): the pool declares no `credentialSecretRef`; agents dial **direct**, signing;
the provider meters per agent identity. Admission requires a declared harness budget
(RFC 0025) because the pool's aggregate ledger does not apply on a path the modelgateway
never sees. This is the documented trade, chosen per-pool by the operator — not a default.

---

## 8. A2A (future)

Inbound: the A2A gateway accepts agent tokens beside OIDC —
`access.aauth: {trustedProviders: [issuer…], requiredClaims: {…}}` — per-`iss` trust,
letting agents from *other organizations' APs* call our agents with portable identity.
Outbound: blocked on agentd Ask 5 (the A2A client is bearer/mTLS-only). Both are the
federated-mesh arc of RFC 0014, not v1 of this track.

---

## 9. Security considerations

1. **Replay & freshness:** signatures carry `created` (±60s window) and cover
   method/authority/path; tokens are PoP-bound (`cnf.jwk`) — a captured request replays
   nowhere else and nowhere later.
2. **Revocation latency:** identity revocation is ≤ token TTL (1h) — slower than the
   facade's instant Secret-pull. Compensation: unbind/netpol changes are reconcile-fast,
   and both levers exist independently.
3. **Information disclosure:** every dialed server learns `{agent id, provider}` from the
   presented token (§3.2). Acceptable by protocol design (identity is meant to be
   presented); the per-server opt-out arg form is the polish ask for servers where even
   that is unwanted.
4. **SSRF surface on the agent:** discovery fetches `/.well-known/aauth-resource.json` on
   servers it was *configured to dial* — no new reach; endpoints are admission-validated
   `https://` URLs.
5. **Egress honesty:** §4's tier 1 is coarser than per-FQDN; it is stated as such in docs
   and status rather than pretended otherwise (the NetworkPolicies-need-Calico precedent).
6. **DNS:** in-cluster endpoints render absolutized (trailing-dot) — the search-domain
   wildcard-capture class of failure stays dead.

---

## 10. Alternatives considered

- **Call-chaining at the mcpgateway** — protocol-blessed for *auth-token* flows, but for
  identity-based access it presents the gateway's identity upstream; per-agent attribution
  (the point) dies. Revisit only for mission/consent flows where an `act` chain is the
  actual semantic.
- **CONNECT tunneling** — preserves signatures, keeps a daemon that can no longer inspect
  MCP; strictly dominated by netpol + direct dial.
- **Per-agent tokens minted into the facade** (gateway keeps injecting, but per-agent) —
  reintroduces exactly the secret sprawl AAuth eliminates, now ×N agents.
- **Waiting for FQDN policy everywhere before shipping** — rejected; tiered egress with
  stated honesty beats blocking on CNI features agentctl doesn't control.

---

## 11. Phasing & verification

- **Phase 2 (after RFC 0023 phase 1):** `auth.mode: aauth` + render + admission coupling +
  baseline egress policy + docs (public-issuer recipe, partner verification guide pointer).
- **e2e:** mock remote AAuth MCP (verifies signatures, 401s unsigned, ACLs on `sub`);
  scenarios — signed call succeeds; unsigned/foreign-key call rejected; unbound server not
  rendered; missing `capabilities.egress` denied at admission; revoke → access dies within
  TTL; netpol lane (Calico) proves lateral movement stays blocked while the public dial
  works.
- **7.1/7.2 land as their own phases** behind agentd Asks 3 and RFC 0025.
