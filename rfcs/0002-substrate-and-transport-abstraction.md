# agentctl RFC 0002: Substrate & transport abstraction

**Status:** Proposed (agentctl foundational track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentctl control plane — the data-plane *reach* layer every other plane (operator, node-agent, A2A gateway, intelligence, scaling) programs against

> **P0 — agentctl manages a *conformant agent*, not a specific binary.** The data
> plane of this RFC is *any* agent that conforms to the published control contract:
> it serves its self-MCP management profile over a discoverable socket, advertises
> that socket in its capabilities manifest's `surfaces.management` key, reads the
> downward-API env convention, and honours the frozen exit-code/health contract.
> Where this RFC needs a concrete value or a worked example it cites **the reference
> implementation** (`agent`, whose contract is presently specified in agentd RFCs
> 0014–0020); none of those citations make the reference implementation a
> dependency. The transport abstraction here is precisely the seam that lets a
> future agent from another vendor drop in unchanged.

> **The distinctive claim is microVM-only.** The vision's signature posture —
> *"vsock-everything, no cluster network"* (ideas.md §2) — is a **premium isolation
> tier that exists only on a microVM substrate**, not a property agentctl can offer
> on stock Kubernetes. This RFC demotes vsock from *the* substrate to *a* tier,
> defines the one abstraction the rest of agentctl programs against so the tier is
> swappable, and is honest about which clusters get which isolation.

---

## 1. Problem / Context

agentctl is the control plane for a fleet of conformant agents (agentctl RFC 0001).
Most of the planes it owns ultimately have to **reach into a running agent**:

- the **operator** reads `agent://capabilities`/`inventory`/`status` to project
  `Agent.status` and calls `drain`/`lame-duck`/`cancel` (agentctl RFC 0003, 0006);
- the **node-agent** holds the long-lived management connection per local agent
  (agentctl RFC 0008);
- the **A2A gateway** bridges HTTP/SSE to the agent's A2A surface (agentctl RFC 0013);
- the **telemetry** path scrapes `agent://metrics` and tails `agent://events`
  (agentctl RFC 0010).

The **intelligence** plane is the deliberate exception: it is the agent dialing
*out* to a model endpoint (agentctl RFC 0012), **not** agentctl reaching *in*, so
it does **not** consume an endpoint descriptor and is the one egress leg this
abstraction does not cover (§10, correction 1).

The inbound planes above are largely the *same physical problem* for the
**management** surface: **open a byte stream to a specific agent and speak the
contract's NDJSON/JSON-RPC management profile over it** (the self-MCP, agentd RFC
0015 §3). The metrics/events surfaces are *not* identical — they speak a different
protocol (Prometheus 0.0.4 text over HTTP / an events stream, agentd RFC 0016) and
may live on a separate socket (§3). What unifies them is the *descriptor*
abstraction, not the wire. The hard part is not the protocol — the contract froze
that — it is that the byte stream crosses a boundary whose **isolation strength
differs by an order of magnitude between substrates**, and the strongest isolation
(a microVM kernel boundary) is available on only a minority of clusters.

The contract already anticipated this and made the abstraction clean. A conformant
agent advertises its management surface in exactly one place — the manifest's
`surfaces.management` key, whose value space is **`false | "vsock:PORT" |
"unix:PATH"`** (agentd RFC 0015 §5.2). And the agent's *serving* code is
transport-agnostic by construction: agentd RFC 0015 §3.2/§3.4 sets
`PeerOrigin::Management` and uses identical NDJSON framing for the unix and vsock
listeners — "the unix server with the socket type swapped." The agent does **not**
know or care which substrate it runs on; it binds a socket and serves. Who
provisions the device — the CID, the port, the host-side path — is explicitly *not
the agent's concern* (agentd RFC 0015 §3.3, RFC 0006). **That provisioning and
reach is this RFC's concern, and it is the whole of it.**

The temptation the vision encodes — make vsock the default everywhere, run agents
with no cluster network at all — does not survive contact with the substrate facts:

- **vsock guest↔host requires a real VM boundary.** A stock runc/containerd
  container shares the host kernel, is *not* a guest, gets no guest context ID, and
  has no `vhost-vsock` device. The only `AF_VSOCK` reachable from a same-kernel
  container is the `VMADDR_CID_LOCAL = 1` loopback, which is **kernel-global** and
  therefore useless as a per-pod bridge (every container on the node shares it).
  vsock-everything *presupposes* a microVM runtime.
- **microVM runtimes are absent on the dominant managed tiers.** GKE Sandbox is
  gVisor (`runsc`, a userspace syscall interceptor — *not* a VM, no guest↔host
  `AF_VSOCK` at all); EKS and Fargate expose no vsock; only AKS Pod Sandboxing or a
  self-managed node pool with Kata installed qualify. Staking the day-one substrate
  on vsock stakes adoption on a runtime most clusters do not have.

So the framing of this RFC is: **transport is a pluggable abstraction with tiers,
the portable tier is the default for development and single-tenant clusters, the
microVM tier is the default for hostile multi-tenant production, and they converge
on one node-agent code path.** The rest follows.

---

## 2. Decision — transport is a pluggable abstraction with tiers — 7 principles

1. **One abstraction: the endpoint descriptor.** Everything in agentctl that
   reaches an agent programs against a single internal type — a *discovered,
   per-agent, attested socket dial string* — never against a substrate. Discovery,
   tier selection, and attestation are confined to the node-agent; the operator,
   gateway, telemetry, and CLI consume descriptors and are substrate-blind (§3).

2. **Three tiers, one code path.** PRIMARY = stock unix-socket-over-hostPath →
   host DaemonSet; HARDENED = vsock-on-Kata-hybrid (Firecracker / Cloud-Hypervisor);
   MOST-PORTABLE = per-pod sidecar over an emptyDir unix socket. The node-agent's
   job in all three reduces to **"open a discovered unix socket"** because on
   Kata-hybrid the host side of the vsock is itself a per-VM Unix-domain socket
   (§4).

3. **Default by tenancy, not by preference.** On a **single-tenant or development**
   cluster the default tier is **stock-unix**. On a **hostile multi-tenant**
   cluster (locked v1 requirement) the default tier is **Kata-hybrid**, because the
   microVM kernel boundary is the only real isolation against an untrusted tenant
   (§5). This is a binding rule, not a recommendation.

