# agentctl Helm chart

The cloud-native install for the **agentctl** control plane — operator, on-node
bridge (node-agent), aggregated APIServer, A2A gateway, intelligence ModelGateway,
and admission webhook — plus the Agent Control Contract CRDs. All TLS (the
APIServer serving cert, the admission webhook cert, and node-agent mTLS) is issued
and rotated by **cert-manager**, with the `caBundle` injected automatically.

> The control plane is **BUSL-1.1**; the contract + SDK are **Apache-2.0** (see the
> repo `LICENSE`). The data plane is *any* conformant agent — the reference agent
> is `agentd` (repo `agentd-dev`), not a dependency.

## Prerequisites

| Requirement | Required? | Why |
|---|---|---|
| **cert-manager** (≥ 1.13) | **hard** | issues every serving/mTLS cert + injects the caBundles. `kubectl apply -f https://github.com/cert-manager/cert-manager/releases/latest/download/cert-manager.yaml` |
| **Postgres** | bundled | the gateway + modelgateway durable store — **bundled** by the chart (eval) or external (prod, `postgres.mode=external`) |
| Aggregation layer enabled | standard | the APIServer registers an `APIService` (standard on stock k8s) |
| **KEDA** (≥ 2.x) | *optional* | only for **claim-mode autoscaling** (`scaler.enabled` — elastic-from-zero AgentFleets). The chart installs and runs fully without it. `helm install keda kedacore/keda -n keda --create-namespace` |
| A **CNI** that enforces NetworkPolicy (Calico/Cilium) | *optional* | only if you set `networkPolicies.enabled=true` (kindnet ignores them) |

**cert-manager is the only hard external prerequisite.** Postgres is bundled; KEDA
and a policy CNI are needed only for the opt-in features that use them (below).

## What this installs

A default install brings up the **core control plane** (no deps beyond cert-manager):
`operator` · `node-agent` (DaemonSet) · aggregated `apiserver` · A2A `gateway` ·
`modelgateway` · `admission` · bundled `postgres` — the "7 components" the verify step
checks. Two planes are **opt-in** (default off): the **coordination** server
(`coordination.enabled` — the `work.*` claim hub) and the **scaler**
(`scaler.enabled` — elastic-from-zero autoscaling, **which requires KEDA**). Enable
both for claim-mode fleets:

```console
helm upgrade agentctl ./charts/agentctl -n agentctl-system --reuse-values \
  --set coordination.enabled=true --set scaler.enabled=true   # needs KEDA installed
```

> **CRDs on upgrade.** Helm installs the `crds/` (`Agent`/`AgentFleet`/`ModelPool`)
> on **first install** but does **not** update them on `helm upgrade`. When a chart
> upgrade changes a CRD, re-apply: `kubectl apply -f charts/agentctl/crds/`.

## Install

The namespace needs a relaxed PodSecurity level (the node-agent uses `hostPath` +
`hostPID`), and Helm can't reliably own the namespace it installs into, so
pre-create it:

```console
kubectl create namespace agentctl-system
kubectl label  namespace agentctl-system \
  pod-security.kubernetes.io/enforce=privileged \
  pod-security.kubernetes.io/warn=privileged

helm install agentctl ./charts/agentctl -n agentctl-system
```

Verify:

```console
kubectl -n agentctl-system get pods                       # 7 components Running
kubectl -n agentctl-system get certificate                # all READY=True
kubectl get apiservice v1alpha1.management.agents.x-k8s.io # AVAILABLE=True
```

### From the published GHCR registry (no local image loading)

CI publishes the component images and the chart itself to `ghcr.io/agentctl-dev`
on every `vX.Y.Z` tag. To install the published artifacts, pull the chart over OCI
and use the GHCR registry overlay:

```console
helm install agentctl oci://ghcr.io/agentctl-dev/charts/agentctl \
  -n agentctl-system --version 0.1.0 \
  --set image.registry=ghcr.io/agentctl-dev --set image.tag=0.1.0
```

For tamper-evident installs, pin by digest via `image.digests.<comp>` (see
`values-ghcr.yaml`) — a digest entry renders `…/<comp>@sha256:…` instead of the tag.

### Via the `agentctl` CLI

```console
agentctl install -n agentctl-system            # defaults to the OCI chart above
agentctl install --chart ./charts/agentctl --dry-run   # render against a local chart
```

The CLI pre-flights cert-manager, ensures the namespace + PodSecurity label, then
runs `helm upgrade --install` (pass `--registry`, `--tag`, `--version`, `--set`).

> An **OLM/OperatorHub bundle** (alpha) lives in [`bundle/`](../../bundle/README.md)
> for clusters running Operator Lifecycle Manager.

## Key values

