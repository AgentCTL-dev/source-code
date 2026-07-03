# agentctl — security & auth model

How every endpoint is authenticated/authorized, the certificate fabric, RBAC, and
the hardening posture.

> ⚠️ **Contract 2.0 — identity is the boundary.** In v2 the agent **serves its
> control surface over mTLS HTTPS** (`POST /mcp`) and **dials the gateways keyless**;
> the **node-agent is retired**, so there is no host socket and no `SO_PEERCRED`
> attestation on the management path. Authority is now **cryptographic**: a verified
> mTLS **client cert** into agents (⇒ `Management`), and an **attested source IP**
> into the gateways. See **[RFC 0021](../rfcs/0021-contract-2.0-network-substrate-pivot.md)**.
> The OIDC, trusted-proxy, coordination-attestation, and supply-chain sections below
> are unchanged by the pivot.

The guiding shape: the **management/control path is strongly authenticated** (direct
mTLS client-cert identity + SubjectAccessReview); the **data-plane utility paths**
(intelligence, tools, coordination, A2A-inbound) attest the caller by **source IP**
and are **network-isolated** (NetworkPolicy + cluster boundary).

## Endpoint auth map

| Endpoint (port) | Transport | AuthN — who is verified | AuthZ | Callers |
|---|---|---|---|---|
| **Aggregated APIServer** :6443 (`drain`/`status`) | HTTPS, front-proxy mTLS | aggregator client cert (requestheader CA from `kube-system`); identity via `X-Remote-User/Group` | **SubjectAccessReview** per verb (kube RBAC) | kube-apiserver on behalf of an RBAC'd user (`--as=nobody`→403) |
| **agent self-MCP** :8443 (`/mcp`) | HTTPS, **mTLS** | CA-signed **client cert** (`agentctl-client-tls`) verified vs the pinned client CA ⇒ `PeerOrigin::Management` | operator/A2A methods gated on `Management`; a non-Management caller ⇒ `-32601` | APIServer (a2a.* admin verbs) + A2A gateway (bare A2A methods), **direct to the pod** |
| **ModelGateway** :8443 (TLS) / :8080 | HTTPS (server-auth; agent dials keyless) | **source-IP attested** (kube pod lookup on `status.podIP`, cold-start retry) — the default v2 identity, no header trusted; optional **`AGENTCTL_API_TOKEN`** bearer gate | ModelPool existence + budget | agents (keyless, NetworkPolicy-scoped) |
| **MCPGateway** :8443 (TLS) / :8080 | HTTPS (server-auth; agent dials keyless) | **source-IP attested** (kube pod lookup) → scoped to the agent's bound `MCPServerSet` | per-server credential injected off-pod + budget | agents (keyless) |
| **A2A gateway** :8080 (plaintext) + **:8443 trusted-proxy mTLS** (opt-in) | HTTP/SSE (+ optional Bearer); **trusted-proxy mTLS** when `trustedProxy.enabled` | optional **`AGENTCTL_API_TOKEN`** bearer gate (cards are JWS-signed for the *caller* to verify); per-agent **OIDC** (JWKS JWT); **trusted-proxy** — front-proxy client cert verified vs the agentctl CA + `allowedNames`, then asserted `<prefix>-subject/-email/-groups` trusted (prefix `trustedProxy.headerPrefix`, default `x-agentctl`; stripped from untrusted plaintext callers) | per-agent **`requiredClaims`** (OIDC native or trusted-proxy pass-through) | A2A clients / peer agents / fronting API gateway (APISIX) |
| **coordination server** :8080 (`work.*`) + **:8443 scaler mTLS** (opt-in) | HTTP (+ optional Bearer); **mTLS** on :8443 when `coordination.mtls.enabled` | optional **`AGENTCTL_API_TOKEN`** bearer gate; optional **source-IP attested** (`coordination.attestIdentity` — kube pod lookup, anti-spoof) binding claim ownership; **scaler mTLS** — scaler client cert verified vs the agentctl CA + `coordination.mtls.allowedNames` on the :8443 listener | claim ownership **bound to the attested identity** (blocks cross-tenant ack/release) when `attestIdentity` | producers + agents + scaler |
| **scaler** gRPC :9100 | gRPC | none (in-cluster, KEDA-only); reads the coordination backlog over **mTLS** (CA-signed client cert) when `coordination.mtls.enabled` | — | KEDA |
| **admission webhook** :8443 | HTTPS, `with_no_client_auth` | kube-apiserver trusted (caBundle lets *it* verify the webhook) | the gate logic | kube-apiserver |
| **`/healthz` `/readyz` `/metrics`** :8080 | plaintext | none (intentional) | — | probes + Prometheus |
| agent → model provider | via ModelGateway | the real key is **injected by the gateway** | provider-side | only the ModelGateway egresses |

