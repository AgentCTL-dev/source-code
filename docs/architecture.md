# agentctl — architecture & wiring

How the control-plane components and the data-plane agents (e.g. `agentd`) are
connected and communicate. Each diagram is one slice of the system; together they
show the whole wiring.

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
    op[operator]
    na[node-agent DaemonSet]
    gw[A2A gateway]
    mg[ModelGateway]
    coord[coordination server]
    scaler[KEDA external scaler]
    keda[KEDA]
    cm[cert-manager]
    pg[(Postgres)]
  end

  subgraph dp[Data plane: your agents]
    agent[agentd pod]
  end

  kubectl -->|apply Agent/Fleet/ModelPool| api
  kubectl -->|drain / status verbs| api
  api -->|mutate then validate| adm
  op -->|watch CRDs, render workload| agent
  api -->|mTLS| na
  na -->|unix socket MCP mgmt| agent
  peer -->|A2A HTTP + SSE| gw
  gw -->|mTLS| na
  gw <-->|durable tasks| pg
  mg <-->|usage / tasks| pg
  agent -->|infer: no key| mg
  mg -->|provider call: key injected| prov
  prod -->|work.submit MCP| coord
  agent -->|work.claim/ack MCP egress| coord
  keda -->|IsActive/GetMetrics gRPC| scaler
  scaler -->|work.stats| coord
  keda -->|scale 0..N| agent
  na -->|scrape-proxy /metrics| prom
  cm -.->|certs + caBundle| api
  cm -.->|cert| adm
  cm -.->|mTLS certs| na
```

---

## 2. An agent's two MCP directions

An agent **serves** a management profile (the control plane drives it) and is a
**client** for work + intelligence + sources (it reaches out). These are opposite
directions and easy to conflate.

```mermaid
flowchart TB
  na[node-agent] -->|inbound: tools/call over unix socket| srv

  subgraph agent[agentd pod]
    srv[SERVES mgmt MCP: status / drain / subagent.spawn]
    cli[CLIENT: outbound calls]
  end

  cli -->|work.claim / ack: MCP, egress| coord[coordination server]
  cli -->|infer: HTTP| mg[ModelGateway]
  cli -->|subscribe / tools: MCP| src[work source / other MCP servers]
```

---

## 3. Trust: cert-manager issuance + caBundle injection

```mermaid
flowchart TB
  ss[SelfSigned Issuer] --> ca[agentctl CA certificate]
  ca --> caissuer[CA Issuer]
  caissuer --> apicert[apiserver serving cert]
  caissuer --> admcert[admission serving cert]
  caissuer --> nacert[node-agent mTLS server cert]
  caissuer --> clcert[control-plane mTLS client cert]
  apicert -.->|caBundle inject| apisvc[APIService]
  admcert -.->|caBundle inject| webhook[Validating + Mutating Webhooks]
  nacert --> na[node-agent :8443]
  clcert --> apiclient[apiserver + gateway clients]
```

A self-signed bootstrap issuer mints the agentctl CA; the CA issuer mints every
serving/mTLS leaf; cert-manager's cainjector populates the `caBundle` on the
APIService and the webhooks. Renewal is automatic (`renewBefore`).

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
  OP->>API: apply Deployment/Job/StatefulSet (confined + downward env + mgmt socket)
  API-->>AG: scheduled + started
  AG->>AG: serve mgmt MCP on unix socket; idle / reactive
```

---

## 5. Management path — kubectl drain

```mermaid
sequenceDiagram
  actor U as kubectl
  participant KA as kube-apiserver
  participant API as aggregated APIServer
  participant NA as node-agent
  participant AG as agentd
  U->>KA: create --raw .../agents/x/drain
  KA->>API: proxy (front-proxy mTLS + identity)
  API->>API: SubjectAccessReview (RBAC)
  API->>NA: POST /drain (mTLS client cert)
  NA->>NA: SO_PEERCRED attest pod uid
  NA->>AG: tools/call drain (unix socket MCP)
  AG-->>NA: draining -> proc.exit reason=drain
  NA-->>API: ok
  API-->>U: Success
```

---

## 6. Intelligence path — secretless + budgeted

```mermaid
sequenceDiagram
  participant AG as agentd
  participant MG as ModelGateway
  participant K8 as kube Secret via ModelPool
  participant P as Provider
  Note over AG: networkless + secretless
  AG->>MG: infer (X-Agent identity, no key)
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

### 6a. Routed-infer attestation — the networkless (Kata) tier

On the networkless tier the agent has no routable pod IP, so the ModelGateway cannot attest it
by source IP. Opt-in (`nodeAgent.inferProxy.enabled` + the operator annotation
`agentctl.dev/routed-infer: "true"`) routes inference through the node-agent's unix-socket
forwarder, which kernel-attests the peer (`SO_PEERCRED`) and re-stamps the identity so the
client cannot self-assert. See docs/security.md → "Networkless-tier infer attestation".

```mermaid
sequenceDiagram
  participant AG as agentd (networkless)
  participant NA as node-agent (infer-proxy)
  participant MG as ModelGateway
  Note over AG: AGENT_INTELLIGENCE = unix:/run/agentctl/infer/infer.sock (read-only mount)
  AG->>NA: infer over unix socket (no IP, may assert any header)
  NA->>NA: SO_PEERCRED -> /proc cgroup -> pod uid
  NA->>NA: strip client identity; re-stamp X-Agent-Pod-Uid
  NA->>MG: infer + X-Agent-Pod-Uid (trusted forwarder, source IP)
  MG->>MG: trust forwarder IP; resolve uid -> namespace/identity
  MG-->>NA: completion (metered + budgeted)
  NA-->>AG: completion
```

---

## 7. A2A path — agents reachable by other agents

```mermaid
sequenceDiagram
  participant C as A2A client / peer agent
  participant GW as A2A gateway
  participant PG as Postgres
  participant NA as node-agent
  participant AG as agentd
  C->>GW: GET /.well-known/agent-card.json
  GW-->>C: JWS-signed Agent Card
  C->>GW: message/send or message/stream
  GW->>PG: create Task (durable)
  GW->>NA: forward (mTLS)
  NA->>AG: a2a (unix socket)
  AG-->>NA: result / SSE frames
  NA-->>GW: relay
  GW->>PG: update Task -> completed
  GW-->>C: result / SSE: working -> artifact -> completed
```

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

- **Two trust planes meet at the agent.** *Inbound* management is mTLS
  (apiserver → node-agent) then a kernel-attested (`SO_PEERCRED`) unix socket
  (node-agent → agent). *Outbound* work/intelligence is the agent dialing out
  (egress on the hardened/networkless tier).
- **State.** Postgres is shared durable state for the gateway (A2A tasks) and the
  ModelGateway (token usage); the coordination server is in-memory (the claim
  ledger), deliberately separate (and behind a `ClaimStore` trait for a future
  durable backend).
- **The contract is the boundary.** agentctl never depends on a specific agent —
  every arrow into `agentd` above is an ACC surface (management profile, A2A,
  `work.*`, the downward-API env, `/metrics`), so any conformant agent wires in
  identically.
