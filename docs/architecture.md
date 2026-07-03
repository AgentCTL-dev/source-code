# agentctl — architecture & wiring

How the control-plane components and the data-plane agents (e.g. `agentd`) are
connected and communicate. Each diagram is one slice of the system; together they
show the whole wiring.

> ⚠️ **Contract 2.0 — the network is the substrate.** These diagrams reflect the
> **v2** model: agents **serve mTLS HTTPS** (`POST /mcp`) and **dial the gateways
> keyless**; the **node-agent is retired** (no host socket, no on-node bridge);
> identity is cryptographic — a verified mTLS **client cert** into agents, an
> **attested source IP** into the gateways. See
> **[RFC 0021](../rfcs/0021-contract-2.0-network-substrate-pivot.md)** for the full design.

**Legend:** solid = request/data path · dashed = certificates / out-of-band ·
`agentd` = any conformant agent (the data plane). The control plane is Rust; the
data plane is *any* agent that speaks the Agent Control Contract (ACC).

See also: [STATUS](STATUS.md) · [operations runbook](operations.md) ·
[cloud-native roadmap](cloud-native-roadmap.md).

---

## 1. Component topology — who talks to whom

```mermaid
flowchart LR
  subgraph ext[External / users]
    kubectl[kubectl / kube-apiserver]
    prod[Producers]
    peer[Other agents / A2A clients]
    prov[Model provider]
    prom[Prometheus / Grafana]
  end

  subgraph cp[Control plane: agentctl-system]
    adm[admission webhook]
    api[aggregated APIServer]
    op[operator + PKI]
    gw[A2A gateway]
    mg[ModelGateway]
    mcpg[MCPGateway]
    coord[coordination server]
    scaler[KEDA external scaler]
    keda[KEDA]
    cm[cert-manager]
    pg[(Postgres)]
  end

  subgraph dp[Data plane: your agents]
    agent[agentd pod - serves mTLS HTTPS :8443 /mcp]
  end

  kubectl -->|apply Agent/Fleet/ModelPool/MCPServerSet| api
  kubectl -->|drain / status verbs| api
  api -->|mutate then validate| adm
  op -->|watch CRDs, render workload| agent
  api -->|a2a.* admin over mTLS /mcp| agent
  peer -->|A2A HTTP + SSE| gw
  gw -->|forward direct to pod: mTLS /mcp| agent
  gw <-->|durable tasks| pg
  mg <-->|usage / tasks| pg
  agent -->|infer: keyless HTTPS| mg
  agent -->|tools: keyless HTTPS| mcpg
  mg -->|provider call: key injected| prov
  mcpg -->|tool call: credential injected| src2[MCP tool servers]
  prod -->|work.submit MCP| coord
  agent -->|work.claim/ack MCP egress| coord
  keda -->|IsActive/GetMetrics gRPC| scaler
  scaler -->|work.stats| coord
  keda -->|scale 0..N| agent
  prom -->|scrape /metrics directly| agent
  cm -.->|certs + caBundle| api
  cm -.->|cert| adm
  cm -.->|per-workload serving cert + per-ns CA| agent
  cm -.->|client cert| api
```

---

## 2. An agent's two MCP directions

An agent **serves** a management profile (the control plane drives it) and is a
**client** for work + intelligence + sources (it reaches out). These are opposite
directions and easy to conflate.

```mermaid
flowchart TB
  cp[control plane: APIServer / A2A gateway] -->|inbound: mTLS HTTPS POST /mcp, client cert = Management| srv

  subgraph agent[agentd pod]
    srv[SERVES mgmt + A2A MCP on :8443: status / a2a.Drain / SendMessage / subagent.spawn]
    cli[CLIENT: outbound keyless calls]
  end

  cli -->|work.claim / ack: MCP, egress| coord[coordination server]
  cli -->|infer: keyless HTTPS| mg[ModelGateway]
  cli -->|tools: keyless HTTPS| mcpg[MCPGateway]
  cli -->|work.claim/subscribe: MCP| src[work source]
```

---

## 3. Trust: cert-manager issuance + caBundle injection

