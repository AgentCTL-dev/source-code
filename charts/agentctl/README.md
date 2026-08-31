# agentctl Helm chart

Installs the **agentctl** control plane — a Kubernetes control plane for fleets of
conformant AI agents — and the Agent Control Contract Custom Resource Definitions.
The chart deploys the control-plane Deployments, issues all control-plane TLS through
cert-manager, and gates every non-core feature behind a value that defaults to off, so
a stock install renders a minimal, secure footprint.

agentctl manages any agent that conforms to the published Agent Control Contract; it
never depends on a specific agent binary. The reference agent is used only by the
bundled examples.

- Chart version / app version: **1.3.0**
- API group for the CRDs: **`agentctl.dev/v1alpha1`**
- Management API group (aggregated API): **`management.agentctl.dev/v1alpha1`**

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

All components are Deployments. Each is one container image; the state and tenant
gateways are built on the Apache-licensed **mcpg** image. Turn on the planes your
product needs — the core (operator/apiserver/admission/gateway/identity) is on by
default; the rest are opt-in. See the full reference in
[docs/reference.md](../../docs/reference.md#1-components).

| Component | Purpose | Value block | Default |
| --- | --- | --- | --- |
| **operator** | Reconciles the CRD family into workloads; leader-elected for HA; owns per-workload PKI + CA distribution, per-namespace NetworkPolicies, KEDA wiring, tenant-namespace creation, and the guarded shard-resize choreography. | `operator` | Always installed |
| **apiserver** | Aggregated API (`management.agentctl.dev`): runtime verbs + the state-plane lifecycle verbs (backup/restore/reset/stop/start/migrate) + metering-export/audit-query; `SubjectAccessReview`-authorized. | `apiserver` | `enabled: true` |
| **admission** | Validating + mutating + conversion webhooks (image allow-list, lethal-trifecta gate, class floors, handle uniqueness, dangling-Secret rung; storage version `v1alpha2`). | `admission` | `enabled: true` |
| **identity** | OIDC federation, RFC 8628 device flow, sealed credential custody, the RFC 8693 exchange, the connections flow, and the AAuth agent-identity provider. | `identity` | `enabled` |
| **gateway** | The tenant data-plane front door: org routes, Agent Cards + `message/send`/`stream`, the external hooks ingress, HITL fan-out, supervisor auto-ensure/park, and inbound OIDC → per-(user,agent) principal injection. | `gateway` | `enabled: true` |
| **control** | The `control.*` MCP (agents manage agents): list/get/status/resolve/create + subagents, AAuth-verified and namespace-scoped server-side. | `control` | `enabled` |
| **coordination** | The `work.*` claim hub: exactly-one-owner leasing, result channel, dead-lettering, in-memory or durable-Postgres. Its backlog is the scale-from-zero signal. | `coordination` | `enabled: false` |
| **scaler** | KEDA external scaler reading the coordination backlog + per-fleet inbox metric. | `scaler` | `enabled: false` |
| **sandbox** | The `sandbox.run` backend — single-use, network-denied, capability-stripped code-execution pods (optional Kata/gVisor). | `sandbox` | `enabled: false` |
| **artifacts** | The `artifacts.put/get/list` backend over S3 (bundled MinIO), org-fenced with per-org byte quotas. | `artifacts` | `enabled: false` |
| **state** | The `state.*` seq-CAS checkpointer for `store.class: managed` — a governed mcpg SQL binding over Postgres, TLS-serving, server-side tenant-fenced. | `state` | `enabled: false` |
| **tenant mcpg** | *(per org, operator-provisioned)* The governance capability plane: a proxy-only mcpg federating the org's `MCPService` registry, with per-user credential injection. | `tenantMcpg` | `enabled: false` |
| **postgres** (bundled) | Durable store for the gateway, coordination, identity custody, metering, audit, and managed state. | `postgres` | Rendered when `postgres.mode: bundled` |

The chart also renders, as needed: cert-manager `Issuer`/`Certificate` objects and the
CA bundle; the `APIService` registration and webhook configurations (with caBundle
injection); Services; optional `PodDisruptionBudget`/`HorizontalPodAutoscaler` per
component; NetworkPolicies; and observability objects (see below).

### Custom Resource Definitions

The CRDs live in `charts/agentctl/crds/` and are installed automatically on first
`helm install`. All are namespaced and belong to `agentctl.dev/v1alpha1`.

| Kind | Plural | Short names | Purpose |
| --- | --- | --- | --- |
| `Agent` | `agents` | `agent`, `agents` | One agent workload. Renders to a Job (`once`/`workflow`), a CronJob (`schedule`), or a Deployment (`loop`/`reactive`). |
| `AgentFleet` | `agentfleets` | `afleet`, `afleets` | A replicated, autoscaled worker set with an optional coordinator and work policy. Claim mode renders a KEDA-scaled Deployment; shard mode renders a StatefulSet of N hash partitions. |
| `ModelPool` | `modelpools` | `mp` | A thin registry of model access for the intelligence plane (provider endpoint, allowed models, optional `credentialSecretRef`) that the agent dials directly — via a keyless AAuth-signed dial or a mounted `INTELLIGENCE_TOKEN`. |

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
`image.tag: 1.3.0`), or install the chart straight from the OCI registry:

```bash
helm install agentctl oci://ghcr.io/agentctl-dev/charts/agentctl \
  --version 1.3.0 \
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
kubectl get apiservice v1alpha1.management.agentctl.dev
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
| `apiserver.enabled` | `true` | Deploy the aggregated management apiserver + `APIService`. |
| `gateway.enabled` | `true` | Deploy the A2A gateway. |
| `admission.enabled` | `true` | Deploy the admission webhooks. |
| `<component>.autoscaling.enabled` | `false` | HPA for `apiserver`, `gateway`, `admission` (CPU-target). |
| `<component>.pdb.enabled` | `false` | PodDisruptionBudget for a component. |

### Planes (opt-in)

| Key | Default | Description |
| --- | --- | --- |
| `identity.service.enabled` | (per profile) | The identity plane: OIDC federation, custody, the RFC 8693 exchange, and the AAuth provider. |
| `control.enabled` | `false` | The `control.*` MCP (agents manage agents) + per-managed-namespace defaults seeding. |
| `coordination.enabled` | `false` | The `work.*` claim hub (needed by claim fleets). |
| `scaler.enabled` | `false` | The KEDA external scaler (needs KEDA installed). |
| `sandbox.enabled` | `false` | The sandbox cell + its deny-all cell namespace. |
| `artifacts.enabled` | `false` | The artifacts façade + bundled MinIO; `artifacts.orgQuotaBytes`, `artifacts.s3Endpoint` (point at a managed S3), `artifacts.credentials`. |
| `state.enabled` | `false` | The managed `state.*` checkpointer (mcpg + Postgres); `store.class: managed` agents need it. |
| `tenantMcpg.enabled` | `false` | Per-org governance gateways; `tenantMcpg.exchangePlugin` (OBO), `tenantMcpg.nonProductionLicense`, `tenantMcpg.auditShipperImage`. |

### Security gates

| Key | Default | Description |
| --- | --- | --- |
| `admission.allowedRegistries` | `agentd:,mock-agent,agentctl/,gcr.io/,registry.k8s.io/,ghcr.io/` | CSV image-prefix allow-list the validating webhook enforces (empty = allow all). |
| `coordination.attestIdentity` | `true` | Bind each work claim to the caller's attested source IP so a tenant cannot ack/release another tenant's claim (takes effect only when `coordination.enabled`). |
| `apiToken.enabled` | `false` | Require an `Authorization: Bearer <token>` on the coordination server, A2A gateway, and scaler (token kept in the `agentctl-api-token` Secret). |
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
# ModelPool objects in the cluster:
kubectl delete crd \
  agents.agentctl.dev \
  agentfleets.agentctl.dev \
  modelpools.agentctl.dev

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

## Profiles

Two curated value profiles ship with the chart:

- `profiles/dev.yaml` — the full stack on one kind/minikube cluster: bundled
  Postgres, the identity service (device-flow login, per-user principal
  minting, the AAuth Agent-Provider role) armed, the operator's
  house-provisioning pointed at it. Add your IdP under
  `identity.service.providers` to log in.
- `profiles/enterprise.yaml` — the hardened posture: external Postgres,
  NetworkPolicies (needs a policy-enforcing CNI), the coarse in-cluster
  bearer gate, HA operator/identity replicas, ServiceMonitors, and the
  managed seal/signing keys you should pin in production.

```
helm install agentctl charts/agentctl -f charts/agentctl/profiles/dev.yaml
```

Multi-version CRDs: `Agent`/`AgentFleet` serve v1alpha1 AND v1alpha2 with a
conversion webhook (storage v1alpha2). Helm installs `crds/` on FIRST install
only — on upgrades apply `deploy/crds/` yourself. The conversion stanza pins
the default `agentctl-system` namespace (Helm does not template `crds/`); a
custom-namespace install must patch the CRDs' `spec.conversion.webhook`
service ref and `cert-manager.io/inject-ca-from` annotation.