The agent is **secretless** — no provider key (the ModelGateway holds it), no MCP
credential (the MCPGateway holds it), no bearer on the pod (management is mTLS-only).
The only material on the pod is its rotatable serving key + the public CA bundles.

## Certificate fabric (cert-manager)

```
SelfSigned Issuer → agentctl-ca ClusterIssuer →
  agentctl-apiserver-tls   (APIServer serving)   → caBundle injected into the APIService
  agentctl-admission-tls   (webhook serving)     → caBundle injected into the webhooks
  agentctl-modelgateway-tls / -mcpgateway-tls / -gateway serving certs (agent-facing TLS)
  <name>-serving-tls       (PER-WORKLOAD agent serving cert + ca.crt to verify Management clients)
  agentctl-ca ConfigMap    (per-namespace PUBLIC CA cert → agents trust the gateways via --tls-ca)
  agentctl-client-tls      (mTLS CLIENT cert: apiserver + A2A gateway → agent /mcp, mints Management)
  agentctl-trusted-proxy-tls         (A2A gateway trusted-proxy mTLS server + ca.crt to verify the front-proxy — `trustedProxy.enabled`)
  agentctl-trusted-proxy-client-tls  (mTLS client cert for the front-proxy/APISIX → A2A gateway — `trustedProxy.enabled`)
  agentctl-coordination-mtls-tls     (coordination mTLS server on :8443 + ca.crt to verify the scaler — `coordination.mtls.enabled`)
  agentctl-scaler-client-tls         (mTLS client cert for the scaler → coordination :8443 — `coordination.mtls.enabled`)
```
The **front-proxy CA** is read at runtime from `kube-system/extension-apiserver-authentication`
(not cert-manager) — it's what authenticates the aggregator. All leaves are rustls/`ring`
(no OpenSSL/aws-lc), auto-rotated (`renewBefore: 720h`).

## RBAC — least-privilege per ServiceAccount

| Component | Key grants |
|---|---|
| operator | `agents/agentfleets` get/list/watch/update/patch + `/status` + `/finalizers`; `mcpserversets` get/list/watch; `apps` deploy/sts + `batch` jobs CRUD; **cert-manager `certificates` CRUD + `configmaps`** (per-workload PKI + per-namespace CA distribution); `events`; `coordination.k8s.io/leases`; `keda.sh/scaledobjects` CRUD |
| apiserver | `system:auth-delegator` (SAR/TokenReview) + `extension-apiserver-authentication-reader` (kube-system); `pods` get/list (resolve Agent → `status.podIP` for the direct-mTLS dial) |
| gateway | `pods` + `agents/agentfleets` get/list (resolve target → pod IP; forward direct to the pod `/mcp`) |
| modelgateway | `modelpools` get/list/watch + `/status`; `secrets` get/list — **scoped** to `secretsNamespaces`; `pods` get/list (source-IP identity attestation) |
| mcpgateway | `mcpserversets` get/list/watch; `secrets` get/list (staticToken creds, scoped); `pods` get/list (source-IP attestation) |
| admission | `modelpools` get/list |
| coordination | none by default; `pods` get/list (source-IP claim-ownership attestation, `coordination.attestIdentity`) when enabled |
| scaler | none / minimal (KEDA-only; reads the coordination backlog) |

The management path is **doubly authorized**: kube RBAC (reach the APIService) *then* the
agentctl apiserver's own SAR per verb; the verb then dials the agent under a client cert
the agent verifies against the pinned client CA.

