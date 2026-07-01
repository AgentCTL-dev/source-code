# agentctl RFC 0019: MCP server registration, identity & authentication — the `MCPServerSet` CRD, the MCP broker & the two-tier auth model

**Status:** Proposed (agentctl tools/identity track)
**Author:** Andrii Tsok
**Date:** 2026-07-01
**Part of:** the agentctl control plane — how an `Agent`/`AgentFleet` **registers** the MCP tool servers it reasons with, **identifies** to them, and **authenticates** to them, secret-free at the pod, for both fully-autonomous and on-behalf-of-a-human workloads

> **Contract-first, not agent-first (P0).** A conformant agent reasons over a set of
> MCP tool servers named to it at provisioning time and reached over the substrate
> (agentctl RFC 0002). This RFC owns *which* servers exist for an agent, *how the
> credential to each is held off the pod*, and *how the agent's call is authenticated
> to the server* — for **any** agent that conforms to the Agent Control Contract's MCP
> surface (the per-tool trifecta tags, the granted-MCP-subset trust budget, the
> `--mcp`/`--mcp-config` config, agentd RFC 0012 §3.1/§3.2 / RFC 0017 §3.3). It MUST
> NOT encode one data-plane binary's MCP client internals. Where a concrete shape is
> needed it cites the **reference implementation** (`agentd` v1.0.0 — whose MCP client
> is a **stdio child** speaking NDJSON, `agentd .../mcp/client.rs`) as *where the
> contract is presently written down*, never as a dependency. The agent-branded
> surfaces this plane resolves into (`AGENT_SERVE_MCP`, `--mcp`/`--mcp-config`, the
> `env_passthrough` name list, the `agent://` scheme, the `agent_mcp_*` metric family)
> are contract-normative-but-branded — cited as the reference spelling and flagged for
> neutralization under the P0 contract-extraction open question (agentctl RFC 0001 §9).

> **The 0004 / 0019 split is load-bearing (the same shape as 0004 / 0012).** agentctl
> RFC 0004 §5 owns the **schema of a reusable tagged tool bundle** — `MCPServerSet` as
> a namespaced set of named servers with per-tool trifecta `tags`, composed onto an
> `Agent` by union (ADD). This RFC **realizes that CRD** (it is unimplemented today —
> `Agent.spec.mcpServerSetRefs` is a name-only `LocalRef` and no `MCPServerSet` CRD
> exists, agent-api `crates/agent-api/src/lib.rs`) **and extends it** with the one
> concern 0004 §5 deliberately left to a later RFC: **per-server identity &
> authentication** (`transport`, `endpoint`, the `auth` union, `budget`). And it owns
> the **runtime plane** — the **MCP broker** data path that holds the credential,
> performs the OAuth, and exposes each server to the agent **keyless**. This is the
> exact 0004-owns-the-CRD / 0012-owns-the-runtime division applied to tools instead of
> intelligence. Where this RFC names an `MCPServerSet` field already in 0004 §5
> (`servers[].name`, `tags`), it is *citing* 0004, not redefining it.

