# agentctl — operations runbook (day-2)

Operational procedures for a running agentctl control plane installed via the Helm
chart at [`charts/agentctl`](../charts/agentctl). This is the day-2 companion to the
chart `README.md` (install/values) and [`STATUS.md`](STATUS.md) (what is built).

All commands assume the release is `agentctl` in namespace `agentctl-system`
(`-n agentctl-system`); adjust if you installed elsewhere. The control-plane
components are: `agentctl-operator` (Deployment, leader-elected), `agentctl-node-agent`
(DaemonSet), `agentctl-apiserver`, `agentctl-gateway`, `agentctl-modelgateway`,
`agentctl-admission` (Deployments), and `agentctl-postgres` (bundled Deployment, or an
external managed Postgres). State lives in **Postgres** (A2A tasks, push configs, token
usage) and in a handful of **secrets** (signing key, cert-manager CA + leaf certs).

---

## 1. Backup & restore

### What actually holds state

| Object | Kind | Carries `helm.sh/resource-policy: keep`? | Back up? |
|---|---|---|---|
| `agentctl-postgres` (bundled) | Deployment + Secret + Service | Secret: **yes** | DB dump + the Secret |
| `agentctl-postgres` PVC (`storage=pvc`) | PersistentVolumeClaim | **yes** | volume snapshot |
| `agentctl-gateway-signing` | Secret (Ed25519 JWS seed) | **yes** | yes — critical |
| `agentctl-ca-key` | Secret (cert-manager CA, 10y) | no (cert-manager owned) | yes — critical |
| `agentctl-apiserver-tls` / `-admission-tls` / `-node-agent-tls` / `-client-tls` (and `-trusted-proxy-tls` / `-trusted-proxy-client-tls` when `trustedProxy.enabled`) | Secrets (leaf certs) | no | optional (re-mintable from the CA) |

Everything else (Deployments, RBAC, CRD instances) is declarative and recreated by
`helm upgrade` + `kubectl apply` of your CRs, so it does not need a separate backup.

### Bundled Postgres — logical dump / restore

The bundled Postgres is a **single pod**, user/db `agentctl`. Dump and restore with
`pg_dump`/`pg_restore` exec'd into the pod.

```sh
# Resolve the pod
PG=$(kubectl -n agentctl-system get pod -l app.kubernetes.io/name=agentctl-postgres \
  -o jsonpath='{.items[0].metadata.name}')

# --- Backup (custom format, compressed; password is in the pod's env) ---
kubectl -n agentctl-system exec "$PG" -- \
  sh -c 'PGPASSWORD="$POSTGRES_PASSWORD" pg_dump -U agentctl -d agentctl -Fc' \
  > agentctl-$(date +%F).dump

# --- Restore into a fresh/empty DB (clean + recreate objects) ---
cat agentctl-2026-06-29.dump | kubectl -n agentctl-system exec -i "$PG" -- \
  sh -c 'PGPASSWORD="$POSTGRES_PASSWORD" pg_restore -U agentctl -d agentctl --clean --if-exists'
```

Notes:
- The gateway + modelgateway are **stateless**; restoring the DB is sufficient to
  recover A2A task history, push-notification configs, and token-usage/budget counters.
- After a restore, restart `agentctl-gateway` and `agentctl-modelgateway` so they
  re-read a consistent schema: `kubectl -n agentctl-system rollout restart deploy/agentctl-gateway deploy/agentctl-modelgateway`.
- Take the dump while the pod is `Ready` (`pg_isready` readiness probe). For a
  consistent point-in-time dump, `pg_dump -Fc` is transactionally consistent on its own.

### Bundled Postgres — PVC volume snapshot (`storage=pvc`)

When `postgres.bundled.storage=pvc`, data lives in the `agentctl-postgres` PVC
(`helm.sh/resource-policy: keep`, ReadWriteOnce, default size `5Gi`). In addition to
(or instead of) logical dumps you can take a **CSI VolumeSnapshot** if your
StorageClass driver supports it:

```sh
cat <<'EOF' | kubectl apply -f -
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: agentctl-postgres-snap
  namespace: agentctl-system
spec:
  # volumeSnapshotClassName: <your-csi-snapclass>
  source:
    persistentVolumeClaimName: agentctl-postgres
EOF
```