4. **Reject QEMU real-vhost-vsock for v1.** A host-global CID space makes
   per-pod allocation and collision-avoidance intractable, and even the Linux
   netns-local vsock mode cannot reach the host. The HARDENED tier is **hybrid-vsock
   only** (per-VM uds), which sidesteps CID allocation entirely (§4.4).

5. **Discovery, not allocation.** The node-agent **discovers** each agent's
   host-reachable socket from the pod UID + the CRI; it does **not** allocate vsock
   CIDs in v1. The hostPath socket layout and the Kata per-VM uds are both *found*,
   not *assigned* (§6).

6. **Pod→socket attestation is v1-blocking.** Because tenancy is hostile, the
   node-agent MUST prove which pod owns a discovered socket before it speaks the
   management profile to it — reachability *is* full operator authority (agentd RFC
   0015 §7), so a squatted socket is a privilege grant. `SO_PEERCRED` + cgroup→pod
   mapping on stock-unix; the runtime-owned per-VM uds on Kata (§7).

7. **The contract owes one new primitive: an exec-health verb.** kubelet HTTP/TCP/
   gRPC probes cannot reach a networkless pod and a scratch image has no shell to
   read a health file, so the isolation tiers depend on a contract-level exec-health
   verb. This RFC raises it as a contract ask (§8); it is the single hard
   cross-repo dependency of the HARDENED tier.

These seven are final for the substrate surface. Each defers to its owning sibling
RFC where it touches another plane (noted inline).

---

## 3. The endpoint descriptor — the one abstraction agentctl programs against

The endpoint descriptor is the contract between the node-agent's discovery/attest
machinery and **every other consumer in agentctl**. It is the answer to "how do I
reach agent X's management (or A2A, or metrics, or events) surface?" without the
caller knowing anything about substrates, CIDs, or hostPaths.

```jsonc
// EndpointDescriptor — produced by the node-agent discovery loop (agentctl RFC 0008),
// consumed by the operator, A2A relay, telemetry scraper, and CLI bridge.
{
  "agent": {                            // identity, from the downward API (agentd RFC 0014 §6.4)
    "uid":       "f3c1…-…-…",           // metadata.uid  — the join key for everything
    "instance":  "triage-abc",          // metadata.name
    "namespace": "agents",
    "node":      "node-3"
  },
  "surface":   "management",            // management | a2a | metrics | events  (the contract surface)
  "tier":      "kata-hybrid",           // stock-unix | kata-hybrid | sidecar-emptydir
  "advertised":"vsock:5005",            // VERBATIM from surfaces.management (provenance / self-report)
  "dial":      "unix:/run/vc/vm/3f2b/clh.sock",   // HOST-side reality the node-agent connects to —
                                                  //   this value is REPORTED BY THE CRI (PodSandboxStatus),
                                                  //   NOT parsed from runtime-internal files like persist.json (§6)
  "connect_hint": { "vsock_port": 5005 },         // hybrid-vsock needs a `CONNECT <port>` handshake (§4.2)
  "attestation": {                      // §7 — MUST be present and verified before first management call
    "method":   "kata-sandbox-uds",     // kata-sandbox-uds | so-peercred-cgroup | pod-local-sidecar
    "verified": true,
    "pod_uid":  "f3c1…-…-…"             // the UID the node-agent PROVED owns `dial`
  }
}
```

The load-bearing fields and their invariants:

- **For the management surface, `dial` is always a unix connect.** Post-discovery,
  for `surface: "management"` the node-agent never opens a vsock socket on the
  *stock* or *Kata-hybrid* tier — on Kata-hybrid the hypervisor exposes the guest's
  vsock port as a host Unix-domain socket (the per-VM `uds_path`), so `dial` is
  `unix:…` in **all three** tiers. This is what collapses three substrates into one
  code path for management (§4). The only tier-specific step is an optional
  `CONNECT <port>` line on hybrid-vsock (`connect_hint`, §4.2). **This unification
  is a management-surface property, not a universal one:** the metrics surface on a
  *networked* pod is a **TCP** dial (`:9090`, Prometheus text per agentd RFC 0016),
  not a unix connect — see the `surface` bullet below.
- **`advertised` carries provenance, `dial` carries reachability.** The agent
  reports `surfaces.management = "vsock:5005"` (its *in-guest* view, agentd RFC 0015
  §5.2); the node-agent records that verbatim for audit and translates it to the
  host-side `dial`. The agent's self-report is **descriptive, never load-bearing** —
  the node-agent MUST NOT dial `advertised` directly (a tenant controls it).
- **`attestation.verified` gates everything.** A descriptor with
  `attestation.verified == false` MUST NOT be handed to any management-capable
  consumer; the node-agent surfaces it as `Ready=False` with reason
  `AttestationFailed` (agentctl RFC 0003 §6.2 conditions taxonomy) and retries
  discovery. There is no "best-effort, unattested" path under hostile tenancy.
- **`surface` selects the contract surface; the descriptor *shape* is shared, the
  wire and the socket are not.** Only `surface: "management"` is the NDJSON/JSON-RPC
  profile whose `advertised` derives from `surfaces.management` and whose `dial` is
  unix in all tiers (above). The other surfaces are genuinely different:
  - **metrics/events** speak **Prometheus 0.0.4 text over HTTP** (and an events
    stream), not the management profile (agentd RFC 0016); they may live on a
    **separate socket** advertised via `surfaces.metrics` (not `surfaces.management`),
    and on a networked pod the metrics `dial` is **TCP** (`--health-http ADDR`, e.g.
    `:9090`), not unix. The descriptor abstraction is reused, but discovery must
    target `surfaces.metrics` and tolerate a second socket and a TCP dial.
    Furthermore, **whether `agent://metrics` is reachable over the management
    socket at all is an unresolved contract conflict** (agentd RFC 0005/0015 do not
    list it, agentd RFC 0019 does) — the contract **ask P4**, symmetric to the A2A
    ambiguity below, and a *hard* dependency on networkless (HARDENED) pods where
    there is no TCP fallback (open question (h)).
  - **A2A** (agentd RFC 0020) reuses the descriptor shape, but whether it shares the
    management listener or gets its own `surfaces.a2a` address is a contract open
    item — the **ask P2** (open question (d)); `surfaces.a2a` is not yet in the
    frozen manifest (agentd RFC 0015 §5.2).

