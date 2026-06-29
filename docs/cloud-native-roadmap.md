# Cloud-native productization roadmap

Driven from a readiness audit across five dimensions (52 findings: 9 P0, 28 P1,
15 P2). Tracks what makes agentctl a production-grade cloud-native product.
Status: ✅ done · 🔜 next · ⬜ planned.

## ✅ Wave 1 — chart productization + CI supply-chain (done, verified on kind)

- ✅ Per-component Helm knobs: resources, nodeSelector, affinity, tolerations
  (node-agent → `Exists`), topologySpread, priorityClassName, podAnnotations/Labels,
  extraEnv/envFrom, logLevel, serviceAccount.annotations; global commonLabels/Annotations.
- ✅ PodDisruptionBudgets, HorizontalPodAutoscalers, Prometheus ServiceMonitors (opt-in).
- ✅ Bundled-Postgres `storage=pvc` branch (was silently ignored → data loss).
- ✅ startupProbe on gateway/modelgateway (DB-schema wait no longer trips liveness).
- ✅ **Idempotent upgrades**: postgres password + gateway signing seed reuse the
  existing Secret via `lookup` (regeneration had broken DB auth on first upgrade).
- ✅ Multi-arch images via **cross-compilation** (cargo-zigbuild, no QEMU) + GHA layer cache.
- ✅ **cosign** keyless signing of images + chart by digest; **cargo-deny** CI job + `deny.toml`.

## ✅ Wave 2 — observability + reliability code (done, verified on kind)

- ✅ **Prometheus `/metrics` on every component** (P0): operator + all 4 HTTP services
  now expose `/metrics` (the node-agent already did), hand-rolled Prometheus text in the
  node-agent style (`agentctl_operator_*` / `_gateway_*` / `_modelgateway_*` / `_apiserver_*`
  / `_admission_*`). apiserver `/metrics` stays behind its mTLS gate; admission via its
  HTTPS server. ServiceMonitors (Wave 1) now have real targets.
- ✅ **Operator leader election** (P0): a `coordination.k8s.io` Lease (`agentctl-operator`,
  15s/10s acquire-renew, holder = POD_NAME) — only the leader reconciles, standbys serve
  `/healthz` and `/readyz=503`; safe at >1 replica. Added a health/metrics server +
  liveness `/healthz` + readiness `/readyz` probes + an operator Service + leader RBAC.
  Verified: the Lease is held + renewing, `/readyz=200`, `agentctl_operator_leader 1`.
- ✅ **Graceful shutdown** (P0): all 4 HTTP services drain in-flight requests on SIGTERM/SIGINT
  (`with_graceful_shutdown` / `axum_server` handle), SSE streams close cleanly.
- 🔜 **OpenTelemetry/OTLP tracing** (P1) across apiserver → node-agent → agent and the gateway.
- 🔜 **Operator Kubernetes Events** (P1) for reconcile outcomes (RBAC already held).
- 🔜 Dependency-aware readiness (P1): probes reflect real backing-store/dependency health.
- 🔜 Grafana dashboard + Prometheus alert rules; release the Lease on SIGTERM for instant failover.

## 🔜 Wave 3 — security hardening (P0/P1)

- 🔜 **Tenant agent pod securityContext** (P0): the operator renders agent pods with no
  securityContext — harden the hostile multi-tenant data plane (runAsNonRoot, drop caps,
  seccomp, readOnlyRootFilesystem) in `render.rs`.
- 🔜 **node-agent minimal privilege** (P0): it runs root + hostPID + hostPath — drop all
  caps it doesn't need, add seccompProfile, readOnlyRootFilesystem.
- 🔜 ModelGateway secrets RBAC scoping (P1, currently cluster-wide get/list).
- 🔜 NetworkPolicy completeness (P1): cover control plane + Postgres, parametrize namespaces.
- 🔜 Postgres hardening (P1): securityContext, TLS (sslmode), non-default creds.
- 🔜 PodSecurity: keep node-agent ns privileged; run the rest under `restricted`.

## ⬜ Wave 4 — API/CRD lifecycle (P0/P1)

- ⬜ **Admission covers AgentFleet + ModelPool** (P0): the trifecta/registry gate only
  validates `Agent` today; extend the webhook rules + logic.
- ⬜ **Defaulting/mutating webhook** (P0): absent defaults make `substrate` resolve to the
  least-isolated stock-unix, contradicting the documented secure default.
- ⬜ AgentFleet **scale subresource** (P1) so `kubectl scale` + HPA can target it.
- ⬜ Spec-invariant enforcement (P1) via CEL/admission.
- ⬜ Conversion webhook + multi-version evolution past v1alpha1 (P1, L).
- ⬜ Krew plugin manifest; CRD categories; consistent app.kubernetes.io labels.

## ⬜ Wave 5 — day-2 / docs

- ⬜ values.schema.json; helm test hooks; Grafana dashboards + Prometheus alerts.
- ⬜ Backup/restore (Postgres); upgrade/rollback runbook; SLOs.
