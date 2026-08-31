# Sandbox cell — threat model (P5-5)

The sandbox cell runs **untrusted, agent-authored code**. It is the one place
in agentctl where hostile code execution is the *expected* input, so its
containment is defense-in-depth: no single layer is trusted alone.

## Asset and adversary

- **Asset:** the cluster (other tenants' pods, the control plane, node
  secrets, the cloud metadata endpoint) and the platform's own credentials.
- **Adversary:** code submitted through `sandbox.run` — assume it is actively
  malicious, attempts network egress, tries to read a service-account token,
  and probes for a container escape.
- **In scope:** confidentiality/integrity of everything outside the cell pod;
  availability of the platform (a run must not exhaust the node).
- **Out of scope (documented residual):** a kernel 0-day container escape on
  the *shared* runtime — mitigated, not eliminated, by the optional
  `runtimeClassName` (Kata/gVisor) kernel-isolation upgrade.

## Layers (each independent)

| # | Control | Defeats |
|---|---|---|
| 1 | **Cell namespace + deny-all NetworkPolicy** (Ingress *and* Egress, `podSelector: {}`) | Egress to other pods, the API server, the cloud metadata IP, the internet. No DNS, no lateral movement. |
| 2 | **`automountServiceAccountToken: false` + `enableServiceLinks: false`** | Credential theft: there is *no* token in the pod to steal even if the code fully owns the container. The runner's own RBAC (pod exec, cell namespace only) never enters the pod. |
| 3 | **Hardened SecurityContext** — `runAsNonRoot`, uid 65534, `allowPrivilegeEscalation: false`, `capabilities.drop: [ALL]`, `readOnlyRootFilesystem: true`, `seccompProfile: RuntimeDefault` | setuid escalation, capability abuse, writing the rootfs, the syscall surface for many escapes. Writable space is one size-limited `/work` emptyDir. |
| 4 | **Resource limits + wall-clock timeout** (CPU/memory `limits`, `sizeLimit` on emptyDirs, server-side `timeout_secs` ≤ 300) | Node exhaustion, fork bombs, crypto-mining, infinite loops. A timed-out run is *killed by pod deletion*. |
| 5 | **Single-use pods** — every run leases a warm pod, execs once, and **deletes it**; a replacement spawns behind it | Cross-run contamination: run N cannot leave state, a listener, or a poisoned interpreter for run N+1. A leased pod is never handed to a second run. |
| 6 | **Server-side output caps** — stdout/stderr and each declared `out_file` truncated (64 KiB / 256 KiB), file names must be bare (no `..`, no `/`) | Exfil-by-output-flooding, path traversal out of `/work`, memory blow-up in the runner. |
| 7 | **RuntimeClass (optional)** — `runtimeClassName` puts each cell in a Kata/gVisor microVM | Kernel-level container escape (the residual from a shared runtime). Off by default (needs a node-installed runtime); on with one values flag. |

## Trust boundary

Callers reach `sandbox.run` only through a tenant gateway (`MCPService`
registration), so the **verified caller identity** (P5-2) and per-tool
governance already gate *who* may run code before the request reaches the
cell. The cell adds no identity of its own — it is a pure execution backend
behind that boundary, which is why the runner holds no cluster credentials
beyond pod-exec in its own cell namespace.

## Residual risks (accepted, tracked)

- **Shared-kernel escape** without `runtimeClassName`: mitigated by seccomp +
  dropped caps + non-root, not eliminated. Production handling sensitive code
  sets a Kata/gVisor RuntimeClass.
- **NetworkPolicy enforcement depends on the CNI.** kindnet does not enforce;
  a policy-capable CNI (Calico/Cilium) is required for layer 1. The other six
  layers hold regardless, and the deny-all policy renders unconditionally so
  it is active the moment the cluster can enforce it.
- **Warm-pool cost:** idle cells consume requests-worth of CPU/memory per
  language. `warmPool: 0` trades latency for zero idle cost.
