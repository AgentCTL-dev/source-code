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

## ✅ Wave 5 — observability + day-2 polish (done, verified on kind)

- ✅ **OTLP distributed tracing** (P1): a shared `agentctl-telemetry` crate wires an OTLP/gRPC
  exporter (no TLS deps — stays rustls/ring) into all 6 binaries, **off unless
  `OTEL_EXPORTER_OTLP_ENDPOINT` is set** (default = fmt-only, byte-identical). `#[instrument]`
  on the key handlers + **W3C `traceparent` propagation** apiserver→node-agent. Verified all
  services start healthy with it linked.
- ✅ **Operator Kubernetes Events** (P1): a kube `Recorder` emits `Reconciled`/`RenderFailed`/
  `ReconcileError` Events on Agent + AgentFleet reconcile (+ the `events.k8s.io` RBAC). Verified
  a live `Normal Reconciled agent/mock "Deployment workload applied"` event.
- ✅ **Grafana dashboard + PrometheusRule alerts** (P2): opt-in (`observability.dashboards/alerts.enabled`)
  — dashboard ConfigMap (sidecar-discoverable) + alerts (no-leader, reconcile-errors, component-down,
  deny-spike, budget-rejections).
- ✅ **values.schema.json** (P2): validates the values shape; verified it rejects type errors.
- ✅ **helm test hook** (P2): `helm test` connectivity probe (gateway `/healthz` + APIService
  discovery) — verified Phase: Succeeded.
- ✅ **Krew plugin manifest** (P2) for `kubectl agent`.
## ✅ Wave 6 — residual tail (done, verified on kind)

- ✅ **Operator `status.replicas`/`selector` write-back** (P1): the operator populates the
  AgentFleet scale subresource's status (selector = the rendered pod labels; replicas =
  shard count / claim `spec.replicas`, KEDA-safe). Verified `status.selector` is set so an
  HPA can read the scale subresource.
- ✅ **Bundled Postgres TLS** (P1, opt-in `postgres.bundled.tls.enabled`): cert-manager serving
  cert + `ssl=on`; the gateway/modelgateway DB client gained `tokio-postgres-rustls` (ring — no
  openssl) and connect `sslmode=require`. Verified live: `SHOW ssl` → `on`, gateway connects over
  TLS. Default stays `sslmode=disable` (unchanged).
- ✅ **Operations runbook** (`docs/operations.md`): backup/restore, upgrade/rollback (incl. the
  CRD-not-upgraded-by-helm caveat), scaling, observability + SLOs, disaster scenarios.

## ✅ Scaling track — reference coordination server (done, verified on kind)

- ✅ **Reference coordination MCP server** (`crates/agentctl-coordination`, RFC 0011 §3.2): the
  claim-mode correctness backbone. Serves the frozen `work.*` surface over MCP JSON-RPC/HTTP —
  **atomic `work.claim`** (single-Mutex serializing point; exactly one of N racers wins),
  lease TTL + background expiry sweep, `work.renew`/`ack`/`release`, **transactional dedupe on
  `agent/claim_key`**, `work.submit` (producer enqueue), and `work.stats` + `work://pending`
  (the scale-from-zero backlog, P9). In-memory single-Mutex store behind a `ClaimStore` trait
  (Redis/Postgres backend slots in later); deterministic lease ids (no RNG/wall-clock).
  Chart Deployment+Service (opt-in `coordination.enabled`), Dockerfile, CI matrix, `/metrics`.
  Verified live on kind: grant-once + `held_by` contention + ack-dedupe + stats + counters;
  19 unit tests incl. a 2-thread/200-iteration concurrent-claim race.
- 🔜 **KEDA external scaler** (`crates/scaler`): reads the coordination server's `work.stats`
  backlog → `IsActive`/`GetMetrics` for KEDA scale-from-zero. The last piece to make claim
  fleets fully elastic-from-zero (the server already exposes the signal).
- 🔜 Coordination server HA/durability (the documented single-serializing-point risk) — the
  `ClaimStore` trait is the seam; v1 is single-replica/in-memory.

## 🔜 Explicitly deferred (documented in docs/operations.md)

- ⬜ **PodSecurity namespace split** — the `agentctl-system` ns is labeled `privileged` for the
  node-agent; all other components already self-confine via securityContext. Splitting into a
  privileged node-agent ns + a restricted control-plane ns is a structural change with high
  regression surface and low incremental security; deferred.
- ⬜ **Conversion webhook / multi-version** — CRDs are `v1alpha1`; there's no `v1beta1` schema to
  convert to yet, so conversion scaffolding is premature. Revisit when a v2 schema lands.
- ⬜ **Postgres `verify-full`** (client CA verification) and externalized-creds rotation; client
  currently does `sslmode=require` (encrypt, no CA verify).