```mermaid
flowchart TB
  ss[SelfSigned Issuer] --> ca[agentctl-ca ClusterIssuer]
  ca --> apicert[apiserver serving cert]
  ca --> admcert[admission serving cert]
  ca --> gwcert[gateway serving certs: A2A / ModelGateway / MCPGateway]
  ca --> wlcert[per-workload agent serving cert: name-serving-tls]
  ca --> nscm[per-namespace agentctl-ca ConfigMap: public CA]
  ca --> clcert[control-plane mTLS client cert]
  apicert -.->|caBundle inject| apisvc[APIService]
  admcert -.->|caBundle inject| webhook[Validating + Mutating Webhooks]
  wlcert --> agent[agentd :8443 serve + verify client CA]
  nscm -.->|--tls-ca outbound trust| agent
  clcert --> apiclient[apiserver + A2A gateway clients: Management at /mcp]
```

A self-signed bootstrap issuer mints the **`agentctl-ca` ClusterIssuer** (one cluster
CA); it mints every serving/mTLS leaf — including a **per-workload serving cert**
(`<name>-serving-tls`) for each agent — and the control plane's client cert. A
**per-namespace `agentctl-ca` ConfigMap** distributes the public CA so agents trust
the gateways' serving certs (`--tls-ca`). cert-manager's cainjector populates the
`caBundle` on the APIService and webhooks; renewal is automatic (`renewBefore`), and
agentd **hot-reloads its serving cert** on rotation without a restart.

---

## 4. Provisioning — apply a CR → running agent

```mermaid
sequenceDiagram
  actor U as kubectl
  participant API as kube-apiserver
  participant ADM as admission webhook
  participant OP as operator
  participant AG as agentd pod
  U->>API: apply Agent (image, mode, modelPool, caps)
  API->>ADM: mutate (defaults) then validate (trifecta + registry)
  ADM-->>API: patched + admitted
  OP->>API: watch Agents
  OP->>OP: ensure per-workload PKI (serving cert + per-ns CA ConfigMap)
  OP->>API: apply Deployment/Job/StatefulSet (restricted-PSS + downward env + TLS mounts, zero pod creds)
  API-->>AG: scheduled + started
  AG->>AG: serve mgmt + A2A MCP on mTLS HTTPS :8443; idle / reactive
```

---

## 5. Management path — kubectl drain

```mermaid
sequenceDiagram
  actor U as kubectl
  participant KA as kube-apiserver
  participant API as aggregated APIServer
  participant AG as agentd
  U->>KA: create --raw .../agents/x/drain
  KA->>API: proxy (front-proxy mTLS + identity)
  API->>API: SubjectAccessReview (RBAC)
  API->>API: resolve Agent -> status.podIP
  API->>AG: a2a.Drain JSON-RPC over mTLS HTTPS POST /mcp (client cert = Management)
  AG->>AG: verify client cert vs pinned client CA -> Management
  AG-->>API: draining -> proc.exit reason=drain
  API-->>U: Success
```

The verbs `drain` / `lame-duck` / `cancel` / `pause` / `resume` map to
`a2a.Drain` / `a2a.LameDuck` / `a2a.Cancel` / `a2a.Pause` / `a2a.Resume`. Each stays
SAR-gated at the APIServer; the call goes **direct to the agent pod** — no node-agent,
no host socket, no `pods/proxy`.

---

## 6. Intelligence path — secretless + budgeted

```mermaid
sequenceDiagram
  participant AG as agentd
  participant MG as ModelGateway
  participant K8 as kube Secret via ModelPool
  participant P as Provider
  Note over AG: secretless — dials AGENT_INTELLIGENCE=https://…modelgateway… keyless
  AG->>MG: infer over TLS (no key, no identity header)
  MG->>MG: attest caller by SOURCE IP (kube pod lookup) -> namespace/identity
  MG->>K8: read ModelPool credentialSecretRef
  MG->>MG: meter tokens; check budget
  alt within budget
    MG->>P: provider call (real key injected)
    P-->>MG: completion + usage
    MG-->>AG: completion
  else over budget
    MG-->>AG: HTTP 429
  end
```

### 6a. Source-IP attestation & the cold-start race