Restore by provisioning a new PVC `dataSource`'d from the snapshot, or fall back to the
`pg_restore` path above (driver-portable). Because the PVC and both secrets carry
`helm.sh/resource-policy: keep`, a `helm uninstall` does **not** delete them — reclaim by
hand (`kubectl delete pvc/secret …`) when you want a clean slate.

### Signing key + cert-manager secrets

- **`agentctl-gateway-signing`** (`helm.sh/resource-policy: keep`) holds the 32-byte
  Ed25519 seed the gateway uses to JWS-sign Agent Cards and serve `/.well-known/jwks.json`.
  Losing it invalidates every previously published/cached card signature. Back it up:
  `kubectl -n agentctl-system get secret agentctl-gateway-signing -o yaml > gateway-signing.yaml`.
  On `helm upgrade` the chart reuses the existing seed via `lookup` (it is **not**
  regenerated), so cards verify consistently across replicas and upgrades.
- **`agentctl-ca-key`** is the cert-manager CA (10y `duration`, `renewBefore: 720h`).
  Back it up — restoring it lets cert-manager re-mint all leaf certs without re-trusting a
  new CA across the APIService/webhook caBundles. The leaf-cert secrets
  (`agentctl-apiserver-tls`, `-admission-tls`, `-node-agent-tls`, `-client-tls`) are
  re-issued automatically from the CA, so backing them up is optional.
- If you supply your own `certManager.caIssuerRef`, the CA lives in your issuer and is
  out of agentctl's backup scope — back it up per your PKI process.

### External Postgres

When `postgres.mode=external`, agentctl holds **no** database state of its own — it only
consumes the DSN from your pre-created Secret (`postgres.external.dsnSecretName`, key
`DATABASE_URL`). Backup, PITR, and restore are the **managed provider's** responsibility;
follow your provider's runbook (snapshots / WAL archiving). Still back up
`agentctl-gateway-signing` and the cert-manager CA as above.

### Postgres TLS (`sslmode`)

The gateway + modelgateway connect with a **rustls/ring** client (no aws-lc-rs, no C
toolchain). The DSN's `sslmode` selects the transport:

- `sslmode=disable` (the default for the bundled store) — plain `NoTls`; the hop relies
  on the in-cluster NetworkPolicy scope.
- `sslmode=require` / `prefer` — the connection is **encrypted** but the server cert is
  **not** CA-verified (libpq `require` semantics, distinct from `verify-ca`/`verify-full`).
- `sslmode=verify-full` — the client verifies the server cert **chains to a pinned CA**
  **and** the hostname matches a cert SAN (the strongest mode). The rustls client reads the
  CA from `DB_CA_FILE` / `PGSSLROOTCERT`.

**Bundled TLS (opt-in).** Set `postgres.bundled.tls.enabled=true` to encrypt the bundled
hop: the chart issues a cert-manager `Certificate` (`agentctl-postgres-tls`, signed by the
chart CA) for `agentctl-postgres.<ns>.svc[.cluster.local]`, mounts it at `/tls` (the key is
`root:<fsGroup>` mode `0640`, readable by the postgres uid), runs postgres with `ssl=on`,
and flips `DATABASE_URL` to `sslmode=require`. Requires cert-manager (`certManager.enabled`,
the default).

**Bundled `verify-full` (CA pinning, opt-in).** Set `postgres.bundled.tls.verifyFull=true`
(with `tls.enabled=true`) to pin the chart CA. The chart then projects the cert's `ca.crt`
into the gateway + modelgateway (and the coordination server when
`coordination.store=postgres`) at `/etc/agentctl-pg-ca/ca.crt`, sets `DB_CA_FILE` +
`PGSSLROOTCERT` to that path, and flips `DATABASE_URL` to `sslmode=verify-full` against the
`agentctl-postgres.<ns>.svc` host (covered by the cert SANs, so hostname verification
passes). This closes the encrypt-without-verify gap for the bundled store. Default off keeps
`sslmode=require`.

**External TLS.** For a managed Postgres, put the desired mode in your DSN Secret. The
client encrypts on `sslmode=require`/`prefer`, and CA-pins on `sslmode=verify-full` when the
DSN carries it and you provide the CA via `DB_CA_FILE`/`PGSSLROOTCERT` (mount it via the
component's `extraEnv` + a `volumes`/`volumeMounts` overlay, or use the provider's
system-trust CA). For the in-cluster bundled store prefer `postgres.bundled.tls.verifyFull`.

---

