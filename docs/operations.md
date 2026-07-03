# Operations runbook

Install, configure, and run the agentctl control plane in production. agentctl is a
Kubernetes control plane for fleets of conformant AI agents: it provisions, scales,
secures, and exposes agents through Custom Resources. This runbook covers installation,
chart configuration, high availability, observability, and day-2 operations. For the
architecture and the security model, see [`architecture.md`](architecture.md) and
[`security.md`](security.md).

All commands assume the Helm release is `agentctl` in namespace `agentctl-system`; adjust
`-n <namespace>` if you install elsewhere.

## Control-plane components

The control plane is eight Deployments (eight container images). All expose Prometheus
`/metrics`; the port and scheme differ by component.

| Component | Role | Service port | `/metrics` scheme | Default |
|---|---|---|---|---|
| `agentctl-operator` | Reconciles `Agent`/`AgentFleet` into workloads; leader-elected | `8080` (http) | http | always on |
| `agentctl-apiserver` | Aggregated API for management verbs (drain, lame-duck, cancel, pause, resume) | `443` → `6443` (https) | https (mTLS-gated) | `apiserver.enabled: true` |
| `agentctl-admission` | Validating + mutating webhooks | `443` → `8443` (https) | https | `admission.enabled: true` |
| `agentctl-gateway` | A2A surface (Agent Cards, `message/send`, `message/stream`, tasks) | `80` → `8080` (http) | http | `gateway.enabled: true` |
| `agentctl-modelgateway` | Secret-free inference broker (intelligence plane) | `80` → `8080` (http) | http | `modelgateway.enabled: true` |
| `agentctl-mcpgateway` | Secret-free MCP tool broker (tools plane) | `80` → `8080` (http) | http | `mcpgateway.enabled: true` |
| `agentctl-coordination` | Work fabric (`work.*`); claim-fleet backlog | `80` → `8080` (http) | http | `coordination.enabled: false` |
| `agentctl-scaler` | KEDA external scaler; reads the coordination backlog | `80` → `8080` (http) | http | `scaler.enabled: false` |

The reference agent (`agentd`) and any conformant agent run as ordinary pods on the pod
network, serving mTLS HTTPS on `:8443`. They are not part of the control plane and are not
installed by the chart.

---

## 1. Install

### Prerequisites

| Prerequisite | Required? | Why |
|---|---|---|
| **cert-manager** (>= 1.13) | **Required** | Issues every control-plane serving/mTLS certificate and injects the `caBundle` into the aggregated `APIService` and the validating webhook. |
| **KEDA** | Optional | Claim-fleet autoscaling only. Needed when `scaler.enabled=true`; the operator renders a `keda.sh` `ScaledObject` per claim fleet. |
| **NetworkPolicy-capable CNI** (Calico, Cilium, …) | Optional | Tenant isolation. `networkPolicies.enabled=true` ships policies, but a policy-enforcing CNI must apply them — kindnet ignores NetworkPolicies. |
| **Postgres** | Optional | Durable coordination/task/usage state. Bundled single-pod Postgres is the default; in-memory is the single-replica fallback. |

Install cert-manager first:

```sh
helm repo add jetstack https://charts.jetstack.io
helm install cert-manager jetstack/cert-manager -n cert-manager \
  --create-namespace --set crds.enabled=true
```

Every non-core feature is gated and defaults to off, so a stock install ships no
KEDA-, Prometheus-Operator-, or Grafana-coupled objects.

### Helm chart (recommended)

The chart lives at [`charts/agentctl`](../charts/agentctl) and is also published as an OCI
chart. Helm does not reliably own the namespace it installs into, so either pass
`--create-namespace` or pre-create the namespace:

```sh
# From the published OCI chart
helm install agentctl oci://ghcr.io/agentctl-dev/charts/agentctl \
  -n agentctl-system --create-namespace

# Or from a local checkout
helm install agentctl ./charts/agentctl -n agentctl-system --create-namespace
```

Verify the rollout and the aggregated API:

```sh
kubectl -n agentctl-system get pods
kubectl -n agentctl-system rollout status deploy/agentctl-operator
kubectl get apiservice v1alpha1.management.agents.x-k8s.io   # AVAILABLE should read True
```

### `agentctl install` (thin Helm wrapper)

The CLI wraps Helm with two preflight steps Helm cannot do itself: it fails fast if
`helm` is missing or cert-manager is absent, and it creates and labels the install
namespace. It then shells out to `helm upgrade --install`.

```sh
agentctl install -n agentctl-system \
  --registry ghcr.io/agentctl-dev --tag <version> \
  --set gateway.replicas=2

agentctl install --dry-run          # render + validate, no cluster mutation
agentctl uninstall -n agentctl-system
```

`--registry`/`--tag` map to `image.registry`/`image.tag`; `--set KEY=VALUE` is passed to
Helm verbatim (repeatable). The default chart reference is
`oci://ghcr.io/agentctl-dev/charts/agentctl`; pass `--chart ./charts/agentctl` for a local
path.

### Kustomize (raw manifests)

The [`deploy/`](../deploy) tree holds raw per-component manifests and kustomize overlays,
plus the generated CRDs. Use it for development, overlays, or when you need to understand
each object. cert-manager is still a hard prerequisite. The Helm chart is the supported
production path — the raw manifests do not wire TLS, caBundle injection, or Postgres for
you the way the chart does.

### OLM bundle (preview)

The [`bundle/`](../bundle) directory is an Operator Lifecycle Manager bundle (alpha /
preview) that installs the operator and the CRDs through OLM tooling. OLM's `deployment`
install strategy cannot carry the aggregated `APIService`, the webhook registration, or
the cert-manager `Certificate`/`Issuer` objects, so those must be applied separately.
Prefer the Helm chart for a complete, wired install; use the bundle only to evaluate the
operator through OperatorHub tooling.

### CRDs

