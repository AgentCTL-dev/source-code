# agentctl RFC 0021: Contract 2.0 — the network is the substrate

**Status:** Proposed (agentctl pivot track; supersedes-in-part 0002/0008/0009/0010/0012/0013/0015/0019)
**Author:** Andrii Tsok
**Date:** 2026-07-03
**Part of:** the agentctl control plane — the re-architecture that realigns agentctl to **agentd v2** (the reference agent's HTTPS-everywhere refactor): agents **serve** their control surface over mTLS HTTPS and **dial** the control-plane gateways keyless, the on-node bridge is **retired**, and identity becomes **cryptographic** (a verified client cert, or an attested source IP) instead of on-node reachability

> **Contract-first, not agent-first (P0).** This RFC changes the *substrate and
> transport* model, not the principle: agentctl still consumes only the published
> **Agent Control Contract** and manages *any* conformant agent. What changed is the
> contract itself — **contract 2.0** (`contract/`, RFC 0018 re-vendor) — which the
> reference implementation **agentd 2.x** now speaks. Where this RFC cites a concrete
> flag, path, or wire shape it names agentd as *where the contract is presently
> written down*, never as a dependency. A second conformant agent that serves mTLS
> HTTPS `POST /mcp` and dials the gateways keyless is managed by agentctl unchanged.

---

## 1. Summary

agentd underwent a ground-up realignment: it **removed every non-HTTP transport**
(stdio / unix-socket / vsock) and the **local exec surface**, and now **serves its
entire control surface over mTLS HTTPS** (`POST /mcp`, SSE for streaming) while
**dialing outward keyless** to whatever endpoints the operator injects. The one-line
model is *"the network is the substrate; identity is the boundary."*

agentctl's job is unchanged in spirit — provision agents, configure them (intelligence,
MCP tools, instruction), reach them for management, expose their A2A surface, scale
and observe them — but the **mechanism** every plane used (an on-node **node-agent**
bridging a host unix socket into the pod) is now obsolete. This RFC records the
re-architecture that lands that pivot, already built and verified on kind across all
planes (the phase series `c01bba7`→`8b94261`, plus the contract-2.0 re-vendor `2cd5bf5`).

**What agentctl is, restated for v2:** a Kubernetes control plane that **orchestrates**
conformant agents — it renders their pods, mints and rotates their identity (PKI),
brokers their intelligence and tools **secret-free**, routes and gates their A2A
communication, and drives their management verbs. agentctl **never** touches the
agent's execution layer, shell, or environment secrets; it configures the agent and
lets it run. Agents work **only** through operator-provided MCP tools and a
gateway-brokered model endpoint — no exec, no pod-resident credentials.

---

## 2. Motivation — why the v1 substrate model had to go

The v1 design (RFC 0002/0008) put a **node-agent** DaemonSet on every node. An agent
served its self-MCP over a **host unix socket** in a shared `hostPath` dir; the
node-agent opened that socket and bridged it to the network for the operator,
APIServer, A2A gateway, and telemetry. Reachability *was* authorization: whoever could
open the socket (filesystem perms / the VM boundary) was trusted. Three forces made
this untenable once agentd refactored:

1. **agentctl must not touch the execution layer.** The control plane's remit is
   orchestration — it cannot have access to the agent's bash, exec, or env-var
   secrets. A host-socket bridge that the control plane operates on the node blurs
   exactly that line; a network boundary with cryptographic identity restores it.
2. **The node-agent was a privileged blast radius.** It needed `hostPath` (and, per
   substrate tier, host-adjacent placement) to reach the sockets — a standing
   privileged component on every node, the opposite of the "no pod holds power it
   doesn't need" posture RFC 0015 wanted.
3. **agentd v2 removed the transports the bridge depended on.** With stdio/unix/vsock
   gone from the reference agent, there is no host socket to bridge. The agent serves
   HTTPS; the only question is who is allowed to dial it — which is an *identity*
   question, not a *reachability* one.