## 2. Upgrade & rollback

### Helm upgrade (idempotent secrets)

```sh
helm upgrade agentctl ./charts/agentctl -n agentctl-system \
  --reuse-values --set image.tag=<new> [--set ...]
```

Upgrades are safe to re-run: the bundled Postgres password and the gateway signing seed
are read from the existing Secret via `lookup` and **reused**, not regenerated — so DB
auth and JWS signatures survive every upgrade (regenerating them previously broke DB auth
and card verification, which is why this is `lookup`-guarded).

### CRD caveat (Helm does not upgrade CRDs)

Helm installs the CRDs in `charts/agentctl/crds/` (`agents`, `agentfleets`, `modelpools`
in group `agents.x-k8s.io`, version **`v1alpha1`**) **only on first install**. Helm
deliberately **never updates `crds/` on `helm upgrade`**. After a chart upgrade that
changes a CRD schema/printer-column/subresource, apply them by hand:

```sh
kubectl apply -f charts/agentctl/crds/
```

Versioning/conversion note: the CRDs are single-version `v1alpha1`, so there is no served
multi-version conversion today and `kubectl apply` of the new schema is sufficient. A
**conversion webhook** would be required only once a second served version is introduced —
that is deferred work (see §6). Until then, never `kubectl delete` a CRD to "refresh" it:
deleting the CRD cascades-deletes every `Agent`/`AgentFleet`/`ModelPool` instance.

### Rollback

```sh
helm rollback agentctl <REVISION> -n agentctl-system   # helm history agentctl to list
```

`helm rollback` reverts the templated objects (Deployments, RBAC, Services, certs config)
but, mirroring upgrade, does **not** touch CRDs or the `keep`-policy secrets/PVC. If a
rollback crosses a CRD-schema change, re-apply the matching `crds/` for that revision by
hand. The DB schema is migrated forward by the gateway on startup; a rollback to an older
binary that expects an older schema is **not** guaranteed safe — prefer rolling forward,
and restore the DB from a pre-upgrade dump if you must go back across a schema migration.

### Operator HA rollout behaviour

The operator runs leader-elected against a `coordination.k8s.io` Lease named
`agentctl-operator` (acquire 15s / renew 10s, holder = pod name). During a rolling
upgrade the old leader's pod terminates and a standby (or the new pod) acquires the Lease;
reconciliation resumes after roughly the **lease duration** (~15s worst case) — the gap is
bounded leader handoff, not an outage. Run `operator.replicas: >= 2` for warm standbys so
handoff is fast. Readiness (`/readyz`) reflects manager-up (not leadership), so HA
standbys stay `Ready` and a RollingUpdate does not deadlock; the operator Service uses
`publishNotReadyAddresses` so `/metrics` is scrapeable on every replica (exactly one
reports `agentctl_operator_leader 1`).

---

## 3. Scaling

### AgentFleet (data plane) — scale subresource

`AgentFleet` exposes the Kubernetes **`scale` subresource** mapped to `.spec.replicas`
(claim mode → Deployment). Scale it like any workload:

```sh
kubectl scale agentfleet/<name> --replicas=3
# HPA can also target it:
kubectl autoscale agentfleet/<name> --min=2 --max=10 --cpu-percent=70
```

The operator does **not** own `.spec.replicas` (KEDA-safe / HPA-safe — verified via
managedFields), so an external autoscaler can drive the count without the operator
fighting it. In claim mode with `minReplicas` unset, the fleet may scale to 0 / defer to
KEDA. (Caveat: the operator does not yet write back `status.replicas`/`status.selector`,
so HPA read-back from the fleet status is a deferred item — see §6.)

### Coordination server (claim mode)

The **coordination server** is the claim-mode work broker: a single in-cluster
HTTP/MCP service that hands out work claims to `AgentFleet` replicas and tracks
per-source `work.stats`. It is **disabled by default**; enable it with:

```sh
helm upgrade --install agentctl charts/agentctl --set coordination.enabled=true
```

This renders a `agentctl-coordination` ServiceAccount, Deployment, and Service
(`:80` → container `:8080`, `/healthz` liveness + `/readyz` readiness, `/metrics`).
A claim-mode `AgentFleet` points its work source at the in-cluster endpoint —
set its `work.claim.server` (the pull/claim `workSource`) to the cluster DNS name:

```
http://agentctl-coordination.<release-namespace>.svc/
```