## Admission-time gate (Agent + AgentFleet, incl. `spec.template`)

- **Validating** — denies the lethal trifecta (`exec && egress && secrets`) without
  `agentctl.dev/allow-trifecta:"true"`; enforces an image-registry allow-list; checks
  cross-object `modelPool` existence.
- **Mutating** — defaults `app.kubernetes.io/*` labels + `mode` + minimal `surfaces`
  (does **not** hard-default `substrate`).

## Pod / workload security

- Confined securityContext on every control-plane pod **and** operator-rendered tenant
  agent pods: `runAsNonRoot`, drop-`ALL`-caps, `seccomp:RuntimeDefault`,
  `allowPrivilegeEscalation:false`, `readOnlyRootFilesystem`, and
  `automountServiceAccountToken:false`. Contract 2.0 removed the host socket, so the
  agent pod needs **no** `hostPath`/`hostPID`/privilege and holds **zero credentials**
  (only its rotatable serving key + public CA bundles).
- **No privileged component.** With the node-agent retired, nothing in the control plane
  needs `hostPath`/`hostPID`/privileged.
- PodSecurity: `agentctl-system` and tenant agent namespaces run at **`baseline`**
  (the node-agent that forced `privileged` is gone); everything self-confines to
  `restricted`-equivalent.
- NetworkPolicies (opt-in): default-deny + narrow allows; Postgres ingress-only from
  gateway/modelgateway; agent egress to DNS + the gateways only (needs a policy CNI).

## Secrets — who reads what

| Secret | Reader |
|---|---|
| cert-manager TLS leaves | the owning component |
| **provider credentials** (ModelPool) | **only the ModelGateway** (scoped RBAC) — never the agent |
| gateway JWS signing seed | only the gateway |
| `agentctl-api-token` (bearer gate) | the gated services + injected into agent pods |
| Postgres creds | gateway + modelgateway + postgres |

## API token (in-cluster auth gate)

The data-plane utility paths (coordination `work.*`, ModelGateway `/v1/infer`, A2A
gateway ingress) are network-isolated but **open by default** — any pod that can
reach them may call them. The optional **`AGENTCTL_API_TOKEN`** bearer gate closes
"any in-cluster pod can call these" without standing up per-client identity.

- **Enable:** `helm upgrade --set apiToken.enabled=true` (default **off** — disabled
  installs are unchanged; the services run open). The chart then creates a
  lookup-stable Secret **`agentctl-api-token`** (key `AGENTCTL_API_TOKEN`,
  `helm.sh/resource-policy: keep`, a random 40-char token kept across upgrades;
  override with `apiToken.value`).
- **What the chart wires:** the coordination server, ModelGateway, A2A gateway, and
  scaler each get `AGENTCTL_API_TOKEN` from that Secret via `secretKeyRef`. The
  gated services then require `Authorization: Bearer <token>`; the scaler presents
  it when reading the coordination backlog.
- **Callers must send it:** producers and external A2A clients must add
  `Authorization: Bearer <token>` (read it from the Secret:
  `kubectl -n agentctl-system get secret agentctl-api-token -o jsonpath='{.data.AGENTCTL_API_TOKEN}' | base64 -d`).
- **Agent injection (operator):** when `apiToken.enabled`, the operator
  (`API_TOKEN_ENABLED`) injects the same `secretKeyRef` into rendered agent pods so
  a conformant agent presents the token automatically. **No extra RBAC** — the
  kubelet resolves the `secretKeyRef` at pod start, not the operator.
- **Cross-namespace limitation (honest):** a `secretKeyRef` resolves **only within
  the pod's own namespace**, and the Secret lives in the control-plane namespace
  (`agentctl-system`). So the operator injects **only for agents in the
  control-plane namespace**. Agents in other namespaces are **not** injected (that
  would yield a pod that cannot start); replicate `agentctl-api-token` into the
  agent's namespace and wire it there. This is a coarse v1 access gate, not per-pod
  identity — see the Hardening checklist for the attested-identity follow-ups.

