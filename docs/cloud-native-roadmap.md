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

## ✅ Wave 3 — security hardening (done, verified on kind)

- ✅ **Tenant agent pod securityContext** (P0): `render.rs` now confines every rendered
  agent pod — `capabilities: drop [ALL]`, `allowPrivilegeEscalation: false`,
  `readOnlyRootFilesystem: true` (+ writable `/tmp` emptyDir), pod `seccompProfile:
  RuntimeDefault`. Verified: a re-rendered mock runs confined and the node-agent still
  reads its socket. (`runAsNonRoot` is the documented follow-up — gated on substrate
  socket-perms, RFC 0002.)
- ✅ **node-agent minimal privilege** (P0): stays root + hostPID (needs it) but drops ALL
  caps, `allowPrivilegeEscalation: false`, `readOnlyRootFilesystem` + seccomp RuntimeDefault.
- ✅ **Postgres non-root** (P1): `runAsNonRoot` as the image's postgres uid (values knob,
  default 70 for -alpine) + fsGroup; drop caps + seccomp. Verified `uid=70(postgres)`.
- ✅ **NetworkPolicy completeness** (P1): parametrized namespaces + control-plane default-deny
  with narrow allows + Postgres ingress-only (renders; enforcement needs a policy CNI).
- ✅ **ModelGateway secrets RBAC** (P1): `secretsNamespaces` knob → namespaced Roles instead
  of cluster-wide secrets get/list when set.
- ✅ **Bonus fix:** operator leader-election readiness no longer gates on leadership (was
  deadlocking RollingUpdate + leaving HA standbys un-Ready) — readiness = manager-up.
- 🔜 PodSecurity: split so only node-agent's namespace is `privileged`, the rest `restricted`.
- 🔜 Postgres TLS (sslmode=require) + externalized creds.

## ✅ Wave 4 — API/CRD lifecycle (done, verified on kind)

- ✅ **Admission covers AgentFleet** (P0): the registry + lethal-trifecta gate now runs over
  an `AgentFleet`'s `spec.template` (shared `evaluate()`), and the ValidatingWebhookConfiguration
  rules include `agentfleets`. Verified: a fleet with an off-allow-list image OR a trifecta
  template is **denied**. (ModelPool invariants stay enforced by CRD CEL — not duplicated.)
- ✅ **Defaulting mutating webhook** (P0): a `/mutate` handler on the admission service applies
  the standard `app.kubernetes.io/*` labels + `mode`/`surfaces` minimal-exposure defaults
  (verified live). `substrate` is deliberately **not** hard-defaulted — the secure tier is
  tenancy-derived and kata-hybrid needs a runtime absent on stock clusters, so it's left
  auditable for the renderer to resolve (RFC 0002/0007).
- ✅ AgentFleet **scale subresource** (P1): `kubectl scale agentfleet` + HPA can target
  `.spec.replicas`. Verified `kubectl scale --replicas=3`.
- 🔜 Spec-invariant enforcement gaps not covered by CEL; conversion webhook + multi-version
  evolution past v1alpha1 (L); operator wiring of `status.replicas`/`selector` for HPA read-back.
- 🔜 Krew plugin manifest; CRD categories.

## ⬜ Wave 5 — day-2 / docs

- ⬜ values.schema.json; helm test hooks; Grafana dashboards + Prometheus alerts.
- ⬜ Backup/restore (Postgres); upgrade/rollback runbook; SLOs.
