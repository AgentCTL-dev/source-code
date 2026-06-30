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
| **ModelGateway** :8080 (`/v1/infer`) | HTTP (+ optional Bearer) | identity header-asserted (`X-Agent-Namespace`/`-Name`); optional **`AGENTCTL_API_TOKEN`** bearer gate | ModelPool existence + budget | agents (NetworkPolicy-scoped) |
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
| modelgateway | `modelpools` get/list/watch + `/status`; `secrets` get/list — **scoped** to `secretsNamespaces` |
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
- [ ] **Attested ModelGateway identity** — replace the header-asserted `X-Agent-*` with
  `SO_PEERCRED`-attested identity by routing infer through the node-agent (anti-spoof within
  the trusted set; the token closes *access*, not per-pod identity).
- [ ] **Authenticated A2A *ingress*** for internet exposure — per-client mTLS/JWT at an
  ingress/API-gateway + the card `securitySchemes` (the shared token is a coarse v1 gate).
- [ ] NetworkPolicy enforcement — needs Calico/Cilium (kindnet ignores).
- [ ] Postgres `verify-full` (client CA pinning) — today `sslmode=require` (encrypt, no CA verify).
- [ ] coordination/scaler stronger-than-token (attested) auth; coordination HA/durability.