## OIDC per agent/fleet (caller identity)

The shared `AGENTCTL_API_TOKEN` above is a coarse, in-cluster gate (one token,
no per-caller identity). For **internet-exposed A2A ingress** an `Agent` (or an
`AgentFleet`, via `spec.template`) can instead declare a **per-agent OIDC policy**
in the CR. The A2A gateway turns it into a real authn+authz gate: it verifies the
caller's JWT against the agent's **own issuer JWKS**, enforces **required claims**,
and forwards the caller identity to the agent.

This is **native**: the `Agent` CR is the single source of truth — no extra
infrastructure (no service mesh, no external API-gateway policy CRDs, no sidecar)
is required to gate traffic. Because the gateway terminates plain HTTP, you still
**front it with an Ingress/LoadBalancer for TLS** (and to expose it to the
internet); OIDC is the per-caller identity layer on top of that transport.

### The `spec.access.oidc` block

```yaml
spec:
  access:
    public: false                       # doc-only intent flag (v1)
    oidc:
      issuer: https://idp.example.com    # required, https:// — JWKS discovered from
                                         #   <issuer>/.well-known/openid-configuration
      audiences: [agentctl-a2a]          # required, non-empty — accepted `aud` claims
      jwksUri: https://idp.example.com/keys   # optional https:// override (skips discovery)
      requiredClaims:                    # authz: ALL requirements must hold (AND of claims)
        - claim: groups                  #   a claim's value (array-contains OR scalar-equals)
          anyOf: [support]               #   must match one of `anyOf` (OR within a claim)
      forwardIdentity: true              # inject caller sub/email/groups to the agent
```

### How the gateway enforces it

1. **JWKS-verified JWT** — the caller presents `Authorization: Bearer <jwt>`. The
   gateway fetches/caches the issuer's JWKS (from `jwksUri`, else OIDC discovery
   off `issuer`), verifies the signature, and checks `iss` + `exp`/`nbf` + that
   `aud` is one of `audiences`.
2. **Required-claims authz** — every entry in `requiredClaims` must be satisfied;
   each is an OR over `anyOf` (array claims match by contains, scalar claims by
   equals). All-of across entries, any-of within one.
3. **Identity forwarding** — with `forwardIdentity: true` the gateway passes the
   verified `sub`/`email`/`groups` to the agent so the workload can do its own
   fine-grained decisions. The agent never sees or verifies the raw token.

**Admission-validated.** The webhook (Agent `spec.access.oidc` and AgentFleet
`spec.template.access.oidc`) rejects a malformed gate up front: `issuer` must be a
non-empty `https://` URL, `audiences` must be non-empty, and any `jwksUri` must be
`https://` — so a typo can't silently widen the gate (e.g. an empty `audiences`
that would accept any `aud`) or downgrade JWKS to MITM-able plaintext.

### Worked example — an Agent served only to group "support"

```yaml
apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata:
  name: support-bot
  namespace: support
spec:
  image: ghcr.io/acme/support-bot:v1
  surfaces:
    a2a: true                            # expose the A2A surface
  access:
    oidc:
      issuer: https://login.acme.example
      audiences: [support-bot]
      requiredClaims:
        - claim: groups
          anyOf: [support]               # only callers whose `groups` include "support"
      forwardIdentity: true
```

Front the gateway with an Ingress/LB for TLS; callers from your IdP that present a
JWT for `aud: support-bot` whose `groups` include `support` are admitted, and the
agent receives their identity. Everyone else is rejected at the gateway.

**Future option (documented, not v1 default):** exporting enforcement to an
external API-gateway/service-mesh (e.g. emitting equivalent mesh `AuthorizationPolicy`
/ ingress JWT config from the same CR) is a planned alternative for orgs that
standardize identity at the mesh edge. The native per-agent gate above is the v1
path and needs none of that.

## Trusted front-proxy (external API gateway)

The OIDC gate above makes the A2A **gateway itself** the policy decision point (it verifies
the JWT). Many orgs instead standardize edge auth at a **fronting API gateway** (APISIX,
Kong, Envoy, …) that terminates OIDC/SAML/mTLS at the internet edge and then asserts the
authenticated identity to upstreams as headers (`X-Forwarded-User`/`-Email`/`-Groups`).

