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

| Requirement | Why |
|---|---|
| **cert-manager** (≥ 1.13) | issues every serving/mTLS cert + injects the caBundles. `kubectl apply -f https://github.com/cert-manager/cert-manager/releases/latest/download/cert-manager.yaml` |
| A **CNI** that enforces NetworkPolicy (Calico/Cilium) | only if you set `networkPolicies.enabled=true` (kindnet ignores them) |
| **Postgres** | the gateway + modelgateway durable store — bundled (eval) or external (prod) |
| Aggregation layer enabled | the APIServer registers an `APIService` (standard on stock k8s) |

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
| `substrate.socketRoot` | `/run/agentctl/sockets` | on-node socket root the node-agent watches |

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