The chart installs the four CRDs in [`charts/agentctl/crds/`](../charts/agentctl/crds) on
first install — `agents`, `agentfleets`, `modelpools`, and `mcpserversets`, all in group
`agents.x-k8s.io`, version `v1alpha1`. Helm intentionally never updates `crds/` on
`helm upgrade`; see [Upgrades & CRDs](#6-upgrades--crds) for the upgrade procedure.

---

## 2. Configuration

Configure the chart through values. The table below lists the load-bearing knobs; see
[`charts/agentctl/values.yaml`](../charts/agentctl/values.yaml) for the full set and the
per-component scheduling/resource/env passthroughs (`nodeSelector`, `affinity`,
`tolerations`, `topologySpreadConstraints`, `priorityClassName`, `podAnnotations`,
`podLabels`, `extraEnv`, `envFrom`, `serviceAccount.annotations`).

| Value | Default | Effect / gate |
|---|---|---|
| `image.registry` | `""` | Empty pulls local (kind-loaded) `agentctl/<comp>` names; set `ghcr.io/agentctl-dev` for the published images. |
| `image.tag` | `dev` | Image tag for every component. Pin by digest via `image.digests` for reproducible installs. |
| `certManager.enabled` | `true` | **Required.** Creates a self-signed bootstrap Issuer → CA → CA Issuer and issues every serving/mTLS cert. |
| `certManager.caIssuerRef` | `""` | Use an existing cluster CA `ClusterIssuer` instead of the self-signed bootstrap. |
| `postgres.mode` | `bundled` | `bundled` (single-pod Postgres) or `external` (managed DSN Secret). |
| `postgres.bundled.storage` | `emptyDir` | `emptyDir` (eval — data lost on pod restart) or `pvc` (durable, `pvcSize` default `5Gi`). |
| `postgres.bundled.tls.enabled` | `false` | Encrypt the in-cluster Postgres hop (`sslmode=require`). Requires cert-manager. |
| `postgres.bundled.tls.verifyFull` | `false` | CA-pin + hostname-verify the hop (`sslmode=verify-full`). Requires `tls.enabled`. |
| `postgres.external.dsnSecretName` | `""` | `mode=external`: pre-created Secret holding `DATABASE_URL`. |
| `coordination.enabled` | `false` | The work fabric (`work.*`). Enable for claim-mode fleets. |
| `coordination.store` | `memory` | `memory` (single-replica, in-process) or `postgres` (durable + shared, HA-capable). |
| `coordination.attestIdentity` | `true` | Bind each claim to the caller's source-IP-attested pod identity (blocks cross-tenant ack/release). |
| `coordination.mtls.enabled` | `false` | Mutually authenticate the scaler → coordination backlog hop. Requires cert-manager. |
| `scaler.enabled` | `false` | KEDA external scaler for claim-depth autoscaling. Also flips the operator's `SCALER_ENABLED` on. **Requires KEDA.** |
| `modelgateway.attestIdentity` | `true` | Derive the agent namespace from the caller's source IP (not the spoofable `X-Agent-Namespace` header). Set `false` only for a trusted single-tenant install. |
| `modelgateway.secretsNamespaces` | `[]` | Namespaces the ModelGateway may read provider-credential Secrets in. Empty = cluster-wide read (dev). Scope in production. |
| `mcpgateway.secretsNamespaces` | `[]` | Namespaces the MCPGateway may read tool-credential Secrets in. Empty = cluster-wide read (dev). The MCPGateway is always source-IP-attested. |
| `apiToken.enabled` | `false` | In-cluster bearer gate (`AGENTCTL_API_TOKEN`) on the coordination server, ModelGateway, A2A gateway, and scaler. |
| `trustedProxy.enabled` | `false` | Second mTLS listener on the gateway (`:8443`) that trusts a fronting proxy's asserted identity headers. |
| `networkPolicies.enabled` | `false` | Ship default-deny + sanctioned-flow policies. `networkPolicies.agentNamespaces` lists tenant namespaces. Needs a policy CNI. |
| `metrics.serviceMonitor.enabled` | `false` | Render one Prometheus-Operator `ServiceMonitor` per component. |
| `observability.dashboards.enabled` | `false` | Render the Grafana dashboard ConfigMap. |
| `observability.alerts.enabled` | `false` | Render the `PrometheusRule`. Needs the Prometheus-Operator CRDs. |
| `operator.replicas` | `1` | Leader-elected; raise for warm-standby HA (see [High availability](#3-high-availability)). |
| `<comp>.autoscaling.enabled` | `false` | HPA for `apiserver`, `gateway`, `modelgateway`. When on, the HPA owns replicas and `<comp>.replicas` is ignored. |
| `<comp>.pdb.enabled` | `false` | PodDisruptionBudget for `operator`, `apiserver`, `gateway`, `modelgateway`, `admission`, `coordination`, `scaler`. |
| `namespace.create` | `false` | Whether the chart renders the namespace object. |
| `admission.allowedRegistries` | `agentd:,mock-agent,agentctl/,gcr.io/,registry.k8s.io/,ghcr.io/` | CSV image-registry prefix allow-list the webhook enforces (empty = allow all). |

### Identity and attestation

Two identity mechanisms are enforced cryptographically and are on by default:

- **Outbound (agent → gateway).** The ModelGateway (`modelgateway.attestIdentity`, default
  `true`), MCPGateway (always on), and coordination server (`coordination.attestIdentity`,
  default `true`) resolve the caller's **source pod IP** to the owning pod via the
  Kubernetes API and derive its namespace/identity from that — never from a self-asserted
  request header. Confined tenant pods drop `CAP_NET_RAW` and so cannot spoof their source
  IP, so one tenant cannot bill another's ModelPool budget or ack/release another's claim.
  These paths require the cluster-wide `pods get/list` grant the chart renders for them.

- **Inbound (control plane → agent).** The aggregated apiserver and A2A gateway dial the
  agent pod directly over mTLS, presenting the control-plane client certificate that
  authenticates them as the **Management** origin — the only origin the agent accepts for
  management/A2A. The trust anchor is the cluster CA, not DNS.

Turn attestation off (`modelgateway.attestIdentity=false`, `coordination.attestIdentity=false`)
only for a deliberately trusted single-tenant install; the header/self-asserted fallbacks are
spoofable by any in-cluster pod.

### Inbound A2A authentication

The gateway enforces inbound auth by one of three mechanisms. Two are chart-level:

- **`apiToken.enabled`** — a single coarse in-cluster bearer token (`AGENTCTL_API_TOKEN`).
  The chart mints a `keep`-policy Secret `agentctl-api-token` and wires it into the
  coordination server, ModelGateway, A2A gateway, and scaler; those services then require
  `Authorization: Bearer <token>`. The operator injects it into agent pods **only in the
  control-plane namespace** (a `secretKeyRef` cannot cross namespaces) — replicate the
  Secret into other tenant namespaces to gate agents there. Read the token with:

  ```sh
  kubectl -n agentctl-system get secret agentctl-api-token \
    -o jsonpath='{.data.AGENTCTL_API_TOKEN}' | base64 -d
  ```

- **`trustedProxy.enabled`** — a fronting API gateway (e.g. APISIX) terminates edge auth
  and asserts the verified identity as `<prefix>-subject/-email/-groups` headers (prefix
  `trustedProxy.headerPrefix`, default `x-agentctl`). The gateway honors those headers
  **only** over the mTLS listener on `:8443`, verifying the proxy's client cert against the
  agentctl CA and `trustedProxy.allowedNames`; on any other path it strips the headers so
  they cannot be self-asserted.

Per-agent **OIDC** inbound policy is not a chart value — it is configured on each `Agent`
via `spec.access.oidc` (issuer, audience, `requiredClaims`) and validated by the admission
webhook. See [`security.md`](security.md) for the full trust model.

### Postgres TLS

The gateway and modelgateway connect with a rustls client; the DSN's `sslmode` selects the
transport:

| Mode | Behavior |
|---|---|
| `sslmode=disable` | Plaintext; relies on in-cluster NetworkPolicy scope. Default for the bundled store. |
| `sslmode=require` | Encrypted, server cert **not** verified. Set by `postgres.bundled.tls.enabled=true`. |
| `sslmode=verify-full` | Encrypted, server cert CA-pinned and hostname-verified. Set by `postgres.bundled.tls.verifyFull=true` (reads the CA from `DB_CA_FILE`/`PGSSLROOTCERT`). |

For a managed Postgres (`mode=external`), put the desired `sslmode` in the DSN Secret and
supply the CA per your provider's process.

---

## 3. High availability

### Operator leader election

The operator is a level-triggered singleton: two reconcile loops at once would race
server-side applies. Every replica contends for a `coordination.k8s.io` **Lease** named
`agentctl-operator` in the control-plane namespace; only the holder runs the controllers.
Raise `operator.replicas` above 1 for warm standbys — this adds failover speed, not
reconcile throughput.

- **Lease timings:** lease duration 15s, renew every 10s, retry every 2s. A standby takes
  over once `renewTime + 15s` has passed, so worst-case reconcile handoff is bounded at
  roughly the lease duration.
- **Probes:** liveness (`/healthz`) is 200 on every replica, leader or standby, so the
  kubelet never kills a healthy standby. Readiness (`/readyz`) is 200 once the manager is
  up and participating — **not** gated on holding leadership — so a `RollingUpdate` does
  not deadlock and standbys stay `Ready`.
- **Metrics reachability:** the operator Service sets `publishNotReadyAddresses: true` so
  `/metrics` is scrapeable on every replica. Exactly one replica reports
  `agentctl_operator_leader 1`.
- **Loss of leadership:** if the leader cannot renew within the lease duration (apiserver
  unreachable) or a peer takes over, the process exits and Kubernetes restarts it to rejoin
  the election — guaranteeing two reconcile loops never run at once.

Enable a PodDisruptionBudget so node drains keep a replica up:

```sh
helm upgrade agentctl ./charts/agentctl -n agentctl-system --reuse-values \
  --set operator.replicas=2 --set operator.pdb.enabled=true
```

### Stateless components

`apiserver`, `gateway`, `modelgateway`, and `mcpgateway` are stateless (state lives in
Postgres). Raise `<comp>.replicas`, or enable a CPU HPA for `apiserver`, `gateway`, and
`modelgateway`:

```sh
helm upgrade agentctl ./charts/agentctl -n agentctl-system --reuse-values \
  --set gateway.autoscaling.enabled=true \
  --set gateway.autoscaling.minReplicas=2 \
  --set gateway.autoscaling.maxReplicas=10
```

When the HPA is enabled it owns the replica count and `<comp>.replicas` is ignored. The
`admission`, `coordination`, and `scaler` components have no CPU HPA — scale them with
`<comp>.replicas`.

### Durable shared state

For HA, back shared state with Postgres and replicate the durable components:

- **A2A tasks, push configs, token usage** persist to Postgres. The gateway and modelgateway
  are stateless, so they scale horizontally against a shared DB. For HA/DR use an external
  managed Postgres (`postgres.mode=external`) — the bundled Postgres is a single pod with no
  replication or failover.
- **Coordination.** The default `coordination.store=memory` keeps the claim queue in
  process — keep `coordination.replicas=1`. Set `coordination.store=postgres` to make the
  queue durable and shared, then raise `coordination.replicas` for HA:

  ```sh
  helm upgrade agentctl ./charts/agentctl -n agentctl-system --reuse-values \
    --set coordination.enabled=true \
    --set coordination.store=postgres \
    --set coordination.replicas=2
  ```

The bundled Postgres Secret and PVC, and the gateway signing Secret
(`agentctl-gateway-signing`), carry `helm.sh/resource-policy: keep` — they survive
`helm uninstall` and are reused (via `lookup`) across upgrades, so DB auth and Agent-Card
JWS signatures stay stable. Back up the signing Secret and the cert-manager CA
(`agentctl-ca-key`) as part of DR.

---

## 4. Observability

### Metrics

Every component serves hand-rolled Prometheus text at `/metrics`. Scrape targets and metric
families:

| Component | Endpoint | Key metric families |
|---|---|---|
| operator | `:8080` http | `agentctl_operator_leader`, `agentctl_operator_reconcile_total`, `agentctl_operator_reconcile_errors_total`, `agentctl_operator_reconcile_duration_seconds` |
| apiserver | `:6443` https (behind the front-proxy mTLS gate) | `agentctl_apiserver_verb_forwarded_total`, `agentctl_apiserver_verb_denied_total` |
| gateway | `:8080` http | `agentctl_gateway_rpc_requests_total`, `_stream_requests_total`, `_card_requests_total`, `_tasks_total`, `_upstream_errors_total` |
| modelgateway | `:8080` http | `agentctl_modelgateway_infer_requests_total`, `_infer_errors_total`, `_tokens_total`, `_budget_rejections_total` |
| admission | `:8443` https | `agentctl_admission_admit_total`, `_deny_total`, `_reviews_total`, `_mutations_total`, `_mutations_patched_total` |
| mcpgateway / coordination / scaler | `:8080` http | component `/metrics` |

The apiserver serves `/metrics` on the same `:6443` mTLS surface as the API — only a
CA-signed client cert can scrape it (the ServiceMonitor scrapes `scheme: https` with
`insecureSkipVerify`).

### ServiceMonitor, dashboard, and alerts

The chart ships three opt-in observability objects. All require the corresponding cluster
add-on:

```sh
helm upgrade agentctl ./charts/agentctl -n agentctl-system --reuse-values \
  --set metrics.serviceMonitor.enabled=true \
  --set observability.dashboards.enabled=true \
  --set observability.alerts.enabled=true \
  --set observability.alerts.labels.release=prometheus
```

- **`metrics.serviceMonitor.enabled`** renders one `ServiceMonitor` per enabled component
  (scrape job `agentctl-<comp>`, interval `30s`). Requires the Prometheus-Operator CRDs.
  Use `metrics.serviceMonitor.labels` to match your Prometheus `serviceMonitorSelector`.
- **`observability.dashboards.enabled`** renders the Grafana dashboard as a ConfigMap
  labeled `grafana_dashboard: "1"` for the Grafana sidecar's auto-discovery (no Grafana
  CRDs needed). The dashboard covers the operator (reconcile/error rate, latency, leader),
  the A2A gateway (per-surface request + upstream-error rates), the ModelGateway (inference,
  tokens, budget rejections), and the admission webhook (admit/deny, reviews, mutations).
- **`observability.alerts.enabled`** renders the `PrometheusRule`. Requires the
  Prometheus-Operator CRDs. Set `observability.alerts.labels` to whatever your Prometheus
  `ruleSelector` matches (kube-prometheus-stack defaults to `release: <name>`).

Shipped alert rules:

| Alert | Severity | Expression |
|---|---|---|
| `AgentctlOperatorNoLeader` | critical (5m) | `sum(agentctl_operator_leader) == 0` |
| `AgentctlReconcileErrors` | warning (10m) | `sum(rate(agentctl_operator_reconcile_errors_total[5m])) > 0.1` |
| `AgentctlComponentDown` | critical (5m) | `(up{job=~"agentctl-.*"} == 0) or (absent(up{job="agentctl-operator"}) == 1)` |
| `AgentctlAdmissionDenySpike` | warning (10m) | `sum(rate(agentctl_admission_deny_total[5m])) > 1` |
| `AgentctlBudgetRejections` | warning (15m) | `sum(rate(agentctl_modelgateway_budget_rejections_total[5m])) > 0` |

### Distributed tracing (OTLP)

Tracing is off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set (the default is fmt-only logging
with byte-identical output). Enable the OTLP/gRPC exporter and W3C `traceparent` propagation
per component via `extraEnv`:

```yaml
apiserver:
  extraEnv:
    - name: OTEL_EXPORTER_OTLP_ENDPOINT
      value: http://otel-collector.observability.svc:4317
```

The apiserver injects `traceparent` into its management calls to the agent pod, so an agent
run joins the operator's trace when tracing is on.

---

## 5. Day-2 operations

### Management verbs

The aggregated apiserver serves five lifecycle verbs under `management.agents.x-k8s.io` for
both `agents` and `agentfleets`:

| Verb | Effect |
|---|---|
| `drain` | Stop accepting new work and finish in-flight tasks. |
| `lame-duck` | Mark the agent unhealthy for load-balancing without stopping it. |
| `cancel` | Cancel in-flight work. |
| `pause` | Suspend processing. |
| `resume` | Resume a paused agent. |

Each request is a `create` on the `<resource>/<verb>` connect subresource. The apiserver
authorizes it via a `SubjectAccessReview`, then dials the target pod(s) directly over mTLS
as the Management origin. For an `AgentFleet` the verb fans out to **every** Running replica
and returns a partial-success `Status` (`207` when some replicas failed).

```text
kubectl ──create──▶ kube-apiserver ──aggregates──▶ agentctl-apiserver
                                                     │  1. verify front-proxy client cert
                                                     │  2. SubjectAccessReview(user, verb)
                                                     │  3. dial pod(s) over mTLS  ──▶ agent :8443 /mcp
```

Invoke a verb with `kubectl create --raw` against the subresource path:

```sh
# Drain a single Agent
kubectl create --raw \
  /apis/management.agents.x-k8s.io/v1alpha1/namespaces/default/agents/my-agent/drain \
  -f /dev/null

# Pause an entire AgentFleet (fans out to all replicas)
kubectl create --raw \
  /apis/management.agents.x-k8s.io/v1alpha1/namespaces/default/agentfleets/workers/pause \
  -f /dev/null
```

Access is RBAC-gated. Grant a role the verb by allowing `create` on the subresource in the
`management.agents.x-k8s.io` group:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: agent-operator
rules:
  - apiGroups: ["management.agents.x-k8s.io"]
    resources:
      - agents/drain
      - agents/lame-duck
      - agents/cancel
      - agents/pause
      - agents/resume
      - agentfleets/drain
      - agentfleets/lame-duck
      - agentfleets/cancel
      - agentfleets/pause
      - agentfleets/resume
    verbs: ["create"]
```

A subject without a matching binding is denied by the `SubjectAccessReview` (`403`).

### Scaling fleets

`AgentFleet` exposes the Kubernetes **scale subresource** mapped to `.spec.replicas`, so any
workload autoscaler or `kubectl scale` can drive it:

```sh
kubectl scale agentfleet/workers --replicas=3
```

- **Claim mode** renders a Deployment. With the scaler off, `.spec.replicas` sets the count.
  With `coordination.enabled` and `scaler.enabled`, KEDA's HPA owns the count and drives it
  from the coordination backlog — including scale-from-zero:

  ```sh
  helm upgrade agentctl ./charts/agentctl -n agentctl-system --reuse-values \
    --set coordination.enabled=true --set scaler.enabled=true
  ```

  The operator renders a `keda.sh` `ScaledObject` per claim fleet, applied best-effort: if
  the KEDA CRDs are absent it logs the failure and sets a `ScaledObject=False` condition on
  the fleet **without failing the workload** — the fleet still runs, just without
  claim-depth autoscaling.

- **Shard mode** renders a StatefulSet of `scaling.shards` fixed hash partitions. Changing
  the shard count is a guarded, stop-the-world rebalance, not an elastic scale.

The coordination server is reachable in-cluster at `http://agentctl-coordination.<ns>.svc/`
(or the mTLS listener on `:8443` when `coordination.mtls.enabled`), and its backlog is the
scale-from-zero signal.

### Rolling a component

Restart a Deployment to pick up a rotated Secret or recover after a Postgres restore:

```sh
kubectl -n agentctl-system rollout restart \
  deploy/agentctl-gateway deploy/agentctl-modelgateway
```

---

## 6. Upgrades & CRDs

### Chart upgrade

```sh
helm upgrade agentctl ./charts/agentctl -n agentctl-system \
  --reuse-values --set image.tag=<new>
```

Upgrades are idempotent: the bundled Postgres password and the gateway signing seed are read
from their existing Secrets via `lookup` and reused, not regenerated, so DB auth and JWS
signatures survive every upgrade. `keep`-policy Secrets and the Postgres PVC are never
deleted by `helm uninstall`.

### CRDs are additive

Helm installs the CRDs in `charts/agentctl/crds/` on **first install only** and never
updates them on `helm upgrade`. After a chart upgrade that changes a CRD schema, printer
column, or subresource, apply the CRDs by hand:

```sh
kubectl apply -f charts/agentctl/crds/
```

The CRDs are single-version `v1alpha1`, so `kubectl apply` of the new schema is sufficient —
there is no multi-version conversion. Never `kubectl delete` a CRD to refresh it: deleting a
CRD cascade-deletes every `Agent`/`AgentFleet`/`ModelPool`/`MCPServerSet` instance.

### Rollback

```sh
helm history agentctl -n agentctl-system
helm rollback agentctl <REVISION> -n agentctl-system
```

`helm rollback` reverts the templated objects but, mirroring upgrade, does not touch CRDs or
`keep`-policy Secrets/PVC. The gateway migrates the DB schema forward on startup, so rolling
back to an older binary across a schema migration is not guaranteed safe — prefer rolling
forward, and restore Postgres from a pre-upgrade dump if you must go back.

---

## See also

- [`architecture.md`](architecture.md) — components, planes, and the Agent Control Contract.
- [`security.md`](security.md) — identity, attestation, tenant isolation, and the trust model.
- [`charts/agentctl/values.yaml`](../charts/agentctl/values.yaml) — the full value reference.
</content>
</invoke>