**The problem:** those headers are only trustworthy if the upstream trusts the *channel*,
not anyone who can set them — any in-cluster pod that reaches the gateway's plaintext
`:8080` could forge `X-Forwarded-User: admin`. So agentctl must trust **the channel, not
headers from anyone**.

The **trusted front-proxy** mode (`trustedProxy.enabled`, default **off**) closes that with
the same pattern the Kubernetes **aggregated-apiserver front-proxy** uses (requestheader-CA +
`X-Remote-User/Group`): authenticate the *proxy* over an mTLS channel, and only then trust
the identity headers it asserts.

### The 3-step model

1. **Authenticate the proxy — mTLS client cert + allowed-names.** The gateway serves a
   trusted-proxy mTLS listener (`:8443`): it verifies the front-proxy's **client cert against
   the agentctl CA** and checks the cert's subject/SAN against an **allowed-client-names**
   list (`trustedProxy.allowedNames`). Only a connection presenting a CA-signed cert
   whose name is allow-listed is a *trusted channel*. The CA + name allow-list are exactly
   what make the asserted identity headers trustworthy — mirroring the aggregated-apiserver
   requestheader-CA.
2. **Trust the asserted identity headers.** Over a trusted channel the gateway reads the
   proxy-asserted identity from `<prefix>-subject` → `sub`, `<prefix>-email`,
   `<prefix>-groups`. The **prefix is configurable** via `trustedProxy.headerPrefix`
   (default `x-agentctl` → `x-agentctl-subject`/`-email`/`-groups`); individual names can be
   overridden with `trustedProxy.identityHeaders` (e.g. a proxy that emits `X-Forwarded-User`).
   This is exactly how the apiserver trusts `X-Remote-User/Group` from the aggregator.
3. **Enforce `requiredClaims` + authorize, then forward.** The gateway runs the target
   agent's `spec.access.oidc.requiredClaims` against the asserted identity (e.g. `groups`
   must contain `support`), denies on mismatch, and **forwards the identity to the agent**
   (the same `X-Forwarded-*` the OIDC pass-through path forwards). The agent does its own
   fine-grained checks; it never sees or verifies a raw token.

### Anti-spoof: strip on untrusted callers

The protection is symmetric. On **any plaintext / non-trusted-proxy** path the gateway
**strips** the inbound identity headers (the configured `<prefix>-subject/-email/-groups`,
plus the legacy `X-Forwarded-*` set as belt-and-suspenders) before processing, so a caller
that did *not* arrive over the authenticated mTLS channel can never assert an identity — the
headers are honored **only** over the trusted-proxy channel. This is the direct analogue of
the apiserver discarding `X-Remote-*` from anyone but the front-proxy.

### How it composes

- **Mirrors the aggregated-apiserver front-proxy** (requestheader-CA + `X-Remote-*`): the
  same trust-the-channel-not-the-header shape, reused for the data-plane A2A edge. See the
  management-APIServer row in the auth map.
- **Composes with per-agent OIDC (pass-through).** When the front-proxy terminates OIDC,
  agentctl runs in *pass-through*: the proxy verified the JWT, agentctl trusts the asserted
  identity over mTLS and still enforces the agent's `requiredClaims`. The native OIDC gate
  (gateway verifies the JWT itself) and the trusted-proxy gate are two front-ends to the
  **same** `requiredClaims` authz + identity-forwarding core — pick where the JWT is verified
  (at the gateway, or at the edge proxy).
- **Composes with `apiToken` (coarse).** The shared `AGENTCTL_API_TOKEN` stays a coarse
  in-cluster gate; the trusted-proxy channel is the per-caller identity layer on top of it.

### Worked APISIX config sketch

Edge OIDC termination + upstream mTLS with a client cert minted from the agentctl CA +
`proxy-rewrite` asserting the `X-Forwarded-*` identity:

