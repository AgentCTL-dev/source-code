# deploy/

Install artifacts for agentctl and a local end-to-end walkthrough.

```
deploy/
  crds/                  # generated CRDs (cargo run -p agentctl-crdgen)
    agent.yaml
    agentfleet.yaml
  examples/
    agent-once.yaml      # a minimal once-mode Agent
```

## Local end-to-end (kind)

This runs the operator **out of cluster** against a kind cluster — the fastest
loop. (In-cluster Helm/Kustomize packaging for the operator + node-agent +
RBAC is a later step.)

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

**Note on the example image.** `agent-once.yaml` uses `busybox` as a placeholder —
there is no real conformant-agent image in this repo yet, so the rendered Job's
pod will fail (busybox does not understand the agent args). That is expected: the
walkthrough proves the *control plane* (reconcile → render → apply → status →
GC), not a running agent. Point `.spec.image` at a real conformant-agent image to
run an actual agent.
