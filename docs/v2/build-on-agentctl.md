# Build on agentctl — the managed-service profile (P7-5)

> agentctl is a platform you build a product on, the way Svix is the backbone
> for webhooks. This guide is for the team standing up **their own managed
> agent service** on top of it: what the platform gives you, the boundaries
> you build against, and the hardening the multi-tenant profile expects.

This is the *operator/integrator* view. For the architecture, see
[ARCHITECTURE.md](ARCHITECTURE.md); for the security model,
[security.md](../security.md) and [sandbox-threat-model.md](sandbox-threat-model.md).

## What you get out of the box

Everything below is one Helm release; you compose which planes are on.

| You want to offer… | agentctl gives you | Your product surface |
|---|---|---|
| "Sign up, get an agent" | `Organization` CR → per-org namespace, tenant mcpg gateway, supervisor-per-user | Your signup calls the management API / applies CRs |
| "Bring your own login" | OIDC federation (`identity.providers[]`); device flow (CLI) + auth-code (web) | You map IdP claims → orgs/groups; no passwords to hold |
| "Let my agent use my Zendesk" | Connections (`agentctl connect`), custody + RFC 8693 exchange, per-user tokens injected upstream (P5-3/4) | A "Connect" button hitting `/v1/connections/start` |
| "Run agent code safely" | The sandbox cell — single-use, network-denied pods (P5-5) | Register `sandbox.run` in the org's MCPService registry |
| "Humans approve risky actions" | HITL gates answered under the right identity, fanned to a channel (P5-6) | Set `spec.approval` + `hitl: [webhook:…]`; wire your Slack |
| "Charge for it" | Billing-ready metering — attributed events + export (P7-4) | Your billing reads `…/metering/export` |
| "Prove what happened" | One queryable audit trail across the planes (P7-3) | Your compliance UI reads `…/audit/query` |
| "Scale to zero" | Webhook/claim scale-from-zero, supervisor park/wake (P6-5/P7-6) | Nothing — it's automatic |

## The four boundaries you build against

1. **The management API** (`management.agentctl.dev/v1alpha1`) — an aggregated
   Kubernetes API server. Lifecycle verbs (`drain`/`pause`/…), metering export,
   audit query. Authenticated as your control-plane's ServiceAccount; authorized
   by Kubernetes RBAC (SubjectAccessReview). This is your programmatic surface.
2. **The CRDs** (`agentctl.dev/v1alpha2`) — `Organization`, `Agent`,
   `AgentFleet`, `AgentClass`, `MCPService`, `Supervisor`. Declarative; apply
   them from your product backend. `AgentClass` is your product's *plan*: it
   caps what an org's agents may do (images, capabilities, HITL floor, model
   pools), and an org cannot weaken a class floor.
3. **The A2A gateway** (`/orgs/{org}/…`) — the tenant-scoped data plane your
   end users' clients talk to. Inbound OIDC bearer → introspected → the caller's
   per-(user,agent) principal injected upstream. This is where conversations and
   the hooks door live.
4. **The tenant mcpg gateway** (per org) — the capability plane. You never call
   it directly; you populate the org's `MCPService` registry and it federates
   those tools, governed, to the org's agents.

## Multi-tenant hardening checklist

The single-tenant defaults are safe; the *managed-service* profile adds:

- [ ] **A policy-enforcing CNI** (Calico/Cilium). The chart renders every
      NetworkPolicy — the sandbox deny-all, the per-agent tenancy boundary, the
      identity/state perimeters — but kindnet does **not** enforce them. Without
      a real CNI the sandbox cell's network isolation and cross-tenant isolation
      are inert. This is the single most important production prerequisite.
- [ ] **`identity.sealKeySecret`** wired to a real 32-byte key (not the
      ephemeral dev default) — it is the only thing standing between a database
      dump and every stored refresh token. Rotate via envelope re-encryption.
- [ ] **`apiToken.enabled: true`** — arm the coarse data-plane bearer so the A2A
      ingress isn't open; per-agent OIDC (`access.oidc`) is the finer gate on top.
- [ ] **The verified tier for tenant gateways** — `identity.service.aauth`
      armed so callers present JWKS-verified, audience-bound tokens (P5-2), not
      the header-asserted bootstrap tier.
- [ ] **Budgets** (`AgentFleet.spec.budget`) and per-agent quotas so one tenant
      cannot exhaust the metered planes; breach pauses intake, it does not spill.
- [ ] **Audit shipping on** (`tenantMcpg.auditShipperImage`) so the mcpg
      capability-plane records join the queryable trail off-pod (P7-3).
- [ ] **A KMS-backed seal** and **s3-worm audit** for the compliance profile —
      both slot behind the same seams the dev defaults use.
- [ ] **`sandbox.runtimeClassName`** (Kata/gVisor) if you run genuinely hostile
      code — the shared-kernel escape is the sandbox's one residual risk.
- [ ] **PodSecurity `restricted`** on tenant namespaces — every workload the
      operator renders already satisfies it (non-root, caps dropped, RO rootfs).

## API hardening the platform already applies

You inherit these; know they are load-bearing so you don't defeat them:

- **Admission validates before it admits** — image allowlists, capability
  trifecta gating, model-pool permission, handle uniqueness, HITL floors,
  webhook-exposure-must-have-a-trigger, and a referenced-Secret existence rung
  (a dangling `{{secret:…}}` is caught at admission, not at a crash-looping pod).
- **Fail-closed by default** — identity admin surfaces refuse with no token;
  the exchange refuses a user-less subject; a gate with no channel refuses at
  compile; budget/metering outages fail *open* only for availability, never for
  authorization.
- **Attribution is not forgeable** — org membership is stamped server-side by
  the gateway (a supervisor cannot assert its own groups); the audit shipper's
  org is forced from its token, not its payload; OBO acting-user comes from a
  signed claim the workload cannot mint.
- **Config hot-reload is real, but scoped** — the operator rolls a pod only on
  a *restart-required* change (image, mounts, `store`, listeners/TLS,
  `security`, `a2a.principals`, `webhooks`); persona, workflows, model bindings
  and MCP servers hot-reload through the ConfigMap with no roll. One subtlety
  if you template an agent's instruction: an embedded `:::workflow` directive
  changing is a workflow change, and one disappearing *retires* that workflow
  under its unload policy — so an innocuous prose edit to a templated
  instruction can retire a workflow. Keep workflow definitions in
  `spec.workflows[]`, not embedded in templated prose, if that matters.
- **SSRF guards on every outbound webhook** — push notifications and HITL
  channels resolve to public addresses and pin the connection against DNS
  rebinding (relax only with the dev-only `gateway.allowPrivateWebhooks`).

## Deployment postures

The same chart serves all of them (ARCHITECTURE §3.7):

- **Self-hosted enterprise** — one org, your own IdP, no tenant isolation
  overhead needed (still get it for free).
- **Our managed multi-tenant service** — many orgs, hostile-tenant assumptions,
  every box above checked.
- **Customer-embedded** — your product ships agentctl as its agent backbone;
  your users never know the substrate.
- **Cross-cluster federation** (RFC 0035) — gateway↔gateway mTLS; hooks and
  supervisors terminate at the hub.

Start from `values.yaml`, turn on the planes your product needs, and treat the
four boundaries above as your integration contract.