```yaml
# APISIX route → agentctl A2A gateway (trusted-proxy mTLS listener)
routes:
  - uri: /a2a/*
    plugins:
      openid-connect:                         # edge auth: terminate OIDC at the proxy
        client_id: agentctl-a2a
        discovery: https://login.acme.example/.well-known/openid-configuration
        bearer_only: true
      proxy-rewrite:                          # assert the verified identity upstream
        headers:                              # default prefix x-agentctl (trustedProxy.headerPrefix)
          set:
            x-agentctl-subject: "$http_x_userinfo_sub"
            x-agentctl-email:   "$http_x_userinfo_email"
            x-agentctl-groups:  "$http_x_userinfo_groups"
    upstream:
      scheme: https                           # mTLS to the gateway trusted-proxy listener
      nodes: { "agentctl-gateway.agentctl-system.svc:8443": 1 }
      tls:
        client_cert: |                        # APISIX client cert (from the agentctl CA)
          -----BEGIN CERTIFICATE----- ... -----END CERTIFICATE-----
        client_key: |
          -----BEGIN PRIVATE KEY----- ... -----END PRIVATE KEY-----
        # verify the gateway serving cert against the agentctl CA (ca.crt from the secret)
```

The APISIX client cert/key come from the chart-issued **`agentctl-trusted-proxy-client-tls`**
Secret (retrieval in operations.md §8). Its subject/SAN **must** be on
`trustedProxy.allowedNames`, or the gateway rejects the channel and strips the headers.

**Pass-through alternative.** If you prefer agentctl to verify the JWT itself (native OIDC,
above) while still fronting with APISIX for TLS/routing, **drop** the `openid-connect` +
`proxy-rewrite` plugins and simply proxy the caller's `Authorization: Bearer <jwt>` through;
the gateway's `spec.access.oidc` gate verifies it against the issuer JWKS. Use **trusted-proxy
mode** when edge auth is terminated at APISIX; use **native OIDC pass-through** when you want
the gateway to be the verification point.

## Attested agent identity (ModelGateway)

By default the ModelGateway trusts the `X-Agent-Namespace`/`X-Agent-Name` headers the
caller asserts to pick the ModelPool, meter tokens, and enforce the budget — any pod
that can reach `:8080` could set those headers and bill/borrow another tenant's pool.

Enabling **`modelgateway.attestIdentity`** (default **off**) replaces that trust with a
**source-IP attestation**: the gateway reads the connection's source IP and resolves it
to the calling pod via a kube `pods` lookup (matching `status.podIP`), deriving the agent
**namespace** from the real pod rather than the header. The header becomes advisory.

- **Why it is robust:** confined tenant pods run with `drop: ["ALL"]` capabilities, so
  they have **no `CAP_NET_RAW`** and cannot craft raw packets to spoof a source IP. The
  kernel-attributed source IP is therefore a trustworthy identity for the default
  (networked) tier — a tenant cannot impersonate another namespace's pool or budget.
- **RBAC:** this needs cluster-wide `pods` get/list, granted unconditionally in the
  modelgateway ClusterRole (harmless when the toggle is off, and it keeps the role stable
  across the flag).
- **Enable:** `helm upgrade --set modelgateway.attestIdentity=true` — the chart then sets
  `IDENTITY_ATTEST=true` on the Deployment. The same source-IP attestation guards the
  **MCPGateway** (tool plane).

### One attestation model — no networkless tier (contract 2.0)

Contract 2.0 makes **every** agent network-native: it dials the gateways over TLS from a
**routable pod IP**, so source-IP attestation covers all agents uniformly. The v1
"networkless (Kata) tier" — an agent with no NIC whose infer traffic had to be routed
through a node-agent unix-socket forwarder with `SO_PEERCRED` re-stamping — **no longer
exists**; the node-agent, the routed-infer path, and the `X-Agent-Pod-Uid` forwarder are
all **retired**. Kata *tenancy hardening* still applies (a pod may be a Kata VM), but it is
reached over the network with mTLS like any other pod.

The one remaining wrinkle is a **cold-start race**: an agent may issue its first dial
before its `status.podIP` has propagated into the gateway's pods watch-cache, so the source
IP briefly resolves to nothing. The gateways handle it with a **bounded retry** (3× / 500 ms)
before failing closed — a startup-timing robustness fix, not a trust relaxation.