The resolution: **delete the node-agent entirely**, have the control plane reach each
agent **directly over mTLS HTTPS**, and have each agent reach the gateways **directly
over TLS, keyless**. Identity is the client certificate (for callers into the agent)
or the attested source IP (for the agent's calls into a gateway).

---

## 3. The v2 model

```
                         ┌──────────────────── control plane (agentctl-system) ─────────────────────┐
   kubectl / operator ──▶│  aggregated APIServer ──┐                                                 │
                         │  operator (reconcile+PKI)│  ModelGateway (TLS)   MCPGateway (TLS)          │
   A2A client ─────────▶ │  A2A gateway ────────┐   │  cred-inject+meter    attest+scope+inject       │
                         └──────────────────────┼───┼──────────────┼──────────────┼─────────────────┘
                                 mTLS client cert│   │ keyless      │ source-IP    │ source-IP
                                (Management)     ▼   ▲ dial         ▼ attest       ▼ attest
                                         ┌──────────────────── agent pod (tenant ns) ───────────────┐
                                         │  agentd  ── serves mTLS HTTPS :8443 /mcp  (self-MCP+A2A)  │
                                         │          ── dials AGENT_INTELLIGENCE / --mcp keyless      │
                                         │  no pod credential · runAsNonRoot · drop ALL · no hostPath│
                                         └───────────────────────────────────────────────────────────┘
```

Two directions, two identity mechanisms:

- **Into the agent (Management).** The APIServer and the A2A gateway dial the agent's
  `https://<podIP>:8443/mcp` presenting the **control-plane client certificate**
  (`agentctl-client-tls`). agentd's TLS acceptor verifies it against the **pinned
  client CA**; a verified cert ⇒ `PeerOrigin::Management`. No verified cert ⇒ the
  request is unauthenticated and refused (never downgraded). This is the whole
  management + A2A authority model — see RFC 0009/0013 as amended by §6/§8 here.
- **Out of the agent (keyless).** The agent dials the ModelGateway
  (`AGENT_INTELLIGENCE=https://…`) and each MCP server (`--mcp name=https://…`)
  **without any credential on the pod**. The gateway **attests the caller by source
  IP** (a kube pod lookup keyed on `status.podIP`), scopes it to what that agent is
  bound to, injects the real credential it holds off-pod, meters/budgets, and
  forwards. The agent trusts the gateway's serving cert via `--tls-ca` (the
  per-namespace public CA bundle).

Everything the agent needs to be *reached* is its serving identity; everything it needs
to *reach out* is a URL plus a CA to trust. No secret ever lands on the pod.

---

## 4. Supersession map

This RFC amends the earlier RFCs rather than deleting them (they record the v1 design
that was built and then pivoted). Each affected RFC carries a banner pointing here.

| RFC | v1 role | v2 disposition |
|---|---|---|
| **0002** Substrate & transport | tiered stock-unix / kata-hybrid / sidecar substrates converging on "open a discovered socket"; the `endpoint` reach descriptor | **Superseded (transport).** The reach abstraction is now a network address (`https://<podIP>:8443`); there is no discovered socket. Kata *tenancy hardening* (D1) survives — a pod may still be a Kata VM — but it is reached over mTLS, not a host socket. |
| **0008** node-agent architecture | the on-node keystone (Tier A control+telemetry, Tier B A2A relay) every plane reached the data plane through | **Retired in full.** The crate, DaemonSet, its cert, and its RBAC are deleted. Each function is re-homed (§11). |
| **0009** Management access path | operator → node-agent direct mTLS; humans → aggregated APIServer | **Amended.** The APIServer/operator now dial the **agent pod directly** over mTLS; the node-agent hop is gone. The aggregated-APIServer + per-verb RBAC seam is unchanged. |
| **0010** Observability & telemetry bridge | node-agent as the scrape-proxy + `http_sd` for networkless agents | **Amended.** Agents are network-native and serve `/metrics` directly; Prometheus scrapes the pod. No bridge. |
| **0012** Intelligence plane | egress proxy *out of the node-agent* | **Amended.** The **ModelGateway** is a standalone TLS Deployment the agent dials keyless; source-IP attestation replaces the SO_PEERCRED node-agent hop. |
| **0013** A2A gateway & task store | replicated stateless gateway fronting the node-pinned relay | **Amended.** The gateway resolves the target and forwards **direct to the agent pod** `/mcp`; there is no relay. Wire is contract-2.0 (bare PascalCase, `{"task"}` envelope). |
| **0015** Security & multi-tenancy | *transport is the boundary*; four caller→agent PEPs incl. the node-agent chokepoint | **Amended.** *Identity is the boundary*: mTLS client cert (into agents) + attested source IP (into gateways). The node-agent PEP is gone; the egress-authority PEPs (ModelGateway, MCPGateway) remain. |
| **0019** MCP registration/identity/auth | secret-free MCP **broker** + the stdio↔broker bridge for the stdio-only agent | **Amended.** Realized as the **MCPGateway** + `MCPServerSet` CRD. The stdio↔broker bridge is **deleted** — agentd speaks HTTPS MCP natively (the P-mcp-egress contract ask is delivered). |

