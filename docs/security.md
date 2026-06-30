# agentctl — security & auth model

How every endpoint is authenticated/authorized, the certificate fabric, RBAC, and
the hardening posture. The guiding shape: the **management/control path is strongly
authenticated** (mTLS + kernel attestation + SubjectAccessReview); the **data-plane
utility paths** (intelligence, coordination, A2A-inbound) are **network-isolated**
(NetworkPolicy + cluster boundary), with per-call auth being added (see Hardening).

## Endpoint auth map

| Endpoint (port) | Transport | AuthN — who is verified | AuthZ | Callers |
|---|---|---|---|---|
| **Aggregated APIServer** :6443 (`drain`/`status`) | HTTPS, front-proxy mTLS | aggregator client cert (requestheader CA from `kube-system`); identity via `X-Remote-User/Group` | **SubjectAccessReview** per verb (kube RBAC) | kube-apiserver on behalf of an RBAC'd user (`--as=nobody`→403) |
| **node-agent control API** :8443 | HTTPS, **mTLS** (`WebPkiClientVerifier`) | CA-signed **client cert** (`agentctl-client-tls`) | + **`SO_PEERCRED` attestation**: 403 unless the attested pod-uid matches the requested `<uid>` | apiserver + gateway |
| **node-agent → agent** (unix socket) | unix socket (hostPath) | **`SO_PEERCRED`** → `/proc` cgroup → pod uid | file perms + attestation | node-agent (local) |
| **ModelGateway** :8080 (`/v1/infer`) | HTTP (+ optional Bearer) | identity header-asserted (`X-Agent-Namespace`/`-Name`), **source-IP attested** (`modelgateway.attestIdentity` — kube pod lookup, anti-spoof), **or pod-uid attested** via the node-agent forwarder (`X-Agent-Pod-Uid`, networkless/routed-infer tier — `SO_PEERCRED`); optional **`AGENTCTL_API_TOKEN`** bearer gate | ModelPool existence + budget | agents (NetworkPolicy-scoped) |
| **A2A gateway** :8080 | HTTP/SSE (+ optional Bearer) | optional **`AGENTCTL_API_TOKEN`** bearer gate (cards are JWS-signed for the *caller* to verify) | — | A2A clients / peer agents |
| **coordination server** :8080 (`work.*`) | HTTP (+ optional Bearer) | optional **`AGENTCTL_API_TOKEN`** bearer gate | — | producers + agents + scaler |
| **scaler** gRPC :9100 | gRPC | none (in-cluster, KEDA-only) | — | KEDA |
| **admission webhook** :8443 | HTTPS, `with_no_client_auth` | kube-apiserver trusted (caBundle lets *it* verify the webhook) | the gate logic | kube-apiserver |
| **`/healthz` `/readyz` `/metrics`** :8080 | plaintext | none (intentional) | — | probes + Prometheus |
| agent → model provider | via ModelGateway | the real key is **injected by the gateway** | provider-side | only the ModelGateway egresses |

The agent is **networkless + secretless** — no provider key (the ModelGateway holds
it), and on the hardened tier no NIC (outbound `work.*`/infer ride the substrate egress).

## Certificate fabric (cert-manager)

```
SelfSigned Issuer → agentctl CA → CA Issuer →
  agentctl-apiserver-tls   (APIServer serving)   → caBundle injected into the APIService
  agentctl-admission-tls   (webhook serving)     → caBundle injected into the webhooks
  agentctl-node-agent-tls  (node-agent mTLS server + ca.crt to verify clients)
  agentctl-client-tls      (mTLS client cert: apiserver + gateway → node-agent)
```
The **front-proxy CA** is read at runtime from `kube-system/extension-apiserver-authentication`
(not cert-manager) — it's what authenticates the aggregator. All leaves are rustls/`ring`
(no OpenSSL/aws-lc), auto-rotated (`renewBefore: 720h`).

## RBAC — least-privilege per ServiceAccount

| Component | Key grants |
|---|---|
| operator | `agents/agentfleets` get/list/watch/update/patch + `/status` + `/finalizers`; `apps` deploy/sts + `batch` jobs CRUD; `events` (core + `events.k8s.io`); `coordination.k8s.io/leases` (leader election); `keda.sh/scaledobjects` CRUD |
| apiserver | `system:auth-delegator` (SAR/TokenReview) + `extension-apiserver-authentication-reader` (kube-system); `pods` get/list |
| gateway | `pods` + `agents/agentfleets` get/list |
| modelgateway | `modelpools` get/list/watch + `/status`; `secrets` get/list — **scoped** to `secretsNamespaces`; `pods` get/list (source-IP identity attestation, `modelgateway.attestIdentity`) |
| admission | `modelpools` get/list |
| node-agent / coordination / scaler | none / minimal (no cluster reads — discovery is local hostPath) |

