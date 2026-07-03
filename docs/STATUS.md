# agentctl — status

A snapshot of what is built and verified, and what remains. Updated as the
implementation lands; the binding plan is `docs/design/agentctl-architecture-brainstorm.md`,
and the current substrate/transport design is **[RFC 0021](../rfcs/0021-contract-2.0-network-substrate-pivot.md)**.

> ⚠️ **Contract 2.0 — the network is the substrate.** agentctl was re-architected to
> **agentd v2**: agents **serve mTLS HTTPS** (`POST /mcp`) and **dial the gateways
> keyless**, the **node-agent is retired**, and identity is **cryptographic** (a verified
> client cert into agents, an attested source IP into gateways). The v1 rows below have
> been rewritten to the v2 model; see RFC 0021 for the full design and the phase series
> `c01bba7`→`8b94261` + the contract-2.0 re-vendor `2cd5bf5` for the change history.

## Verified end-to-end on a kind cluster (k8s 1.31)

| Plane / piece | RFC | State | Evidence |
|---|---|---|---|
| **CRDs** (`Agent`, `AgentFleet`, `ModelPool`, `MCPServerSet`) | 0003/0004/0019 | ✅ | install on stock k8s; `kubectl api-resources` resolves; short names work |
| **Operator + per-workload PKI** | 0006 / 0021 §5 | ✅ | reconciles `Agent`→Deployment/Job serving **mTLS HTTPS**; cert-manager mints a per-workload `<name>-serving-tls` Certificate + distributes the per-namespace `agentctl-ca` ConfigMap; status `Ready=True`; finalizer + owner-ref GC; 0 Forbidden |
| **Provisioning** (agentd v2, mTLS HTTPS serve) | 0021 §5 | ✅ | rendered pod serves `https://0.0.0.0:8443/mcp` (`--serve-cert/--serve-key/--serve-client-ca`), dials gateways keyless (`--tls-ca`), **restricted-PSS** (runAsNonRoot, drop ALL, seccomp RuntimeDefault, `automountServiceAccountToken:false`), **zero pod credentials**, no hostPath |
| **Management** (aggregated APIServer → pod, mTLS) | 0009 / 0021 §6 | ✅ | `Available=True`; `kubectl create --raw …/agents/x/{drain,lame-duck,cancel,pause,resume}` → SAR-authorized → **direct mTLS to the agent pod `/mcp`** as an `a2a.*` admin JSON-RPC call (client cert = `Management`); `--as=nobody`→403; **no node-agent, no host socket** |
| **Intelligence plane** (ModelPool + ModelGateway, keyless) | 0012 / 0021 §7 | ✅ | keyless `AGENT_INTELLIGENCE=https://…modelgateway…` → ModelGateway **attests the caller by source IP** (kube pod lookup, cold-start retry), injects the `ModelPool` credential (mock-provider 401s without it), **meters** tokens in Postgres, **enforces the budget** (3rd call → 429); the agent holds **no provider key** |
| **A2A gateway** (direct-to-pod, contract-2.0 wire) | 0013 / 0021 §8 | ✅ | gateway resolves the target and forwards **direct to the pod** `https://<podIP>:8443/mcp`; **bare PascalCase** methods (`SendMessage`/`GetTask`/`CancelTask`/`ListTasks` + streaming pair), `{"task"}` envelope, proto3-JSON shapes, **SSE** streaming terminated by terminal state + close (no `final`); signed Agent Card built from `agent://capabilities` via `resources/read`; Postgres durable task store; push-config gateway-owned |
| **MCP tool plane** (MCPServerSet + MCPGateway) | 0019 / 0021 §9 | ✅ | operator renders `--mcp name=https://…mcpgateway…/<server>`; the **MCPGateway** attests source-IP → scopes to the bound `MCPServerSet` → injects the `staticToken` credential (held off-pod) → meters budget → forwards; **dual listeners** (`:8080` health always + `:8443` TLS) so a probe never races the bind |
| **Workflow mode** | 0021 §5 | ✅ | `--mode workflow` renders a **Job**; an inline or `configMapKeyRef` graph is mounted + passed via `--workflow` |
| **node-agent retirement** | 0008 / 0021 §10 | ✅ | crate + DaemonSet + its cert + its RBAC **deleted**; every plane above re-verified with the node-agent **absent**; tenant namespace relaxed to **baseline** PodSecurity (no component needs hostPath/hostPID/privileged) |
| **Admission** (CEL + validating webhook) | 0007 | ✅ | CEL invariants (schedule⇔mode, shards⇔shardMode, budget>0, workflow⇒has(workflow)) reject violating CRs; the webhook denies the lethal **trifecta** without `allow-trifecta`, off-allow-list registries, and a missing cross-object `modelPool` |
| **Security: identity is the boundary** | 0015 / 0021 §11 | ✅ | into agents = **mTLS client cert** (verified vs the pinned client CA ⇒ `Management`; an uncertified call is rejected at the handshake); into gateways = **attested source IP** (headers never trusted; confined pod drops `CAP_NET_RAW`); **mTLS-only** (never a pod-resident bearer); NetworkPolicies for egress/tenant isolation shipped |
| **Contract client + conformance oracle** | 0018 | ✅ | typed manifest client at **`SUPPORTED_MAJOR = 2`**; fixtures regenerated from the v2 binary (contract 2.0, bare A2A methods, exec removed); a 1.x agent no longer negotiates; `mock-agent` emits a contract-2.0 manifest |
| **Product install** (Helm + cert-manager) | 0017 | ✅ | `helm install ./charts/agentctl` brings up the core control plane Running; the **`agentctl-ca` ClusterIssuer** + per-component serving certs issue + inject the caBundles; APIService `Available=True`; upgrades use `--reset-then-reuse-values` |

