# MCP authentication & identity — research and cloud-native mapping

Research artifact behind **agentctl RFC 0019** (MCP server registration, identity &
authentication) and **RFC 0020** (instruction source). It surveys the current MCP
authorization landscape, the cloud-native identity patterns, our current state, and
the recommended agentctl-idiomatic design. Every external claim carries a source.

> **Bottom line.** The MCP spec converged on agentctl's own model — an MCP server is
> an OAuth 2.1 **Resource Server** delegating to an external Authorization Server, and
> the ecosystem productized the **secretless credential-broker gateway** (the
> ModelGateway pattern, applied to MCP). Autonomous agents use the **Client-Credentials
> extension (SEP-1046)** with **`private_key_jwt` (RFC 7523)**; enterprises use
> **Enterprise-Managed Authorization (EMA, SEP-990)** — but EMA **requires a human SSO
> assertion and does not support headless agents**, so it is our *on-behalf-of-user*
> tier, not the autonomous tier.

## 1. The MCP authorization spec (external, authoritative)

Three revisions: **2024-11** (no HTTP auth) → **2025-03-26** (OAuth 2.1, MCP server *as*
the auth server) → **2025-06-18** (current): **MCP server = OAuth 2.1 Resource Server**
delegating to a *separate* Authorization Server (PR #338), with **RFC 8707 Resource
Indicators mandatory** for clients (PR #734). The HTTP flow:

1. `401 + WWW-Authenticate` → **RFC 9728** Protected Resource Metadata (`/.well-known/oauth-protected-resource`, `authorization_servers[]`).
2. **RFC 8414** AS metadata discovery (or OIDC discovery).
3. Client identity: **RFC 7591 DCR**, now increasingly **SEP-991 Client-ID Metadata Documents (CIMD)** as the default.
4. **Authorization Code + mandatory PKCE**, with the **`resource` parameter (RFC 8707)** on both authorize and token requests → token **audience-bound** to one server.
5. `Authorization: Bearer` on every request (never in the URI); server validates signature + **`aud` (RFC 9068)**.

Two non-negotiables from the Security Best Practices page: **audience binding**
("never accept a token not issued for you") and **no token passthrough** (a server
calling upstream must mint its *own* token). **STDIO transport is exempt** — local
children get credentials from the environment; no network OAuth.
*Sources: modelcontextprotocol.io/specification/2025-06-18/{basic/authorization, basic/security_best_practices}; PRs #338, #734.*

## 2. Headless / autonomous agents — the Client-Credentials extension (SEP-1046)

The core spec is interactive. Machine-to-machine / no-human-in-the-loop is a **separate
official extension, SEP-1046 "OAuth client credentials"** (shipped 2025-11-25) for
"agent-talks-to-agent / cron / background agents where an end-user is unavailable."
Recommended credential: **asymmetric `private_key_jwt` (RFC 7523)** — short-lived, no
secret on the wire (client-secret allowed for compat). Official SDKs ship
`PrivateKeyJwtProvider`/`ClientCredentialsProvider`. **This is agentctl's default tier.**
*Source: modelcontextprotocol.io/seps/1046.*

## 3. Enterprise-Managed Authorization (EMA / SEP-990) — the enterprise extension

Official, **stable 2026-06-18** (`io.modelcontextprotocol/enterprise-managed-authorization`,
SEP-990 by Aaron Parecki/Okta, in the `ext-auth` repo; co-announced by Anthropic). Makes
the **enterprise IdP the single authority**: admins approve which MCP servers exist and
who may use them; users get **zero-touch SSO, no per-server consent**; revocation is
centralized and instant. Mechanism = **Cross-App Access (XAA) / the IETF ID-JAG draft**:
the IdP mints an **Identity-Assertion JWT (ID-JAG)** via **RFC 8693 token exchange** from
the user's ID Token; the client redeems it at the MCP AS via **RFC 7523** for an
audience-restricted token — the consent endpoint is never visited. Opt-in via `initialize`
capability `io.modelcontextprotocol/enterprise-managed-authorization`; advertised via
`authorization_grant_profiles_supported: [urn:ietf:params:oauth:grant-profile:id-jag]`;
ID-JAG `typ=oauth-id-jag+jwt` with `iss/sub/aud/resource/scope`.

**Critical boundary for agentctl:** EMA's `subject_token` **must** be a human's ID
Token/SAML assertion — *"no mechanism for service accounts or autonomous agents to acquire
tokens independently."* And EMA stops at **token issuance**; runtime per-call
authorization is the implementer's job.
*Sources: blog.modelcontextprotocol.io/posts/enterprise-managed-auth (2026-06-18); claude.com/blog/enterprise-managed-auth; datatracker.ietf.org/doc/draft-ietf-oauth-identity-assertion-authz-grant (draft-04, 2026-05-21); ehosseini.info/articles/...ema; aaronparecki.com/2025/11/25/1.*

## 4. Cloud-native identity & the ecosystem's broker pattern

- **Workload identity:** OAuth client-credentials (RFC 6749 §4.4) + mTLS/cert-bound tokens
  (RFC 8705); **SPIFFE/SPIRE** SVIDs (auto-rotating, no long-lived secret); **Kubernetes
  projected ServiceAccount tokens** (audience-bound, validated via **TokenReview**), which
  can be **federated** into IdPs. SPIFFE↔OAuth via **RFC 8693 token exchange** or JWT-SVID
  as `private_key_jwt`.
- **Secret delivery:** External Secrets Operator (sync→etcd), Secrets Store CSI (runtime
  mount, out of etcd), Sealed Secrets (GitOps), Vault (dynamic/short-lived; 2026 native
  AI-agent identity + AWS Bedrock AgentCore "resource token vault").
- **The broker pattern (= our ModelGateway):** the gateway holds the real credentials and
  performs the OAuth; the workload connects **credential-free**. Closest MCP analog:
  **Aembit "MCP Identity Gateway"** (agent presents a signed JWT workload identity →
  gateway validates, evaluates policy fail-closed, retrieves the credential, brokers the
  call). Also **Cloudflare workers-oauth-provider**, **Docker MCP Gateway**, **IBM
  ContextForge**; the **official MCP Registry** is discovery-only metadata.
- **Enterprise direction:** converging on **XAA/ID-JAG** (Okta/Auth0/WorkOS, 25+ adopters
  incl. Anthropic); OBO delegation token chains (RFC 8693); token vaulting; DPoP/CAEP;
  **CoSAI Workstream 4 "Agentic IAM"** (SPIFFE SVIDs + short-lived OAuth + OBO chains + audit).
*Sources: spiffe.io; k8s bound-SA-tokens (KEP-1205); external-secrets.io; docs.aembit.io MCP gateway; developers.cloudflare.com/agents; workos.com/blog/mcp-2026-spec-agent-authentication.*

## 5. Current agentctl / agentd state (internal) and gaps

| Area | Today | Gap |
|---|---|---|
| MCP transport (agentd) | **stdio-only** NDJSON child (`crates/agentd/src/mcp/client.rs`) | no HTTP/streamable client → can't speak the OAuth flow directly |
| Registration | `Agent.spec.mcp_server_set_refs` is a **name-only `LocalRef`; `MCPServerSet` has no CRD** (RFC 0004 §5 introduces it, unimplemented) | no first-class "register a server for an agent/fleet" |
| MCP auth | **`env_passthrough` of variable *names* only** (`config.schema.json`) | no per-server credential, OAuth, token, or broker |
| Broker | **none** for MCP; **ModelPool + ModelGateway** exist for *intelligence only* (RFC 0012) | the exact pattern to extend to MCP |
| Agent identity | downward-API env + **SO_PEERCRED/source-IP attestation** (`crates/agentctl-*/src/attest.rs`) | a real base to authenticate agent→broker |

## 6. Recommended cloud-native design (→ RFC 0019)

Extends patterns already shipped: **`ModelPool`/ModelGateway** (secretless credential
broker) and the **attested workload identity**.

1. **`MCPServerSet` CRD** (realizes RFC 0004 §5): register MCP servers per agent/fleet —
   `spec.servers[]` with `transport` (streamableHttp|stdio|unix), `endpoint`/`command`, and
   `auth` (one of `oauthClientCredentials{privateKeyJwt|clientSecret, scopes, resource}` |
   `ema{enterpriseIdp}` | `staticToken{secretRef}` | `envPassthrough` | `none`) + optional
   `budget`, mirroring `ModelPoolSpec`. Agents reference it via the existing
   `mcp.serverSetRefs`. Admission validates an endpoint/IdP allow-list.
2. **MCP Gateway / broker** (mirrors RFC 0012's intelligence proxy): holds the credentials
   (scoped `credential_secret_ref`), performs the OAuth **on the agent's behalf** (Resource
   Indicators, audience binding, **no token passthrough**), and exposes each server to the
   agent **keyless** — for stdio agents via a local stdio↔HTTP **bridge** (mirroring the
   `work-mcp-bridge` + routed-infer). The agent→broker hop is authenticated by the agent's
   **attested workload identity** (SO_PEERCRED/source-IP, or a projected SA token via
   TokenReview, or SPIFFE).
3. **Two auth tiers:** **Tier 1 (headless default)** = client-credentials (SEP-1046) +
   `private_key_jwt` (RFC 7523); **Tier 2 (on-behalf-of-user / enterprise)** = EMA/ID-JAG,
   the broker holding the user's IdP assertion + doing the token exchange per server,
   inheriting centralized policy/consent/audit/revocation. Pure-autonomous agents cannot use
   EMA (spec limitation) → Tier 1 covers them.
4. **Runtime authz stays ours:** EMA/OAuth stop at issuance; the broker (+ admission policy)
   is the PDP/PEP for per-tool/per-call policy, rate limits, and the trifecta.

Invariants preserved: **P0** (contract-side broker → any conformant agent benefits),
**secretless agent**, **stdio simplicity**, **attested identity**, **hostile-multitenancy
isolation**.

## 7. Composition with instruction source (→ RFC 0020)

The instruction-source **`mcpResource`** option references an `MCPServerSet` server, so the
same registration + broker that authenticates a *tool* server also authenticates the MCP
server a *live instruction* subscribes to — one MCP-identity model, two consumers.

## 8. Open decisions (resolved in the RFCs)

1. Broker placement — a dedicated `agentctl-mcpgateway` vs. a unified "agent egress broker"
   with the ModelGateway. 2. Keep agentd stdio-only + bridge, vs. add an HTTP-MCP client.
   3. Workload-identity primitive for agent→broker — reuse SO_PEERCRED/source-IP vs. adopt
   projected SA tokens (TokenReview) / SPIFFE. 4. EMA v1 scope (Tier-1 first vs. both).
   5. Honor the official MCP Registry metadata for population vs. purely declarative v1.