The management path is **doubly authorized**: kube RBAC (reach the APIService) *then* the
agentctl apiserver's own SAR per verb.

## Admission-time gate (Agent + AgentFleet, incl. `spec.template`)

- **Validating** — denies the lethal trifecta (`exec && egress && secrets`) without
  `agentctl.dev/allow-trifecta:"true"`; enforces an image-registry allow-list; checks
  cross-object `modelPool` existence.
- **Mutating** — defaults `app.kubernetes.io/*` labels + `mode` + minimal `surfaces`
  (does **not** hard-default `substrate`).

## Pod / workload security

- Confined securityContext on every control-plane pod **and** operator-rendered tenant
  agent pods: drop-`ALL`-caps, `seccomp:RuntimeDefault`, `readOnlyRootFilesystem`,
  no-priv-escalation, `runAsNonRoot` where the socket allows.
- node-agent: root + `hostPID` + hostPath (required for socket/`/proc` reads), but minimal
  (drops caps, seccomp, read-only rootfs).
- PodSecurity: `agentctl-system` is `privileged` (for node-agent); everything else self-confines.
- NetworkPolicies (opt-in): default-deny + narrow allows; Postgres ingress-only from
  gateway/modelgateway; agent egress to DNS + control plane only (needs a policy CNI).

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
apiVersion: agentctl.dev/v1
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
  `IDENTITY_ATTEST=true` on the Deployment (default off renders no env, so the code keeps
  the header-asserted path).
- **Complement for the NETWORKLESS (Kata) tier:** hardened agents on the Kata substrate
  have **no NIC / no pod IP**, so there is nothing to attest by source IP; their infer
  traffic rides the substrate egress and is identified by the node-agent's `SO_PEERCRED`
  attestation (kernel peer-credential → `/proc` cgroup → pod uid) when infer is routed
  through the node-agent. Source-IP attestation covers the networked tier; `SO_PEERCRED`
  infer-routing covers the networkless tier.

### Networkless-tier infer attestation (routed infer)

On the networked tier the ModelGateway attests the caller by its **source IP** (above).
On the **networkless (Kata) tier** the hardened agent has **no routable pod IP**, so there
is no source IP to attest — and a confined pod must not be trusted to *self-assert* its
identity in a header. The **routed-infer** path closes this with a kernel-attested unix
socket forwarder. The trust chain, end to end:

1. **agent → node-agent (unix socket).** The agent's `AGENT_INTELLIGENCE` points at a
   node-agent-owned **unix socket** on a **read-only** hostPath (`/run/agentctl/infer/infer.sock`),
   not at the ModelGateway directly. The operator wires this only when the Agent/Pod opts
   in via the annotation `agentctl.dev/routed-infer: "true"` (the Kata tier defaults it on);
   the agent mounts the socket dir **read-only** and can only *dial* it — never bind or
   replace it (the node-agent owns the bind).
2. **node-agent attests the peer (`SO_PEERCRED`).** The node-agent reads the connecting
   peer's kernel credential (`SO_PEERCRED`) and resolves it via `/proc/<pid>/cgroup` to the
   **pod uid** that owns the socket peer — exactly the attestation used on the node-agent
   control path. The kernel, not the client, supplies this identity.
3. **node-agent forwards to the ModelGateway with `X-Agent-Pod-Uid`.** The node-agent
   **strips** any client-supplied identity headers and **re-stamps** the attested pod uid as
   `X-Agent-Pod-Uid` before forwarding to the in-cluster ModelGateway Service
   (`MODELGATEWAY_URL`). Because it strips + re-stamps, a tenant **cannot self-assert** another
   pool's identity by setting its own header — the header the ModelGateway sees is the one the
   node-agent derived from the kernel peer credential.