(the MCP/HTTP coordination endpoint; substitute your release namespace, e.g.
`agentctl-system`).

**Store backends.** The default `coordination.store=memory` keeps the claim queue and
`work.stats` **in process** — durable only for the life of the pod, so run a **single
replica** (raising `coordination.replicas` would give each replica its own queue). For
HA / durability set:

```sh
helm upgrade --install agentctl charts/agentctl \
  --set coordination.enabled=true \
  --set coordination.store=postgres \
  --set coordination.replicas=2
```

With `store=postgres` the backend reads `DATABASE_URL` (the bundled-or-external Postgres,
via the same `agentctl.databaseUrlEnv` helper the gateway uses) and persists the queue +
stats in Postgres, so claims **survive restarts** and replicas share one durable queue —
safe to scale `coordination.replicas` for HA. Pair with `postgres.bundled.tls.verifyFull`
to CA-pin the hop, and `metrics.serviceMonitor.enabled=true` to scrape the coordination
`/metrics` (the chart already renders an `agentctl-coordination` ServiceMonitor).

**Attested claim ownership (anti-cross-tenant).** By default any in-cluster caller (holding
the `AGENTCTL_API_TOKEN` when `apiToken.enabled`) can ack/release any claim. Set
`coordination.attestIdentity=true` to bind claim ownership to the caller's
**source-IP-attested** identity (the server resolves the source IP to the owning pod via a
kube `pods` lookup), so a tenant cannot ack/release another tenant's claim:

```sh
helm upgrade --install agentctl charts/agentctl \
  --set coordination.enabled=true \
  --set coordination.attestIdentity=true
```

This renders an `agentctl-coordination` ClusterRole + ClusterRoleBinding (cluster-wide `pods`
get/list — the coordination server has no cluster RBAC otherwise) and sets
`COORDINATION_ATTEST_IDENTITY=true` + `POD_NAMESPACE` (downward API) on the Deployment.
Default off renders no RBAC and no env. See security.md ("Attested claim ownership
(coordination)").

### KEDA autoscaler (claim-depth scaling)

The **KEDA external scaler** closes the claim-mode loop: it reads `work.stats` off
the coordination server and drives each claim-mode `AgentFleet`'s Deployment replica
count by claim depth — **including scale-from-zero**. It is **disabled by default**.

**Prerequisite: [KEDA](https://keda.sh) must be installed in the cluster** (it owns
the `keda.sh/v1alpha1` CRDs and runs the HPA that the ScaledObject configures).

```sh
helm upgrade --install agentctl charts/agentctl \
  --set coordination.enabled=true \
  --set scaler.enabled=true
```

This renders an `agentctl-scaler` ServiceAccount, Deployment, and Service — a
stateless gRPC service on `:9100` (the KEDA `externalscaler` API) plus an HTTP
`:8080` surface (`/healthz`, `/readyz`, `/metrics`). Enabling `scaler.enabled` also
flips the operator's `SCALER_ENABLED` on (wired via `operator.scaler.*` →
top-level `scaler.*` → in-cluster Service DNS), so:

- **the operator renders a `keda.sh/v1alpha1` ScaledObject per claim-mode fleet.**
  Its `scaleTargetRef` points at the fleet's Deployment; `minReplicaCount` comes
  from `scaling.min` (default `0`, i.e. scale-to-zero), `maxReplicaCount` from
  `scaling.max`; the single **external trigger** points KEDA at the scaler
  (`scalerAddress`) and carries the coordination backlog source (`coordinationUrl`
  = the fleet's `workSource`, else the operator's `COORDINATION_URL`), the
  per-replica `threshold` (`scaling.target.value`, default `5`), and an
  `activationThreshold` of `1` (the scale-from-zero trip point).
- **scale-from-zero flow:** at zero replicas KEDA polls the scaler's `IsActive`;
  when the coordination backlog for the fleet's work source becomes non-empty
  (depth ≥ `activationThreshold`), KEDA scales the Deployment `0 → 1`; thereafter
  the HPA scales `1 → N` toward `threshold` claims-per-replica up to
  `maxReplicaCount`, and back down to `minReplicaCount` (0) once the backlog drains.