Contract 2.0 makes every agent network-native, so the ModelGateway (and the MCPGateway)
attest the caller **by source IP** directly — the v1 node-agent infer-proxy forwarder is
**retired**. The gateway maps the TCP source IP to the calling pod via a kube watch-cache,
deriving the agent's namespace/identity; a header is never trusted, and a confined pod
drops `CAP_NET_RAW` so it cannot spoof its source IP. The one hazard is a **cold-start
race** — an agent may dial before its `status.podIP` has propagated into the gateway's
watch-cache — handled by a bounded retry.

```mermaid
sequenceDiagram
  participant AG as agentd (routable pod IP)
  participant MG as ModelGateway / MCPGateway
  participant KW as kube watch-cache (pods by IP)
  AG->>MG: infer / tool call over TLS (keyless; source IP = pod IP)
  MG->>KW: resolve source IP -> pod
  alt pod not yet in cache (cold start)
    MG->>MG: retry 3x / 500ms
  end
  KW-->>MG: pod -> namespace / identity
  MG->>MG: scope + inject credential + meter + budget
  MG-->>AG: response
```

---

## 7. A2A path — agents reachable by other agents

```mermaid
sequenceDiagram
  participant C as A2A client / peer agent
  participant GW as A2A gateway
  participant PG as Postgres
  participant AG as agentd
  C->>GW: GET /.well-known/agent-card.json
  GW->>AG: read agent://capabilities (resources/read, mTLS /mcp)
  GW-->>C: JWS-signed Agent Card
  C->>GW: SendMessage or SendStreamingMessage
  GW->>PG: create Task (durable)
  GW->>AG: forward direct to pod: bare method over mTLS HTTPS /mcp
  AG-->>GW: {"task":…} result / SSE frames (terminal state closes stream)
  GW->>PG: update Task -> completed
  GW-->>C: result / SSE: working -> artifact -> completed
```

The gateway forwards **direct to the agent pod** with the contract-2.0 wire (bare
PascalCase methods, `{"task"}` envelope, SSE terminated by terminal state + close);
there is no node-agent relay. Durable history + push config stay gateway-owned.

### 7a. OIDC-gated A2A request — per-agent caller identity

When an `Agent` declares `spec.access.oidc` (see security.md), the gateway gates the
A2A surface on a JWKS-verified JWT + required-claims authz before forwarding, and
passes the verified identity to the agent.

```mermaid
sequenceDiagram
  participant C as A2A client (caller)
  participant GW as A2A gateway
  participant IdP as OIDC issuer (JWKS)
  participant AG as agentd
  Note over GW: agent's spec.access.oidc = {issuer, audiences, requiredClaims}
  C->>GW: message/send + Authorization: Bearer <JWT>
  GW->>IdP: fetch/cache JWKS (jwksUri or discovery off issuer)
  IdP-->>GW: signing keys
  Note over GW: verify signature + iss/aud/exp, then requiredClaims (e.g. groups has support)
  alt JWT invalid or claims unmet
    GW-->>C: 401/403 deny
  else verified + authorized
    GW->>AG: forward + caller identity (sub/email/groups, if forwardIdentity)
    AG-->>GW: result / SSE frames
    GW-->>C: result / SSE
  end
```

### 7b. Trusted front-proxy A2A request — edge auth at an external API gateway

When `trustedProxy.enabled` is set, a fronting API gateway (e.g. APISIX) terminates edge auth
and asserts the identity over an **mTLS channel**. The A2A gateway authenticates the *proxy*
(client cert vs the agentctl CA + an `allowedNames` list), trusts the asserted
`<prefix>-subject/-email/-groups` (prefix `trustedProxy.headerPrefix`, default `x-agentctl`),
**strips** those headers from any untrusted plaintext caller, enforces the
agent's `requiredClaims`, and forwards the identity to the agent. Mirrors the
aggregated-apiserver front-proxy. See security.md → "Trusted front-proxy (external API gateway)".