## Attested claim ownership (coordination)

By default the claim-mode **coordination server** authenticates callers only with the
optional `AGENTCTL_API_TOKEN` bearer (a coarse, shared in-cluster gate) and trusts the
claim metadata a caller asserts — so any in-cluster pod holding the token could ack or
release another tenant's work claim.

Enabling **`coordination.attestIdentity`** (default **off**) binds each claim's lifecycle to
a **source-IP attestation**: the server reads the connection's source IP and resolves it to
the calling pod via a kube `pods` lookup (matching `status.podIP`), then binds claim ownership
to that attested identity. Ack/release is authorized against the attested owner, so a tenant
**cannot ack or release another tenant's claim** (cross-tenant claim tampering blocked).

- **Why it is robust:** confined tenant pods run with `drop: ["ALL"]` capabilities — no
  `CAP_NET_RAW` — so they cannot spoof a source IP; the kernel-attributed source IP is a
  trustworthy identity (the same property the ModelGateway source-IP attestation relies on).
- **RBAC:** needs cluster-wide `pods` get/list. Unlike the ModelGateway (which grants it
  unconditionally), the coordination server holds **no cluster RBAC by default**, so the chart
  renders the `agentctl-coordination` ClusterRole + ClusterRoleBinding **only** when
  `attestIdentity` is on.
- **Enable:** `helm upgrade --set coordination.enabled=true --set coordination.attestIdentity=true`
  — the chart then sets `COORDINATION_ATTEST_IDENTITY=true` + `POD_NAMESPACE` (downward API
  `metadata.namespace`) on the Deployment (default off renders no env + no RBAC, so the code
  keeps the token-only path).

## Supply chain

cosign keyless (OIDC) signatures on every image **and** the chart by digest; SBOM +
provenance in the build; `cargo deny` in CI; admission image allow-list + chart digest
pinning close the loop at deploy.

## Hardening checklist (posture + what's resolved)

- [x] Front-proxy mTLS + SAR on the management APIServer.
- [x] **Direct mTLS client-cert identity** into the agent's `/mcp` (contract 2.0): the
  agent verifies the caller's cert against the pinned client CA ⇒ `Management`; a
  non-Management caller gets `-32601`. Replaces the retired node-agent + `SO_PEERCRED`
  socket path. **mTLS-only** — agentctl never renders a pod-resident bearer.
- [x] Confined, admission-gated, supply-chain-signed workloads; least-privilege RBAC;
  **zero credentials on the agent pod** and **no privileged component** (node-agent retired).
- [x] **Optional bearer-token (`AGENTCTL_API_TOKEN`) on the coordination server, ModelGateway,
  and A2A gateway** — closes "any in-cluster pod can call these" when enabled
  (`apiToken.enabled`); the scaler presents it; the operator injects it into agent pods.
- [x] **Attested ModelGateway identity** — source-IP attestation (`modelgateway.attestIdentity`)
  derives the agent namespace from the caller's source IP via a kube pod lookup, replacing the
  spoofable header-asserted `X-Agent-*` for the default (networked) tier. Robust because confined
  tenant pods drop `CAP_NET_RAW` and so cannot spoof their source IP. See "Attested agent
  identity (ModelGateway)" below.
- [x] **Tool-plane attestation (MCPGateway)** — agents dial MCP tools keyless; the MCPGateway
  attests the caller by source IP, scopes to the bound `MCPServerSet`, injects the per-server
  credential (held off-pod), meters budget, and forwards. Same source-IP identity model as the
  ModelGateway; the v1 stdio↔broker bridge is retired (native HTTPS MCP). See RFC 0021 §9.
- [x] **Authenticated A2A *ingress*** for internet exposure — **per-agent OIDC**
  (`spec.access.oidc`, admission-validated) gives the A2A gateway a JWKS-verified JWT +
  required-claims authz + identity forwarding, native to the CR (front it with an
  Ingress/LB for TLS). See "OIDC per agent/fleet". Exporting enforcement to an external
  API-gateway/mesh is a documented future option.