**Safe + additive.** ScaledObject emission is gated on `SCALER_ENABLED` and applied
**best-effort**: the rendered Deployment never carries `.spec.replicas` (KEDA owns
it), and if the KEDA CRDs are absent the operator logs the failure and sets a
`ScaledObject=False` condition on the fleet **without failing the workload
reconcile** — the fleet still runs, just without claim-depth autoscaling. Turn the
scaler off (`scaler.enabled=false`, the default) on a non-KEDA cluster and scale
claim-mode fleets manually (see above). Shard-mode fleets are a fixed partition
count and never get a ScaledObject.

**Scaler → coordination mTLS (opt-in).** By default the scaler reads the coordination
backlog over plaintext HTTP (gated only by the optional `AGENTCTL_API_TOKEN`). Set
`coordination.mtls.enabled=true` (requires `certManager.enabled`) to mutually authenticate
that hop:

```sh
helm upgrade --install agentctl charts/agentctl \
  --set coordination.enabled=true \
  --set scaler.enabled=true \
  --set coordination.mtls.enabled=true
```

This issues a serving cert (`agentctl-coordination-mtls-tls`) for a second coordination
listener on `:8443` and a client cert (`agentctl-scaler-client-tls`) the scaler presents —
both off the agentctl CA Issuer, both carrying `ca.crt`. The coordination server verifies
the scaler's client cert against the CA + `coordination.mtls.allowedNames` (default
`agentctl-scaler`); the scaler verifies the serving cert against the CA and points
`COORDINATION_URL` at `https://agentctl-coordination.<ns>.svc.cluster.local.:8443` (the
trailing dot makes it an absolute FQDN so the `ndots` search list cannot capture it). The
client-cert CN is `coordination.mtls.scalerCommonName` — keep it in sync with `allowedNames`.
Default off keeps the plaintext http + token path. See security.md "Certificate fabric".

### Control-plane components

- **operator** — leader-elected singleton. Raise `operator.replicas` for HA (1 active +
  warm standbys); raising it does **not** add reconcile throughput, only failover speed.
  No HPA (intentionally excluded).
- **apiserver / gateway / modelgateway** — stateless Deployments. Set
  `<comp>.replicas`, or enable per-component HPA: `--set gateway.autoscaling.enabled=true`
  (`minReplicas`/`maxReplicas`/`targetCPUUtilizationPercentage`). When the HPA is enabled
  it owns the replica count and `<comp>.replicas` is ignored. The gateway is stateless
  (task state is in Postgres), so it scales horizontally freely.
- **admission** — webhook Deployment; scale via `admission.replicas` (HPA block exists but
  webhooks rarely need it). **node-agent** is a DaemonSet (one per node) — not scaled.

### PDB / availability toggles

Enable PodDisruptionBudgets per component to keep replicas up across voluntary disruptions
(node drains/upgrades): `--set gateway.pdb.enabled=true --set gateway.pdb.minAvailable=1`
(available for operator, apiserver, gateway, modelgateway, admission, coordination). PDBs and HPAs are
off by default so a stock install renders unchanged.

---

## 4. Observability & SLOs

### Metrics endpoints