**Engineering:** 15 crates (node-agent deleted; `agentctl-mcpgateway` added), `agentctl-e2e`
excluded from the workspace to keep `cargo test --workspace` hermetic. Workspace builds clean;
**324 workspace tests pass** and the contract-2.0 conformance oracle is green. **Install:** Helm chart at `charts/agentctl`
(see its `README.md`); cert-manager-driven TLS via the `agentctl-ca` ClusterIssuer.

## Remaining (roadmap)

- **agentd v2.1.1 release.** The `--tls-ca` + serving-cert live-rotation + SNI-trailing-dot
  fixes are verified against working-tree builds; a tagged `ghcr.io/agentd-dev/agentd` image
  carrying them must be cut so published installs match (RFC 0021 §14).
- **e2e mock-agent HTTPS rework.** `mock-agent` emits a contract-2.0 *manifest* and dispatches
  both A2A spellings, but still serves the v1 NDJSON-over-unix transport; the full mTLS-HTTPS
  `POST /mcp` mock is the remaining e2e-harness item.
- **Observability** (rest of 0010): events pipeline (`agent://events`→logs), run-outcome
  capture, CLI `top`, trace correlation. Metrics are now a **direct scrape** of the agent's
  `/metrics` (the node-agent bridge is retired).
- **Intelligence plane** (rest of 0012): `ModelPool.status.usedTokens` write-back; real provider
  adapters (Anthropic/OpenAI) + streaming; per-agent (not just per-pool) budgets. (Done: keyless
  dial + source-IP attestation + credential injection + metering + budget.)
- **MCP tool plane** (rest of 0019): the full MCP 2025-06-18 OAuth broker tiers (client-credentials
  / on-behalf-of-user) — the forward design; today the MCPGateway does `staticToken` injection +
  source-IP attestation + scoping + budget.
- **Instruction sourcing** (0020): hot (no-restart) live reload once `instruction` is made
  reloadable (contract ask **P-instr-file**); managed roll is the interim. Unaffected by the pivot.
- **P0**: extract the contract into a neutral repo. The contract is vendor-neutral (`AGENT_*` /
  `agent://` spellings); the reference implementation is **agentd 2.x**, which speaks contract 2.0.

## Cross-repo contract asks

The implementation leans on contract primitives a conformant agent must expose; the consolidated
list (CC, P1–P12, …) is in the brainstorm §14. Contract 2.0 delivered several (P-mcp-egress via
native HTTPS MCP; the A2A binding resolution). `mock-agent` implements enough of the management +
metrics surface to exercise the control plane today.