- [x] **Trusted front-proxy (external API gateway)** — `trustedProxy.enabled` (default off)
  lets a fronting gateway (e.g. APISIX) terminate edge auth and assert identity: the A2A
  gateway authenticates the *proxy* over **mTLS** (client cert vs the agentctl CA +
  `allowedNames`), then trusts the asserted `<prefix>-subject/-email/-groups` (prefix
  `trustedProxy.headerPrefix`, default `x-agentctl`; individual names overridable),
  **strips** those headers from untrusted plaintext callers (anti-spoof), enforces the
  agent's `requiredClaims`, and forwards the identity to the agent. Mirrors the
  aggregated-apiserver front-proxy (requestheader-CA + `X-Remote-*`); composes with per-agent
  OIDC (pass-through) and the `apiToken` gate. See "Trusted front-proxy (external API gateway)".
- [x] **Postgres `verify-full` (client CA pinning), opt-in** — `postgres.bundled.tls.verifyFull`
  (with `tls.enabled`) pins the chart CA: the chart mounts `ca.crt` at `/etc/agentctl-pg-ca` in
  the gateway + modelgateway (and the coordination server when `coordination.store=postgres`),
  sets `DB_CA_FILE`/`PGSSLROOTCERT`, and flips `DATABASE_URL` to `sslmode=verify-full` against the
  cert-SAN `.svc` host. Default off keeps `sslmode=require` (encrypt, no CA verify). See
  operations.md §1.
- [x] **Coordination HA/durability, opt-in** — `coordination.store=postgres` backs the claim queue
  with the durable Postgres store (shared across replicas), so `coordination.replicas` can be raised
  for HA. Default `store=memory` stays single-replica/in-process. **Load-tested** at 2 replicas:
  72 concurrent claims across 12 items → exactly 12 grants, zero double-grants (the atomic
  conditional-UPSERT grant-one holds under cross-replica contention); state also survives a pod
  restart (durability).
- [x] **NetworkPolicy enforcement** — `networkPolicies.enabled` ships a default-deny + narrow
  allow-list (control plane + per agent namespace). Enforcement requires a policy CNI (kindnet
  ignores NetworkPolicies); **verified under Calico**: control-plane default-deny holds,
  the Postgres ingress allow-list enforces by pod label, the ModelGateway/MCPGateway
  `namespaceSelector` restricts to the tenant namespace, and agent egress is limited to DNS +
  the gateway pods (cross-tenant + wrong-port + admission-webhook traffic dropped).
- [x] **Coordination stronger-than-token (attested) auth** — `coordination.attestIdentity`
  (default off) binds the **claim lifecycle to a source-IP-attested identity** (the server
  resolves the caller's source IP to the owning pod via a kube `pods` lookup), so the
  `AGENTCTL_API_TOKEN` bearer is no longer the only gate: a tenant **cannot ack/release another
  tenant's claim** (cross-tenant ack/release blocked). Robust because confined tenant pods drop
  `CAP_NET_RAW` and cannot spoof their source IP; needs cluster-wide `pods` get/list (rendered
  only when enabled). See "Attested claim ownership (coordination)".
- [x] **Scaler → coordination mTLS** — `coordination.mtls.enabled` (default off) opens a second
  coordination listener on **:8443** and has the **scaler** read the claim backlog over it with a
  CA-signed **client cert** (`agentctl-scaler-client-tls`), verified against the agentctl CA +
  `coordination.mtls.allowedNames`; the scaler verifies the coordination serving cert
  (`agentctl-coordination-mtls-tls`) against the same CA. The scaler dials the URL the
  **operator** renders into the ScaledObject (`scalerMetadata.coordinationUrl`), which the
  chart points at `https://agentctl-coordination.<ns>.svc.cluster.local.:8443` (trailing-dot
  FQDN) when `coordination.mtls.enabled`. Both leaves come off the agentctl
  CA Issuer (requires `certManager.enabled`). This closes the residual "scaler path is
  token / in-cluster-only" gap — the scaler hop is now mutually authenticated. Default off keeps
  the plaintext http + token path.
