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
| **A2A gateway + streaming + mesh registry** | 0013/0014 | ✅ | `message/send` + **`message/stream`** (live SSE: working→artifact→completed) + `tasks/get`/`tasks/cancel`; per-agent **Agent Card** at `/.well-known/`; **`GET /agents`** discovery registry (Agent + AgentFleet) |
| **A2A durable task store** | 0013 | ✅ | Postgres-backed: `message/send` persists tasks, **`tasks/list`** returns history, `tasks/get` resolves from the store; **survives a gateway pod restart** (state in the DB, gateway stays stateless) |
| **A2A push notifications** | 0013 | ✅ | `tasks/pushNotificationConfig/set·get·list·delete` (gateway-owned, since agents are networkless); on completion the gateway **delivers the task to the webhook** with retries + `Authorization: Bearer` (verified: sink received the task with `auth=Bearer …`) |
| **A2A mesh: card signing + fleet card + federation** | 0014 | ✅ | Agent **+ fleet** Cards are **JWS-signed** (Ed25519); `GET /.well-known/jwks.json` serves the key — **a live card verifies against it**; `GET /agents` **federates** peer registries (`origin` per row); `tasks/resubscribe` replays a stored task over SSE |
| **Contract client + CRD gen + conformance fixture** | 0018 | ✅ (hand-written) | typed manifest client validated vs real golden `--capabilities` fixtures; `mock-agent` as a conformant stand-in |

**Engineering:** 9 crates, 64 tests, `clippy -D warnings` clean, `cargo fmt` clean.

## Remaining (roadmap)

- **Observability** (rest of 0010): events pipeline (`agent://events`→logs), run-outcome capture (`kubectl agents results`), CLI `top`, trace correlation.
- **A2A — protocol surface complete.** Done: `message/send`, **`message/stream`** (live SSE), `tasks/get`·`list`·`cancel`·**`resubscribe`**, **`pushNotificationConfig/*`** + webhook delivery (retry + bearer auth), per-agent **and fleet JWS-signed Agent Cards** + JWKS, the **`GET /agents` federated registry**, and the **Postgres durable task store**. Remaining is hardening/scale only: cross-cluster federation *trust* (verify peer card signatures + dedup), live in-flight stream resume (agents complete synchronously today), and push delivery for streaming tasks.
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
