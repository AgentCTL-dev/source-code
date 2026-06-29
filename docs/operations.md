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
| `agentctl-apiserver-tls` / `-admission-tls` / `-node-agent-tls` / `-client-tls` | Secrets (leaf certs) | no | optional (re-mintable from the CA) |

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

**Bundled TLS (opt-in).** Set `postgres.bundled.tls.enabled=true` to encrypt the bundled
hop: the chart issues a cert-manager `Certificate` (`agentctl-postgres-tls`, signed by the
chart CA) for `agentctl-postgres.<ns>.svc[.cluster.local]`, mounts it at `/tls` (the key is
`root:<fsGroup>` mode `0640`, readable by the postgres uid), runs postgres with `ssl=on`,
and flips `DATABASE_URL` to `sslmode=require`. Requires cert-manager (`certManager.enabled`,
the default).

**External TLS.** For a managed Postgres, put the desired mode in your DSN Secret. The
client encrypts on `sslmode=require`/`prefer`; **full CA + hostname verification
(`sslmode=verify-ca` / `verify-full`) is not yet implemented client-side** — it currently
behaves as encrypt-without-verify. If your threat model needs `verify-full`, terminate TLS
at a verifying sidecar/proxy in front of the DSN until client-side CA verification lands.

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
(available for operator, apiserver, gateway, modelgateway, admission). PDBs and HPAs are
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
  hop is encrypted but **not** CA-verified (`sslmode=require`, see §1). For HA/DR
  and a CA-verified DSN, point at an **external managed Postgres**
  (`postgres.mode=external`, DSN with `sslmode=verify-full`) and defer
  backup/restore to that provider (§1).
- **Other tracked residuals:** CA-verified (`sslmode=verify-full`) TLS for the
  bundled path (today it is encrypt-without-verify),
  operator `status.replicas`/`status.selector` write-back for `AgentFleet` HPA read-back,
  and NetworkPolicy enforcement (manifests ship via `networkPolicies.enabled`, but
  enforcement needs a policy CNI such as Calico/Cilium — kindnet ignores them).
