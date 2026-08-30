# RFC 0028 — agentctl-identity: custody, exchange, and agent identity

- **Status:** Proposed
- **Date:** 2026-08-30
- **Decisions:** ADR-0005, ADR-0004 · **Supersedes:** RFC 0014; amends 0023–0025
- **Consumers:** gateway (RFC 0029), tenant mcpg issuers (RFC 0030), control MCP
  (RFC 0027), operator (principal projection), apiserver (authn)

## 1. Motivation

Durable agents act for humans across days (req. 16). The only sound way to keep
their calls authorized is to keep **long-lived grants in one custodian** and hand
agents **short-lived, audience-scoped tokens per call** — the on-behalf-of chain
(req. 17), the pattern AWS AgentCore Identity productizes. agentctl additionally
needs workload identity for the agents themselves (AAuth), user federation
against enterprise IdPs (req. 29), and per-user principals at each agentd's A2A
listener (addressed gates, quotas).

## 2. Component shape

Standalone Rust service (axum + rustls). State: Postgres (own schema/database),
**every secret column envelope-encrypted** with a pluggable KMS (age-file for
dev; cloud KMS / Vault transit in production). HA: stateless replicas over PG;
key cache in memory only. Strictest NetworkPolicy in the system: inbound from
gateway/apiserver/mcpg/operator only; outbound to IdPs and KMS only.

### 2.1 Surfaces

| Surface | Path family | Callers |
|---|---|---|
| OIDC federation | `/oidc/*` (device flow, auth-code+PKCE, callback) | CLI, web |
| Token validation | `/introspect`, JWKS proxy/cache | gateway, apiserver, mcpg identity chain |
| **Exchange** | `/exchange` (RFC 8693 profile, §4) | mcpg credential issuer, control MCP backend |
| Connections | `/connections/*` (consent init, list, revoke) | CLI/UI, HITL cards |
| **Agent identity** | `/aauth/*` (enroll, token, JWKS, revoke) | agentd (native AAuth client), mcpg (verify) |
| Principal mint | `/principals/*` | operator (projection), gateway (fetch) |
| Admin/audit | `/admin/*` | apiserver |

## 3. Provider federation (req. 29)

`identity.providers[]` (Helm values / Organization CR): issuer, client, scopes,
claim mappings (`sub`, org/group globs), and **capability flags** discovered +
declared: `token_exchange` (RFC 8693 at the IdP), `id_jag` (identity-assertion
JWT authorization grant), `offline_access`, `dpop`. Auth0, Keycloak, Okta and
generic OIDC profiles ship pre-mapped. Discovery/JWKS fetches are SSRF-guarded
and issuer-pinned.

**Access resolution.** On validation, identity resolves the token's claims/
groups/scopes against the org's `accessPolicies` (RFC 0033 §Organization) into
a compact, cacheable **policy document** — the roles-over-label-selectors set
this principal holds — attached to the introspection result. Gateway, apiserver
and mcpg consume that one document; token `scope` values may narrow a session
below the policy ceiling but never widen it. This is how "the engineering group
in Okta operates engineering-labeled agents; platform-admins operate all" stays
a single centrally-stated rule.

## 4. The exchange (`/exchange`)

Input (RFC 8693 vocabulary): `subject_token` = the **acting user** reference
(an opaque grant handle or IdP token), `actor_token` = the **agent's** AAuth/
workload token, `audience`/`resource` = the target (an `MCPService` audience or
the management API), plus requested scopes. Output: short-lived access token for
exactly that audience.

Resolution ladder per target provider:

1. **IdP-native exchange** where supported (RFC 8693 grant at the IdP, or ID-JAG
   assertion → token) — the user's `sub` stays end-to-end, ideal for first-party
   APIs behind the same IdP.
2. **Connection-based OAuth**: identity holds the user's per-provider refresh
   token (obtained via `/connections` consent, `offline_access`); mints/refreshes
   access tokens per audience. Covers third-party SaaS (Google, GitHub, Zendesk…)
   regardless of IdP capability.