| Key | Default | Purpose |
|---|---|---|
| `image.registry` | `""` | image registry; empty ⇒ local `agentctl/<comp>:<tag>` (kind-loaded) |
| `image.tag` | `dev` | image tag for all components |
| `certManager.enabled` | `true` | issue all certs via cert-manager (required) |
| `certManager.caIssuerRef` | `""` | use an existing cluster CA `ClusterIssuer` instead of the bundled self-signed CA |
| `postgres.mode` | `bundled` | `bundled` (in-cluster Postgres) or `external` |
| `postgres.external.dsnSecretName` | `""` | Secret holding `DATABASE_URL` when `mode=external` |
| `apiserver.enabled` / `gateway.enabled` / `modelgateway.enabled` / `admission.enabled` | `true` | toggle planes (operator + node-agent always install) |
| `admission.allowedRegistries` | (CSV) | image-registry prefixes the webhook permits |
| `networkPolicies.enabled` | `false` | ship egress/tenant NetworkPolicies (needs a policy CNI) |
| `apiToken.enabled` | `false` | in-cluster bearer-token gate (`AGENTCTL_API_TOKEN`) on coordination/modelgateway/A2A-gateway; injected into scaler + control-plane-namespace agents (see [security.md](../../docs/security.md)) |
| `apiToken.value` | `""` | override the generated token with a managed value (empty ⇒ chart generates a lookup-stable random one) |
| `substrate.socketRoot` | `/run/agentctl/sockets` | on-node socket root the node-agent watches |

## Observability (day-2)

All metrics are exposed on each component's `/metrics` endpoint
(`agentctl_operator_*`, `agentctl_gateway_*`, `agentctl_modelgateway_*`,
`agentctl_admission_*`, node-agent, plus `process_start_time_seconds`). Wire them up:

| Key | Default | Purpose |
|---|---|---|
| `metrics.serviceMonitor.enabled` | `false` | emit a Prometheus-Operator `ServiceMonitor` per scrape target (jobs `agentctl-<comp>`) |
| `observability.dashboards.enabled` | `false` | ship a Grafana dashboard as a `ConfigMap` labeled `grafana_dashboard: "1"` (sidecar auto-discovery) |
| `observability.alerts.enabled` | `false` | ship a `PrometheusRule` (needs the Prometheus-Operator CRDs) |
| `observability.alerts.labels` | `{}` | extra labels so your Prometheus `ruleSelector` picks up the rule, e.g. `--set observability.alerts.labels.release=prometheus` |

```console
helm upgrade agentctl ./charts/agentctl -n agentctl-system \
  --set metrics.serviceMonitor.enabled=true \
  --set observability.dashboards.enabled=true \
  --set observability.alerts.enabled=true \
  --set observability.alerts.labels.release=prometheus
```

The dashboard (`templates/dashboard.yaml`, uid `agentctl-control-plane`) charts the
operator (reconcile/error rate, latency, leader), the A2A gateway, the ModelGateway
(inference, tokens, budget rejections), and the admission webhook. The alerts
(`templates/prometheusrule.yaml`) are `AgentctlOperatorNoLeader`,
`AgentctlReconcileErrors`, `AgentctlComponentDown`, `AgentctlAdmissionDenySpike`, and
`AgentctlBudgetRejections`. Both are off by default so a stock install needs no
Grafana or Prometheus-Operator CRDs.

### `helm test`

After install, `helm test agentctl -n agentctl-system` runs a connectivity probe
(`templates/tests/test-connection.yaml`) that checks the gateway `/healthz` and that
the aggregated `APIService management.agents.x-k8s.io/v1alpha1` is registered and
Available — exiting non-zero (failing the test) if either is unreachable.

### `kubectl agent` (Krew)

The `kubectl-agent` CLI plugin ships separately via Krew
([`plugins/krew-agent.yaml`](../../plugins/krew-agent.yaml)); it is the `agentctl`
CLI installed under the `kubectl-agent` name so it runs as `kubectl agent …`.

## What cert-manager wires (replacing the old `install.sh` scripts)

A self-signed bootstrap `Issuer` → an **agentctl CA** → a CA `Issuer` mints:
- `agentctl-apiserver-tls` — APIServer serving cert; CA injected into the `APIService.spec.caBundle`.
- `agentctl-admission-tls` — webhook serving cert; CA injected into the `ValidatingWebhookConfiguration`.
- `agentctl-node-agent-tls` — node-agent mTLS **server** cert (+ CA to verify clients).
- `agentctl-client-tls` — the mTLS **client** cert the apiserver + gateway present to the node-agent.

cert-manager handles renewal (`renewBefore: 720h`), so certs rotate without a redeploy.

## Production notes
- **External Postgres:** `--set postgres.mode=external --set postgres.external.dsnSecretName=my-pg` (Secret with key `DATABASE_URL`).
- **Private registry:** `--set image.registry=ghcr.io/your-org --set image.tag=vX.Y.Z` (+ `image.pullSecrets`).
- **Your own CA:** `--set certManager.caIssuerRef=my-ca-clusterissuer` to chain into an existing PKI.
- **Uninstall:** `helm uninstall agentctl -n agentctl-system`. The gateway signing key + bundled Postgres secret carry `helm.sh/resource-policy: keep`; delete them manually if you want a clean slate. CRDs (installed from `crds/`) are not removed by `helm uninstall` — delete them explicitly to drop all `Agent`/`AgentFleet`/`ModelPool` objects.