Every component exposes hand-rolled Prometheus text at `/metrics`:
`agentctl_operator_*` (operator, `:8080` http), `_gateway_*`, `_modelgateway_*`
(`:8080` http), `_apiserver_*` (behind its mTLS gate, `:8443` https),
`_admission_*` (its HTTPS server), and the node-agent (`:8080` http, also re-exports
networkless agents' metrics relabeled with `agent_pod_uid`).

### Toggles

- **ServiceMonitors:** `--set metrics.serviceMonitor.enabled=true` renders one
  ServiceMonitor per component (scrape job `agentctl-<comp>`). Requires the Prometheus
  Operator CRDs. `metrics.serviceMonitor.labels` lets your Prometheus select them.
- **Grafana dashboard:** `--set observability.dashboards.enabled=true` ships a ConfigMap
  labeled `grafana_dashboard: "1"` for sidecar auto-discovery (no Grafana CRDs needed).
- **Alerts:** `--set observability.alerts.enabled=true` ships the `PrometheusRule` (needs
  the Prometheus Operator CRDs). Set `observability.alerts.labels` (e.g.
  `release=prometheus`) so your Prometheus `ruleSelector` loads it.
- **OTLP tracing:** off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set (default is fmt-only,
  byte-identical output). Set it via `<comp>.extraEnv` to enable the OTLP/gRPC exporter +
  W3C `traceparent` propagation (apiserver → node-agent). Stays rustls/ring (no TLS dep).

### Suggested SLOs

| SLO | Target (suggested) | Signal / PromQL |
|---|---|---|
| Control-plane availability | 99.9% | `up{job=~"agentctl-.*"}`; alert `AgentctlComponentDown` |
| Operator reconcile health | error rate < 0.1/s | `sum(rate(agentctl_operator_reconcile_errors_total[5m]))` |
| Operator leadership | always exactly 1 leader | `sum(agentctl_operator_leader) == 1` |
| Admission latency / deny rate | deny rate steady | `sum(rate(agentctl_admission_deny_total[5m]))` |
| Inference budget rejections | ~0 sustained | `sum(rate(agentctl_modelgateway_budget_rejections_total[5m]))` |
| A2A task success | high success ratio | gateway task metrics (`agentctl_gateway_*`) |

### Shipped alert rules (PromQL the chart's `PrometheusRule` uses)

- `AgentctlOperatorNoLeader` (critical, for 5m): `sum(agentctl_operator_leader) == 0`
- `AgentctlReconcileErrors` (warning, 10m): `sum(rate(agentctl_operator_reconcile_errors_total[5m])) > 0.1`
- `AgentctlComponentDown` (critical, 5m): `(up{job=~"agentctl-.*"} == 0) or (absent(up{job="agentctl-operator"}) == 1)`
- `AgentctlAdmissionDenySpike` (warning, 10m): `sum(rate(agentctl_admission_deny_total[5m])) > 1`
- `AgentctlBudgetRejections` (warning, 15m): `sum(rate(agentctl_modelgateway_budget_rejections_total[5m])) > 0`

---

## 5. Disaster scenarios

### Operator leader lost
A leader pod crash/eviction releases (or expires) the Lease; a standby or the rescheduled
pod re-acquires within ~lease-duration (~15s) and reconciliation resumes. `AgentctlOperatorNoLeader`
fires if `sum(agentctl_operator_leader)` stays 0 for 5m — check operator logs, the Lease
(`kubectl -n agentctl-system get lease agentctl-operator`), and RBAC. **Running agents are
unaffected** while the operator is down; only reconciliation of new/changed CRs stalls.

### Postgres down
The gateway and modelgateway depend on Postgres for durable state:
- **gateway** — A2A task persistence, `tasks/list`/`get`/`resubscribe`, push configs all
  read/write the DB; with the DB down these operations fail. The gateway stays stateless,
  so it recovers as soon as the DB returns (no gateway restart needed for connectivity,
  though a `rollout restart` is the clean recovery after a restore). Its `startupProbe`
  (~60s budget) keeps a slow DB-wait from crash-looping the pod.
- **modelgateway** — metering + budget enforcement persist to the DB; inference accounting
  is impacted while it is down.
Recover the DB (restart the pod / restore from §1), then `rollout restart` both
Deployments. For HA/DR, run external managed Postgres (§6).

### Cert expiry / rotation
cert-manager auto-renews all certs before expiry (`renewBefore: 720h` = 30 days; leaf
`duration` 1y, CA 10y) and re-injects the caBundle into the APIService and the
ValidatingWebhookConfiguration. If a cert is stuck, inspect:
`kubectl -n agentctl-system get certificate,certificaterequest` and the cert-manager logs.
To force a leaf rotation, delete its `-tls` Secret and cert-manager re-mints it from the
CA. Rotating the **CA** (`agentctl-ca-key`) re-mints every leaf and re-injects the new
caBundle — expect a brief mTLS re-handshake window across apiserver ↔ node-agent ↔ gateway.

### node-agent down on a node
The node-agent is a DaemonSet; if its pod on a node is unavailable, **that node's** agents
lose the control bridge (drain via the aggregated apiserver, `/metrics` re-export, A2A
bridging) until the DaemonSet pod is rescheduled. Other nodes are unaffected. The agent
workloads themselves keep running (they are networkless and serve over the host socket);
only node-agent-mediated control/observability for that node is interrupted. Check
`kubectl -n agentctl-system get pod -l app.kubernetes.io/name=agentctl-node-agent -o wide`.

---

## 6. Known limitations / deferred (honest residuals)

These are tracked P1/P2 items from the cloud-native roadmap (Wave 5 tail) — none are
production-blocking, but operators should know them:

- **PodSecurity namespace split** — `agentctl-system` is labeled
  `pod-security.kubernetes.io/enforce=privileged` (set on `namespace.podSecurity`, default
  `privileged`) because the **node-agent** DaemonSet needs `hostPath` + `hostPID`. All
  other components self-confine via `securityContext` (drop ALL caps,
  `allowPrivilegeEscalation: false`, `readOnlyRootFilesystem`, `seccompProfile:
  RuntimeDefault`, non-root). Splitting so only the node-agent's namespace is `privileged`
  and the rest run under `restricted` is deferred.
- **Conversion webhook** — CRDs are single-version `v1alpha1`. Multi-version evolution past
  `v1alpha1` and the accompanying conversion webhook are future work; until then upgrade
  CRDs by `kubectl apply -f charts/agentctl/crds/` (Helm does not upgrade CRDs — §2).
- **Bundled Postgres is single-replica** — one pod, `Recreate` strategy, data on
  `emptyDir` (eval default; data is lost on pod restart) or a PVC. There is **no
  replication or automated failover**. In-cluster TLS is **opt-in** via
  `postgres.bundled.tls.enabled` (default off → `sslmode=disable`); when enabled the
  hop is encrypted (`sslmode=require`), and adding `postgres.bundled.tls.verifyFull`
  CA-pins it (`sslmode=verify-full`, see §1). For HA/DR point at an **external managed
  Postgres** (`postgres.mode=external`) and defer backup/restore to that provider (§1).
- **Coordination HA/durability is opt-in** — the default `coordination.store=memory`
  is single-replica/in-process; `coordination.store=postgres` makes the claim queue
  durable + shared so `coordination.replicas` can be raised for HA (see "Coordination
  server" above).
- **Bundled `verify-full` is opt-in** — `postgres.bundled.tls.verifyFull` CA-pins the
  bundled hop (default off is still the `require` encrypt-without-verify path, §1).
- **Other tracked residuals:**
  operator `status.replicas`/`status.selector` write-back for `AgentFleet` HPA read-back,
  and NetworkPolicy enforcement (manifests ship via `networkPolicies.enabled`, but
  enforcement needs a policy CNI such as Calico/Cilium — kindnet ignores them).

---

## 7. API token (in-cluster auth gate)

The data-plane utility paths — the coordination server (`work.*`), the ModelGateway
(`/v1/infer`), and the A2A gateway ingress — are network-isolated but **open by
default**: any pod that can reach them may call them. The optional
**`AGENTCTL_API_TOKEN`** bearer gate (`apiToken.enabled`, default **off**) closes
that with a single shared in-cluster token. Disabled installs are unchanged (the
services run open, no token Secret, no env wired).

### Enable

```sh
helm upgrade agentctl charts/agentctl -n agentctl-system --reuse-values \
  --set apiToken.enabled=true
# (optional) pin a managed token instead of the generated one:
#   --set apiToken.value=$(openssl rand -hex 24)
```

This creates a **lookup-stable** Secret `agentctl-api-token` (key
`AGENTCTL_API_TOKEN`, `helm.sh/resource-policy: keep`) holding a random 40-char
token that is **kept across upgrades** (same idempotency pattern as the gateway
signing seed, §1). The chart then wires that token (via `secretKeyRef`) into the
coordination server, ModelGateway, A2A gateway, and the scaler. The services'
code requires `Authorization: Bearer <token>`; the scaler presents it when reading
the coordination backlog.

### Who must send the token

- **Producers + external A2A clients** must add `Authorization: Bearer <token>`.
  Read the value with:

  ```sh
  kubectl -n agentctl-system get secret agentctl-api-token \
    -o jsonpath='{.data.AGENTCTL_API_TOKEN}' | base64 -d
  ```

- **Scaler + control-plane-namespace agents** get it **injected automatically** —
  the scaler from the chart, and agent pods from the operator (`API_TOKEN_ENABLED`,
  set from `apiToken.enabled`). No extra RBAC is needed: the kubelet resolves the
  `secretKeyRef` at pod start, not the operator.

### Cross-namespace caveat (important)

A `secretKeyRef` resolves **only within the pod's own namespace**, and the
`agentctl-api-token` Secret lives in the control-plane namespace
(`agentctl-system`). So the operator injects the token **only for agents running in
the control-plane namespace**. For agents in **other** namespaces the operator does
**not** inject (it would render a pod that cannot start because the Secret is
absent there). To token-gate those agents, **replicate** the Secret into their
namespace, e.g.:

```sh
kubectl -n agentctl-system get secret agentctl-api-token -o yaml \
  | sed 's/namespace: agentctl-system/namespace: my-agents/' \
  | kubectl -n my-agents apply -f -
```

then provide it to the agent (e.g. via the agent spec's own env / `extraEnv`).
Keep the replicas in sync if you rotate the token.

### Rotate

Edit the Secret (or `helm upgrade --set apiToken.value=<new>`), then
`kubectl -n agentctl-system rollout restart deploy/agentctl-coordination
deploy/agentctl-modelgateway deploy/agentctl-gateway deploy/agentctl-scaler` and
restart the agent pods so every consumer re-reads it. Update any replicated copies
and external clients. This shared token is a coarse v1 access gate, not per-pod
identity — see `docs/security.md` for the attested-identity follow-ups.

---

## 8. Trusted front-proxy (external API gateway)

A fronting API gateway (e.g. APISIX) can terminate edge auth (OIDC/SAML/mTLS) and assert
the authenticated identity to the A2A gateway as `<prefix>-subject/-email/-groups` (prefix
`trustedProxy.headerPrefix`, default `x-agentctl`). agentctl honors those headers **only**
over an authenticated mTLS channel, gated by `trustedProxy.enabled` (default **off**). See
`docs/security.md` → "Trusted front-proxy (external API gateway)" for the full trust model
(authenticate the proxy → trust the asserted identity → enforce `requiredClaims`, strip on
untrusted callers).

### Enable

```sh
helm upgrade agentctl charts/agentctl -n agentctl-system --reuse-values \
  --set trustedProxy.enabled=true \
  --set 'trustedProxy.allowedNames={apisix}'
```

This adds a trusted-proxy **mTLS listener** on the gateway (`:8443`), issues a cert-manager
serving cert (`agentctl-trusted-proxy-tls`, signed by the chart CA) for it, and issues a
client cert (`agentctl-trusted-proxy-client-tls`) for the front-proxy to present. The gateway
verifies the proxy's client cert against the **agentctl CA** and checks its subject/SAN
against `allowedNames`; on any other (plaintext) path it **strips** the configured
`<prefix>-*` identity headers (and the legacy `X-Forwarded-*` set) so they can't be
self-asserted (anti-spoof).

### Retrieve the APISIX client cert

```sh
# client cert + key for APISIX to present on the upstream mTLS hop
kubectl -n agentctl-system get secret agentctl-trusted-proxy-client-tls \
  -o jsonpath='{.data.tls\.crt}' | base64 -d > apisix-client.crt
kubectl -n agentctl-system get secret agentctl-trusted-proxy-client-tls \
  -o jsonpath='{.data.tls\.key}' | base64 -d > apisix-client.key
# the agentctl CA, to verify the gateway serving cert
kubectl -n agentctl-system get secret agentctl-trusted-proxy-client-tls \
  -o jsonpath='{.data.ca\.crt}' | base64 -d > agentctl-ca.crt
```

### Configure the APISIX route / upstream

Point an APISIX route at the gateway's trusted-proxy mTLS listener, terminate OIDC at the
edge, and assert the verified identity upstream (full sketch in `docs/security.md`):

- **upstream** — `scheme: https`, node `agentctl-gateway.agentctl-system.svc:8443`,
  `tls.client_cert`/`tls.client_key` = the `apisix-client.crt`/`.key` above (its subject/SAN
  must be in `trustedProxy.allowedNames`); verify the gateway cert against
  `agentctl-ca.crt`.
- **plugins** — `openid-connect` (edge auth) + `proxy-rewrite` setting the prefix headers
  `x-agentctl-subject/-email/-groups` (or your `trustedProxy.headerPrefix`) from the verified
  userinfo. **Pass-through alternative:**
  omit both plugins and proxy the caller's `Authorization: Bearer` through for the gateway's
  native `spec.access.oidc` gate to verify.

Pair with the target agent's `spec.access.oidc.requiredClaims` so the asserted identity is
authorized (e.g. `groups` must contain `support`), and with `forwardIdentity: true` to pass
it to the agent.

### Default-off note

`trustedProxy.enabled` defaults **off**: no mTLS listener, no extra certs are rendered, and
the gateway honors **no** `X-Forwarded-*` from anyone (it strips them) — installs are
unchanged. Turn it on only when a trusted front-proxy fronts the A2A surface.