3. **Service credential**: no user context needed → the target's service secret
   (from the registry entry), still attributed via `acting_for` metadata.

Guards: an exchange requires a live **binding** (agent ↔ user): supervisors bind
to their owner implicitly; other agents bind to the principal whose message/
webhook started the run (asserted by the gateway/agentd principal machinery and
carried as `acting_for`). No binding → `connection_required` error, which the
HITL bridge renders as a consent card (URL mode) — the human connects once, the
run proceeds. All mints are cached (per agent+user+audience), proactively
refreshed, and revocable (kill the connection or binding → downstream minting
stops within cache TTL ≤ 5 min).

DPoP/MTLS-bound tokens are minted where the provider supports them (flagged per
provider; plain bearer otherwise).

## 5. Agent identity: the AAuth provider role

- **Enrollment** is secret-free: the operator mounts a projected ServiceAccount
  token (audience `agentctl-identity`) at the agent's
  `security.aauth.enroll_assertion_file`; agentd re-reads it fresh per enroll
  (verified upstream behavior), so rotation is automatic. Identity validates the
  SA token against the cluster (TokenReview / OIDC), binds the agent's Ed25519
  key to its CR identity (`org/name/generation`), and issues agent tokens.
- **Verification**: identity publishes JWKS; the **mcpg AAuth identity plugin**
  verifies RFC 9421 signatures on every governed MCP call — closing agentd's
  inbound-verification gap on the capability plane. (A2A east–west stays mTLS;
  the AAuth-on-A2A upstream ask is tracked from the 0023–0025 lineage.)
- **Amendment of RFCs 0023–0025**: the July "blocking facts" list shrinks —
  agentd now ships enroll (open/token/**federated**), signing on MCP +
  intelligence + A2A-peer, and Case B/C token adoption. Still upstream:
  delegation chains and agentd-side inbound verification; our 0024 delegation
  design is re-expressed as identity-side exchange (§4) until then.

## 6. A2A principal minting

For each (user, agent) pair that may converse: identity mints a bearer secret,
stores it as a K8s Secret in the agent's namespace; the operator projects
`a2a.principals[] = {match: {bearer_ref}, role, labels: {org, group, user},
quotas}` (principals are hot-reloadable in agentd — no restart). The gateway
fetches the bearer per session and injects it upstream. Rotation: mint new →
project both → gateway switches → retire old.

Lazy minting: on first routed conversation, bounded by org size caps; bulk
pre-mint for fleets with declared audiences.

## 7. Secrets-store posture (req. 23)

Identity's own vault covers **user grants only**. Platform/service secrets
(model keys, MCP service tokens, webhook HMACs) stay in K8s Secrets, ESO-synced
from external stores (Vault/ASM/GSM) — identity is not a general secrets
manager, deliberately.

## 8. Threat model (summary; full table in the doc)

| Threat | Answer |
|---|---|
| Agent pod compromise | agent key ≠ user grant; exchanges are audience-scoped, short-lived, binding-gated, audited |
| Identity DB theft | envelope encryption; KMS separation; no plaintext at rest |
| Token replay to wrong audience | RFC 8707 resource binding; audience-scoped mints; DPoP where available |
| Rogue exchange requests | mTLS-restricted callers (mcpg/apiserver only), per-caller rate + anomaly alerts, binding checks |
| IdP outage | cached JWKS + refresh-ahead windows; conversation degrades before capability does; documented RTO |

## 9. Alternatives considered

Bundled IdP; exchange-inside-mcpg-only; SPIRE as the workload-identity backbone
(kept as an *integration* option for the mcpg workload plugin, not the spine —
AAuth already gives per-agent keys with agentd-native signing).

## 10. Open questions

1. Grant-handle format for `subject_token` (opaque handle vs re-presented IdP
   token) — lean opaque handle minted at gateway authn (no user tokens in agent
   space at all).
2. Whether autonomous (`autonomous_as`) exchanges may ever reach connection-based
   user grants (lean: never — service credentials only; org-configurable
   allowlist at most).
3. Per-org KMS keys (BYOK) — Deferred to the managed-service profile (PLAN).