4. **ModelGateway trusts the forwarder, then resolves uid → identity.** The ModelGateway
   accepts `X-Agent-Pod-Uid` **only** from the node-agent forwarder, identified by an
   **unforgeable** anchor: its source IP resolves to a pod **in the control-plane namespace**
   (`POD_NAMESPACE`, the gateway's own namespace) running the **`agentctl-node-agent`
   ServiceAccount** (`spec.serviceAccountName`) — *not* a free-form, self-settable label.
   A tenant cannot create a pod in the control-plane namespace, nor run a pod as that
   ServiceAccount, so it cannot impersonate the forwarder; the `app.kubernetes.io/name`
   label is kept only as defense-in-depth. The gateway then resolves the uid to the agent's
   namespace/identity for ModelPool selection, metering, and budget. **Fail closed:** if
   `POD_NAMESPACE` is unset, the gateway anchors no forwarder and trusts none (direct
   source-IP attestation still works).

**Why it is needed:** without a pod IP, source-IP attestation cannot run on the networkless
tier; the unix-socket peer credential is the substrate-local replacement. **Why the client
cannot cheat:** the socket is read-only to the agent, the identity is kernel-attested
(`SO_PEERCRED`), the node-agent strips client headers and re-stamps the attested uid, and the
forwarder itself is anchored to the control-plane namespace + ServiceAccount (unforgeable by a
tenant), so a confined tenant has no way to assert a different identity.

**Wiring:** enable `nodeAgent.inferProxy.enabled=true` (the chart sets `NODE_AGENT_INFER_SOCKET`
+ `MODELGATEWAY_URL` on the node-agent and mounts the socket dir **read-write** there); opt an
agent in with the `agentctl.dev/routed-infer` annotation (the operator points
`AGENT_INTELLIGENCE` at the socket and mounts the dir **read-only** into the agent pod).

## Supply chain

cosign keyless (OIDC) signatures on every image **and** the chart by digest; SBOM +
provenance in the build; `cargo deny` in CI; admission image allow-list + chart digest
pinning close the loop at deploy.

## Hardening checklist (posture + what's resolved)

- [x] Front-proxy mTLS + SAR on the management APIServer.
- [x] mTLS + `SO_PEERCRED` attestation on the node-agent control path.
- [x] Confined, admission-gated, supply-chain-signed workloads; least-privilege RBAC.
- [x] **Optional bearer-token (`AGENTCTL_API_TOKEN`) on the coordination server, ModelGateway,
  and A2A gateway** — closes "any in-cluster pod can call these" when enabled
  (`apiToken.enabled`); the scaler presents it; the operator injects it into agent pods.
- [x] **Attested ModelGateway identity** — source-IP attestation (`modelgateway.attestIdentity`)
  derives the agent namespace from the caller's source IP via a kube pod lookup, replacing the
  spoofable header-asserted `X-Agent-*` for the default (networked) tier. Robust because confined
  tenant pods drop `CAP_NET_RAW` and so cannot spoof their source IP. See "Attested agent
  identity (ModelGateway)" below.
- [x] **Networkless-tier infer attestation (`SO_PEERCRED` complement)** — the routed-infer path
  (`nodeAgent.inferProxy.enabled` + the operator `agentctl.dev/routed-infer` annotation): the
  agent dials a node-agent-owned **read-only** unix socket instead of the ModelGateway; the
  node-agent attests the peer via `SO_PEERCRED` (→ pod uid), **strips** any client-asserted
  identity, **re-stamps** `X-Agent-Pod-Uid`, and forwards to the ModelGateway (which trusts the
  forwarder by source IP). Closes identity attestation for the NETWORKLESS (Kata) tier where
  pods have no IP to attest — the client cannot self-assert. See "Networkless-tier infer
  attestation (routed infer)".
- [x] **Authenticated A2A *ingress*** for internet exposure — **per-agent OIDC**
  (`spec.access.oidc`, admission-validated) gives the A2A gateway a JWKS-verified JWT +
  required-claims authz + identity forwarding, native to the CR (front it with an
  Ingress/LB for TLS). See "OIDC per agent/fleet". Exporting enforcement to an external
  API-gateway/mesh is a documented future option.
- [x] **Postgres `verify-full` (client CA pinning), opt-in** — `postgres.bundled.tls.verifyFull`
  (with `tls.enabled`) pins the chart CA: the chart mounts `ca.crt` at `/etc/agentctl-pg-ca` in
  the gateway + modelgateway (and the coordination server when `coordination.store=postgres`),
  sets `DB_CA_FILE`/`PGSSLROOTCERT`, and flips `DATABASE_URL` to `sslmode=verify-full` against the
  cert-SAN `.svc` host. Default off keeps `sslmode=require` (encrypt, no CA verify). See
  operations.md §1.
- [x] **Coordination HA/durability, opt-in** — `coordination.store=postgres` backs the claim queue
  with the durable Postgres store (shared across replicas), so `coordination.replicas` can be raised
  for HA. Default `store=memory` stays single-replica/in-process.
- [ ] NetworkPolicy enforcement — needs Calico/Cilium (kindnet ignores).
- [ ] coordination/scaler stronger-than-token (attested) auth.