> **Optional, never load-bearing for v1 usability.** An `Agent` with **no** MCP
> servers, or with only **local stdio** servers baked into the verified image and
> given credentials by the environment (the MCP spec's STDIO exemption, §4.6), runs on
> day one with none of this plane engaged. Remote/authenticated MCP servers add a
> **reuse + secret-free-identity seam**; they remove no capability and gate no MVP
> milestone (brainstorm §16 — not on the Phase-0 critical path).

---

## 1. Problem / Context

An agent's usefulness is mostly its **tools**, and in the ecosystem tools are MCP
servers. agentctl already reserved the developer-facing hook — `Agent.spec.mcp`
(`serverSetRefs` + inline `servers`, agentctl RFC 0003 §3.1) resolving against a
reusable `MCPServerSet` (agentctl RFC 0004 §5) — and the contract already fixes the
agent-side trust model — per-tool glob **trifecta tags**, a **granted-MCP-subset
trust budget** enforced per subagent spawn (agentd RFC 0012 §3.1/§3.2), and MCP
servers **baked into the verified image** as the supply-chain unit (brainstorm §10.2).
Three things are **missing**, and they are exactly the things that turn "an agent can
run a tool" into "a fleet of hostile-multi-tenant agents can safely use *remote,
credentialed* tools":

1. **The registration CRD does not exist.** `MCPServerSet` is introduced in RFC 0004
   §5 but has **no CRD** — `Agent.spec.mcpServerSetRefs` is a name-only `LocalRef`
   with nothing to resolve against (agent-api `crates/agent-api/src/lib.rs`). There is
   no first-class "register these servers for this agent / this fleet."

2. **There is no credential/identity model for a *remote* MCP server.** The contract
   today carries only `env_passthrough` — a list of **variable *names*** the agent
   forwards into a **stdio** child (`contract/schemas/config.schema.json`; "the config
   file NEVER carries a credential"). That is correct and sufficient for a local child
   given a token by its environment. It says **nothing** about a *remote* MCP server
   that speaks HTTP and demands OAuth — which is now the entire authenticated MCP
   ecosystem (§1.1). There is no per-server credential, no OAuth, no token, no
   audience binding, no broker.

3. **A token on a hostile-tenant pod is a credential one prompt-injection from
   exfiltration.** In the locked v1 posture (hostile multi-tenancy, brainstorm §0.6)
   the agent pod is the untrusted blast surface. An MCP access token minted for a
   corporate GitHub / Jira / internal-API server, sitting in that pod's env, is the
   lethal-trifecta exfiltration target. The same argument that forced the **provider
   credential off the pod** for intelligence (agentctl RFC 0012 §5) forces the **MCP
   token off the pod** for tools. Tools are simply a *second* governed egress.

This is the same problem RFC 0012 solved for the model channel, one plane over: an
egress the agent dials **out** to, whose credential must stay **off** the untrusted
pod, and which must be **governed** (authz, rate-limit, audit, budget). The answer is
the same shape — a **secret-free broker** the agent dials keyless — plus the one thing
intelligence did not need: a real **identity & authentication** model for the agent as
an OAuth actor, because MCP servers are OAuth **Resource Servers** and the agent (via
its broker) is an OAuth **client**.

### 1.1 What the MCP ecosystem converged on (the external ground truth)

The design is anchored in the researched landscape (persisted in full at
[`docs/design/mcp-auth-research.md`](../docs/design/mcp-auth-research.md), with every
external claim sourced). The load-bearing findings:

- **An MCP server is an OAuth 2.1 Resource Server delegating to a separate
  Authorization Server** (MCP spec **2025-06-18**). The HTTP flow: `401 +
  WWW-Authenticate` → **RFC 9728** Protected Resource Metadata → **RFC 8414** AS
  discovery → client identity (**RFC 7591** Dynamic Client Registration, increasingly
  **SEP-991** Client-ID Metadata Documents) → Authorization Code + **mandatory PKCE**
  → **RFC 8707 Resource Indicators** binding the token's **audience** to one server →
  `Authorization: Bearer` on every call. Two non-negotiables: **audience binding**
  (never accept a token not issued for you) and **no token passthrough** (a server
  calling upstream mints its *own* token). **STDIO transport is exempt** — local
  children get credentials from the environment; no network OAuth.
- **Headless / autonomous agents are a *separate* official extension — SEP-1046
  "OAuth client credentials"** (shipped 2025-11-25), for the no-human-in-the-loop
  case, with **asymmetric `private_key_jwt` (RFC 7523)** as the recommended
  credential (short-lived, no secret on the wire). **This is agentctl's default tier.**
- **Enterprise-Managed Authorization (EMA / SEP-990)** — stable 2026-06-18, the
  enterprise extension making the IdP the single authority (admin-approved servers,
  zero-touch SSO, central revocation) via **Cross-App Access / the ID-JAG draft**
  (an IdP-minted Identity-Assertion JWT, **RFC 8693** token-exchanged from a **human's
  ID token**, redeemed via **RFC 7523**). **EMA has no mechanism for service accounts
  or autonomous agents** — its `subject_token` must be a human assertion — so it is
  the **on-behalf-of-a-user** tier, not the autonomous one. And EMA stops at **token
  issuance**; runtime per-call authorization is the implementer's job.
- **The ecosystem productized exactly agentctl's broker.** The dominant secure pattern
  is a **secretless credential-broker gateway** (Aembit's "MCP Identity Gateway",
  Cloudflare's `workers-oauth-provider`, Docker MCP Gateway): the gateway holds the
  real credentials and performs the OAuth; the workload connects credential-free,
  authenticated by a **workload identity** (SPIFFE SVID, a bound Kubernetes
  ServiceAccount token, or an attested peer). agentctl already **has** this component
  for intelligence (the ModelGateway, RFC 0012) and the attested-peer identity
  (SO_PEERCRED / vsock, RFC 0002/0008). This RFC applies both to MCP.

The mapping is therefore not an invention; it is the ecosystem's converged pattern
expressed in agentctl's existing primitives.

---

## 2. Decision — the MCP registration, identity & broker plane (nine principles)

1. **`MCPServerSet` is realized as a CRD and extended with identity/auth (§3).** The
   RFC 0004 §5 reusable tagged-bundle schema becomes a real, namespaced CRD in
   `agent-api`, mirroring the as-built `ModelPool` shape (agent-api
   `crates/agent-api/src/lib.rs`: `endpoint`, `credentialSecretRef`, `budget`), and
   gains the two concerns 0004 §5 left open: a per-server **`transport`** +
   **`endpoint`/`command`** and a per-server **`auth`** union
   (`oauthClientCredentials` | `ema` | `staticToken` | `envPassthrough` | `none`). The
   `tags` field and union-ADD composition are **unchanged from 0004 §5**.

2. **Behind every remote/authenticated MCP server sits an MCP broker (§4), mirroring
   the RFC 0012 intel proxy.** The operator resolves an `Agent`'s bound
   `MCPServerSet`s into (a) the broker's per-server upstream config (endpoint +
   credential + auth method) and (b) the agent's MCP config pointing at the **broker**
   over the substrate, **keyless**. The broker holds the credential, performs the
   OAuth, and is the single governed MCP egress chokepoint.

3. **The broker is its own component, categorically OUT of the node-agent (§4.1) —
   restated from RFC 0012 §3.1 / RFC 0008 §6.** A per-node MCP broker embedded in the
   node-agent would concentrate the second dangerous egress in the god-mode host
   process and make a tool call a per-node SPOF. `broker.topology` is `sidecar`
   (per-pod) or `node-local` (a separate Deployment) — **never** the node-agent. (It
   MAY be **co-deployed with the ModelGateway** as one "agent egress broker" binary —
   §Open 1 — but never in the node-agent.)

4. **Zero-token-in-pod is the default and the only hostile-tenant-legal posture (§4.3).**
   The MCP OAuth token — and the OAuth *client* credential that mints it — live in the
   **broker**, never the agent pod. The agent dials the broker over a substrate-local
   socket, keyless. The **only** exempt case is a **local stdio** server whose
   credential is an environment variable to a child baked into the verified image (the
   MCP spec STDIO exemption); that is `auth: envPassthrough`, single-tenant-leaning,
   and never carries a *remote* token (§4.6).

5. **The broker performs the full MCP OAuth flow so the agent never does (§4.4).** On
   an upstream `401`, the broker walks RFC 9728 PRM → RFC 8414 AS discovery → client
   identity (DCR / CIMD) → token acquisition → **RFC 8707 Resource Indicators** →
   validates **audience** (RFC 9068) → attaches `Authorization: Bearer` upstream. It
   **never passes the agent's inbound identity through as the upstream token** (the
   spec's no-passthrough rule): the upstream token is one the broker mints/holds for
   *that* server's audience.

6. **Two auth tiers, chosen per server (§6, §7).** **Tier 1 — headless (default):**
   OAuth **client-credentials (SEP-1046)** with **`private_key_jwt` (RFC 7523)**, for
   the fully-autonomous agent; the broker is the OAuth client and holds the signing
   key. **Tier 2 — on-behalf-of-a-user (enterprise):** **EMA / ID-JAG (SEP-990)** —
   the broker holds a human's IdP assertion and token-exchanges a per-server,
   audience-restricted token, inheriting the enterprise's central consent / policy /
   revocation / audit. A **pure-autonomous** agent (no human principal) **cannot** use
   Tier 2 (the EMA spec limitation) → Tier 1 is its path.

7. **Workload identity authenticates the agent→broker hop (§5), reusing the RFC 0012
   §5.4 chokepoint.** The dialing agent is identified by the **attested peer**
   (SO_PEERCRED on stock-unix, attested vsock/uds on kata-hybrid — agentctl RFC 0002
   §6 / RFC 0008), and authorized against an operator-rendered **`peer → Agent →
   allowed servers`** map — the *same* map shape the ModelGateway uses for pools. A
   **projected ServiceAccount token** (validated via **TokenReview**) or a **SPIFFE
   SVID** are optional, stronger primitives (§5.2, §Open 3) for when the broker is
   off-node or federates into an external IdP.

8. **Runtime authorization stays agentctl's — OAuth/EMA stop at issuance (§8).** The
   broker (+ admission, agentctl RFC 0007) is the **PDP/PEP**: it enforces per-server
   / per-tool policy, rate limits, the SSRF/egress allow-list, and **surfaces** the
   trifecta tag union to admission — while the **contract enforces the granted-subset
   trust budget per spawn** (agentd RFC 0012 §3.2). agentctl never re-implements the
   per-spawn check; it governs the *egress and the credential*.

9. **Transport reality: agentd is stdio-only, so a stdio↔HTTP bridge fronts the
   broker today (§9).** The reference agent's MCP client spawns a **stdio** child;
   there is no HTTP-MCP client. So the operator renders a tiny **stdio↔broker bridge**
   (precedent: the `work-mcp-bridge`, `crates/agentctl-coordination/src/bin/`) as the
   spawned `command` — the agent speaks stdio to the bridge, the bridge dials the
   broker keyless over the substrate. A **direct streamable-HTTP MCP client** in the
   agent is contract ask **P-mcp-egress**; until it lands, the bridge makes the whole
   plane work with **no contract change**.

### 2.1 What this RFC owns vs reuses (the boundary)

| Concern | Home | Note |
|---|---|---|
| `MCPServerSet` **bundle schema** (`servers[].name`, per-tool `tags`, union-ADD composition, `status.tagUnion`) | **agentctl RFC 0004 §5** | this RFC realizes it as a CRD + cites it; never redefines `tags`/composition |
| `MCPServerSet` **identity/auth extension** (`transport`, `endpoint`/`command`, the `auth` union, `budget`) | **agentctl RFC 0019 (here)** | the concern 0004 §5 left to a later RFC |
| The MCP **broker data path** (deploy, per-server OAuth, credential hold, keyless expose, metering, authz) | **agentctl RFC 0019 (here)** | the *execution* of the CRD declaration — mirrors RFC 0012's proxy |
| Per-tool **trifecta tags** vocabulary + the **granted-subset trust budget** enforced per spawn | **the contract** (agentd RFC 0012 §3.1/§3.2) | agentctl surfaces tags; the agent enforces the budget |
| **Substrate reach** (how the agent's dial crosses to the broker; the keyless socket) + **attested peer identity** | **agentctl RFC 0002 / RFC 0008** | this RFC consumes the descriptor + SO_PEERCRED/vsock attestation |
| The **`peer → Agent → allowed`** authz-map shape + per-pod/tenant enforcement | **agentctl RFC 0012 §5.4** (reused) | this RFC extends the map's target set from *pools* to *servers* |
| **Tenancy / PKI** trust model, the guest→host egress restriction, the internal mTLS CA | **agentctl RFC 0015** | this RFC consumes; 0015 owns the threat model + the CA the broker's client cert chains to |
| **Admission** (name-collision across the union, advisory trifecta union, cross-ns `Secret` ref rejection, hostile-tenant rejection of in-pod-token auth) | **agentctl RFC 0007** | this RFC specifies the rules; 0007 executes them |
| **Renderer** compiling a set into broker config + the agent's `--mcp` config + the bridge command | **agentctl RFC 0006** | this RFC specifies *what* it renders |
| Token **metering / cost rollups** substrate; the run-report | **agentctl RFC 0010 / RFC 0012 §7** | MCP calls are metered on `agentctl_mcp_*`; budget enforcement mirrors RFC 0012 §7 |
| The **instruction-source `mcpResource`** consumer of this CRD | **agentctl RFC 0020** | one MCP-identity model, two consumers (§11) |

---

## 3. The `MCPServerSet` CRD — registration with identity & auth

`MCPServerSet` is **namespaced** (like `ModelPool`/`IntelligenceService`) and is the
RFC 0004 §5 reusable tagged bundle **plus** the per-server identity/auth this RFC
adds. A platform/dev operator defines a set once; many `Agent`s reference it via the
existing `mcp.serverSetRefs`, composing by **union (ADD)** with inline `servers`
exactly as 0004 §5.2 specifies.

### 3.1 Full schema

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: MCPServerSet                          # namespaced (RFC 0004 §5)
metadata:
  name: engineering-tools
  namespace: agents
spec:
  servers:
    # ── (a) a REMOTE server, fully autonomous → Tier 1 (SEP-1046 client-credentials) ──
    - name: github                          # server name; unique across the resolved Agent union (RFC 0004 §5.2)
      transport: streamableHttp             # stdio | unix | streamableHttp  (§3.3)
      endpoint: https://mcp.github.example.com/mcp   # the MCP server URL (Resource Server)
      tags:                                 # per-tool glob trifecta tags — UNCHANGED from RFC 0004 §5
        "*":       [untrusted_input]
        "create_*": [egress]
      auth:                                 # the identity/auth extension THIS RFC adds (§3.2)
        mode: oauthClientCredentials        # → Tier 1 headless (SEP-1046) (§6)
        oauthClientCredentials:
          # resource server / audience — RFC 8707 Resource Indicator the broker requests
          resource: https://mcp.github.example.com/mcp
          scopes: [repo.read, issues.write]
          # the OAuth CLIENT credential — held BY THE BROKER, never the pod (§4.3)
          credential:
            kind: privateKeyJwt             # privateKeyJwt (RFC 7523, recommended) | clientSecret (compat)
            clientId: agentctl-eng-bot
            # the signing key (privateKeyJwt) or the secret (clientSecret) — a namespace-local Secret
            keyRef: { name: github-mcp-client-key, key: private-key.pem }
          # discovery: auto (RFC 9728 PRM → RFC 8414) unless pinned
          authorizationServer: ""           # optional pin; empty ⇒ discover from the 401/PRM
      budget: { maxTokens: 2000000 }        # optional per-server call/token budget (mirrors ModelPool.budget) (§8.3)

    # ── (b) a REMOTE corporate server, on behalf of a human → Tier 2 (EMA / ID-JAG) ──
    - name: corp-jira
      transport: streamableHttp
      endpoint: https://mcp.corp.example.com/jira
      tags: { "*": [sensitive] }
      auth:
        mode: ema                           # → Tier 2 on-behalf-of-user (SEP-990) (§7)
        ema:
          enterpriseIdp: https://idp.corp.example.com   # the IdP that mints the ID-JAG (RFC 8693 → RFC 7523)
          resource: https://mcp.corp.example.com/jira   # audience the exchanged token is restricted to
          scopes: [read, comment]
          # the human principal is carried at REQUEST time (the A2A caller identity, RFC 0015/§7.2),
          # NOT stored here — EMA's subject_token MUST be a human assertion (§7.1)

    # ── (c) a LOCAL stdio server baked into the image → the STDIO exemption (env creds) ──
    - name: fs
      transport: stdio
      command: ["mcp-fs", "--root", "/watch"]   # operator config; resolves INSIDE the verified image (RFC 0004 §5.2)
      tags: { "*": [untrusted_input] }
      auth:
        mode: envPassthrough                # the ONLY in-pod-credential mode; stdio-exempt (§4.6)
        envPassthrough: [GITHUB_TOKEN]      # variable NAMES only (contract: config never carries the value)

  # where the broker runs (mirrors IntelligenceService.proxy — RFC 0004 §4.1 / RFC 0012 §3.2)
  broker:
    topology: node-local                  # sidecar | node-local; absent ⇒ derived from the effective
                                          #   tenancy/class tier (hostile ⇒ node-local, §4.1). NEVER the node-agent
    image: registry.example.com/agentctl/mcp-broker@sha256:…   # this RFC's broker component (agentctl-operated)

status:
  tagUnion: [untrusted_input, egress, sensitive]   # informational (RFC 0004 §5.3) — the Agent-level union is the gate
  servers:                                         # per-server: {name, authReady, authMode, authReason?, lastTokenRefresh?}
    - { name: github,   authReady: true,  authMode: oauthClientCredentials, lastTokenRefresh: "2026-07-01T10:00:00Z" }
    - { name: corp-jira, authReady: true, authMode: ema }
    - { name: fs,       authReady: true,  authMode: envPassthrough }
    # a failed binding carries authReason, e.g. { name: legacy, authReady: false, authMode: ema, authReason: EmaUnsupported }
  conditions:
    - { type: Resolved, status: "True", reason: ServersValid }
```

### 3.2 The `auth` union — one server, one method

`auth.mode` is a closed enum; exactly one sibling object is populated (CEL, §3.4).
The modes and their tier:

| `auth.mode` | Tier | Credential the **broker** holds | Token on the **agent pod**? | Hostile-tenant legal? | Spec basis |
|---|---|---|---|---|---|
| `oauthClientCredentials` | **1 (headless)** | the OAuth **client** key (`privateKeyJwt` — RFC 7523; or `clientSecret`) | **no** (broker mints the RS token) | **yes** | SEP-1046 |
| `ema` | **2 (on-behalf-of-user)** | (holds no static secret) exchanges the human's IdP assertion → ID-JAG → RS token | **no** | **yes** | SEP-990 / ID-JAG (RFC 8693 + 7523) |
| `staticToken` | — (compat) | a long-lived bearer `Secret` (legacy servers with no OAuth) | **no** (broker attaches it upstream) | **yes** (token still off-pod) | pre-OAuth MCP |
| `envPassthrough` | — (local) | none — the child gets an env var (STDIO exemption) | **the value is in the pod** | **no** for a *remote* token; stdio-local only (§4.6) | MCP spec STDIO exemption |
| `none` | — | none (unauthenticated server) | n/a | yes | — |

`staticToken` and `envPassthrough` are the **compat / local** escape hatches; the two
first-class, cloud-native modes are `oauthClientCredentials` (autonomous) and `ema`
(human-delegated). Admission (agentctl RFC 0007) rejects `envPassthrough` with a
*remote* `transport` (`streamableHttp`) on **any** class and on a **hostile** class
outright (a remote credential must not be in the pod — §4.6).

### 3.3 Field reference

| Field | Type | Owner | Contract/impl anchor | Notes |
|---|---|---|---|---|
| `servers[].name` | string | dev/ops | RFC 0004 §5 | unique across the resolved `Agent` union; duplicate ⇒ webhook reject (RFC 0004 §5.2 / RFC 0007) |
| `servers[].transport` | enum `stdio\|unix\|streamableHttp` | ops | agentd RFC 0012 §3.4 / MCP spec | `stdio` (default, confinement win) & `unix` are local children; `streamableHttp` is a remote Resource Server (broker-fronted, §4) |
| `servers[].endpoint` | URL | ops | MCP spec 2025-06-18 | required for `streamableHttp`; the Resource Server URL. Forbidden for `stdio`/`unix` (CEL) |
| `servers[].command`/`argv` | []string | ops | agentd RFC 0012 §3.4 | required for `stdio`/`unix`; resolves **inside** the verified image (RFC 0004 §5.2). Forbidden for `streamableHttp` (CEL) |
| `servers[].tags` | per-tool glob map | ops | agentd RFC 0012 §3.1 | **unchanged from RFC 0004 §5**; first-match/longest-glob-wins; bare list = `{"*":[legs]}` shorthand |
| `servers[].auth.mode` | enum (§3.2) | ops | this RFC / SEP-1046 / SEP-990 | closed enum; one sibling populated |
| `…oauthClientCredentials.resource` | URL | ops | RFC 8707 | the Resource Indicator/audience the broker requests + validates (§4.4) |
| `…oauthClientCredentials.scopes` | []string | ops | OAuth 2.1 | requested scopes |
| `…oauthClientCredentials.credential.kind` | enum `privateKeyJwt\|clientSecret` | ops | RFC 7523 | `privateKeyJwt` recommended (no secret on the wire); `clientSecret` for compat |
| `…oauthClientCredentials.credential.keyRef` | `SecretKeyRef{name,key}` | ops | agent-api | the broker-mounted signing key / client secret — **namespace-local, never in the pod** |
| `…oauthClientCredentials.authorizationServer` | URL | ops | RFC 8414 | optional pin; empty ⇒ discover via the 401/PRM |
| `…ema.enterpriseIdp` | URL | ops | SEP-990 | the IdP minting the ID-JAG (§7) |
| `…ema.resource`/`scopes` | URL/[]string | ops | RFC 8707 | audience + scopes for the exchanged token |
| `…staticToken.tokenRef` | `SecretKeyRef` | ops | — | a long-lived bearer the broker attaches upstream (compat) |
| `…envPassthrough` | []string | ops | `contract/schemas/config.schema.json` | variable **names** only; the value never appears in the CRD/manifest (STDIO-exempt) |
| `servers[].budget` | `{maxTokens}` | ops | agent-api `Budget` | optional per-server budget (§8.3); mirrors `ModelPool.budget` |
| `broker.topology` | enum `sidecar\|node-local` | ops | RFC 0012 §3.2 (mirrors `proxy.topology`) | where the broker runs; **absent ⇒ derived from the effective tenancy/class tier** (hostile ⇒ `node-local`, §4.1). **Never** the node-agent |
| `broker.image` | image | ops | this RFC | the broker component image (agentctl-operated) |

### 3.4 Validation (CEL on the CRD + the mandatory webhook)

- **CEL (single-object).** `transport == 'streamableHttp'` ⇒ `has(endpoint) &&
  !has(command)`; `transport in ['stdio','unix']` ⇒ `has(command) && !has(endpoint)`;
  exactly one `auth.<mode>` sibling is populated for the chosen `mode`;
  `auth.mode == 'envPassthrough'` ⇒ `transport in ['stdio','unix']` (a remote server
  may not use an in-pod env credential — the local-only rule, §4.6).
- **Webhook (cross-object, agentctl RFC 0007 — the four checks CEL cannot do).**
  (1) **name-collision** across the resolved `serverSetRefs ⊎ inline` union (RFC 0004
  §5.2); (2) **cross-namespace `Secret` ref** rejection — a `keyRef`/`tokenRef` must
  be in the set's namespace unless an explicit grant exists (mirrors RFC 0012 §5.3);
  (3) **in-pod-credential / hostile-tenant rejection** — reject `envPassthrough` (the
  **only** mode that lands a credential *in the pod*) with a remote `transport`, and
  reject any in-pod credential on a **hostile** class outright (`staticToken` is
  broker-held and off-pod per §3.2, so it is **not** rejected here); (4)
  **`ema`-without-human** — reject an `ema`-bound server when the `Agent` has no
  human-identity source (`access.oidc`, §7.2) to supply the EMA subject assertion
  (§7.1). The advisory **trifecta union** (RFC 0004 §5.3) is computed here too —
  warning, never blocking.

---

## 4. The MCP broker — the credential-holding data path

The broker is to MCP what the ModelGateway/intel-proxy (agentctl RFC 0012) is to
intelligence: the component that holds the credential **off the pod**, performs the
provider-side protocol, and exposes a **keyless** endpoint to the agent over the
substrate. Everything in RFC 0012 §3–§5 about placement, zero-secret rendering, secret
lifecycle, and per-pod/tenant authz applies here, adapted from *pools* to *servers*.

### 4.1 Placement — out of the node-agent; two legal topologies

Restated binding from RFC 0012 §3.1 / RFC 0008 §6: the broker is **never** in the
node-agent (it would concentrate the second dangerous egress in the god-mode host
process and make a tool call a per-node SPOF). `broker.topology`:

```
 (A) sidecar — per-pod, in-pod, unix:/run/mcp/<server>.sock
   ┌──────────────────── Agent pod ────────────────────┐
   │ agent ──stdio bridge──▶ mcp-broker (holds cred,    │──OAuth+TLS──▶ remote MCP server
   │ (no token)              does OAuth)                 │              (Resource Server)
   └────────────────────────────────────────────────────┘
   blast radius = ONE pod · pod is network-attached (shared netns, RFC 0002 §10) ⇒ single-tenant-leaning

 (B) node-local — separate Deployment (≥2 replicas + PDB), agent dials over the substrate
   ┌──── Agent pod (NETWORKLESS) ────┐        ┌──── mcp-broker Deployment ────────────────┐
   │ agent ──bridge──vsock|uds──────▶│───────▶│ authz (peer→Agent→servers) · hold cred ·  │──OAuth+TLS──▶ MCP server
   └──────────────────────────────────┘        │ per-server OAuth · meter · SSRF allow-list│
                                                └────────────────────────────────────────────┘
   blast radius = bounded by replicas · pod stays OFF the cluster network (max isolation)
```

| Topology | Credential lives in | Agent pod on the network? | Blast radius | Hostile-tenant legal? |
|---|---|---|---|---|
| `sidecar` | the in-pod broker | **yes** (shared netns) | one pod | leaning single-tenant (cred off the agent *container*, but pod is network-attached) |
| `node-local` | the broker Deployment | **no** (pod stays networkless) | bounded by replicas | **yes** — the maximal-isolation path |

The kata-hybrid hardened tier (agentctl RFC 0002) pairs with `node-local`; `sidecar`
is the dev / single-tenant convenience. The broker MAY be the **same binary/process
as the ModelGateway** ("one agent egress broker"), co-deployed — that is a deployment
choice (§Open 1), but it is **never** folded into the node-agent.

### 4.2 The broker is never a *hard* SPOF

Same three layers as RFC 0012 §3.3, adapted: (1) a tool egress failing is **not** the
blocking reasoning path the model call is — an MCP `tools/call` failure returns a tool
error the agent's loop handles, so a broker outage degrades a *tool*, not the whole
run; (2) `node-local` runs ≥2 replicas behind a Service + PDB (rendered by agentctl
RFC 0006); (3) a total broker outage surfaces as **MCP `tools/call` errors** the agent
reports through its normal tool-error path — never a crash-loop, never a wedged
supervisor (liveness is not failed for a dead tool upstream).

### 4.3 Zero-token-in-pod

The load-bearing security property, identical in spirit to RFC 0012 §5: **the MCP
access token — and the OAuth client credential that mints it — are resolved at the
broker and never enter the agent pod.** It rides the same contract facts (secrets are
env/file-only behind `resolve()`; the credential carrier is structurally
unserializable; on the keyless tier the agent dials a substrate-local socket with no
token — agentd RFC 0006 §6 / RFC 0012 §3.7 / RFC 0002 §10). By topology:

| `auth.mode` × topology | Token/credential mounted into | Agent pod sees | Agent's MCP config points at | Hostile-tenant? |
|---|---|---|---|---|
| `oauthClientCredentials` / `ema` / `staticToken`, `node-local` | the broker Deployment | **nothing** (keyless) | the broker socket over vsock/uds | **yes** |
| same, `sidecar` | the in-pod broker | **nothing in the agent container** | `unix:/run/mcp/<server>.sock` | leaning single-tenant |
| `envPassthrough`, `stdio` | **the agent pod** (a child env var) | the env value (local only) | the spawned child (stdio) | **no** for a remote token; local-only (§4.6) |

The `keyRef`/`tokenRef`/exchanged-token value appears **nowhere** in the CRD, the
manifest, `Agent.status`, broker logs, or traces (the never-logged discipline of
agentd RFC 0006 §6).

### 4.4 The OAuth flow the broker performs (so the agent never does)

On the first upstream call (or a `401`), the broker executes the MCP 2025-06-18 flow
**on the agent's behalf**, caching per `(server, audience)`:

```
 broker                                   MCP server (Resource Server)      Authorization Server
   │ ── tools/call ─────────────────────────▶ │ 401 + WWW-Authenticate            │
   │ ◀───────────────────────────────────────  │ (RFC 9728 resource_metadata URL)  │
   │ ── GET /.well-known/oauth-protected-resource ▶ (RFC 9728 PRM: authorization_servers[])
   │ ── GET AS metadata (RFC 8414 / OIDC disc.) ───────────────────────────────────▶ │
   │ ── client identity: DCR (RFC 7591) or CIMD (SEP-991) ──────────────────────────▶ │
   │ ── token request  (Tier 1: client_credentials + private_key_jwt RFC 7523;        │
   │                    Tier 2: token-exchange ID-JAG RFC 8693 → RFC 7523)            │
   │                    + resource=<audience>  (RFC 8707 Resource Indicator) ────────▶ │
   │ ◀── access_token (aud-bound) ────────────────────────────────────────────────── │
   │ ── tools/call  Authorization: Bearer <token> ▶ │ 200 (validates aud, RFC 9068)   │
```

Binding rules the broker enforces:

- **Audience binding (RFC 8707 / RFC 9068).** Every token is requested with a
  `resource` indicator equal to the server's `auth.<mode>.resource` and is used **only**
  against that server. A token whose `aud` does not match is never forwarded.
- **No token passthrough.** The agent's inbound identity (the attested peer, §5, or
  the human principal, §7) is **never** replayed as the upstream token. The upstream
  token is one the broker mints/holds for that server's audience — a distinct
  credential, per the MCP Security Best Practices.
- **PKCE / discovery.** Interactive Authorization-Code is **not** used for the
  autonomous Tier 1 (no human/redirect); the broker uses client-credentials (§6). PKCE
  applies only where an interactive leg exists (it does not, in the headless default).
  Discovery (PRM → AS metadata) is cached with a TTL; a pinned `authorizationServer`
  skips it.
- **Refresh & rotation.** Tokens are short-lived; the broker refreshes ahead of expiry
  and re-reads a rotated `keyRef`/`Secret` from its file mount **without** touching the
  agent (the RFC 0012 §5.3 file-rotation discipline).

### 4.5 Metering & health

The broker re-exports MCP-call telemetry on agentctl's own namespace —
`agentctl_mcp_calls_total{server,tool,result}`, `agentctl_mcp_call_latency_ms`,
`agentctl_mcp_auth_refresh_total{server,mode,result}`,
`agentctl_mcp_authz_denied_total{reason}` (agentctl RFC 0010 §10.1) — so the
per-server tool-call and auth view is visible even when the agent pod is networkless
and the agent's own `agent_mcp_*` counters (if any) see only the pod↔broker hop. This
mirrors the two-tier metrics correction of RFC 0012 §4.5.

### 4.6 The STDIO exemption — the only in-pod-credential path

The MCP spec exempts **STDIO** transport from the network OAuth flow: a local child
process is handed its credential by its **environment**, because there is no network
leg to protect and the child already shares the agent's trust boundary. agentctl
honors this exactly, and bounds it so it can never become a *remote*-token leak:

- **`auth.mode: envPassthrough` is legal only with `transport: stdio`/`unix` (CEL,
  §3.4)** — a local child, spawned by the agent, **baked into the verified image** (RFC
  0004 §5.2), handed a credential by an environment variable **name** (`config.schema.
  json` — the value never appears in the CRD/manifest, only the name).
- **It is the one path with a credential in the pod, and it carries no remote token.**
  The child is local, so the exposed surface is the child's own env — not a
  broker-minted Resource-Server token for a *remote* server. That is precisely the
  spec's rationale, and why an in-pod env credential is acceptable here where a remote
  token never is.
- **It is single-tenant-leaning.** Admission rejects `envPassthrough` with a *remote*
  `transport`, and rejects any in-pod credential on a **hostile** class outright (§3.4).
  A remote server always uses the broker (`oauthClientCredentials` / `ema` /
  `staticToken` — all off-pod), never `envPassthrough`.

The plane's invariant therefore holds cleanly: **remote MCP ⇒ broker, keyless pod;
local stdio MCP ⇒ the spec's env-credential exemption — no remote token, no
hostile-tenant use.**

---

## 5. Workload identity — authenticating the agent→broker hop

The broker serves many pods (and, `node-local`, many tenants). It MUST authenticate
the **dialing agent** and authorize it against an operator-rendered map — the *same*
`peer → Agent → allowed` chokepoint the ModelGateway uses (agentctl RFC 0012 §5.4),
with the target set changed from *pools* to *servers*:

```
   agent identity (workload) ──▶ Agent (namespace/name) ──▶ allowed MCPServerSet servers
```

### 5.1 Primary: the attested peer (reuse, no new primitive)

- **stock-unix:** `SO_PEERCRED` on the substrate socket + the operator-assigned socket
  mapping identifies the pod (agentctl RFC 0002 §6 / RFC 0008). This is the same
  attestation the routed-infer path and the coordination attested-claim-ownership use
  today (verified in `crates/agentctl-*/src/attest.rs`).
- **kata-hybrid:** an attested vsock/uds peer per microVM (agentctl RFC 0002 §6). The
  broker keys authz off that attested identity — never a self-asserted header.

A dial to a server the peer's `Agent` is not bound to is **refused and counted**
(`agentctl_mcp_authz_denied_total`). A `sidecar` broker is single-pod and needs only
the trivial "this pod, its servers" map. This is the v1 default because it needs **no
new component** and no external IdP.

### 5.2 Optional stronger primitives (for off-node / federated brokers)

When the broker is off-node, shared across many tenants, or must **federate the
agent's identity into an external IdP** (e.g. to obtain an ID-JAG under Tier 2), a
richer, portable workload identity is warranted:

- **Projected ServiceAccount token.** The operator projects a short-lived,
  **audience-bound** SA token (`aud: agentctl-mcp-broker`) into the agent pod; the
  broker validates it via the Kubernetes **TokenReview** API (KEP-1205 bound tokens).
  This is a real, cluster-issued, revocable identity that a downstream IdP can
  **federate** (workload-identity-federation) into the enterprise trust domain — the
  clean bridge to Tier 2 without a human in the loop for the *agent→broker* hop.
- **SPIFFE/SPIRE SVID.** Where a mesh SPIFFE identity exists, the agent presents a
  JWT-SVID (or mTLS X.509-SVID); the broker verifies against the trust bundle. SPIFFE
  ↔ OAuth composes via RFC 8693 token-exchange (the SVID as `subject_token`) — the
  most portable option for multi-cluster / cross-domain fleets.

Both are **opt-in** and gated behind the same trust roots the security plane owns
(agentctl RFC 0015). v1 ships the attested peer; the SA-token/SPIFFE choice is
§Open 3.

### 5.3 The map is rendered, not self-asserted

The `peer → Agent → allowed servers` map is compiled by the operator (agentctl RFC
0006) from the `Agent`↔`MCPServerSet` bindings, exactly as the ModelGateway's pool map
is. The broker never trusts a server name the agent *claims* — it looks up what the
**attested identity** is bound to. This closes the cross-tenant credential/budget-theft
hole (one tenant's pod dialing another tenant's server and spending its credential) by
construction, the same way RFC 0012 §5.3/§5.4 does for pools.

---

## 6. Tier 1 — headless / autonomous (SEP-1046 client-credentials + RFC 7523)

**This is the default tier and the one a fully-autonomous fleet uses.** There is no
human, no browser, no redirect — so interactive Authorization-Code + PKCE is
inapplicable, and the correct grant is the official headless extension, **SEP-1046
OAuth client credentials**, with **`private_key_jwt` (RFC 7523)** as the client
credential.

- **The broker is the OAuth client.** It holds the client **signing key**
  (`credential.keyRef`, `kind: privateKeyJwt`) — a namespace-local `Secret` mounted
  into the broker, **never** the pod. On each token request it signs a short-lived
  client-assertion JWT (RFC 7523) and requests an access token for the server's
  `resource` (RFC 8707). No client secret ever traverses the wire; a compromised
  *pod* yields no OAuth client credential (it never had one).
- **`clientSecret` is the compat fallback.** For an AS that does not support
  `private_key_jwt`, `credential.kind: clientSecret` uses a symmetric secret (still
  broker-held, off-pod). Flagged in `status` as the weaker posture.
- **Key lifecycle.** The signing key is one of the security plane's managed secret
  classes (agentctl RFC 0015 §five-class lifecycle); rotation is a `Secret`/file
  replacement the broker re-reads live (RFC 0012 §5.3). The public JWK is registered
  with the AS out-of-band (or via DCR/CIMD, §4.4).
- **Audience-bound, per server.** One token per `(server, audience)`; never reused
  across servers (RFC 8707). Scopes come from `auth.oauthClientCredentials.scopes`.

Tier 1 requires **no** human principal and **no** enterprise IdP — it works for a
standalone fleet with nothing but the target server's AS. It is the honest floor: an
autonomous agent authenticating **as itself** (its broker's registered client
identity), least-privilege-scoped and audience-bound.

---

## 7. Tier 2 — on-behalf-of-a-user / enterprise (EMA / ID-JAG, SEP-990)

When an agent acts **for a specific human** inside an enterprise — and the enterprise
wants **one** authority over which MCP servers exist, who may use them, zero-touch SSO,
and instant central revocation — the right model is **Enterprise-Managed Authorization
(SEP-990, stable 2026-06-18)** over **Cross-App Access / ID-JAG**.

### 7.1 The flow, and the hard human-principal boundary

The enterprise IdP is the single authority. The broker, holding the **human's IdP
assertion** (their ID token / SAML assertion — carried at *request* time as the A2A
caller identity, §7.2, **not** stored in the CRD), performs an **RFC 8693 token
exchange** at the IdP to obtain an **Identity-Assertion JWT (ID-JAG)** for the target
server, then redeems the ID-JAG at the server's AS (via **RFC 7523**) for an
**audience-restricted access token** — the user's consent endpoint is never visited
(admin pre-approval covers it).

```
 human ID token (from the A2A caller identity, §7.2)
        │  RFC 8693 token-exchange at enterpriseIdp
        ▼
   ID-JAG (typ=oauth-id-jag+jwt; iss/sub/aud/resource/scope)   ── advertised via
        │  RFC 7523 assertion at the server's AS                  authorization_grant_
        ▼                                                         profiles_supported: [id-jag]
   access_token (aud = auth.ema.resource)  ──▶  MCP server (validates aud, RFC 9068)
```

**The boundary that decides tier (binding):** EMA's `subject_token` **must** be a
human's assertion — *there is no EMA mechanism for a service account or autonomous
agent to acquire a token independently* (the SEP-990 limitation, persisted research
§3). Therefore **Tier 2 is available only when the request carries a human principal**;
a **pure-autonomous** agent (no human on whose behalf it acts) **cannot** use `ema` and
is served by Tier 1 (§6). Admission (agentctl RFC 0007) rejects an `Agent` binding an
`ema` server when the agent has no configured human-identity source, with a clear
message pointing at Tier 1.

### 7.2 Where the human principal comes from

The human on whose behalf the agent acts is the **authenticated A2A caller** — the
identity established at the A2A gateway by the per-agent OIDC/JWT access policy already
in `agent-api` (`Access.oidc` / `OidcAccess`, `crates/agent-api/src/lib.rs` — the
inbound-call OIDC this codebase ships). That verified caller identity (the human's ID
token / claims) is propagated as the request's principal to the broker, which uses it
as the EMA `subject_token`. So Tier 2 composes cleanly with the existing inbound OIDC:
**inbound OIDC authenticates the human to the agent; EMA carries that same human to the
tool.** A reactive/scheduled agent with **no** inbound human call has no principal to
carry → Tier 1.

### 7.3 What the enterprise inherits (and what it does not)

- **Inherited from EMA:** admin-approved server catalog, zero per-server user consent,
  centralized policy over who-may-use-what, **instant central revocation**, and audit
  at the IdP — the enterprise-grade properties SEP-990 exists to give.
- **Still agentctl's (the runtime authz gap):** EMA stops at **token issuance**. The
  broker (§8) remains the PDP/PEP for **per-tool / per-call** policy, rate limiting,
  the SSRF/egress allow-list, budget, and the trifecta surface. EMA decides *whether
  this human may reach this server at all*; agentctl decides *which tools, how often,
  under what egress policy, within what budget* — the granular, per-call layer the
  enterprise standard explicitly leaves to the implementer.

### 7.4 Wiring

The operator sets `auth.mode: ema` + `auth.ema.enterpriseIdp` on the server; the
broker advertises the EMA capability on `initialize`
(`io.modelcontextprotocol/enterprise-managed-authorization`) and negotiates the
`id-jag` grant profile (`authorization_grant_profiles_supported`). No per-agent secret
is stored — the flow is assertion-based end to end. If the target server or IdP does
not advertise EMA/ID-JAG, the binding is marked `authReady: false` with reason
`EmaUnsupported`, and the operator must fall back to Tier 1 or `staticToken`.

---

## 8. Runtime authorization — the broker is the PDP/PEP (OAuth/EMA stop at issuance)

Both tiers deliver a **valid, audience-bound token**. Neither decides **per-call**
policy — that is the gap the persisted research flags twice (EMA "stops at issuance";
OAuth scopes are coarse). agentctl fills it at the broker, the same chokepoint that
holds the credential:

### 8.1 The per-call policy point

For every `tools/call` the broker enforces, **before** attaching the upstream token:

- **peer→server binding (§5)** — is this attested `Agent` bound to this server? Else
  `authz_denied`.
- **per-tool policy** — an optional operator allow/deny per tool-name-glob (the same
  glob grammar as `tags`), so a bound server can still forbid `delete_*` for a class of
  agents. Default allow-all within a bound server; deny is opt-in.
- **rate limit** — per `(Agent, server)` and per `(tenant, server)` token buckets, to
  bound a runaway loop's tool spend (mirrors the model-egress governance intent of RFC
  0012 §7).
- **SSRF / egress allow-list** — the broker is the single MCP egress; it enforces the
  RFC 0015 egress allow-list and metadata-endpoint blocks, because the networkless pod
  can no longer reach anything to attack (RFC 0012 §8, applied to tools).

### 8.2 The trifecta stays contract-enforced; agentctl surfaces it

The lethal-trifecta tag union (`untrusted_input` + `sensitive` + `egress`, RFC 0004
§5.3, agentd RFC 0012 §3.1) is computed at the `Agent` and **surfaced advisorily** at
admission (RFC 0007) — never blocked, because the **contract enforces Rule-of-Two per
subagent spawn over each child's narrowed grant** (agentd RFC 0012 §3.2), and the safe
pattern is a reader/actor split a naive `Agent`-level block would wrongly refuse.
agentctl's broker **surfaces and meters** the tagged egress (a `create_*`-tagged
`egress` tool call is counted and policy-checked); the agent **enforces** the granted
subset at spawn. agentctl never re-implements the per-spawn check — it governs the
*credential and the egress*, which the contract cannot (the credential is off-pod by
design).

### 8.3 Per-server budget

`servers[].budget.maxTokens` (mirroring `ModelPool.budget`, agent-api `Budget`) caps a
server's cumulative tool cost where the server reports usage; the broker meters against
it (`agentctl_mcp_calls_total`) and, on exhaustion, refuses further calls with a tool
error — the same **best-effort v1 / hard-enforcement-gated** tiering as RFC 0012 §7
(hard cross-node budget needs the shared accounting store + a clean signal, §Open 4).

### 8.4 Audit

Every broker decision emits the closed-vocabulary audit event of RFC 0015's
`mgmt.invoked`-class model (agentctl RFC 0010 §audit): `mcp.call.allowed` /
`mcp.call.denied{reason}` / `mcp.auth.refreshed` / `mcp.auth.failed`, with the attested
`Agent`, the server, the tool, and (Tier 2) the human principal — so a credentialed
tool egress is fully attributable without ever logging the token.

---

## 9. Transport & the stdio↔HTTP bridge (the reference agent is stdio-only)

The reference agent's MCP client is a **stdio child** speaking NDJSON (`agentd
.../mcp/client.rs`); it has **no** HTTP/streamable-HTTP MCP client and cannot itself
speak the OAuth flow. This is not a blocker — it is the same shape as intelligence
(where the agent dials a keyless socket and the ModelGateway does the provider
protocol). The realization:

- **The broker fronts the server; a tiny stdio↔broker bridge is the agent's child.**
  For a `streamableHttp` server, the operator renders the agent's `--mcp`/`--mcp-config`
  entry as a **stdio** server whose `command` is a **bridge** binary (precedent: the
  `work-mcp-bridge`, `crates/agentctl-coordination/src/bin/work-mcp-bridge.rs`, ~150
  LOC reusing the crate's HTTP client). The agent speaks stdio to the bridge; the
  bridge dials the **broker** over the substrate (keyless); the broker does OAuth +
  TLS to the remote server. **No agent change, no contract change** — the whole
  authenticated-remote-MCP plane works on `agentd` v1.0.0 today.
- **`sidecar` collapses the two.** In the `sidecar` topology the bridge and broker MAY
  be one in-pod process reached at `unix:/run/mcp/<server>.sock`; the agent's `command`
  is a trivial stdio↔uds shim.
- **Local stdio servers are unchanged.** `transport: stdio` with `auth: envPassthrough`
  is the agent spawning the child directly with the passed-through env — the MCP spec
  STDIO exemption, no broker involved (§4.6).
- **Contract ask P-mcp-egress (NEW).** A future **direct streamable-HTTP MCP client**
  in the agent — advertised in the manifest and dialing the broker keyless over the
  substrate, exactly as the intelligence transport does — removes the bridge hop. It is
  a clean additive contract capability, not required for v1. Until it lands, the bridge
  is the supported path. **Contract ask: P-mcp-egress.**

---

## 10. Worked example — one set, three auth modes, one Agent

```yaml
# ── OPS: register the tool servers once, with per-server identity/auth ─────────────
apiVersion: agents.x-k8s.io/v1alpha1
kind: MCPServerSet
metadata: { name: engineering-tools, namespace: agents }
spec:
  servers:
    - name: github                          # remote, autonomous → Tier 1 (SEP-1046 + RFC 7523)
      transport: streamableHttp
      endpoint: https://mcp.github.example.com/mcp
      tags: { "*": [untrusted_input], "create_*": [egress] }
      auth:
        mode: oauthClientCredentials
        oauthClientCredentials:
          resource: https://mcp.github.example.com/mcp
          scopes: [repo.read, issues.write]
          credential: { kind: privateKeyJwt, clientId: agentctl-eng-bot,
                        keyRef: { name: github-mcp-client-key, key: private-key.pem } }
      budget: { maxTokens: 2000000 }
    - name: corp-jira                        # remote, on behalf of a human → Tier 2 (EMA / ID-JAG)
      transport: streamableHttp
      endpoint: https://mcp.corp.example.com/jira
      tags: { "*": [sensitive] }
      auth: { mode: ema, ema: { enterpriseIdp: https://idp.corp.example.com,
                                resource: https://mcp.corp.example.com/jira, scopes: [read, comment] } }
    - name: fs                               # local child, env creds → STDIO exemption
      transport: stdio
      command: ["mcp-fs", "--root", "/watch"]
      tags: { "*": [untrusted_input] }
      auth: { mode: envPassthrough, envPassthrough: [GITHUB_TOKEN] }
---
# ── DEV: reference it; inbound human OIDC supplies the EMA principal (§7.2) ─────────
apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata: { name: eng-assistant, namespace: agents }
spec:
  classRef: { name: hardened }              # kata-hybrid, hostile tenancy (RFC 0004 §3.3)
  mode: reactive
  instruction: "Triage issues and open PRs."
  mcp:
    serverSetRefs: [engineering-tools]      # github(Tier1) + corp-jira(Tier2) + fs(local)
  access:
    oidc: { issuer: https://idp.corp.example.com, audiences: [eng-assistant] }   # authenticates the human → EMA principal
# Resolution (operator renders, agentctl RFC 0006):
#  • broker.topology = node-local (hostile) ; ≥2 replicas + PDB
#  • github     → broker holds the privateKeyJwt signing key; mints an aud-bound RS token per call; NO token in pod
#  • corp-jira  → broker token-exchanges the inbound human's ID token (from access.oidc) → ID-JAG → aud-bound token
#  • fs         → agent spawns the stdio child directly with GITHUB_TOKEN passed through (local; NOT hostile-legal
#                 as a REMOTE token — here it is a local child, allowed)
#  • agent's --mcp: github/corp-jira rendered as stdio BRIDGE children dialing the broker keyless over vsock
#  • trifecta union {untrusted_input, egress, sensitive} = FULL → admission ADVISORY warning; contract enforces
#    Rule-of-Two per spawn (reader=github-read/actor=github-create split is permitted)
```

The payoff mirrors RFC 0004's: the developer's `Agent` names *which tools*; the
operator's one `MCPServerSet` decides *how each is authenticated and where its
credential lives* — and **no MCP token ever touches the hostile-tenant pod**, whether
the agent acts autonomously (Tier 1) or for a signed-in human (Tier 2).

---

## 11. Composition with the instruction source (agentctl RFC 0020)

RFC 0020 adds an **`mcpResource`** instruction-source option — a live instruction
delivered as an MCP **resource** kept current by subscription. The subscriber is RFC
0020's **resolver** (not the agent's reactive `subscribe`), which opens the MCP session
**through this RFC's broker** — so the credential stays off the pod — and holds a
`resources/subscribe` on the resource (the same MCP resource-subscription *protocol
capability* the contract's reactive mode uses, with the resolver as the subscriber, RFC
0020 §7). That option references **an `MCPServerSet` server by name** (`serverRef` +
`uri`). The consequence is deliberate and clean:

- **One MCP-identity model, two consumers.** The same registration (§3) and the same
  broker (§4) that authenticate a *tool* server also authenticate the MCP server a
  *live instruction* subscribes to. An `mcpResource` instruction on an EMA-brokered
  corporate server inherits Tier 2 by construction; on a Tier 1 server, Tier 1. RFC
  0020 owns the instruction **delivery/reload** semantics; this RFC owns the
  **server's identity/auth**. There is no second MCP-auth path.
- **The sourced instruction is `untrusted_input`.** The *instruction* an `mcpResource`
  delivers is externally-controlled data that becomes the agent's system prompt, so RFC
  0020 §8.2 classifies it as an `untrusted_input` trifecta leg **by construction**
  (distinct from the server's own tool `tags`); the broker's per-call policy (§8.1) and
  the contract's trifecta budget apply unchanged.

---

## 12. Versioning, rollout & compatibility

- **Same group/version as the API set** (`agents.x-k8s.io/v1alpha1`, agent-api): the
  `MCPServerSet` CRD graduates with `Agent`/`AgentFleet`/`ModelPool` under the
  single-served-version + conversion-webhook + SVM posture (agentctl RFC 0003 §8 / RFC
  0005). It is namespaced, like `ModelPool`.
- **Additive to the shipped `LocalRef`.** `Agent.spec.mcpServerSetRefs` is already a
  `Vec<LocalRef>` (agent-api); this RFC gives those refs something to resolve against.
  Existing manifests with **no** refs are unaffected; the field's meaning is unchanged.
  (The worked examples use RFC 0003 §3.1's nested `mcp: { serverSetRefs, servers }`
  design shape — which adds inline `servers` alongside the refs; the currently-shipped
  field is the **flat** `mcpServerSetRefs`, and realizing the nested form + inline
  `servers` is part of this track's CRD work.)
- **The CRD `apiVersion` is decoupled from the agent `contract_version`** (RFC 0005): a
  new auth mode is a CRD field addition, not a contract bump; the bridge (§9) means no
  agent change is needed for v1.
- **Graceful degradation.** A server whose AS/IdP does not support the declared mode
  reports `authReady: false` with a reason (`EmaUnsupported` / `DcrUnsupported`); the
  operator surfaces it and the agent simply does not get that tool (a missing tool, not
  a crashed pod).
- **Usable additively / removable cleanly.** Like RFC 0004's CRDs, adopting
  `MCPServerSet` is a refactor toward reuse + secret-free identity; none of it gates an
  MVP milestone (brainstorm §16).

---

## Non-goals

- **The reconcile/render loop, the broker-config compiler, the bridge-command
  rendering.** agentctl RFC 0006. This RFC fixes the *shape*, the *auth model*, and the
  *broker contract*; the controller behaviour is there.
- **The admission webhook that executes these rules** (name-collision across the union,
  cross-ns `Secret` rejection, the hostile-tenant `envPassthrough`/`staticToken`
  rejection, the advisory trifecta union, the `ema`-without-human rejection). agentctl
  RFC 0007.
- **The per-tool trifecta tag vocabulary and the per-spawn Rule-of-Two trust-budget
  check.** The contract (agentd RFC 0012 §3.1/§3.2). agentctl surfaces tags; the agent
  enforces the budget.
- **The internal mTLS CA, the guest→host egress restriction, the five-class secret
  lifecycle** the broker's client key/token chain into. agentctl RFC 0015. This RFC
  requires the broker to be the enforcement point; 0015 owns the trust roots.
- **The A2A inbound OIDC verification** that establishes the human principal Tier 2
  carries. Already shipped (`Access.oidc`/`OidcAccess`, agent-api); this RFC consumes
  the verified identity.
- **A hard, cross-node MCP budget** (needs the shared accounting store + a clean
  signal, mirroring RFC 0012 §7.4). v1 is per-server best-effort (§8.3); hard
  enforcement is §Open 4.
- **Any data-plane MCP client internals.** This CRD/broker describe contract-level MCP
  identity; they MUST NOT encode one binary's MCP client (P0).

---

## Open questions

1. **One "agent egress broker" or two brokers?** The MCP broker and the ModelGateway
   (RFC 0012) are the same pattern (credential-holding egress proxy, attested-peer
   authz, keyless dial). v1 MAY ship them as **one binary** ("agent egress broker",
   two upstream families — models + MCP) or **two** components. Leaning: one binary,
   two listeners, shared authz map — less to run/HA, one egress chokepoint. Confirm
   before implementation (affects RFC 0006 rendering + RFC 0008 §6 inventory).
2. **DCR (RFC 7591) vs CIMD (SEP-991) as the default client-identity mechanism.** DCR
   is universal but stateful (a registered client per AS); CIMD (a client-ID metadata
   URL) is cleaner and increasingly the ecosystem default but younger. v1 supports both
   (§4.4); which is the *rendered default* is an ops-policy question.
3. **Workload-identity primitive for agent→broker: attested peer (v1) vs projected SA
   token / SPIFFE.** §5.1 ships the attested peer (no new component). §5.2's SA-token
   (TokenReview) is the clean bridge to Tier-2 federation without a human; SPIFFE is
   the most portable multi-cluster option. Decide whether v1 adds the SA-token path for
   the EMA-federation case or defers both to the security-plane rollout (RFC 0015).
4. **Hard cross-node MCP budget.** §8.3 is best-effort per-server. Hard, fleet-wide
   MCP-cost enforcement needs the same shared accounting store + clean-signal pair as
   RFC 0012 §7.4 (P-cost). Gate it on that store, or accept per-run/per-server only for
   v1?
5. **Honor the official MCP Registry for population.** The MCP Registry (discovery-only
   metadata) could seed an `MCPServerSet` (endpoint, auth requirements) instead of
   pure hand-declaration. Additive; decide whether v1 reads it or stays declarative.
6. **stdio child credential rotation for `envPassthrough`.** A rotated
   `GITHUB_TOKEN`-style env requires a child respawn (env is read at spawn). Acceptable
   for local children; confirm we do not need a file-based rotate-in-place for the
   stdio-exempt path (the remote path already rotates at the broker, §4.4).

---

## References

**Sibling agentctl RFCs**

- **agentctl RFC 0001** — stack & Contract-as-Schema (P0): the contract-not-agent
  framing this plane's field neutralization follows; the `agent-contract-client` the
  renderer reads.
- **agentctl RFC 0002** — substrate & transport: the keyless substrate socket the agent
  dials the broker over, and the SO_PEERCRED / attested-vsock peer identity (§5).
- **agentctl RFC 0003** — `Agent`/`AgentFleet` CRDs: the reserved `mcp.serverSetRefs` /
  inline `servers` this CRD resolves, the reactive `subscribe` RFC 0020 rides.
- **agentctl RFC 0004 §5** — `MCPServerSet` reusable tagged bundle: the schema this RFC
  **realizes as a CRD and extends** with `transport`/`endpoint`/`auth`/`budget`; the
  `tags` field, union-ADD composition (§5.2), and advisory trifecta union (§5.3) are
  cited unchanged.
- **agentctl RFC 0005** — CRD versioning & conversion: the single-served-version + SVM
  posture §12 commits to.
- **agentctl RFC 0006** — operator reconcile: the renderer compiling a set into broker
  config + the agent's `--mcp` config + the bridge command; the referrer-enumerating
  finalizers.
- **agentctl RFC 0007** — admission ladder: enforces the §3.4 name-collision /
  cross-ns / hostile-tenant / `ema`-without-human rules and the advisory trifecta.
- **agentctl RFC 0008** — node-agent: the broker-is-not-the-node-agent invariant (§6),
  the discovery/attestation the peer identity uses.
- **agentctl RFC 0010** — observability: the `agentctl_mcp_*` metric namespace (§4.5)
  and the `mcp.*` audit vocabulary (§8.4).
- **agentctl RFC 0012** — intelligence plane: the **broker pattern** this RFC mirrors —
  out-of-node-agent placement (§3.1), zero-secret rendering (§5), the `peer→Agent→`
  authz chokepoint (§5.4, reused with *servers* as the target), the two-tier metrics
  correction (§4.5), best-effort-vs-hard budget tiering (§7).
- **agentctl RFC 0015** — security & multi-tenancy: the internal mTLS CA the broker's
  client cert chains to, the guest→host egress restriction, the five-class secret
  lifecycle the OAuth client key / EMA assertion live under, the audit vocabulary.
- **agentctl RFC 0020** — instruction source: the `mcpResource` consumer of this CRD
  (§11).

**Contract spec (the reference implementation, agentd RFCs / schemas)**

- **agentd RFC 0012** — security posture: the per-tool glob trifecta tags (§3.1), the
  spawn-chokepoint Rule-of-Two granted-subset trust budget (§3.2), the stdio-child MCP
  confinement (§3.4), the `Secret`-has-no-`Serialize` invariant (§3.7) the zero-token
  rule rides.
- **agentd RFC 0017** — declarative config & hot reload: the `--mcp`/`--mcp-config`
  refs+inline ADD deviation (§3.3) the composition mirrors; the reload trigger a
  server/credential change signals.
- **`contract/schemas/config.schema.json`** — the `McpServer{name,command,argv,
  transport:stdio|unix,env_passthrough:[]}` shape and the "config file NEVER carries a
  credential" rule this RFC extends with a remote/broker path.
- **`crates/agent-api/src/lib.rs`** — the as-built `ModelPoolSpec` / `SecretKeyRef` /
  `Budget` / `LocalRef` shapes the `MCPServerSet` CRD mirrors, and the shipped
  `mcpServerSetRefs` / `Access.oidc` / `OidcAccess` this RFC resolves + consumes.

**External standards & landscape** (full citations in
[`docs/design/mcp-auth-research.md`](../docs/design/mcp-auth-research.md))

- **MCP authorization spec 2025-06-18** — MCP server as OAuth 2.1 Resource Server;
  audience binding + no-token-passthrough; STDIO exemption.
- **SEP-1046** (OAuth client credentials) — the headless/autonomous extension (Tier 1).
- **SEP-990 / EMA + ID-JAG** (`draft-ietf-oauth-identity-assertion-authz-grant`) — the
  enterprise on-behalf-of-user extension (Tier 2).
- **RFC 9728** (Protected Resource Metadata), **RFC 8414** (AS metadata), **RFC 7591**
  (DCR) / **SEP-991** (CIMD), **RFC 8707** (Resource Indicators / audience), **RFC
  9068** (JWT access-token aud), **RFC 7523** (JWT client assertion / `private_key_jwt`),
  **RFC 8693** (token exchange), **RFC 8705** (mTLS-bound tokens), **KEP-1205** (bound
  SA tokens / TokenReview), **SPIFFE/SPIRE**.

**Contract asks raised by this RFC** (agentctl brainstorm §14): **P-mcp-egress** — a
direct streamable-HTTP MCP client in the agent, dialing the broker keyless over the
substrate (removes the §9 bridge hop; additive, not required for v1). The agent-branded
MCP surfaces this plane resolves into (`AGENT_SERVE_MCP`, `--mcp`/`--mcp-config`,
`env_passthrough`, `agent_mcp_*`) are flagged for the **P0 contract-extraction** open
question (agentctl RFC 0001 §9).

*Where this RFC and a contract spec disagree on the wire, the contract wins and this
RFC is corrected; where this RFC needs a primitive the contract does not expose
(P-mcp-egress), it is a contract ask — never a leak of cluster logic into the agent.*