```mermaid
sequenceDiagram
  participant C as External client
  participant PX as APISIX (front-proxy)
  participant IdP as OIDC issuer
  participant GW as A2A gateway
  participant AG as agentd
  Note over PX: trustedProxy.enabled; APISIX holds a client cert from the agentctl CA
  C->>PX: request + Authorization: Bearer <JWT>
  PX->>IdP: terminate edge auth (verify JWT)
  IdP-->>PX: verified identity (sub / email / groups)
  PX->>GW: mTLS (client cert) + x-agentctl-subject/-email/-groups
  Note over GW: verify client cert vs agentctl CA; client name allow-listed?
  alt untrusted channel (plaintext / name not allow-listed)
    GW->>GW: STRIP x-agentctl-* + legacy X-Forwarded-* (anti-spoof)
    GW-->>C: 401/403 — no asserted identity honored
  else trusted proxy channel
    Note over GW: trust x-agentctl-*; enforce agent requiredClaims
    alt requiredClaims unmet
      GW-->>PX: 403 deny
    else authorized
      GW->>AG: forward + caller identity
      AG-->>GW: result / SSE frames
      GW-->>PX: result / SSE
      PX-->>C: result / SSE
    end
  end
```

---

## 8. Claim-mode work distribution — elastic from zero

```mermaid
sequenceDiagram
  participant PR as Producer
  participant CO as coordination server
  participant SC as scaler
  participant KE as KEDA
  participant FL as fleet 0..N agentd
  participant MG as ModelGateway
  PR->>CO: work.submit(item, claim_key)
  loop polling
    KE->>SC: IsActive / GetMetrics (gRPC)
    SC->>CO: work.stats -> pending
  end
  KE->>FL: scale 0 -> N (from zero)
  FL->>CO: work.claim(item) + claim_key  (N agents race)
  CO-->>FL: granted=true to ONE; held_by to the rest
  FL->>MG: infer (process the item)
  FL->>CO: work.ack(lease, claim_key)
  Note over CO: claim_key recorded -> redelivery deduped; lease expiry re-offers on crash
  SC->>CO: work.stats -> 0
  KE->>FL: scale N -> 0
```

Distribution is **pull/claim**, not push — the only "assignment" is the atomic
claim picking one winner of N racers. Producers `work.submit` references (not
payloads); the bytes live in your store.

---

## 9. Shard-mode partitioning — keyed / ordered work

```mermaid
flowchart LR
  src[work source items] --> route{fnv1a64 of shard_key mod N}
  route -->|0| s0[shard 0 = StatefulSet ordinal 0]
  route -->|1| s1[shard 1 = ordinal 1]
  route -->|K| sk[shard K = ordinal K]
  route -->|N-1| sn[shard N-1]
```

`N = scaling.shards` is operator-owned (KEDA paused). The predicate runs at intake
before any claim, so out-of-shard items drop at ~zero cost. The same key always
lands on the same shard (ordering). Composes with claim for resize overlap.

---

## 10. Operator HA — leader election

```mermaid
sequenceDiagram
  participant A as operator replica A
  participant B as operator replica B
  participant L as coordination.k8s.io Lease
  A->>A: serve /healthz /readyz /metrics (always, every replica)
  B->>B: serve /healthz /readyz /metrics (always, every replica)
  A->>L: acquire lease (holderIdentity = pod A)
  L-->>A: granted -> LEADER (runs reconcile loop)
  B->>L: try acquire
  L-->>B: held by A -> STANDBY (readyz still 200, leader gauge=0)
  loop renew at ttl/3
    A->>L: renew
  end
  Note over A,B: readiness is NOT gated on leadership -> no RollingUpdate deadlock
  A--xL: A dies / lease expires
  B->>L: acquire
  L-->>B: granted -> new LEADER (failover ~lease duration)
```

---

## Cross-cutting notes

- **Two trust planes meet at the agent — identity, not reachability.** *Inbound*
  management + A2A is **direct mTLS** to the agent's `/mcp`; the agent verifies the
  caller's client cert against the pinned client CA (⇒ `Management`). *Outbound*
  intelligence + tools is the agent **dialing the gateways keyless**, attested by
  **source IP**. No node-agent, no host socket, and **no credential on the pod**.
- **State.** Postgres is shared durable state for the gateway (A2A tasks) and the
  ModelGateway (token usage); the coordination server is in-memory (the claim
  ledger), deliberately separate (and behind a `ClaimStore` trait for a future
  durable backend).
- **The contract is the boundary.** agentctl never depends on a specific agent —
  every arrow into `agentd` above is an ACC surface (management profile, A2A,
  `work.*`, the downward-API env, `/metrics`), so any conformant agent wires in
  identically.
