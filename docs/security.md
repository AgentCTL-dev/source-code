# Security reference

How agentctl authenticates every caller, isolates tenants, hardens workloads, and
manages its certificate fabric — for reviewers and operators hardening a
production install. For the component overview see [architecture.md](architecture.md);
for day-2 procedures (enabling gates, rotating tokens, retrieving certs) see
[operations.md](operations.md).

## The trust model at a glance

agentctl treats **identity as cryptographic**, and splits its surfaces into two
classes with different roots of trust:

- **Control / management path — strongly authenticated.** Callers that drive an
  agent (management verbs, A2A ingress that reaches the pod) present a
  cert-manager-issued **mTLS client certificate** that identifies them as the
  single **Management** origin. Management access through the aggregated API is
  additionally gated by Kubernetes RBAC (SubjectAccessReview) per verb.
- **Data-plane utility path — direct-dial and network-isolated.** Agents dial
  model providers and MCP servers **directly**, authenticating themselves (an
  AAuth signature, or a key mounted onto the pod); the only in-cluster data-plane
  hop is the **coordination** work fabric, which derives the caller's tenant from
  its **attested source IP** (the pod IP, resolved to a pod through the Kubernetes
  API). NetworkPolicies confine which pods can reach which surface at all.

Two properties hold across both classes:

- **Secret-free with AAuth.** An agent given a portable AAuth identity holds no
  provider or tool credential — it signs each direct request itself. For a
  key-authenticated provider or server the operator mounts the key onto the agent
  (`INTELLIGENCE_TOKEN`, or an MCP `staticToken`); the agent holds it, because
  there is no off-pod broker. Beyond any such mounted key, the only key material
  on the pod is its own rotatable serving + identity keys plus the public CA
  bundle.
- **Agent pods are confined.** Nonroot, no privilege escalation, all Linux
  capabilities dropped, read-only root filesystem, no auto-mounted ServiceAccount
  token, `RuntimeDefault` seccomp — satisfying the `restricted` Pod Security
  Standard.

```
                        ┌──────────────────────────────────────────┐
   kubectl (RBAC user)  │            control-plane namespace         │
        │               │                                            │
        ▼ SAR per verb  │  apiserver ─┐                              │
   aggregated apiserver ─┼────────────┤  mTLS client cert            │
                        │  A2A gateway ┘  (= Management origin)       │
                        │        │                                   │
                        └────────┼───────────────────────────────────┘
                                 │ dial pod directly, mTLS  :8443
                                 ▼
                        ┌──────────────────────┐
   providers + MCP  ◀───│  tenant agent pod    │◀── ingress: control plane only
   direct dial (AAuth   │  (hardened, netpol'd,│
   sig or mounted key)  │   AAuth-signing)     │──▶ egress: DNS + control plane
                        └──────────┬───────────┘        + public HTTPS
                                   │ work.* : source-IP attested
                                   ▼
                             coordination
                           (work.* claim leasing)
```

## Identity model

### Inbound to an agent — the Management origin (mTLS client cert)

Every rendered agent pod serves its control surface (self-MCP + A2A) over mTLS
HTTPS on port **8443**. The operator renders the agent with these serve
arguments:

```
--serve-mcp        https://0.0.0.0:8443
--serve-cert       /etc/agentctl/tls/tls.crt
--serve-key        /etc/agentctl/tls/tls.key
--serve-client-ca  /etc/agentctl/ca/ca.crt
--tls-ca           /etc/agentctl/ca/ca.crt
```

The agent verifies the **client** certificate of any caller against the pinned
cluster CA (`--serve-client-ca`). A caller that presents a CA-signed client cert
authenticates as the **Management** origin — the only origin permitted to drive
management and A2A methods on the agent. A caller without a valid Management
client certificate is rejected.

Exactly two control-plane components present this client certificate:

| Component | Reaches the agent for |
|---|---|
| **apiserver** | management verbs (drain, lame-duck, cancel, pause, resume) |
| **gateway** (A2A) | inbound A2A `message/send` / `message/stream` |

Both dial the target pod **directly** over mTLS using the shared client cert
`agentctl-client-tls` (common name `agentctl-control-plane`). Because the identity
is a certificate, there is no bearer token on the agent pod to steal, and no
network position confers Management authority — only the private key does.

### Outbound from an agent — two paths

An agent's outbound traffic splits in two:

- **To model providers and MCP servers — direct, self-authenticated.** The agent
  dials the provider/server itself and proves who it is: it signs each request
  with its portable **AAuth** identity (the server verifies against the Agent
  Provider's JWKS), or presents a key the operator mounted onto the pod. No
  control-plane component sits on this path.
- **To the coordination work fabric — attested by source IP.** For the in-cluster
  `work.*` hop the coordination server does not trust any self-asserted identity.
  Instead it reads the **kernel-set source IP** of the TCP connection — the pod's
  own IP, which the pod cannot forge — and resolves it to the calling pod through
  the Kubernetes API. The pod's **namespace is the authoritative tenant**, binding
  each work claim to its owner.

Source-IP attestation is enabled by default (`coordination.attestIdentity: true`).
It is robust precisely because agent pods are confined: with
`capabilities.drop: ["ALL"]` a tenant pod has no `CAP_NET_RAW` and cannot craft raw
packets to spoof a source IP, so the kernel-attributed source IP is a trustworthy
tenant identity. A tenant therefore cannot ack or release another namespace's work
claim.

If a request also carries an advisory `X-Agent-Namespace` header that disagrees
with the attested namespace, **the attested namespace always wins** and the
mismatch is recorded as a spoof attempt.

### How attestation resolves a pod

The source-IP → pod resolution is a small, cache-backed lookup:

1. Read the connection's source IP (the pod IP).
2. List/get pods and match the IP against `status.podIP` **and** the `status.podIPs`
   list (dual-stack); the field selector is treated as advisory and the match is
   re-verified locally.
3. Derive the identity from the matched pod: the **namespace** is the tenant; the
   **agent name** is the operator-set `agentctl.dev/agent` label, falling back to
   the pod name.
4. Cache the `IP → identity` mapping with a short TTL (10s) so a burst of requests
   from one pod does not hammer the API server, while a deleted-and-recycled IP is
   re-attested quickly.

Because a freshly scheduled pod may issue its first claim before its `status.podIP`
has propagated into the coordination server's watch cache, the resolution retries
briefly before failing closed — a startup-timing accommodation, not a relaxation of
trust. An IP that resolves to no pod is **rejected**; the coordination server never
falls back to the spoofable header.

This attestation guards work-claim ownership on the coordination server
(`coordination.attestIdentity: true`), binding each claim's lifecycle to the
attested caller so one tenant cannot ack or release another tenant's claim.

## Agent credentials: AAuth-direct or a mounted key

There is no credential-brokering gateway. An agent authenticates to a provider or
MCP server on one of two paths, chosen per binding:

| Path | How the agent authenticates | Where the credential lives |
|---|---|---|
| **AAuth (preferred, secret-free)** | The agent signs each request with its portable AAuth identity (RFC 9421); the remote verifies against the Agent Provider's JWKS. | Nowhere shared — the pod holds only its own Ed25519 identity key (used to sign), provisioned by the operator. |
| **Mounted key** | The operator mounts the key onto the **agent**, which attaches it on each direct request. | Intelligence: `ModelPool.spec.credentialSecretRef` → `INTELLIGENCE_TOKEN` on the pod. Tools: an `mcpServers[].auth.tokenSecretRef` (`staticToken`) → the `Authorization` bearer (or a custom `header`) on the pod. |

The honest trade-off: **AAuth-direct keeps the pod secret-free; a mounted key does
not** — with `staticToken`/`credentialSecretRef` the agent holds that key, because
there is no off-pod broker to hold it instead. Prefer AAuth for untrusted or
multi-tenant workloads (see
[Portable agent identity](architecture.md#portable-agent-identity-aauth)).

A key-authenticated `ModelPool` and its Secret (mounted onto the agent):

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: provider-credentials
  namespace: default
type: Opaque
stringData:
  api-key: sk-...                     # mounted onto the agent as INTELLIGENCE_TOKEN
---
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata:
  name: mockpool
  namespace: default
spec:
  provider: mock
  endpoint: http://mock-provider.default:8080
  credentialSecretRef:                # OPTIONAL — omit for the AAuth (secret-free) path
    name: provider-credentials
    key: api-key
  models: ["mock-model-v1"]
  defaultModel: mock-model-v1
```

## Pod and workload hardening

The operator renders every tenant agent pod (and every control-plane pod) with a
confined security context. For a tenant agent the exact rendered fields are:

```yaml
# container securityContext
securityContext:
  runAsNonRoot: true                  # runAsUser is NOT pinned — the image's
  allowPrivilegeEscalation: false     #   native USER (e.g. 65532) runs unchanged
  readOnlyRootFilesystem: true
  capabilities:
    drop: ["ALL"]
# pod securityContext
securityContext:
  seccompProfile:
    type: RuntimeDefault
# pod spec
automountServiceAccountToken: false   # no ambient ServiceAccount token
shareProcessNamespace: true           # agent is not PID 1 (correct orphan check)
```

Supporting facts:

- **Writable paths are explicit.** With `readOnlyRootFilesystem: true`, the only
  writable location is an `emptyDir` mounted at `/tmp`. The serving cert
  (`/etc/agentctl/tls`) and CA bundle (`/etc/agentctl/ca`) are read-only mounts.
- **No borrowed and no ambient credentials.** `automountServiceAccountToken: false`
  keeps the namespace default ServiceAccount token off the pod; on the AAuth path
  the pod carries only its own serving + AAuth identity keys and the public CA (a
  `staticToken`/`INTELLIGENCE_TOKEN` binding additionally mounts that one key).
- **No privileged component.** Nothing in the control plane requires `hostPath`,
  `hostPID`, or privileged mode. The control-plane namespace and tenant namespaces
  run at the `baseline` Pod Security level (`namespace.podSecurity: baseline`),
  while every workload self-confines to `restricted`.
- **Ports.** An agent exposes `mcp` on 8443 (mTLS) and `metrics` on 9090; its
  readiness probe is `/readyz` on the metrics port, which drain/lame-duck flip so
  that "ready" means "accepting work".

## Tenant network isolation

Tenant isolation is enforced by four NetworkPolicies applied in every agent
namespace. All four select agent pods by the label `app.kubernetes.io/name: agent`.

| Policy | Type | Effect |
|---|---|---|
| `agent-default-deny` | Ingress + Egress | Deny all traffic in and out by default (no rules). |
| `agent-allow-controlplane-and-dns` | Egress | Re-open egress **only** to DNS (UDP/TCP 53, any namespace) and to the remaining control-plane **pods** — the A2A gateway (delegation-out) and coordination (work claims) — on TCP 443 and TCP 8080. |
| `agent-ingress-controlplane-only` | Ingress | Accept ingress **only** from the control-plane namespace (the apiserver + A2A gateway reaching the agent's mTLS 8443). No cross-tenant pod-to-pod traffic. |
| `agent-internet-egress` | Egress | HTTPS (TCP 443) to **public** address space — `0.0.0.0/0` / `::/0` minus private, link-local, and CGNAT ranges — so every agent can reach its model provider and MCP servers by direct dial (and a public Agent Provider for AAuth) while lateral movement into cluster/private space stays default-denied. Vanilla NetworkPolicy cannot express per-FQDN egress; this is the honest coarse tier — a DNS-aware CNI (Cilium/Calico) can tighten it to the declared endpoints. |

Design points worth noting for review:

- **Control-plane egress is pod-scoped, not namespace-scoped.** That egress allow
  uses a `namespaceSelector` (the control-plane namespace) **and** a `podSelector`
  matching only the A2A gateway and coordination app names. A bare namespace
  selector would also expose the admission webhook and the aggregated apiserver to
  a tenant agent; the pod selector forbids that. The apiserver and admission app
  names are explicitly not in the allow set.
- **Internet egress is deliberate, but public-only.** Because agents dial
  providers and MCP servers directly, every agent gets public-HTTPS egress via
  `agent-internet-egress` — but only to **public** address space; every private,
  link-local, and CGNAT range is carved out, so a compromised pod still cannot
  reach cluster-internal or peer-tenant services, and there is no allow for
  pod-to-pod traffic into another tenant's namespace. (Admission additionally
  requires `identity.aauth` + `capabilities.egress: true` for any `auth.mode: aauth`
  MCP binding, matching declared intent to the signed dial.)

### Shipped by the chart AND reconciled by the operator

The chart renders these four policies for the namespaces statically listed in
`networkPolicies.agentNamespaces`. That does not cover a tenant namespace created
**after** install, so on every `Agent`/`AgentFleet` reconcile the **operator**
also ensures the same four policies in the workload's own namespace. The two
sources use byte-identical names and bodies and are applied server-side, so they
co-own each object rather than conflict. The operator-reconciled policies are
namespace singletons carrying no owner reference, so deleting one Agent never
tears down the namespace's isolation.

Both the chart and the operator gate on the same flag
(`networkPolicies.enabled`, default off) and require the control-plane namespace
to be known (the operator's `POD_NAMESPACE`); absent it, the operator skips the
ensure path rather than render an over-broad policy.

> **Requires a policy-enforcing CNI.** NetworkPolicies are enforced only by a CNI
> that implements them (Calico, Cilium). kind's default `kindnet` ignores them —
> the objects render correctly but are inert. Verify your CNI before relying on
> tenant isolation.

## Admission control

A validating and a mutating webhook (served over HTTPS; the kube-apiserver is the
only client) apply the policy checks that CRD-level validation cannot express.
Both webhooks evaluate `Agent` at `spec.*` and `AgentFleet` at `spec.template.*`
— and additionally an `AgentFleet`'s `spec.coordinator.template.*` — so a fleet's
worker or coordinator template is held to exactly the same bar as a standalone
agent.

### Validating webhook

- **Image-registry allow-list.** If configured, `spec.image` must be prefixed by
  an allowed registry, else the request is denied. The default is a non-empty
  list — `admission.allowedRegistries` ships as
  `agentd:,mock-agent,agentctl/,gcr.io/,registry.k8s.io/,ghcr.io/` — so the
  allow-list is **on by default**. Set it to your own registries in production; an
  empty value disables the check (allow any registry).
- **Lethal-trifecta opt-in gate.** An agent that declares all three of
  `capabilities.exec: true`, `capabilities.egress: true`, and a non-empty
  `capabilities.secrets` list requests the "lethal trifecta" and is **denied**
  unless it carries the annotation
  `agentctl.dev/allow-trifecta: "true"` (the value must be literally `"true"`).
  Any two of the three legs is permitted without the annotation.
- **ModelPool existence.** If `spec.model.pool` names a pool, it must exist in the
  same namespace. (A transient API-server error during the lookup fails open — the
  cross-object check is skipped rather than blocking an otherwise-valid admission.)
- **OIDC policy well-formedness.** If `spec.access.oidc` is present, `issuer` must
  be a non-empty `https://` URL, `audiences` must list at least one non-empty
  value, and any `jwksUri` override must be `https://`. This rejects a
  gate-widening typo (for example an empty `audiences`, which would otherwise
  accept any `aud`) at admission rather than failing opaquely at request time.

### Mutating webhook (secure defaults)

The mutating webhook returns a JSON Patch of secure defaults, each conditional on
the field being absent so it never clobbers an author's explicit value:

- standard `app.kubernetes.io/*` labels (`managed-by`, `part-of`, `name`);
- `mode` ⇒ `once` (the conservative run-once shape);
- `surfaces` ⇒ all-`false` (`management`, `metrics`, `a2a`) — minimal exposure; the
  network-exposed `a2a` surface never defaults on.

## Token budgets

With no gateway on the inference path, token budgets are **harness-tracked** — the
agent counts its own usage and stops itself. There is no pool-wide or per-fleet
gateway budget, and no `429`-on-budget.

- **Per-instance lifetime budget** — `spec.limits.lifetimeTokens` caps cumulative
  consumption across every run/reaction of one instance (RFC 0025). On a bounded
  `once` run exhaustion folds into `EXIT_BUDGET(7)`; a `reactive`/`loop`/`schedule`
  daemon stops accepting new reactions and drains cleanly. Each instance (fleet
  member included) carries its own lifetime box.
- **Per-run bound** — `spec.limits.maxTokens` bounds a single run/reaction; the
  companion `maxDepth`/`maxSteps` bound recursion and tool-call loops.

The operator renders these as the agent's `--budget-tokens-lifetime` / `--max-tokens`
flags; enforcement and the exit-code/gauge behaviour are the agent's, per the Agent
Control Contract. A key-authenticated provider may of course also rate-limit or bill
on its own — that is outside agentctl.

## TLS and PKI (cert-manager)

**cert-manager is a required prerequisite** — it issues all control-plane TLS.
The chart bootstraps a self-signed issuer, mints a cluster CA, and exposes that CA
as an issuer for every leaf:

```
agentctl-selfsigned (ClusterIssuer, self-signed)
        │
        ▼
agentctl-ca (Certificate, isCA) ──► agentctl-ca (ClusterIssuer)
        │
        ├─ agentctl-apiserver-tls          aggregated apiserver serving
        ├─ agentctl-admission-tls          admission webhook serving
        ├─ agentctl-client-tls             mTLS CLIENT cert (CN agentctl-control-plane)
        │                                    = the Management origin into agents
        └─ <workload>-serving-tls          PER-AGENT serving cert (operator-issued)
```

If you already run a cluster CA, set `certManager.caIssuerRef` and the chart skips
the self-signed bootstrap and issues every leaf (including the per-workload agent
serving certs) from your issuer.

### Per-workload agent identity

For each reconciled `Agent`/`AgentFleet` the operator ensures, in the workload's
namespace:

- a cert-manager `Certificate` minting the workload's serving identity into the
  Secret the pod mounts (`<workload>-serving-tls`, keys `tls.crt` / `tls.key`).
  The SANs cover the (headless) Service name (`<workload>.<ns>.svc` and its
  `.cluster.local` form) **and** the per-pod DNS form `*.<ns>.pod.cluster.local`
  so the control plane can address and verify a single replica. Keys are ECDSA
  P-256; the cert has a 90-day lifetime (`duration: 2160h`) and renews 30 days
  early (`renewBefore: 720h`), reloaded in place. The Certificate is owner-ref'd
  to the CR so garbage collection reclaims it.
- the `agentctl-ca` ConfigMap (key `ca.crt`) carrying the cluster CA **public**
  certificate. The pod mounts it as its client-CA (to authenticate Management
  callers) and as its trust anchor for the in-cluster control-plane hops it dials
  (the coordination server, and the A2A gateway for A2A delegation-out). It is
  namespace-shared and deliberately un-owned, so deleting one agent never removes
  the namespace's trust anchor. Model providers and remote MCP servers, dialed
  directly over the internet, are verified against the public web PKI, not this CA.

All leaves are pure-Rust rustls/`ring` (no OpenSSL/aws-lc). Public OIDC issuers,
by contrast, are reached over the internet using the bundled Mozilla trust anchors
— never the internal control-plane CA.

## Management RBAC (SubjectAccessReview)

Management verbs (drain, lame-duck, cancel, pause, resume, and status) are served
by the aggregated apiserver under `management.agentctl.dev`. Every request is
**doubly authorized**:

1. **Kubernetes RBAC** admits the caller to the aggregated APIService (the
   kube-apiserver front-proxy authenticates the aggregator via the
   requestheader-CA and asserts the user via `X-Remote-User`/`-Group`).
2. **A SubjectAccessReview per verb** — the apiserver asks the Kubernetes
   authorizer whether *this* subject may perform *this* management verb on *this*
   agent, and denies (403) on a negative. A caller with no RBAC binding (for
   example `kubectl ... --as=nobody`) is refused.

Only after both pass does the apiserver dial the target pod(s) under the Management
client certificate. Fleet verbs fan out to all replicas.

## Optional inbound authentication gates

For A2A ingress the gateway supports three inbound-auth mechanisms, evaluated by
the A2A RPC handler in a fixed precedence. All are off by default (the in-cluster
default is an unauthenticated A2A surface; front it with an Ingress/LoadBalancer
for transport TLS). Probes, `/metrics`, and the public JWKS
(`/.well-known/jwks.json`, the Agent Card verification key) are never gated.

Precedence for a call to `POST /agents/{ns}/{name}`:

1. **Trusted front-proxy identity** (verified mTLS listener) — highest precedence;
2. else **per-agent OIDC** when the target's `spec.access.oidc` is set;
3. else the **coarse bearer token** (`AGENTCTL_API_TOKEN`).

Reading the access policy fails **closed**: a hard error fetching the CR returns
502 rather than admitting the call.

### Coarse bearer token (`AGENTCTL_API_TOKEN`)

An optional shared in-cluster gate (`apiToken.enabled`, default off). When set,
the coordination server, A2A gateway, and scaler require
`Authorization: Bearer <token>`; the compare is constant-time. The chart mints a
lookup-stable Secret (`agentctl-api-token`) kept across upgrades, wires it into
those services, and the operator injects the same `secretKeyRef` into agent pods
in the control-plane namespace. This is a coarse gate — one token, no per-caller
identity — not a substitute for the attested identity above. (A `secretKeyRef`
cannot cross namespaces, so agents outside the control-plane namespace need the
Secret replicated into their namespace; see [operations.md](operations.md).)

### Per-agent OIDC (native JWT verification)

An `Agent` (or `AgentFleet` template) can declare a per-agent OIDC policy in
`spec.access.oidc`. The A2A gateway then verifies the caller's
`Authorization: Bearer <JWT>` **for that specific agent**:

```yaml
spec:
  surfaces:
    a2a: true
  access:
    oidc:
      issuer: https://login.acme.example        # required, https://
      audiences: [support-bot]                  # required, non-empty
      jwksUri: https://login.acme.example/keys  # optional; else OIDC discovery
      requiredClaims:                            # AND across entries…
        - claim: groups
          anyOf: [support]                       # …OR within one entry
      forwardIdentity: true                      # forward identity to the agent
```

Enforcement:

- **JWKS discovery + caching.** The key set is discovered from `jwksUri`, else via
  the issuer's `…/.well-known/openid-configuration`, and cached per issuer with a
  300s TTL; a `kid` miss inside a fresh cache forces one refresh (key rotation).
- **Verification.** The signature is checked against the matching JWK; the
  algorithm is pinned to the **key's** family (not the token header) to block
  `alg`-confusion attacks. `iss` must equal `issuer`, `aud` must intersect
  `audiences`, and `exp`/`nbf` are validated with a 60s leeway.
- **Authorization.** Every `requiredClaims` entry must be satisfied (logical AND);
  within an entry the caller's claim matches by array-contains or scalar-equals
  against `anyOf`. An empty `anyOf` fails closed.
- **Result.** Authentication failures return **401**, authorization failures
  **403**; the response body never leaks token detail (the reason is logged
  server-side). On success, with `forwardIdentity: true`, the verified identity is
  forwarded to the agent as `X-Auth-Subject` / `X-Auth-Email` / `X-Auth-Groups`
  (client-supplied `X-Auth-*` headers are not propagated, so the agent can trust
  these to be gateway-verified). The agent never sees or verifies the raw token.

This is native to the CR — no service mesh, sidecar, or external gateway policy is
required to gate traffic.

### Trusted front-proxy (external API gateway)

When edge auth is terminated at a fronting API gateway (APISIX, Kong, Envoy) that
asserts the authenticated identity as headers, agentctl must trust **the channel,
not headers from anyone** — otherwise any in-cluster pod could forge an identity
header. Trusted-proxy mode (`trustedProxy.enabled`, default off) mirrors the
Kubernetes aggregated-apiserver front-proxy pattern:

1. **Authenticate the proxy over mTLS.** The gateway opens a second listener on
   `:8443` that **requires** a client certificate chained to the trusted-proxy CA
   (`trustedProxy` CA). After the chain verifies, the peer cert's CN/SAN must be in
   the `trustedProxy.allowedNames` allow-list, else **403**. An empty allow-list
   fails closed.
2. **Trust the asserted identity headers only over that channel.** On the verified
   mTLS listener the gateway reads the proxy-asserted identity from
   `<prefix>-subject` / `<prefix>-email` / `<prefix>-groups`
   (`trustedProxy.headerPrefix`, default `x-agentctl`; individual names overridable
   via `trustedProxy.identityHeaders`).
3. **Authorize, then forward.** The gateway runs the target agent's
   `spec.access.oidc.requiredClaims` against the asserted identity, denies on
   mismatch, and forwards the identity to the agent (as the same `X-Auth-*`
   headers as the OIDC path).

**Anti-spoof is symmetric.** On the plaintext `:8080` listener — and on any
non-trusted path — the gateway **strips** the identity headers (the configured
`<prefix>-*` names plus the legacy `X-Forwarded-User`/`-Email`/`-Groups`) before
handling, so a caller that did not arrive over the authenticated mTLS channel can
never assert an identity.

The trusted-proxy and native-OIDC paths are two front-ends to the **same**
`requiredClaims` authorization and identity-forwarding core — choose where the JWT
is verified: at the gateway (native OIDC) or at the edge proxy (trusted-proxy).
The chart issues the proxy's client cert (`agentctl-trusted-proxy-client-tls`,
default CN `apisix`) off the agentctl CA; its CN/SAN must be on `allowedNames`.

## Operator-hardening checklist

- [ ] **cert-manager installed** and healthy (required — all control-plane TLS).
- [ ] **Prefer AAuth for untrusted workloads** so no provider/tool key is mounted
  on the pod; a `credentialSecretRef`/`staticToken` binding puts that one key on
  the agent (there is no off-pod broker).
- [ ] **Keep work-fabric attestation on** (`coordination.attestIdentity: true`);
  disable only for a trusted single-tenant install.
- [ ] **Enable NetworkPolicies** (`networkPolicies.enabled: true`) **and** confirm
  a policy-enforcing CNI (Calico/Cilium); list tenant namespaces in
  `networkPolicies.agentNamespaces` (dynamic namespaces are also covered by the
  operator).
- [ ] **Tighten the registry allow-list** (`admission.allowedRegistries`) to your
  own registries.
- [ ] **Authenticate A2A ingress** for internet exposure: per-agent OIDC
  (`spec.access.oidc`) or trusted-proxy mode, fronted by an Ingress/LB for TLS.
- [ ] Consider the coarse `apiToken.enabled` gate for in-cluster data-plane paths,
  and the opt-in `coordination.mtls` / Postgres `verifyFull` hardening — see
  [operations.md](operations.md).
