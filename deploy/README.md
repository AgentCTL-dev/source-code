# deploy/

Raw, per-component Kubernetes manifests for agentctl plus a local end-to-end
walkthrough. Use this directory for development, Kustomize overlays, and reading
each object in isolation.

> **For a production install, use the Helm chart: [`charts/agentctl`](../charts/agentctl/README.md).**
> One `helm install` brings up every control-plane component and issues all TLS
> (the aggregated APIServer serving cert, the admission webhook cert, each
> agent's serving identity, and the A2A gateway's cert) through **cert-manager**
> with automatic `caBundle` injection and rotation.

## Layout

```
deploy/
  crds/                    # generated CRDs — regenerate with `cargo run -p agentctl-crdgen`
    agent.yaml
    agentfleet.yaml
    modelpool.yaml
  operator/                # in-cluster operator install (namespace + RBAC + Deployment)
    namespace.yaml
    rbac.yaml              # ServiceAccount + least-privilege ClusterRole(+Binding)
    deployment.yaml
    Dockerfile             # distroless runtime over the host-built release binary
    kustomization.yaml
  apiserver/               # aggregated APIServer (management verbs): Deployment + RBAC + APIService
  admission/               # validating + mutating webhooks: Deployment + RBAC + webhook config
  gateway/                 # A2A gateway: Deployment + RBAC + signing Secret
  coordination/            # work-distribution MCP server (Dockerfile)
  scaler/                  # KEDA external scaler (Dockerfile)
  postgres/                # bundled durable store for gateway/coordination
  hardening/               # NetworkPolicies (default-deny + control-plane/DNS + internet egress) + mTLS helper
  examples/                # sample CRs: Agent, AgentFleet, ModelPool, and mock fixtures
```

The `crds/` files are generated from the Rust CRD types in the `agent-api` crate.
Never hand-edit them — run `cargo run -p agentctl-crdgen` to regenerate, and CI
fails on drift.

## In-cluster operator install

Run the operator inside the cluster with its own ServiceAccount and RBAC:

```console
# build + load the operator image into kind
cargo build --release -p agentctl-operator --bin agentctl-operator
docker build -f deploy/operator/Dockerfile -t agentctl/operator:dev .
kind load docker-image agentctl/operator:dev --name agentctl

# install CRDs + operator
kubectl apply -f deploy/crds/
kubectl apply -k deploy/operator/
kubectl -n agentctl-system rollout status deploy/agentctl-operator

# use it
kubectl apply -f deploy/examples/agent-once.yaml
kubectl get agents
kubectl logs -n agentctl-system deploy/agentctl-operator
```

The `deploy/operator/` overlay is the only self-contained Kustomize install here.
The other components (`apiserver/`, `admission/`, `gateway/`, `coordination/`,
`postgres/`, `hardening/`) carry raw manifests you can apply individually, but
they depend on cert-manager-issued TLS Secrets and shared configuration that the
Helm chart wires for you. For a complete control plane, install the chart.

## Local end-to-end (kind)

The fastest development loop runs the operator **out of cluster** against a kind
cluster, reading your kubeconfig:

```console
# 1. cluster + CRDs
kind create cluster --name agentctl
cargo run -p agentctl-crdgen          # (re)generate deploy/crds/
kubectl apply -f deploy/crds/

# 2. run the operator (reads your kubeconfig)
cargo run -p agentctl-operator

# 3. create an Agent and watch it reconcile
kubectl apply -f deploy/examples/agent-once.yaml
kubectl get agent demo -o jsonpath='{.status}'   # Ready=True / phase=Ready
kubectl get jobs                                  # the operator rendered a Job
cargo run -p agentctl-cli -- get                  # NAME MODE READY PHASE AGE
cargo run -p agentctl-cli -- describe demo

# 4. deletion → finalizer + owner-ref GC reclaim the Job
kubectl delete agent demo
kubectl get jobs                                  # gone

# teardown
kind delete cluster --name agentctl
```

**Note on the example image.** `agent-once.yaml` uses `busybox` as a placeholder.
There is no conformant-agent image in this repo, so the rendered Job's pod will
fail (busybox does not understand the agent arguments). That is expected: the
walkthrough exercises the *control plane* — reconcile, render, apply, status, and
garbage collection — not a running agent. Point `.spec.image` at a real
conformant agent (the reference implementation is `agentd`) to run a live agent.

## Related documentation

- [`charts/agentctl/README.md`](../charts/agentctl/README.md) — the production Helm install.
- [`docs/architecture.md`](../docs/architecture.md) — components, planes, and how they fit together.
- [`docs/operations.md`](../docs/operations.md) — day-2 operations and management verbs.
- [`docs/security.md`](../docs/security.md) — the identity, isolation, and hardening model.
