# agentctl — status

A snapshot of what is built and verified, and what remains. Updated as the
implementation lands; the binding plan is `docs/design/agentctl-architecture-brainstorm.md`.

## Verified end-to-end on a kind cluster (k8s 1.31)

| Plane / piece | RFC | State | Evidence |
|---|---|---|---|
| **CRDs** (`Agent`, `AgentFleet`) | 0003 | ✅ | install on stock k8s; `kubectl api-resources` resolves; short names work |
| **Operator** (in-cluster, RBAC) | 0006 | ✅ | reconciles `Agent`→Job, status `Ready=True`, finalizer + owner-ref GC; 0 Forbidden |
| **Substrate** (stock-unix) | 0002 | ✅ | hostPath socket + `subPathExpr` per-pod subdir + downward-API env, on the live Job |
| **Scaling** (`AgentFleet`) | 0011 | ✅ (render+reconcile) | claim→Deployment (operator does NOT own `.spec.replicas` — KEDA-safe, verified via managedFields); shard→StatefulSet |
| **node-agent keystone** | 0008 | ✅ (Tier A) | DaemonSet discovers a live agent's socket + bridges to its management profile |
| **Aggregated APIServer** | 0009 | ✅ | `Available=True` (required front-proxy mTLS); `kubectl create --raw …/agents/x/drain` → SAR-authorized → forwarded to node-agent → agent; `--as=nobody`→403 |
| **Observability** (scrape-proxy) | 0010 | ✅ (metrics) | node-agent `/metrics` re-exposes a networkless agent's metrics, relabeled with `agent_pod_uid` |
| **A2A gateway + Agent Card** | 0013/0014 | ✅ (gateway + per-agent card) | external `message/send` → gateway (spec→reference translate + routing) → node-agent → agent Task round-trip; `GET /.well-known/agent-card.json` projects the card |
| **Contract client + CRD gen + conformance fixture** | 0018 | ✅ (hand-written) | typed manifest client validated vs real golden `--capabilities` fixtures; `mock-agent` as a conformant stand-in |

**Engineering:** 8 crates, 45 tests, `clippy -D warnings` clean, `cargo fmt` clean.

## Remaining (roadmap)

- **Observability** (rest of 0010): events pipeline (`agent://events`→logs), run-outcome capture (`kubectl agents results`), CLI `top`, trace correlation.
- **A2A mesh (rest of 0013/0014)**: fleet-level Agent Card (one card per `AgentFleet`) + card signing + in-cluster discovery registry + federation; durable task store + SSE streaming + webhooks (the gateway + per-agent card + `message/send`/`tasks/get`/`tasks/cancel` are done).
- **Intelligence plane** (0012): the egress proxy / ModelPool; zero-secret-in-pod; cost governance.
- **Admission** (0007): CRD CEL invariants + the validating webhook (trifecta-override gate).
- **Hardening** (0015): apiserver↔node-agent mTLS, pod→socket attestation (`SO_PEERCRED`, 0002 §7), per-tenant isolation, the Kata-hybrid substrate tier.
- **Real agentd**: drive the reference runtime (vs `mock-agent`) — needs a serve-stable management invocation.
- **CI/codegen** (0018): the contract-as-schema codegen pipeline (client is hand-written today); broaden conformance.
- **P0**: extract the contract into a neutral repo + neutralize the `AGENTD_*`/`agentd://` spellings at GA.

## Cross-repo contract asks

The implementation leans on contract primitives a conformant agent must expose;
the consolidated list (CC, P1–P12, …) is in the brainstorm §14. `mock-agent`
implements enough of the management + metrics surface to exercise the control
plane today.