> **Contract-as-Schema (P0, agentctl RFC 0001).** The `surfaces.management` value
> space and the downward-API identity keys this descriptor depends on are part of
> the published control contract. They are presently authored inside the reference
> implementation's repo (agentd RFC 0014/0015); a live open question (§10) is to
> extract them into a neutral *Agent Control Contract* spec with published JSON
> Schemas, so that agentctl validates a descriptor against the schema and any
> conformant agent — not just the reference one — populates it.

Why an abstraction at all, rather than "the node-agent just opens the socket":
because the substrate decision (§4) and the tenancy decision (§5) change *where*
and *how* the socket is reached and *how strongly it is attested* — and we refuse
to let that knowledge leak into the operator, the gateway, or the CLI. A new tier
(say, KubeVirt or a future confidential-VM substrate) is a new `tier` enum variant
and a new discovery+attestation strategy in the node-agent; **zero** change to any
consumer. That is the whole payoff.

---

## 4. The three substrate tiers

One control-plane object (the `AgentClass`'s substrate selector — agentctl RFC 0004)
realized three ways. The tier is chosen per `AgentClass`, defaulted by tenancy (§5),
and surfaced into each descriptor.

| Tier | Transport (in agent) | Host-side `dial` | Isolation | Runtime requirement | Default for |
|---|---|---|---|---|---|
| **PRIMARY — stock-unix** | `unix:PATH` on a hostPath/emptyDir volume | `unix:` hostPath, per-pod subdir (§6.1) | shared host kernel; filesystem ACLs + `SO_PEERCRED` | any stock runc/containerd permitting hostPath | dev, single-tenant, conformance |
| **HARDENED — kata-hybrid** | `vsock:PORT` (agent binds `VMADDR_CID_ANY`) | `unix:` per-VM uds + `CONNECT PORT` (§4.2) | **microVM kernel boundary** | Kata-Containers w/ Firecracker or Cloud-Hypervisor | **hostile multi-tenant production** |
| **MOST-PORTABLE — sidecar-emptydir** | `unix:PATH` on a shared emptyDir | `unix:` pod-local (the sidecar dials, not the node-agent) | weakest — **shared pod netns** (§4.3) | none (works on Autopilot/Fargate/PSS-restricted) | restricted clusters with no hostPath/DaemonSet |

### 4.1 PRIMARY — stock unix-socket-over-hostPath → host DaemonSet

The agent serves its management profile on the contract's bind-address
instruction — reference impl: `--serve-mcp unix:/run/agent/mgmt.sock` (the unix
serve form originates in agentd RFC 0005 §3.6; agentd RFC 0015 §3.1 generalizes it
to the vsock form) — where `/run/agent` is a per-pod directory on a hostPath
volume the node-agent DaemonSet can also see (§6.1). The node-agent opens that
socket host-side and holds the long-lived management connection. (The bind
instruction itself is an agent-branded contract surface to neutralize — open
question (a).)

This tier runs on **any** stock runc/containerd cluster that permits hostPath. There
is **no CID-allocation problem** — it is a filesystem path with filesystem ACLs as
access control, which is *exactly* the unix trust model the contract already
documents ("a unix socket inherits filesystem permissions as its access control",
agentd RFC 0012 §3.8; "whoever can reach the management transport may call the
operator tools", agentd RFC 0015 §7). The agent holds no secrets in the keyless
configuration (agentctl RFC 0012), so the DaemonSet remains the only networked
component. This is the day-one substrate (agentctl RFC 0001 roadmap, Phase 0): it
requires *no* unbuilt contract primitive on a stock cluster.

Its honest limit: it is **not** a kernel boundary. A container escape, or a node-root
process, reaches the socket. That is acceptable for single-tenant and dev (the
tenant already trusts the node); it is **not** acceptable for hostile tenancy, which
forces §5.

### 4.2 HARDENED — vsock on Kata hybrid-vsock (Firecracker / Cloud-Hypervisor)

This is the premium isolation tier and the one the vision is really about. The agent
runs inside a Kata microVM with a real guest kernel; it serves
`--serve-mcp vsock:5005` and binds `VMADDR_CID_ANY` (accepts from its host;
the node-agent connects from `VMADDR_CID_HOST = 2`, agentd RFC 0015 §3.1). The agent
is byte-for-byte unaware it is on Kata — it binds a vsock listener exactly as agent
RFC 0015 §3.2 specifies, same NDJSON framing, same `PeerOrigin::Management`.

The crucial property: **on hybrid-vsock the host side is already a per-VM
Unix-domain socket, not a host CID.** Firecracker and Cloud-Hypervisor implement
"hybrid vsock" by exposing each VM's vsock device as a single host uds; a host
process reaches a guest vsock *port* by connecting to that uds and writing a
`CONNECT <port>\n` handshake line, after which the stream is a transparent byte pipe
to the guest's `vsock:PORT` listener. So the node-agent's code path is **identical**
to the stock tier — "open a discovered unix socket" — plus one `CONNECT 5005`
line emitted from `connect_hint` (§3). There is **no host-global CID**, hence no
allocation and no collision problem.

The kernel boundary is the win: a guest container escape lands the attacker in a
*microVM*, not on the node; reaching the management socket of a *neighbouring*
tenant requires escaping the VM kernel, not merely the container. This is why §5
makes Kata the multi-tenant default.

### 4.3 MOST-PORTABLE — per-pod sidecar over an emptyDir unix socket

Where hostPath is forbidden and DaemonSets cannot run privileged (GKE Autopilot,
EKS/Fargate, PodSecurity `restricted`), neither the stock hostPath bridge nor Kata
is available. The fallback is a **per-pod sidecar** in the same pod as the agent,
sharing an `emptyDir` volume: the agent serves `unix:/run/agent/mgmt.sock` on the
emptyDir, and the sidecar — which *is* the node-agent's role for this pod — dials it
pod-locally and re-exposes the management/telemetry/A2A surfaces on the network.

**The honest caveat, stated loudly:** pod containers share the network namespace,
so a *networked* sidecar means **the agent is not network-isolated** — anything that
can reach the sidecar's pod IP shares the netns with the agent. This tier trades the
isolation posture for portability. The only way to restore reachability isolation in
this tier is an *off-pod* bridge variant, which the restricted substrates that force
this tier do not permit. We ship it because "manage the fleet at all" beats "manage
nothing on Autopilot," and we are explicit that its isolation is the weakest of the
three. It MUST NOT be the default for hostile tenancy (§5).

### 4.4 Rejected for v1 — QEMU real-vhost-vsock

QEMU's real `vhost-vsock` gives each VM a genuine guest CID on a **host-global CID
space**: any host process can dial any guest's `(CID, port)`. That reintroduces
exactly the problem hybrid-vsock avoids:

- **CID allocation/collision is intractable** as a v1 problem — the node-agent would
  have to allocate, track, and garbage-collect CIDs per pod across reschedules and
  node reboots, defend against CID reuse races, and prevent one tenant from dialing
  another's CID (the CID space has no per-tenant ACL). This is the hardest open item
  in the original vision (ideas.md §7) and we decline to solve it for v1.
- **Even Linux netns-local vsock cannot save it.** The kernel's `vsock` local/netns
  mode isolates CIDs to a namespace but, by the same token, **cannot reach the
  host** — so it is unusable as a guest↔host bridge.

Therefore the HARDENED tier is **hybrid-vsock only**. If a real-vhost-vsock
substrate is ever required, it is a future tier with its own CID-allocation RFC, not
a v1 obligation. (This is the binding answer to ideas.md §7's "vsock CID allocation"
open question: in v1 there is no CID allocation, because the two shipped vsock-class
tiers — Kata-hybrid — expose a per-VM uds, not a CID.)

---

## 5. The forced resolution — tenancy × substrate

Two locked decisions interact and must be reconciled here, because they partly
conflict: **"stock-unix is the PRIMARY substrate"** (D1) and **"hostile
multi-tenancy is a v1 requirement"** (locked 2026-06-27). True isolation against an
untrusted tenant needs the microVM kernel boundary, which the stock tier does not
have. The brainstorm flags this as the one tension to settle before this RFC
(§0.6). This RFC settles it:

> **Binding rule.** On a cluster that admits **hostile (untrusted) tenants**, the
> **default and required tier for tenant workloads is Kata-hybrid**; the stock-unix
> tier is **forbidden** for untrusted tenant agents. On a **single-tenant or
> development** cluster, the **default tier is stock-unix**, and Kata-hybrid is an
> opt-in hardening. The MOST-PORTABLE sidecar tier is permitted for untrusted
> tenants **only** on substrates where Kata is unavailable, and **only** with the
> explicit, audited acknowledgement that its isolation is netns-shared (§4.3).

Mechanically, this is enforced at two points:

- **`AgentClass.substrate` selection (agentctl RFC 0004).** A cluster marked
  `tenancy: hostile` defaults every `AgentClass`'s tier to `kata-hybrid` and an
  admission check (agentctl RFC 0007) **rejects** an `AgentClass` that selects
  `stock-unix` for a tenant namespace. A `tenancy: single` cluster defaults to
  `stock-unix`.
- **`runtimeClassName` rendering.** The Kata tier renders the workload with the
  cluster's Kata `RuntimeClass` (agentctl RFC 0003 §mode→workload); the operator
  refuses to render a tenant agent without it under hostile tenancy.

The rationale is the security model's collapse point (agentctl RFC 0015):
**reachability of the management transport == full operator authority** (agentd RFC
0015 §7 — the operator profile is gated by `PeerOrigin`, which is reachability, not
a credential). On stock-unix, "reachability" is filesystem-and-kernel-scoped; a
container escape or a node-root compromise crosses it. Under hostile tenancy that is
not a boundary we can stand behind, so we require the kernel boundary. This is also
why §7 (attestation) is v1-blocking rather than deferred: under hostile tenancy a
mis-attested socket is a cross-tenant authority grant.

---

## 6. Discovery, not allocation

The node-agent **discovers** the host-reachable socket for each local agent; it does
not allocate CIDs (§4.4). Discovery is keyed by the **pod UID** (the descriptor join
key, §3), learned from an apiserver watch scoped to `spec.nodeName == self` plus the
CRI (agentctl RFC 0008). It MUST prefer the **CRI API**
(`PodSandboxStatus` / container annotations) over parsing a runtime's internal state
files (e.g. Kata's `persist.json`), which are version-volatile and break on runtime
upgrades. CRI-socket access is treated as node-root in the threat model
(agentctl RFC 0008, 0015).

### 6.1 The stock-unix hostPath socket layout and lifecycle

The socket lives in a **per-pod subdirectory** of a node-agent-owned hostPath root,
so no tenant pod can write into a neighbour's directory (the structural defence
against squatting, hardened further by attestation in §7):

```yaml
# Rendered by the operator into the agent pod (agentctl RFC 0003). The per-pod
# subdir is selected by the pod UID via subPathExpr + the downward API, so the
# path is unique and pod-scoped WITHOUT the operator knowing the UID at render time.
spec:
  containers:
    - name: agent
      # The contract's BIND-ADDRESS INSTRUCTION (where the agent exposes its
      # management socket). Reference impl spelling: the AGENT_SERVE_MCP env (or
      # the --serve-mcp CLI flag, agentd RFC 0015 §3.1). It is an agent-BRANDED
      # contract surface — a second conformant agent may take a different flag/env
      # to set its bind address — and is flagged for contract-neutralization in
      # open question (a). Prefer the ENV form (env-injectable, mirrors the
      # downward-API keys below) over a hardcoded CLI flag in the pod spec.
      env:
        - name: AGENT_SERVE_MCP
          value: "unix:/run/agent/mgmt.sock"             # contract: surfaces.management = "unix:/run/agent/mgmt.sock"
        - name: AGENT_POD_UID
          valueFrom: { fieldRef: { fieldPath: metadata.uid } }   # agentd RFC 0014 §6.4
      volumeMounts:
        - name: agentctl-sockets
          mountPath: /run/agent
          subPathExpr: $(AGENT_POD_UID)        # → host: /run/agentctl/sockets/<pod-uid>/
  volumes:
    - name: agentctl-sockets
      hostPath: { path: /run/agentctl/sockets, type: DirectoryOrCreate }
```

```
HOST                                                   GUEST (pod mount namespace)
/run/agentctl/sockets/                  (0711, node-agent SA owns)
  ├── f3c1…/  ── mounted as ──▶  /run/agent/        ◀── agent binds mgmt.sock here
  │     └── mgmt.sock            (pod A only; subPathExpr scopes the mount to A's UID)
  ├── a92d…/                     pod B mounts ONLY a92d…/ — cannot see/write f3c1…/
  └── …
node-agent reads:  unix:/run/agentctl/sockets/<pod-uid>/mgmt.sock   ◀── this is `dial`
```

Layout and lifecycle rules (normative):

- The hostPath root `/run/agentctl/sockets` is created by the node-agent DaemonSet
  at startup, mode `0711`, owned by the node-agent ServiceAccount's host identity.
  `0711` lets a pod traverse into its own subdir by exact name but **not list** the
  root (it cannot enumerate sibling UIDs).
- Each pod mounts **only its own** `<pod-uid>/` subdir via `subPathExpr`
  (`$(AGENT_POD_UID)`); a tenant pod cannot traverse up out of its mount, so it
  **cannot create a socket at a sibling's path**. This is squat-resistance by
  construction; §7 adds the cryptographic-grade binding.
- The agent binds the socket post-start; the node-agent's discovery loop watches the
  per-pod subdir for the socket to appear (inotify), builds the descriptor, attests
  (§7), and connects. A socket that never appears within the readiness window
  surfaces `Ready=False`/`ManagementUnreachable`.
- **GC:** when the pod UID disappears from the apiserver watch + CRI, the node-agent
  prunes `<pod-uid>/` (closing any stale connection first). A crashed pod's stale
  socket file is removed on pod GC, never reused.

> The sidecar-emptydir tier (§4.3) uses the same `unix:/run/agent/mgmt.sock`
> contract value, but the volume is a plain `emptyDir` shared with the sidecar in
> the same pod; the sidecar dials it pod-locally. Because an emptyDir is mounted
> into only one pod, it is squat-proof across pods by definition.

### 6.2 The Kata-hybrid per-VM uds discovery

On Kata-hybrid the agent binds `vsock:5005` in the guest; the hypervisor
(Firecracker / Cloud-Hypervisor) exposes that VM's vsock device as a single host
uds. The node-agent discovers the uds path from the CRI `PodSandboxStatus` for the
pod's sandbox (not from runtime-internal files), builds
`dial = unix:<uds_path>` with `connect_hint.vsock_port = 5005`, and reaches the
guest listener via the `CONNECT 5005` handshake (§4.2). The pod UID → sandbox → uds
mapping IS the access-control table (agentctl RFC 0015, the hybrid-vsock
multi-tenancy mandate): the runtime created the uds and bound it to the sandbox, so
the guest cannot have squatted it.

The minimum-privilege shape of this mapping — exactly which CRI fields carry the uds
path, and what host privilege the node-agent needs to open it — is a node-agent
concern (agentctl RFC 0008) and a known open item (§10).

---

## 7. Pod→socket attestation (AT) — v1-blocking

Because tenancy is hostile and reachability equals authority (§5), the node-agent
**MUST prove which pod owns a discovered socket before speaking the management
profile to it.** Without this, a malicious tenant that can place a socket where the
node-agent expects a victim's socket (socket-squatting) receives, in effect,
`drain`/`cancel`/`inventory`/steer authority over the victim. This is promoted from
a deferred open item to a **v1-blocking design requirement** (brainstorm §0.6, AT).

The threat is specific: the node-agent is the *client*; the agent that bound the
socket is the *server*. The node-agent must verify that the **server it just
connected to is the pod UID it intended to manage** — i.e. that `dial` is owned by
`attestation.pod_uid`. The mechanism differs per tier:

| Tier | Attestation method | How the binding is proven |
|---|---|---|
| **stock-unix** | `so-peercred-cgroup` | `getsockopt(SO_PEERCRED)` on the connected stream returns the server peer's `(pid, uid, gid)`; the node-agent maps `pid → /proc/<pid>/cgroup → pod UID` (via the CRI / cgroup-v2 pod slice) and **requires it to equal** the expected `pod_uid`. Mismatch ⇒ squatting ⇒ refuse + alert. |
| **kata-hybrid** | `kata-sandbox-uds` | The per-VM uds is created by the runtime and bound to the sandbox; the guest cannot create a host uds. The node-agent verifies the uds path it dials is the one CRI reports for the pod's sandbox. Isolation is *natural* — there is no squattable shared path. |
| **sidecar-emptydir** | `pod-local-sidecar` | The sidecar shares the pod's emptyDir; the socket is reachable from no other pod. Attestation is the pod boundary itself (one emptyDir, one pod). |

Normative requirements:

- The node-agent MUST perform attestation **before the first `tools/call`** (before
  any `drain`/`cancel`/steer) and MUST re-attest on every reconnect (agentd RFC 0015
  §8: reconnect is a clean re-read, correlated by `identity.uid` — attestation is
  part of that re-read).
- On `stock-unix`, `SO_PEERCRED` returns the peer's credentials *as of `listen()`*,
  so it identifies the process that bound the socket, which is precisely the squat
  test. The `pid → pod UID` resolution MUST go through the CRI/cgroup mapping, never
  a tenant-supplied value. A `pid` that resolves to no pod, or to a different pod, is
  a hard failure (`AttestationFailed`).
- Attestation failure MUST set `attestation.verified = false`, withhold the
  descriptor from all management-capable consumers (§3), emit a security event
  (agentctl RFC 0015 audit vocabulary), and **not** silently degrade.
- The `SO_PEERCRED` UID alone is **insufficient** — two tenant pods can run as the
  same UID — so the cgroup→pod-UID mapping (not the bare UID) is the load-bearing
  check on stock-unix. The per-pod subPathExpr layout (§6.1) makes squatting hard;
  attestation makes it *detected even if the layout assumption is violated* (defence
  in depth).

This is the single most important security mechanism this RFC introduces, and it is
the reason the stock-unix tier is dev/single-tenant-only by default: `SO_PEERCRED`
attestation defends against squatting but **not** against a node-root compromise,
whereas the Kata tier's natural uds isolation rides the kernel boundary.

---

## 8. Probes on networkless pods

A networkless agent pod (the HARDENED and off-pod variants) defeats the standard
kubelet probes: **`httpGet` / `tcpSocket` / `grpc` probes dial INTO the pod
network**, and a pod with no cluster network has nothing to dial. **Only `exec`
probes traverse the CRI** (kubelet → `ExecSync`, no network). But the agent ships on
a `scratch` image with no shell and no `cat`, so an `exec` probe cannot
`["cat", "/healthz"]` a health file.

The reference implementation ships `--health-file` (an on-disk readiness/liveness
file, agentd RFC 0010 §3.7), which is the right *state*, but a scratch image cannot
*read* it via a probe. So the contract needs an **exec-health verb the agent binary
itself answers**:

> **Contract ask (P1).** The contract MUST define an exec-health verb — a
> subcommand of the agent binary that reads its own health/readiness state and exits
> `0` (healthy) / non-zero (unhealthy), with no shell and no network. The reference
> implementation should ship e.g. `agent --check-health PATH` (reads its own
> `--health-file` and exits accordingly). This is the one hard cross-repo dependency
> of the HARDENED tier and gates every milestone on a networkless substrate.

The readiness/liveness story per tier:

| Tier | Liveness probe | Readiness probe | Notes |
|---|---|---|---|
| **stock-unix** (networked) | `exec` health verb (preferred) or `httpGet /healthz` if the agent has a pod IP | same | networked stock pods MAY use HTTP probes; the exec verb is still preferred for parity |
| **stock-unix / Kata / off-pod** (networkless) | `exec` health verb — the **only** option | `exec` health verb | requires P1; `httpGet`/`tcpSocket`/`grpc` are unusable |
| **sidecar-emptydir** | `exec` on the agent container, or the sidecar relays health | sidecar can expose `/healthz` on the shared netns | the sidecar restores an HTTP probe surface if desired |

Normative:

- The operator MUST render **`exec`** probes invoking the contract exec-health verb
  for any networkless agent; it MUST NOT render `httpGet`/`tcpSocket`/`grpc` probes
  for them (they would spuriously fail-CrashLoop the pod). This is compiled from the
  tier, gated on the agent advertising the exec-health verb in `surfaces` (graceful
  degradation, agentd RFC 0014 §6.2/§8).
- Liveness MUST track the supervisor heartbeat / health-file (agentd RFC 0010 §3.7,
  the source of the positive liveness signal), not the management connection — and a
  dropped management connection is **not** a liveness signal (agentd RFC 0015 §8
  supports only this negative point). A networkless pod whose node-agent bounced is
  still alive; only its control/telemetry reach gapped.
- Until P1 lands, a networkless tier is **not shippable**; the stock-unix
  *networked* tier (with HTTP probes or the exec verb if present) is the
  no-unbuilt-primitive day-one path (agentctl RFC 0001 roadmap).

---

## 9. Substrate compatibility matrix

Which tier each common platform supports. "✓ default" marks the tier agentctl
selects by default on that platform under the stated tenancy; "✓" = supported but
not default; "—" = unavailable.

| Platform | stock-unix (PRIMARY) | kata-hybrid (HARDENED) | sidecar-emptydir (PORTABLE) | Why / caveat |
|---|---|---|---|---|
| **EKS** (managed node groups, stock) | ✓ default (single-tenant) | — (no Kata unless self-managed) | ✓ | hostPath + DaemonSet ok; no microVM runtime by default ⇒ hostile tenancy needs self-managed Kata nodes |
| **GKE Standard** | ✓ default (single-tenant) | — (GKE Sandbox is gVisor, not Kata) | ✓ | hostPath + DaemonSet ok; **no guest↔host vsock** anywhere on GKE |
| **GKE Autopilot** | — (no hostPath, no privileged DaemonSet) | — | ✓ default (only option) | restricted; sidecar-emptydir is the **only** tier; isolation is netns-shared |
| **AKS** (stock node pools) | ✓ default (single-tenant) | ✓ default (hostile) **via AKS Pod Sandboxing** | ✓ | AKS Pod Sandboxing = Kata + Cloud-Hypervisor ⇒ the one managed tier with a real microVM boundary |
| **EKS Fargate** | — (no hostPath, no DaemonSets) | — (Firecracker not exposed; no vsock, no DaemonSet) | ✓ default (only option) | Fargate forbids DaemonSets and hostPath ⇒ sidecar-emptydir only; the underlying Firecracker is invisible to us |
| **GKE Sandbox (gVisor)** | △ (runsc gofer makes hostPath uds unreliable) | — (`runsc` is a userspace interceptor, **no `AF_VSOCK`**) | ✓ default | gVisor is a sandbox, **not** a microVM; no vsock; in-sandbox sidecar-emptydir is the reliable path |
| **Self-managed + Kata** | ✓ (single-tenant) | ✓ default (hostile) — **recommended** | ✓ | full control; the canonical HARDENED deployment (Firecracker / Cloud-Hypervisor hybrid-vsock) |

Reading the matrix: **the strongest isolation is a real microVM (Kata-hybrid), and
only AKS Pod Sandboxing and self-managed-Kata clusters offer it.** On every
mainstream managed tier without Kata, the honest posture is stock-unix
(single-tenant) or sidecar-emptydir (restricted) — both *without* the kernel
boundary. This is the substrate-reality answer to the vision's "strongest isolation
in the ecosystem" claim: it is true, but it is a **premium tier on a minority of
clusters**, not a property of agentctl everywhere (open question 1 in the brainstorm
§17).

---

## 10. Honesty corrections — what "no network" does and does not buy

The vision's framing ("NO cluster network — nothing to attack", ideas.md §2) is
seductive and partly false. This RFC carries forward three corrections so no later
RFC inherits the overclaim:

1. **"No network ≠ nothing to attack."** The intelligence/model channel is an
   **irreducible egress leg** — the agent's reasoning loop blocks on an LLM call
   that must leave the trust boundary (agentd RFC 0006), and in the lethal-trifecta
   model that channel is the *dangerous* one (agentd RFC 0012 §1). Moreover,
   **guest→host vsock is itself a live egress channel**: a compromised agent can
   write out over the very vsock it serves/dials. Removing the cluster netns removes
   *IP-layer* reachability; it does not remove egress.

2. **NetworkPolicy does not govern vsock.** NetworkPolicy is IP-layer and
   CNI-dependent; it does nothing against `AF_VSOCK` guest→host traffic. "True no
   network" is a **microVM property** (the VM has no NIC), not a NetworkPolicy
   setting. Under the HARDENED tier the host MUST additionally restrict guest→host
   vsock egress to only the provisioned ports (the intelligence dial and the
   management/A2A serve ports), as a host-side control the node-agent owns
   (agentctl RFC 0015, the guest→host vsock egress restriction). This is a
   deployment control, not an agent feature.

3. **The real isolation win is the kernel boundary + the agent-side firewall, not
   the absence of a NIC.** The two defences that actually matter are (a) the
   **microVM kernel boundary** (Kata-hybrid) — orthogonal to vsock, the thing that
   makes a neighbour-tenant compromise require a VM escape; and (b) the **agent-side
   reader/actor distillate firewall** (agentd RFC 0012 §3.3) — the structural
   CaMeL-style split that quarantines untrusted content in a no-egress reader and
   passes only a low-bandwidth distillate to an actor. agentctl markets *those*, and
   surfaces the trifecta tags from the manifest (agentctl RFC 0015), rather than
   "nothing to attack." The sidecar-emptydir tier in particular keeps the agent on
   the pod netns (§4.3), so it has *neither* the NIC-removal nor the kernel boundary
   — its only isolation is the distillate firewall plus whatever the cluster's
   NetworkPolicy provides.

---

## 11. Non-goals (these live in other planes or in the agent)

- **Provisioning the microVM / vsock device / host model service.** That is the
  node-agent's and the platform's job (agentd RFC 0015 §3.3 is explicit: the agent
  only *uses* the CID/port it is given). This RFC defines *discovery and reach*, not
  device setup.
- **CID allocation.** Out of v1 by §4.4. There is no v1 path that allocates a vsock
  CID; both vsock-class tiers use a per-VM uds.
- **The management access path / RBAC / aggregated APIServer.** *Who* may call which
  verb over the descriptor, and how human identity reaches the node-agent, is
  agentctl RFC 0009. This RFC stops at "a verified socket the authorized caller can
  reach."
- **The A2A HTTP/SSE/auth/webhook machinery and the durable task store.** agentctl
  RFC 0013. This RFC only states that A2A reuses the descriptor abstraction.
- **The metrics scrape-proxy, run-report capture, exit-code→podFailurePolicy.**
  agentctl RFC 0010. This RFC only states that telemetry reaches the agent via a
  descriptor and is gated by the same probe realities (§8).
- **Defining the contract's `surfaces`/manifest/exit-code/exec-health schemas.**
  Those are the contract's (agentd RFC 0014–0016); this RFC consumes them and raises
  the P1 exec-health ask. Schema-extraction is open question (a) below.
- **Any per-tenant isolation that is not a substrate property.** Multi-tenancy is a
  substrate decision (§5), not a NetworkPolicy decision (§10, correction 2); the
  full multi-tenant trust model is agentctl RFC 0015.

---

## 12. Rollout & compatibility

- **The tier is additive and version-negotiated.** A conformant agent advertises
  `surfaces.management = false | "vsock:PORT" | "unix:PATH"` (agentd RFC 0015 §5.2);
  agentctl reads it, picks the tier from the `AgentClass` substrate selector, and
  degrades gracefully — an agent reporting `surfaces.management: false` is managed
  by liveness + exit codes + logs only (agentd RFC 0014 §8). No agent change is
  required to add a tier on the agentctl side.
- **stock-unix ships first, networkless tiers gate on P1.** The day-one,
  no-unbuilt-primitive path is stock-unix on a networked pod (agentctl RFC 0001
  roadmap Phase 0/MVP). The HARDENED (Kata-hybrid) and any networkless variant land
  in a later phase, gated on the exec-health verb (§8, P1) and the per-VM-uds
  discovery mapping (§6.2). The node-agent code is the same; only the discovery +
  attestation strategy and the probe rendering differ.
- **Single dev/CI loop, real risky-path coverage.** The unix fast-loop (a real agent
  over a unix socket) is the contract-clean dev loop and exercises the descriptor +
  attestation logic; CI MUST additionally run a **kind + Kata** lane for the
  guest↔host crossing (the project's actual hard part — hybrid-vsock `CONNECT`,
  per-VM uds discovery, `SO_PEERCRED` mapping), per agentctl RFC 0001/0018. A
  unix-only loop never exercises the crossing and would hide tier bugs.
- **Tier migration is a workload re-render, not a contract change.** Moving an
  `AgentClass` from stock-unix to Kata-hybrid re-renders the workload with the Kata
  `RuntimeClass` and switches the descriptor's discovery strategy; the agent binary,
  its config, and the contract are unchanged. A cluster that flips
  `tenancy: single → hostile` triggers the §5 binding rule for all tenant classes.

---

## 13. Open questions

(a) **Extract the contract into a neutral *Agent Control Contract* spec (P0).** The
`surfaces.management` value space, the downward-API identity keys, the
**bind-address instruction** (the mechanism by which the operator tells a conformant
agent *where* to bind its management socket — reference impl: `AGENT_SERVE_MCP` /
`--serve-mcp`, agentd RFC 0015 §3.1, rendered in §6.1), and the exec-health verb
this RFC depends on are presently authored inside the reference implementation's
repo (agentd RFC 0014–0016) and are agent-**branded**. Recommended: extract them
into a language-neutral spec with published JSON Schemas so the endpoint descriptor
(§3) validates against the schema and *any* conformant agent — taking whatever
flag/env *it* defines for its bind address — populates it, neither side owning the
other. Tracked with agentctl RFC 0001/0018.

(b) **The exec-health verb (P1) — exact shape and `surfaces` advertisement.** Does
the verb read the existing `--health-file`, or compute readiness directly? Which
`surfaces` key advertises it so the operator can gate probe rendering (§8)? This is
the one hard cross-repo dependency of the HARDENED tier.

(c) **Kata per-VM-uds discovery API + minimum privilege.** Exactly which CRI fields
carry the hybrid-vsock uds path across Firecracker and Cloud-Hypervisor, and the
minimum host privilege the node-agent needs to open it (§6.2). Avoid parsing
runtime-internal files (version-volatile).

(d) **Does A2A share the management listener or get its own `surfaces.a2a`
address?** The descriptor abstraction is identical either way, but the discovery
target differs (one socket vs two). Blocked on the contract committing `surfaces.a2a`
(the P2 contract ask; agentd RFC 0015 §5.2 does not yet list it). agentctl RFC 0013.

(e) **`SO_PEERCRED` mapping robustness on exotic cgroup layouts.** The
`pid → cgroup → pod-UID` resolution (§7) assumes a discoverable cgroup-v2 pod slice;
confirm it holds across container runtimes and on nodes with cgroup-v1 or hybrid
hierarchies, and define the failure mode when it does not (hard-fail, per §7).

(f) **Multi-pod-per-VM.** §4 assumes one agent per guest. If a future deployment
runs multiple agents in one microVM sharing one vsock device, port-per-pod
allocation becomes the node-agent's responsibility (agentd RFC 0015 §10 flags this
as a contract open item). Out of v1 scope; recorded so the descriptor's
`connect_hint` can grow a per-pod port without a breaking change.

(g) **Off-pod bridge for the portable tier.** Is there a restricted-substrate
variant that removes netns reachability without hostPath or a DaemonSet (§4.3)? None
is known for Autopilot/Fargate today; revisit if a substrate exposes one.

(h) **Is `agent://metrics` reachable over the management socket? (P4 — a contract
conflict, symmetric to P2/A2A.)** The metrics surface (§3) speaks Prometheus text
over HTTP and may live on its own socket advertised via `surfaces.metrics`; on a
*networkless* (HARDENED) pod there is no TCP fallback, so whether metrics is also
exposed over the management/vsock socket is **load-bearing, not a detail**. The
contract is presently inconsistent (agentd RFC 0005/0015 do not list
`agent://metrics`; agentd RFC 0019 assumes it). Resolution is the contract ask
**P4**; until then the telemetry descriptor (agentctl RFC 0010) cannot rely on
in-socket metrics on networkless pods.

---

## 14. References

**Sibling agentctl RFCs (this foundational track):**

- **agentctl RFC 0001** — Stack & repo decision record: Rust for all five
  components, the Contract-as-Schema (P0) anti-drift strategy, the kind+Kata CI lane
  this RFC's risky path needs.
- **agentctl RFC 0003** — Agent & AgentFleet CRD schema + status contract: the
  `surfaces` exposure flags, `runtimeClassName`/substrate fields, the `Ready`
  condition reasons (`ManagementUnreachable`/`AttestationFailed`), mode→workload rendering.
- **agentctl RFC 0004** — AgentClass / IntelligenceService / MCPServerSet: where the
  substrate tier and the tenancy default (§5) live as ops policy.
- **agentctl RFC 0007** — Admission validation ladder: enforces the §5 binding rule
  (reject stock-unix for hostile-tenant classes) and renders exec probes per tier.
- **agentctl RFC 0008** — node-agent architecture (two tiers): owns discovery,
  the connection manager, CRI access, and the attestation implementation.
- **agentctl RFC 0009** — Management access path & RBAC: who may call which verb
  over a verified descriptor.
- **agentctl RFC 0010** — Observability & telemetry bridge: the scrape-proxy and the
  probe realities (§8) consume descriptors.
- **agentctl RFC 0012** — Intelligence plane: the keyless model-dial egress leg
  (§10, correction 1) and zero-secret-in-pod posture.
- **agentctl RFC 0013** — A2A gateway & task store: A2A reuses the descriptor.
- **agentctl RFC 0015** — Security & multi-tenancy: the hybrid-vsock multi-tenancy
  mandate (§4.2), the guest→host vsock egress restriction (§6.2/§6.3), the attestation
  threat model (§4.3), and the isolation of control-plane execution of tenant images
  (§7.5).

**Contract spec (the reference implementation's current home — agentd RFCs):**

- **agentd RFC 0014 (the reference impl's contract spec)** §2 (the data/control-plane
  split, vsock-as-management), §6.2 (`surfaces{}` is the single discovery point),
  §6.4 (the downward-API env convention this RFC keys descriptors on).
- **agentd RFC 0015 (the reference impl's contract spec)** §3 (`--serve-mcp
  vsock:PORT`, the blocking thread-per-connection listener, `PeerOrigin::Management`
  identical for unix/vsock, the trust domain), §5.2 (the manifest;
  `surfaces.management = false | "vsock:PORT" | "unix:PATH"`), §7 (reachability ==
  operator authority), §8 (reconnect = clean re-read), §10 (the CID/multi-pod-per-VM
  open item).
- **agentd RFC 0006 (the reference impl's contract spec)** — the vsock transport
  (`vsock:<cid>:<port>`, `VMADDR_CID_HOST = 2`) the management listener mirrors; the
  intelligence dial-out egress leg.
- **agentd RFC 0012 (the reference impl's contract spec)** §3.3 (the reader/actor
  distillate firewall — the real injection defence), §3.8 (the unix-socket
  transport-is-the-boundary trust model the stock tier inherits), §3.9 (sandboxing
  delegated to the deployment boundary).
- **agentd RFC 0010 / 0016 (the reference impl's contract spec)** — `--health-file`
  and the health/readiness state the exec-health verb (§8, P1) must expose; the
  frozen metrics surface the telemetry descriptor scrapes.
- **agentd RFC 0020 (the reference impl's contract spec)** — A2A-over-vsock and the
  node-agent-as-gateway posture (§3, descriptor reuse for the A2A surface).

*Where this RFC and a contract spec disagree on the wire, the contract wins and this
RFC is corrected; where this RFC identifies a missing or defective primitive
(the exec-health verb, P1; `surfaces.a2a`, P2; schema extraction, P0), it becomes a
contract ask — never a leak of cluster logic into the agent.*