D2 (Rust for all components), hostile-multi-tenancy-in-v1, and all-planes-in-v1
(brainstorm §0.6) are **unchanged**. D1 is narrowed: the *transport* half is superseded
here; the *Kata tenancy* half stands.

---

## 5. Provisioning & PKI (amends 0006)

The operator renders each agent pod to **serve mTLS HTTPS** and mints its identity via
**cert-manager**:

- A cluster CA `ClusterIssuer` **`agentctl-ca`** (CA cert in the chart's
  `clusterResourceNamespace`) is the single trust root.
- Per workload the operator ensures a serving `Certificate` **`<name>-serving-tls`**
  (SANs: `<name>.<ns>.svc[.cluster.local]` + `*.<ns>.pod.cluster.local`), mounted into
  the pod; agentd serves with `--serve-cert/--serve-key` and verifies management
  clients against `--serve-client-ca`.
- Per namespace the operator distributes an **`agentctl-ca` ConfigMap** (the public CA
  cert) so the agent can trust the gateways' serving certs via `--tls-ca`.
- The control plane holds a client cert **`agentctl-client-tls`** that mints
  `Management` at the agent's `/mcp`.

The rendered pod is **restricted-PSS**: `runAsNonRoot`, `allowPrivilegeEscalation:false`,
`capabilities.drop:[ALL]`, `seccompProfile:RuntimeDefault`, `automountServiceAccountToken:
false`, no `hostPath`/`hostPID`/privileged. It carries **zero credentials** — only its
serving key (a rotatable identity, not a secret it can exfiltrate to anyone useful) and
CA bundles. cert-manager rotates everything; agentd **hot-reloads its serving cert** on
rotation (re-stat on accept, last-good on failure) with no restart.

`Mode::Workflow` renders a **Job** (one-shot graph run) instead of a Deployment; the
inline or `configMapKeyRef` workflow graph is mounted and passed via `--workflow`.

---

## 6. Management access (amends 0009)

The aggregated APIServer's admin verbs — `drain`, `lame-duck`, `cancel`, and the new
`pause` / `resume` — resolve the target Agent to its `status.podIP` and issue an
**a2a.\* admin JSON-RPC** call to `https://<podIP>:8443/mcp` under the control-plane
client cert:

| verb | wire method |
|---|---|
| drain | `a2a.Drain` |
| lame-duck | `a2a.LameDuck` |
| cancel | `a2a.Cancel` |
| pause | `a2a.Pause` |
| resume | `a2a.Resume` |

Each verb stays **SAR-gated** at the APIServer (per-verb RBAC + end-user identity
survive the aggregation seam, RFC 0009 unchanged). The `a2a.` prefix marks these as
agentd operator *extensions*, distinct from the bare A2A-protocol methods (§8). A
non-`Management` caller gets `-32601`. There is no `pods/proxy`, no node-agent, no host
socket in the path.

---

## 7. Intelligence (amends 0012)

The operator renders `AGENT_INTELLIGENCE=https://agentctl-modelgateway.<ns>.svc…`
(keyless) and mounts the per-namespace CA. The **ModelGateway** is a standalone TLS
Deployment (`MODELGATEWAY_TLS_ADDR` / `_DIR`, server-auth-only serving cert) that:

1. **Attests the caller by source IP** — `resolve_ip_to_source` maps the TCP source IP
   to the calling pod via a kube watch-cache, deriving the agent's namespace/identity
   (never trusting a header). A **cold-start retry** (3×/500 ms) covers the race where
   an agent dials before its `status.podIP` has propagated to the gateway's cache.
2. **Injects the ModelPool credential** it holds off-pod, **meters** per-pool tokens in
   Postgres, and **enforces** the pool budget (a 429 on exhaustion).

The agent holds **no provider key**. Confined tenant pods drop `CAP_NET_RAW`, so the
source IP cannot be spoofed.

---

## 8. A2A (amends 0013)

The A2A gateway resolves an inbound task to the target Agent's pod IP and forwards to
`https://<podIP>:8443/mcp` — **direct to the pod**, no relay. It speaks the
**contract-2.0 A2A wire** (spec §9): bare PascalCase methods (`SendMessage`, `GetTask`,
`CancelTask`, `ListTasks`, `SendStreamingMessage`, `SubscribeToTask`), the
`SendMessageResponse` `{"task": <Task>}` envelope, proto3-JSON object shapes
(`TASK_STATE_*`, `ROLE_USER`), SSE streaming terminated by the **terminal task state +
stream close** (no `final` flag), and the error set `-32001` / `-32004` / the standard
JSON-RPC trio. The signed Agent Card is built by reading `agent://capabilities` from the
agent via `resources/read`. The durable task store, push-notification config, version
negotiation, and OAuth remain **gateway-owned** (the agent is stateless; those methods
return `-32601`).

A defensive `absolutize_endpoint` forces `*.svc.cluster.local` → trailing-dot absolute
FQDNs to avoid the ndots:5 + wildcard-search-domain capture trap.

---

## 9. MCP tools (realizes 0019)

Agents work **only** through MCP tools the control plane provides — brokered exactly
like intelligence, never dialed with a pod-resident credential. Two pieces:

- **`MCPServerSet` CRD** (the tool-plane analog of `ModelPool`): a set of
  `McpServer{name, endpoint, auth, tags, budget}` where `auth` is a `McpAuth` union
  (`none` | `staticToken` with a Secret-backed bearer held **off-pod**).
- **MCPGateway** (a new Deployment): the tool-plane broker. It **attests** the caller by
  source IP, **scopes** the request to the servers that agent's `MCPServerSet` binds,
  **injects** the `staticToken` credential, meters the per-server budget, and
  **forwards**. It runs **dual listeners** — `:8080` health *always* + `:8443` TLS as a
  background task — so a health probe never races the TLS bind (the crash-loop fixed in
  phase 5d).

The operator renders `--mcp name=https://agentctl-mcpgateway.<ns>.svc…/<server>` per
bound server; the agent dials keyless. The stdio↔broker bridge RFC 0019 needed for the
stdio-only v1 agent is **deleted** — HTTPS MCP is native in agentd v2.

---

## 10. Node-agent retirement (retires 0008/0010 bridge)

The node-agent is **gone** — crate deleted, DaemonSet + its Certificate + its RBAC
pruned, the tenant namespace relaxed from a node-agent-driven posture to **baseline**
PodSecurity (no control-plane component needs `hostPath`/`hostPID`/privileged anymore).
Each function it performed is re-homed to a network-native path:

| node-agent function (v1) | v2 replacement |
|---|---|
| management bridge (host socket → operator/APIServer) | APIServer/operator dial the pod `/mcp` directly over mTLS (§6) |
| telemetry scrape-proxy + `http_sd` | Prometheus scrapes the agent's `/metrics` directly |
| intelligence infer-proxy (SO_PEERCRED) | ModelGateway direct dial + source-IP attest (§7) |
| A2A node-pinned relay | A2A gateway forwards direct to the pod (§8) |
| MCP stdio↔broker bridge | native HTTPS MCP via the MCPGateway (§9) |

All planes were re-verified on kind with the node-agent absent.

---

## 11. Identity & attestation, consolidated (amends 0014/0015)

