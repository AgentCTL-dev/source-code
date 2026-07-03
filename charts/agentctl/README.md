# agentctl Helm chart

Installs the **agentctl** control plane — a Kubernetes control plane for fleets of
conformant AI agents — and the Agent Control Contract Custom Resource Definitions.
The chart deploys the control-plane Deployments, issues all control-plane TLS through
cert-manager, and gates every non-core feature behind a value that defaults to off, so
a stock install renders a minimal, secure footprint.

agentctl manages any agent that conforms to the published Agent Control Contract; it
never depends on a specific agent binary. The reference agent is used only by the
bundled examples.

- Chart version / app version: **1.0.0**
- API group for the CRDs: **`agents.x-k8s.io/v1alpha1`**
- Management API group (aggregated API): **`management.agents.x-k8s.io/v1alpha1`**

---

## Prerequisites

| Dependency | Required? | Why | Enabled by |
| --- | --- | --- | --- |
| A Kubernetes cluster (with the aggregation layer enabled) | Yes | The management API is served as a Kubernetes aggregated API. | — |
| Helm 3 | Yes | Chart tooling. | — |
| [cert-manager](https://cert-manager.io) (>= 1.13) | **Yes** | Issues every control-plane serving/mTLS certificate and the per-workload agent serving certs. The chart does not vendor it. | `certManager.enabled` (default `true`) |
| [KEDA](https://keda.sh) (>= 2.x) | Optional | Claim-mode `AgentFleet` autoscaling (scale-from-zero off the work backlog). | `scaler.enabled` |
| A NetworkPolicy-capable CNI (Calico, Cilium) | Optional | Tenant isolation. `kindnet` renders policies but does not enforce them. | `networkPolicies.enabled` |
| PostgreSQL | Optional (bundled) | Durable coordination/task/usage state. In-memory is the single-replica default. | `postgres.mode` |

Install cert-manager first and confirm it is healthy:

```bash
kubectl get pods -n cert-manager
```

---

## What the chart deploys

### Control-plane components

All components are Deployments. The chart ships eight control-plane container images.

| Component | Purpose | Value block | Default |
| --- | --- | --- | --- |
| **operator** | Reconciles `Agent`/`AgentFleet` into workloads; leader-elected for HA; issues per-workload serving certs and distributes the cluster CA; reconciles per-namespace NetworkPolicies; wires the KEDA `ScaledObject` for claim fleets; projects status. | `operator` | Always installed |
| **apiserver** | Aggregated API serving the management verbs (`drain`, `lame-duck`, `cancel`, `pause`, `resume`) under `management.agents.x-k8s.io`; authorizes each via `SubjectAccessReview` and dials the target agent pod(s) directly over mTLS. | `apiserver` | `enabled: true` |
| **admission** | Validating webhook (image-registry allow-list, lethal-trifecta gate, ModelPool existence, OIDC-policy shape) and mutating webhook (secure defaults). | `admission` | `enabled: true` |
| **gateway** | The public agent-to-agent (A2A) surface: projects and signs Agent Cards, serves `message/send` and `message/stream` (SSE), persists tasks in Postgres, delivers SSRF-guarded push webhooks, and enforces inbound auth. | `gateway` | `enabled: true` |
| **modelgateway** | Intelligence broker: attests the caller, selects the `ModelPool`, injects the provider credential off-pod, meters tokens, and enforces budgets. | `modelgateway` | `enabled: true` |
| **mcpgateway** | Tools broker: attests the caller, scopes the call to the agent's bound `MCPServerSet`, injects the tool-server credential off-pod, and forwards MCP. | `mcpgateway` | `enabled: true` |
| **coordination** | Work-distribution backbone: an MCP server exposing `work.*` with exactly-one-owner claim leasing, a result channel, dead-lettering, and an in-memory or durable-Postgres store. Its backlog is the scale-from-zero signal. | `coordination` | `enabled: false` |
| **scaler** | KEDA external scaler that reads the coordination backlog so claim fleets scale from zero. | `scaler` | `enabled: false` |
| **postgres** (bundled) | Durable store for the gateway, modelgateway, and (optionally) coordination. | `postgres` | Rendered when `postgres.mode: bundled` |

The chart also renders, as needed: cert-manager `Issuer`/`Certificate` objects and the
CA bundle; the `APIService` registration and webhook configurations (with caBundle
injection); Services; optional `PodDisruptionBudget`/`HorizontalPodAutoscaler` per
component; NetworkPolicies; and observability objects (see below).

### Custom Resource Definitions

The CRDs live in `charts/agentctl/crds/` and are installed automatically on first
`helm install`. All are namespaced and belong to `agents.x-k8s.io/v1alpha1`.

| Kind | Plural | Short names | Purpose |
| --- | --- | --- | --- |
| `Agent` | `agents` | `agent`, `agents` | One agent workload. Renders to a Job (`once`/`workflow`), a CronJob (`schedule`), or a Deployment (`loop`/`reactive`). |
| `AgentFleet` | `agentfleets` | `afleet`, `afleets` | A replicated, autoscaled worker set with an optional coordinator, per-fleet budget, and work policy. Claim mode renders a KEDA-scaled Deployment; shard mode renders a StatefulSet of N hash partitions. |
| `ModelPool` | `modelpools` | `mp` | A pool of model access for the intelligence plane (provider endpoint, credential Secret, allowed models, optional budget). |
| `MCPServerSet` | `mcpserversets` | `mcpset` | A reusable bundle of MCP tool servers for the tools plane (per-server endpoint, Secret-held auth, capability tags, optional budget). |

> **Helm and CRDs:** Helm installs the `crds/` directory on first install but never
> upgrades or deletes it. See [Upgrading the CRDs](#upgrading-the-crds) and
> [Uninstall](#uninstall).

---

## Install

The chart does not create its own namespace by default (`namespace.create: false`),
because Helm release storage requires the namespace to already exist. Use Helm's
`--create-namespace`, or pre-create it.

### From local (kind-loaded) images

The default `image.registry` is empty, which resolves to local image names
(`agentctl/<component>:dev`) — suitable for a `kind` cluster with the images loaded.

```bash
helm install agentctl ./charts/agentctl \
  -n agentctl-system --create-namespace
```

### From the published GHCR images

Use the `values-ghcr.yaml` overlay (sets `image.registry: ghcr.io/agentctl-dev` and
`image.tag: 1.0.0`), or install the chart straight from the OCI registry:

```bash
helm install agentctl oci://ghcr.io/agentctl-dev/charts/agentctl \
  --version 1.0.0 \
  -n agentctl-system --create-namespace \
  -f charts/agentctl/values-ghcr.yaml
```

For reproducible, tamper-evident installs, pin each component by digest under
`image.digests` (a digest entry wins over the tag for that component); this requires
`image.registry` to be set.

### Verify the rollout

```bash
kubectl -n agentctl-system get pods
kubectl -n agentctl-system rollout status deploy/agentctl-operator

# The aggregated management API should report AVAILABLE=True:
kubectl get apiservice v1alpha1.management.agents.x-k8s.io
```

Run the bundled connectivity test (created only by `helm test`, never on install):

```bash
helm test agentctl -n agentctl-system
```

It probes the A2A gateway's `/healthz` and confirms the management `APIService` is
registered and Available.

---

## Upgrading

```bash
helm upgrade agentctl ./charts/agentctl -n agentctl-system --reuse-values
```

To enable claim-mode work distribution and autoscaling on an existing release (install
KEDA first):

```bash
helm upgrade agentctl ./charts/agentctl -n agentctl-system --reuse-values \
  --set coordination.enabled=true \
  --set scaler.enabled=true
```

### Upgrading the CRDs

Helm does not upgrade the `crds/` directory on `helm upgrade`. When a release ships CRD
schema changes, apply them explicitly:

```bash
kubectl apply -f charts/agentctl/crds/
```

---

## Values reference

Defaults below are from `values.yaml`. Every component block additionally accepts the
common knobs `replicas`, `logLevel` (maps to `RUST_LOG`), `resources`, `nodeSelector`,
`affinity`, `tolerations`, `topologySpreadConstraints`, `priorityClassName`,
`podAnnotations`, `podLabels`, `extraEnv`, `envFrom`, `serviceAccount.annotations`,
`pdb.*`, and (where applicable) `autoscaling.*`.

### Images and global metadata

| Key | Default | Description |
| --- | --- | --- |
| `image.registry` | `""` | Registry prefix. Empty resolves to local names (`agentctl/<comp>`); set to e.g. `ghcr.io/agentctl-dev` for the published images. |
| `image.tag` | `dev` | Image tag applied to all components. |
| `image.pullPolicy` | `IfNotPresent` | Image pull policy. |
| `image.pullSecrets` | `[]` | Image pull secrets. |
| `image.digests` | `{}` | Per-component `sha256:` digest pins (requires `image.registry`); a digest wins over the tag. |
| `referenceAgent.image` | `ghcr.io/agentd-dev/agentd:1.0.0` | The reference agent image used only by the bundled examples. |
| `commonLabels` | `{}` | Labels stamped on every rendered object and pod template. |
| `commonAnnotations` | `{}` | Annotations stamped on every rendered object and pod template. |

### Namespace and TLS

| Key | Default | Description |
| --- | --- | --- |
| `namespace.name` | `agentctl-system` | Namespace the control plane runs in. |
| `namespace.create` | `false` | Whether the chart creates the namespace (prefer `helm --create-namespace`). |
| `namespace.podSecurity` | `baseline` | Pod Security Standard label applied when the chart creates the namespace. |
| `certManager.enabled` | `true` | Provision the self-signed bootstrap Issuer → CA → CA Issuer and issue all serving/mTLS certs. Required. |
| `certManager.caIssuerRef` | `""` | Use an existing cluster `ClusterIssuer` as the CA instead of the bootstrap chain. |
| `certManager.clusterResourceNamespace` | `cert-manager` | Namespace holding the bootstrap CA Certificate secret (only used when `caIssuerRef` is unset). |

### Core components

| Key | Default | Description |
| --- | --- | --- |
| `operator.replicas` | `1` | Operator replicas (leader-elected; raise for HA). |
| `operator.modelgatewayUrl` | `""` | Override the ModelGateway URL rendered into agent pods (empty = in-cluster Service DNS). |
| `operator.mcpgatewayUrl` | `""` | Override the MCPGateway URL rendered into agent pods (empty = in-cluster Service DNS). |
| `apiserver.enabled` | `true` | Deploy the aggregated management apiserver + `APIService`. |
| `gateway.enabled` | `true` | Deploy the A2A gateway. |
| `modelgateway.enabled` | `true` | Deploy the intelligence broker. |
| `mcpgateway.enabled` | `true` | Deploy the tools broker. |
| `admission.enabled` | `true` | Deploy the admission webhooks. |
| `<component>.autoscaling.enabled` | `false` | HPA for `apiserver`, `gateway`, `modelgateway`, `admission` (CPU-target). |
| `<component>.pdb.enabled` | `false` | PodDisruptionBudget for a component. |

### Security gates

| Key | Default | Description |
| --- | --- | --- |
| `admission.allowedRegistries` | `agentd:,mock-agent,agentctl/,gcr.io/,registry.k8s.io/,ghcr.io/` | CSV image-prefix allow-list the validating webhook enforces (empty = allow all). |
| `modelgateway.attestIdentity` | `true` | Derive the agent namespace from the caller's attested source IP instead of a spoofable header. Set `false` only for a trusted single-tenant install. |
| `modelgateway.secretsNamespaces` | `[]` | Namespaces the ModelGateway may read provider-credential Secrets in (empty = cluster-wide get/list; scope this in production). |
| `mcpgateway.secretsNamespaces` | `[]` | Namespaces the MCPGateway may read tool-server credential Secrets in (empty = cluster-wide get/list). |
| `coordination.attestIdentity` | `true` | Bind each work claim to the caller's attested source IP so a tenant cannot ack/release another tenant's claim (takes effect only when `coordination.enabled`). |
| `apiToken.enabled` | `false` | Require an `Authorization: Bearer <token>` on the coordination server, ModelGateway, A2A gateway, and scaler (token kept in the `agentctl-api-token` Secret). |
| `apiToken.value` | `""` | Fixed/managed token value (empty = chart generates and keeps a random one). |
| `trustedProxy.enabled` | `false` | Open a second mTLS listener on the gateway (`:8443`) that only a trusted fronting proxy may use, forwarding a verified caller identity via headers. |
| `trustedProxy.allowedNames` | `["apisix"]` | Client-cert CN/SANs accepted on the trusted-proxy listener. |
| `trustedProxy.proxyCommonName` | `apisix` | CN minted on the client cert handed to the fronting proxy. |
| `trustedProxy.headerPrefix` | `x-agentctl` | Prefix for the identity headers the trusted proxy asserts. |
| `trustedProxy.identityHeaders` | `{}` | Advanced per-header name overrides (empty = derive from the prefix). |

> The **lethal-trifecta gate** (an Agent declaring `exec` + `egress` + `secrets`
> together) is enforced by the admission webhook and opted into per-Agent via an
> explicit annotation on the `Agent` resource — there is no chart-level switch.

### Postgres

| Key | Default | Description |
| --- | --- | --- |
| `postgres.mode` | `bundled` | `bundled` (chart deploys Postgres) or `external` (point at a managed instance). |
| `postgres.bundled.image` | `postgres:16-alpine` | Bundled Postgres image. |
| `postgres.bundled.runAsUser` | `70` | Non-root uid the container runs as (70 = postgres in the alpine image). |
| `postgres.bundled.storage` | `emptyDir` | `emptyDir` (eval) or `pvc` (durable). |
| `postgres.bundled.pvcSize` | `5Gi` | PVC size when `storage: pvc`. |
| `postgres.bundled.storageClassName` | `""` | StorageClass for the PVC (empty = cluster default). |
| `postgres.bundled.tls.enabled` | `false` | Encrypt the in-cluster Postgres hop (`sslmode=require`), issuing a cert-manager Certificate for it. |
| `postgres.bundled.tls.verifyFull` | `false` | CA-pinned, hostname-verified hop (`sslmode=verify-full`); requires `tls.enabled`. |
| `postgres.external.dsnSecretName` | `""` | Pre-created Secret holding the DSN when `mode: external`. |
| `postgres.external.dsnSecretKey` | `DATABASE_URL` | Key within that Secret. |

### Coordination and KEDA scaling

| Key | Default | Description |
| --- | --- | --- |
| `coordination.enabled` | `false` | Deploy the work-distribution server (required for claim-mode fleets). |
| `coordination.store` | `memory` | `memory` (single replica) or `postgres` (durable, shared queue; allows HA replicas). |
| `coordination.replicas` | `1` | Keep at 1 with `store: memory`; raise with `store: postgres`. |
| `coordination.mtls.enabled` | `false` | Add an mTLS listener (`:8443`) for the scaler with cert-manager-issued serving/client certs. |
| `coordination.mtls.allowedNames` | `["agentctl-scaler"]` | Client-cert CN/SANs the coordination mTLS listener accepts. |
| `coordination.mtls.scalerCommonName` | `agentctl-scaler` | CN minted on the scaler's client cert. |
| `scaler.enabled` | `false` | Deploy the KEDA external scaler and flip the operator's `SCALER_ENABLED` (operator then renders a `ScaledObject` per claim fleet). Requires KEDA. |
| `scaler.address` | `""` | `SCALER_ADDRESS` stamped into ScaledObjects (empty = in-cluster Service DNS). |
| `scaler.coordinationUrl` | `""` | Backlog URL the scaler reads (empty = in-cluster Service DNS). |

### NetworkPolicies

| Key | Default | Description |
| --- | --- | --- |
| `networkPolicies.enabled` | `false` | Render control-plane default-deny + sanctioned-flow policies and per-agent-namespace data-plane policies. Requires an enforcing CNI. |
| `networkPolicies.agentNamespaces` | `["default"]` | Namespaces where tenant/agent pods run; each gets default-deny + sanctioned egress (DNS + control plane) and ingress (control plane only). |

### Observability

| Key | Default | Description |
| --- | --- | --- |
| `metrics.serviceMonitor.enabled` | `false` | Emit a Prometheus-Operator `ServiceMonitor` per scrape target. Requires the Prometheus-Operator CRDs. |
| `metrics.serviceMonitor.interval` | `30s` | Scrape interval. |
| `metrics.serviceMonitor.scrapeTimeout` | `10s` | Scrape timeout. |
| `metrics.serviceMonitor.labels` | `{}` | Extra labels for the Prometheus `serviceMonitorSelector`. |
| `observability.dashboards.enabled` | `false` | Render the Grafana dashboard ConfigMap (labeled for the Grafana sidecar). |
| `observability.alerts.enabled` | `false` | Render the `PrometheusRule`. Requires the Prometheus-Operator CRDs. |
| `observability.alerts.labels` | `{}` | Extra labels so your Prometheus selects the rule. |

Every component and agent exposes Prometheus `/metrics` regardless of these flags; the
flags only add the Prometheus-Operator-coupled objects.

---

## Uninstall

```bash
helm uninstall agentctl -n agentctl-system
```

Helm leaves the following behind by design — remove them manually if you want a clean
slate:

```bash
# CRDs (Helm never deletes crds/) — this also deletes all Agent/AgentFleet/
# ModelPool/MCPServerSet objects in the cluster:
kubectl delete crd \
  agents.agents.x-k8s.io \
  agentfleets.agents.x-k8s.io \
  modelpools.agents.x-k8s.io \
  mcpserversets.agents.x-k8s.io

# The API token Secret is retained via a keep policy (only if apiToken was enabled):
kubectl -n agentctl-system delete secret agentctl-api-token --ignore-not-found

# The namespace (the chart does not own it):
kubectl delete namespace agentctl-system
```

cert-manager `Certificate`/`Issuer` objects and their backing secrets in the namespace
are removed with the namespace.

---

## Related documentation

- Architecture: [`../../docs/architecture.md`](../../docs/architecture.md)
- Operations: [`../../docs/operations.md`](../../docs/operations.md)
- Security model: [`../../docs/security.md`](../../docs/security.md)
- Example manifests: [`../../deploy/examples/`](../../deploy/examples/)
