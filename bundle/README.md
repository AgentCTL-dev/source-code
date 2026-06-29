# agentctl — OLM / OperatorHub bundle (alpha / preview)

This directory is an [Operator Lifecycle Manager](https://olm.operatorframework.io/)
bundle for the agentctl control plane. It packages the three Agent Control
Contract CRDs and a `ClusterServiceVersion` (CSV) that installs the operator and
the four other plane Deployments via OLM's `deployment` install strategy.

> **This bundle is alpha / preview. It is NOT full OperatorHub-ready packaging.**
> Read [Scope & limitations](#scope--limitations) before installing — several
> components are intentionally out of scope for the CSV and must be applied
> separately (the Helm chart installs everything).

## Layout

```
bundle/
  manifests/
    agents.agents.x-k8s.io.yaml            # Agent CRD       (copied from charts/agentctl/crds)
    agentfleets.agents.x-k8s.io.yaml       # AgentFleet CRD
    modelpools.agents.x-k8s.io.yaml        # ModelPool CRD
    agentctl.clusterserviceversion.yaml    # the CSV (agentctl.v0.1.0)
  metadata/
    annotations.yaml                       # bundle annotations (package/channels/dirs)
  bundle.Dockerfile                        # scratch bundle image (LABELs == annotations)
  README.md
```

- Package: `agentctl` Channel: `alpha` (default `alpha`)
- CSV: `agentctl.v0.1.0` (version `0.1.0`, maturity `alpha`, minKubeVersion `1.31.0`)
- Operator image: `ghcr.io/agentctl-dev/operator:0.1.0` (other planes:
  `apiserver`, `gateway`, `modelgateway`, `admission` under `ghcr.io/agentctl-dev/`)

## Build

The bundle image is a `scratch` image carrying only metadata; the build context
is this `bundle/` directory.

```bash
# from the repo root
docker build -f bundle/bundle.Dockerfile -t ghcr.io/agentctl-dev/agentctl-bundle:0.1.0 bundle
docker push ghcr.io/agentctl-dev/agentctl-bundle:0.1.0
```

## Validate

```bash
# Static validation (requires operator-sdk).
operator-sdk bundle validate ./bundle

# Stricter, OperatorHub-oriented checks (expect alpha findings — see below).
operator-sdk bundle validate ./bundle \
  --select-optional suite=operatorframework

# Run on a cluster with OLM installed:
operator-sdk run bundle ghcr.io/agentctl-dev/agentctl-bundle:0.1.0 \
  --namespace agentctl-system
```

`operator-sdk bundle validate ./bundle` should pass the default suite. The
optional OperatorHub suite will flag the missing icon and the alpha maturity —
those are expected for a preview bundle.

## Scope & limitations (be honest about OLM fit)

OLM's `deployment` install strategy can only carry **Deployments**,
**(Cluster)Roles/(Cluster)RoleBindings**, and **ServiceAccounts**. agentctl is a
six-plane control plane with on-node and aggregation components that do not fit
that mold, so the following are **NOT installed by this CSV**:

| Component / resource | Why it's out of scope | How to install it |
| --- | --- | --- |
| **node-agent DaemonSet** | OLM install strategy has no DaemonSet kind. | Helm chart, or `deploy/node-agent`. |
| **Aggregated APIService** `v1alpha1.management.agents.x-k8s.io` | Modeling it as an `owned` apiservicedefinition forces OLM-managed serving certs and conflicts with the cert-manager flow used here. | Apply the `APIService` separately (Helm `templates/apiserver.yaml`). |
| **ValidatingWebhookConfiguration** | The admission **Deployment** is installed, but its webhook registration (with cert-manager `caBundle` injection) is not. Until it's applied, the trifecta gate does not run. | Apply separately (Helm `templates/admission.yaml`). |
| **cert-manager Certificates / Issuers** | The apiserver, admission webhook, gateway client, and node-agent mount TLS Secrets (`agentctl-apiserver-tls`, `agentctl-client-tls`, `agentctl-admission-tls`, `agentctl-node-agent-tls`). | **cert-manager >= 1.13 is a hard runtime prerequisite.** Install cert-manager, then apply the Certificates (Helm `templates/certificates.yaml`). Until the Secrets exist, those pods stay `Pending`. |
| **Postgres + gateway signing Secret** | The gateway/modelgateway need `DATABASE_URL` (Secret `agentctl-postgres`) and the gateway needs `agentctl-gateway-signing`. | Bundled Postgres + signing Secret come from the Helm chart, or point at an external DSN. |
| **kube-system RoleBinding** to `extension-apiserver-authentication-reader` | CSV `permissions` only create RoleBindings in the install namespace, not in `kube-system`. | Apply the RoleBinding separately (Helm `templates/apiserver.yaml`). Without it the aggregated apiserver can't read the front-proxy CA. |

RBAC note: the chart binds the apiserver SA to the built-in `system:auth-delegator`
ClusterRole. OLM `clusterPermissions` cannot bind a pre-existing ClusterRole, so
the CSV **inlines** the equivalent `create` rules on `tokenreviews` /
`subjectaccessreviews` instead.

### What the CSV *does* install

- The 3 CRDs (`Agent`, `AgentFleet`, `ModelPool`, all `v1alpha1`).
- ServiceAccounts + ClusterRoles + ClusterRoleBindings for the operator,
  apiserver, gateway, modelgateway, and admission.
- Deployments: `operator`, `apiserver`, `gateway`, `modelgateway`, `admission`.

Only the **operator** runs cleanly from the CSV alone; the other four planes
depend on the externally-applied Secrets/registrations above.

## Recommended install (full, wired) — use Helm instead

For a complete, working install, prefer the Helm chart, which installs every
component (including the DaemonSet, APIService, webhook, cert-manager wiring, and
Postgres):

```bash
# cert-manager first (prerequisite)
helm repo add jetstack https://charts.jetstack.io
helm install cert-manager jetstack/cert-manager -n cert-manager \
  --create-namespace --set crds.enabled=true

# then agentctl
helm install agentctl oci://ghcr.io/agentctl-dev/charts/agentctl \
  --namespace agentctl-system --create-namespace
```

Use this OLM bundle when you specifically want to evaluate the operator + CRDs
through OLM/OperatorHub tooling.