- **Into an agent** the boundary is the **mTLS client certificate** (verified against
  the pinned client CA ⇒ `Management`). This is the sole authority for management + A2A.
- **Into a gateway** the boundary is the **attested source IP** (kube pod lookup on
  `status.podIP`), hardened by the confined pod dropping `CAP_NET_RAW` and by the
  cold-start retry. Headers are never trusted for identity.
- **The agent's own identity** is its serving cert (SANs bind it to its Service/pod
  DNS). The fleet's signed Agent Card (RFC 0014) is unchanged — one fleet, one
  centrally-signed card, pinned out-of-band JWKS.

The v1 "transport is the boundary" law (RFC 0015 L7) is **superseded** by "identity is
the boundary." The egress-authority PEPs (ModelGateway, MCPGateway) are unchanged in
role; the node-agent PEP is deleted.

mTLS-only is a **hard policy**: agentctl never renders `--serve-bearer` (agentd accepts
a bearer as an alternative, but a bearer is a secret on the pod — forbidden). The only
thing on the pod is a rotatable serving key.

---

## 12. Contract 2.0 (see RFC 0018 re-vendor)

The wire changes above are ratified in **contract 2.0** (`contract/`, commit `2cd5bf5`):
`contract_version` `2.0`; the config `McpServer`/`A2aPeer` HTTPS shapes; `mode`/`interval`
/`cron` dropped from the config file (startup-only); the manifest intelligence transport
`https|null`, `surfaces.management` an https URL, `operator_tools` as `a2a.*`, the A2A
binding resolved to bare PascalCase over HTTPS, streaming without `final`; the
management-profile PeerOrigin re-anchored to mTLS client-cert identity; `exec_enabled`
present-but-always-false. `agent-contract-client` `SUPPORTED_MAJOR` is **2** — a 1.x
agent no longer negotiates. The behavioral conformance oracle (fixtures regenerated from
the v2 binary) is green.

---

## 13. Compatibility & migration

- **Agents:** a v1 agent (contract 1.x) is **not** manageable by v2 agentctl (major
  negotiation refuses it). The reference path is agentd **2.x**. A conformant third-party
  agent must serve mTLS HTTPS `/mcp` and dial gateways keyless.
- **No dual-stack.** There is no v1/v2 bridge shim; the substrate model changed wholesale.
  This is deliberate — a compatibility shim would reintroduce the on-node privileged
  component the pivot exists to remove.
- **Chart upgrades** must use `helm upgrade --reset-then-reuse-values` (the new
  mcpgateway/gateway value blocks are dropped by `--reuse-values`).

---

## 14. Open items / follow-ups

- **agentd v2.1.1 release.** The `--tls-ca` + live-cert-rotation + SNI-trailing-dot-strip
  work is verified against working-tree builds; a tagged `ghcr.io/agentd-dev/agentd`
  image carrying it must be cut so published installs match.
- **e2e mock-agent HTTPS rework.** `mock-agent` emits a contract-2.0 *manifest* and
  dispatches both method spellings, but still serves the v1 NDJSON-over-unix transport;
  the full mTLS-HTTPS `POST /mcp` mock is the remaining e2e-harness item.
- **Instruction hot-reload (P-instr-file, RFC 0020).** Unaffected by this pivot; still
  the managed-roll interim until the agent makes `instruction` reloadable.
- **agentd P-cost.** A loop-mode agent that exhausts its ModelPool budget crash-loops
  (429 fatal); reactive mode is the stable default. Flagged as an agentd cost-governance
  follow-up.

---

## 15. Why this is the right shape

The pivot makes agentctl a **cleaner orchestrator**: it renders pods, mints identity,
brokers intelligence and tools secret-free, and routes A2A — and it reaches agents the
way Kubernetes reaches anything else, over the network with a verified identity. It
**removes** a standing privileged per-node component, **removes** every pod-resident
credential, and **narrows** the control plane's contact with the agent to exactly two
cryptographically-identified seams. "The network is the substrate; identity is the
boundary" is not only simpler than the tiered-socket model — it is the only shape that
keeps agentctl out of the agent's execution layer while still managing it completely.
