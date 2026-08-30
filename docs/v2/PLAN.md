# agentctl v2 — execution plan and progress register

> **Living document.** Update the `Status` column as work lands; never delete
> rows — flip them to `Done`/`Dropped` with a date and a pointer (PR/commit).
> Statuses: `Todo` · `In progress` · `Blocked(<on>)` · `Done(<date>)` ·
> `Deferred` · `Dropped`. Owner column left blank until assigned.
> Companion: [`ARCHITECTURE.md`](ARCHITECTURE.md) (the design) · RFCs 0026–0035.

## Phase overview

| Phase | Theme | Gate (exit criterion) |
|---|---|---|
| **P0** | Re-integration: speak agentd v1.3.x at all | An `Agent` CR runs the real agentd v1.3.1 image end-to-end in e2e (provision → converse → drain), on the rendered-directory model |
| **P1** | Identity core + tenancy + gateway routing | `agentctl login` (device flow) → chat with a namespaced agent through the gateway as a real per-user principal |
| **P2** | Provisioning v2: CRDs, registry, projection, admission | v1alpha2 set served + converted; registry scopes render + validate via the binary; floors enforced |
| **P3** | Durability plane | `store.class: managed` on the state service passes the SIGKILL/restore matrix; backup/restore/migrate verbs work |
| **P4** | Supervisors + control MCP | First-login supervisor; "create an agent" via chat lands a working Agent, OBO-authorized |
| **P5** | Capability plane: tenant mcpg, OBO egress, sandbox, HITL | Governed tool call with per-user token injection; sandbox run; gate answered from Slack |
| **P6** | Fleets & scaling v2 | All three partition strategies green in e2e incl. guarded resize + scale-from-zero |
| **P7** | Productization: exposure, observability, managed-service profile | Webhook exposure GA; dashboards; usage export; docs site |
| **PM** | Multi-cluster (RFC 0035 leaves Draft) | Design review after P6; implementation phased separately |

Dependency spine: P0 → P1 → P2 → {P3, P4} → P5 → P6 → P7 → PM.
(P3 and P4 can proceed in parallel once P2's renderer exists; P4 needs P1's
identity + a minimal P3 state path for the supervisor's own store.)

## P0 — Re-integration (contract truth first)

| ID | Item | Definition of done | Status |
|---|---|---|---|
| P0-1 | **ACC v2 contract re-baseline** | `contract/` regenerated per RFC 0026 §3 (schemas, A2A profile, exit-intent table, metrics 1.2, env, store profile); fixtures captured from agentd v1.3.1 | **Done(2026-08-30)** — commit `0254d13`; +restart-only partition, +store checkpointer profile, +known-gap annotations (U3/U4) baked into manifest schema |
| P0-2 | **agent-contract-client rebuild** | negotiates via config-schema `x-agentd-contract-version` + probes; no `surfaces{}` dependency; version-skew tests | **Done(2026-08-30)** — commit `0254d13`; four-clock negotiation, vendored exit/metrics/restart-only tables compiled in, permissive Manifest reader, pre-rewrite agents identified as unmanageable |
| P0-3 | **Operator: rendered-directory launch path** | flags-era rendering deleted; `-c services.yaml -c agentd.yml` + folders; explicit `run_until`; probes/drain/podFailurePolicy per intent table | **Done(2026-08-30)** — commit `37af063`; new `agent-config` shared builder (proven via real `--validate-config`), single-document layer for now (the services.yaml catalog layer + conventional folders land with the registry, P2-3); shard fleets keep the guarded resize via the `agentctl.dev/shards` annotation |
| P0-4 | **Admission: binary validation step** | `agentd --validate-config` sandbox job wired; effective surfaces recorded in status | **Done(2026-08-30)** (rung) — in-webhook spawn_blocking run over the SAME composed document (shared builder), secret-ref placeholders auto-derived, binary's config.invalid diagnosis travels into the deny message; chart `admission.binaryValidation` (staging image needed until U9). Effective-surfaces recording moves to P2-3 (needs the services catalog) |
| P0-5 | **e2e vs real agentd** | mock-agent retired where possible; `mock:` intelligence used; provision→converse→drain→delete green in kind | **Done(2026-08-30)** — 20 scenarios GREEN against real agentd 1.3.1 in kind (provisioning 2, management 5 incl. drain/pause/cancel over the A2A root, shard 1 w/ store-fence identity, a2a 3 through the gateway, conformance 2, claim 4 incl. KEDA 0→N); 4 honest skips (sec-oidc/trusted-proxy: pre-existing unarmed-gate gap superseded by P1 authn scenarios; sec-netpol: Calico lane; sec-aauth: pending apd build). Live-run fixes folded back: explicit store for every shape (defaulted store hit the read-only rootfs, exit 6), TLS mock provider w/ HTTP/1.1 framing, capability probes load the pod's config, apiserver+gateway forward to the A2A root (the /mcp path is dead), gateway accepts the native PascalCase dialect, metrics registry reconciled against the LIVE exposition (+11 series, summary-suffix matching), KEDA chart-pin, ServiceMonitor capability guard, coordination attestation off in the direct-driving base lane. NOTE: the images-quick lane runs a host-built gnu binary on distroless/cc — the canonical musl/scratch image lane (images.sh) is CI's |
| P0-6 | **Upstream asks filed** (U1–U7 below) | issues filed with repro + proposal; tracked links here | **Done(2026-08-30)** — delivered to the upstream owners' working sessions with repro + proposals: U1–U5 as a formal batch to agentd (2026-08-30); U6 conveyed to mcpg (store façade alignment + sandbox 0022 status obtained); U7 carried by the standing RFC 0023–0025 asks; U8/U9 already resolved. Statuses tracked per-row below |

## P1 — Identity core, tenancy, gateway routing

| ID | Item | Definition of done | Status |
|---|---|---|---|
| P1-1 | `agentctl-identity` skeleton (axum, PG schema, KMS envelope) | deploys via chart; threat-model doc merged (RFC 0028 §8) | **Core done(2026-08-30)** — crate `agentctl-identity` (BUSL): env config w/ https-issuer enforcement, AES-256-GCM sealer (AAD context binding), Store trait w/ MemoryStore + PgStore (device_sessions + principals migrations, bearer-hash-only at rest), HTTP surface `/healthz` `/v1/providers` `/v1/device/*` `/v1/introspect` `/v1/principals/*` (constant-time admin gate, fail-closed with no token); chart `identity.service.*` (default-off, providers→IDENTITY_PROVIDERS JSON, store=postgres reuses the DATABASE_URL helper + pg CA pin, apiToken→IDENTITY_ADMIN_TOKEN), deploy/identity Dockerfile + release matrix + images-quick lane. Remaining for full DoD: threat-model doc (with P1-4/P1-5 wiring). **Deployed live 2026-08-30**: chart-rendered identity w/ store=postgres (migrations ran on bundled PG), /healthz ok, /v1/providers lists, /v1/introspect 403 fail-closed without the admin token |
| P1-2 | OIDC federation (Auth0/Keycloak/Okta/generic) + device flow | `agentctl login/whoami` against Keycloak (dev) and one SaaS IdP | **Core done(2026-08-30)** — Federation: issuer-pinned discovery (host-poisoning refused), JWKS cache w/ refresh-on-unknown-kid, aud/exp enforcement, provider-prefixed subjects + groups claim, RFC 8628 device start/poll; proven by an in-process mock-IdP integration suite (device e2e, wrong-aud refused, poisoned discovery refused). CLI `agentctl login` (device flow vs the service wire, opaque handle — device_code never leaves the service; session at `~/.config/agentctl/credentials.json` 0600, no refresh token client-side) + `agentctl whoami`. Remaining for full DoD: live Keycloak + one SaaS IdP run (needs a deployed stack — with P1-5 e2e) |
| P1-3 | `Organization` CRD + claim mapping + namespace/quota reconcile | org create → namespace, roles resolve from IdP groups | **Done(2026-08-30)** — cluster-scoped Organization at agentctl.dev/v1alpha2 (new type starts on the target version); pure access engine `agent_api::org::access` (group globs, claim equality/containment, union grants, role ordering, all-labels selector scope, empty-matcher-grants-nothing; the RFC worked example is a test); operator tenancy controller (managed namespaces labeled+owner-referenced → org delete GC-cascades; per-ns ResourceQuota `count/agents.agentctl.dev`, removed when the ceiling is dropped; CRD-presence gate keeps upgraded clusters healthy); **live e2e `org-tenancy` PASS** (ns+label+ownerRef, quota refuses agent #6 with 403 exceeded-quota, status Ready, ns GC on delete). Enforcement wiring is P1-8 |
| P1-4 | Per-(user,agent) principal minting + operator projection | principals hot-projected; addressed gate answerable only by named user in e2e | **Done(2026-08-30)** — `spec.access.principals[]` (subjects) → operator mints via identity `/v1/principals/mint` (admin-gated; Validated=False if identity unarmed), projects into the owner-referenced `<name>-principals` Secret (existing keys never re-minted; removed subjects pruned; Secret landed BEFORE the config references it — a dangling bearer_ref is startup exit 2 that `--validate-config` cannot catch), mounts it 0444, and binds `a2a.principals[]`: per-subject `{{secret-file:…}}` user rules FIRST then the control-plane `operator` san-rule LAST (first-match-wins + the gateway presents the CP client cert on every forward; any non-empty list disables agentd's implicit operator fallback). **Live e2e `a2a-principals-gate` PASS**: minted bearer converses as `user`, unmatched-cert anonymous + wrong bearer refused, CP cert still operator. CAVEAT vs the DoD word "hot": principals are NOT reload-free in agentd 1.3.1 (Resolver::build resolves bearer_refs once at startup; verified with the agentd session — our restart-only contract file corrected) → new user = rolling restart until upstream rebuilds the resolver on reload (+U1). Gateway per-session bearer injection lands in P1-5 |
| P1-5 | Gateway v2 routing core (`orgs/<org>/agents/<name>`, cards, SSE) | chat + streaming through gateway with injected principal; loopback-operator closed on all agents | **Done(2026-08-30)** — org route family (`POST /orgs/<org>/agents|fleets/<name>` + nested `.well-known/agent-card.json`): Organization CR → managed-namespace resolution (404 on unknown org, never guessed); inbound authn REQUIRED via identity `/v1/introspect` over a dedicated admin env pair (`AGENTCTL_IDENTITY_TOKEN` — deliberately not `AGENTCTL_API_TOKEN`, which arms the coarse gate; unarmed identity ⇒ 503, never open); the caller's per-(user,agent) bearer is fetched from the projected `<name>-principals` Secret and injected upstream in `forward_request` (declared-but-unlisted subject ⇒ 403; declared-but-not-yet-projected ⇒ 503 retryable, never silent operator promotion); caller identity forwarded as X-Auth-*. Loopback-operator CLOSED ON ALL AGENTS: every A2A-serving config now carries a principals list (at minimum the control-plane operator san-rule), disabling agentd's implicit loopback/management fallback fleet-wide. identity service gained `IDENTITY_ISSUER_CA` (private-CA IdPs — chart `identity.service.issuerCa` Secret/ConfigMap mount). **Live e2e `org-route-user` PASS**: signed mock-IdP token → introspection (JWKS fetched in-cluster over the chart CA) → principal injection → answered as `user:mock:alice`; streaming SSE through the org route; 401 no-token, 403 unlisted subject, 404 unknown org; tenant-scoped card. Deferred per RFC/PLAN split: handle resolution (P2-7), accessPolicies enforcement + origins/netpol legs (P1-8), SSE resume-cursor + watched routing cache + gateway task-store retirement (RFC 0029 §7, later wave) |
| P1-6 | AAuth provider role (federated enrollment via projected SA token, JWKS) | agentd enrolls secret-free at boot; token refresh observed across rotation | **Done(2026-08-30)** — agentctl-identity IS the fleet's Agent Provider, speaking the SHIPPED 1.3.1 client wire (verified in agentd source): `POST /enroll` + `POST /agent-token` (RFC 9421 `hwk` signatures — inline Ed25519 JWK, covered `@method @authority @path content-digest signature-key`, Content-Digest checked, ±300s freshness), public `/.well-known/aauth-agent.json` + `/aauth-jwks.json` for resource servers, and the operator's four `/admin/*` endpoints (allowed-keys register/delete, agents list, revoke) behind the shared admin bearer. Enrollment: allowlist-by-thumbprint (secret-free for the agent — it only signs with its own key), idempotent by jkt (deterministic `aauth:<local>@<domain>` ids); agent tokens are EdDSA `aa-agent+jwt` with PoP `cnf.jwk` binding; provider key from `IDENTITY_AAUTH_SEED` (chart `identity.service.aauth`). **sec-aauth UNSKIPPED and PASS live (43.8s)**: real agentd 1.3.1 provisioned→enrolled→token→3 signed MCP calls verified by the resource server against identity's JWKS; operator learned status.identity.aauth via /admin/agents. Doc corrections vs RFC folded: 1.3.1 enrolls LAZILY on first signed dial (not boot; no exit 4 — provider outage degrades to unsigned+401); the federated `enrollment_assertion` leg is accepted on the wire but deferred until the renderer projects SA tokens (upstream delta; the legacy apd fixture + image gate are deleted) |
| P1-7 | CLI verbs: `login/chat/get/describe/pause/resume/drain` | against P1 stack | **Done(2026-08-30)** — `login`/`whoami` (P1-2), `get`/`describe` (v1), plus: `chat <org>/<agent>` (gateway org route as the signed-in user — session bearer, reply-text extraction with JSON fallback, minimal stdin REPL; 401→"login again", 403→named-principal message) and `drain|lame-duck|pause|resume|cancel [--fleet]` (POSTs to the aggregated management.agentctl.dev API — rides kubeconfig + K8s RBAC like kubectl; never dials pods). Smoked LIVE against the P1 stack: `whoami` renders the session, `drain` returned the agent's own `{"state":"draining"}`, `chat` round-tripped through introspection + principal injection to a real agentd answer. Remaining niceties deferred: streaming chat output, `agentctl get orgs` |
| P1-8 | **Access policies** (IdP claims/groups/scopes → roles over label selectors) resolved by identity, enforced at gateway + apiserver + RBAC mirror | "eng operates `team: engineering`, marketing refused, admin sees all" — red/green e2e per enforcement point | **Done(2026-08-30)** — GATEWAY: org routes resolve the caller's grants from Organization.accessPolicies (pure engine `agent_api::org::access`, groups matching; claims leg engine-ready) and enforce role-over-labels per method BEFORE any principal work (SendMessage/cancel = operator; task reads/subscribes = viewer; unknown methods fail toward the stronger role; an org with NO policies imposes no scoping — policies restrict once stated). RBAC MIRROR: the operator projects the role ladder per managed namespace (viewer = read; operator = read + the aggregated management verbs as `agents/<verb>` create; admin = `*` on both groups) with RoleBindings from EXACT group names (globs cannot be K8s Group subjects; label-selector scope widens to the namespace in the mirror — precise scoping is the gateway's job; both limits documented); operator ClusterRole gains roles/rolebindings + escalate/bind. APISERVER leg = the mirror (management verbs already ride K8s RBAC through the aggregated API). **Live e2e `org-access-policy` PASS (18.9s)**: eng operates eng-bot 200, refused 403 on mkt-bot (converse AND view), marketing views its team 200 but cannot converse 403, admin operates both 200; the mirror's admin binding carries the exact group. P1 IS COMPLETE |

## P2 — Provisioning v2

| ID | Item | Definition of done | Status |
|---|---|---|---|
| P2-1 | v1alpha2 CRDs (Agent, AgentFleet, AgentTemplate, AgentClass, MCPService, Organization, Supervisor) + conversion webhook | schema tests; v1alpha1 objects convert with warnings | **Done(2026-08-30)** — `agent_api::v1alpha2` family: Agent (class/handle/runtime/shape + `triggers[]` typed union over ALL TEN agentd start kinds w/ exactly-one CEL, workflows/skills bundles, `services[]` grants, intelligence/store/identity/expose/peers/lifecycle/approval; v1-compat fields kept for lossless conversion), AgentFleet (v2 template), AgentTemplate (+params), AgentClass (defaults + narrow-only `floors` + supervisor profile), MCPService (tags-as-floors, allow/exclude, auth service|obo|passthrough, direct), Supervisor (RFC 0027 §2). Conversion: v1→v2 LOSSLESS (mode/subscribe/loop/schedule/workflow → shape+triggers+workflows; tested per mode + round-trip), v2→v1 lossy naming every dropped field; **v1alpha2 is the storage version**; deprecation warnings ride ADMISSION (ConversionReview has no warnings channel). crdgen merges multi-version CRDs (kube merge_crds) + injects the conversion-webhook stanza (admission `/convert`, cert-manager inject-ca-from; Helm crds/ untemplated ⇒ default-ns assumption documented). **Live**: all 8 CRDs applied; v1 write→v2 storage→read-back at BOTH versions verified; deprecation warning surfaced on a v1 write; **full catalogue 25 PASS / 0 fail with every operation through the conversion webhook**. Traps fixed en route: YAML-1.1 `on:`-is-boolean (event trigger field renamed `name`; RFC 0033 updated), CEL-reserved `loop` (`__loop__`), CEL rule-cost budget (sum-of-conditionals + triggers maxItems). Operator/gateway still speak v1alpha1 through conversion — their v2-native flip rides P2-3 |
| P2-2 | Registry resolution engine (scope chain, narrowing floors) | unit-tested resolver; widening attempts rejected naming the floor | Todo |
| P2-3 | Projection compiler (services.yaml/agentd.yml/folders, vars, secrets refs) | renderer output validates via binary across the example matrix; hashes in status | Todo |
| P2-4 | Policy ladder (trifecta/egress/budget floors; tag-laundering guard) | admission e2e red/green cases | Todo |
| P2-5 | Reload-vs-restart classifier + rolling restart path | config edit rolls with zero run loss (pins verified) | Todo |
| P2-6 | Chart v2 umbrella (identity, mcpg-operator dep, profiles, values schema) | `helm install` dev profile → P1+P2 stack up | Todo |
| P2-7 | Agent handles + display names (`spec.handle`, org-unique at admission; gateway route + card resolution) | duplicate handle refused; `orgs/<org>/agents/<handle>` routes | **Done(2026-08-30)** — `spec.handle` + `spec.displayName` on Agent AND AgentFleet (additive on v1alpha1; carries into P2-1); `effective_handle` = handle-or-name, DNS-1123 syntax (`agent_api::{effective_handle,valid_handle}`); admission rung lists Agents+Fleets in the namespace and refuses a second holder naming the current one (fail-open only on transient list errors — CEL cannot express cross-object uniqueness); gateway org routes + cards resolve the path segment BY EFFECTIVE HANDLE (post-authn for RPC so anonymous probes cannot enumerate; 404 says "handle"). **Live in `org-route-user`**: routed + card via handle `helper` ≠ CR name, duplicate-handle create refused with "already held" |
| P2-8 | **Trigger sugar** — `spec.triggers[]` over all 10 agentd start kinds → generated start-node workflows; shape inference (CronJob-vs-internal-schedule policy, explicit `run_until`); prerequisite wiring (webhooks.listen+HMAC, MCPService grants, stream decls); CLI `create agent --schedule/--webhook/--subscribe/…` | one-liner e2e per start kind (10-row matrix): each provisions, fires, and lands the right workload shape | Todo |

## P3 — Durability plane

| ID | Item | Definition of done | Status |
|---|---|---|---|
| P3-1 | `state.*` service (checkpointer profile on PG via mcpg) | agentd `store.kind: mcp` passes upstream-style SIGKILL/restore matrix; p99 ≤ 50 ms in-cluster | Todo — mcpg team briefed (2026-08-30): pure SQL bindings on backend-sql first (their steer; store-class plugin only for semantics SQL can't express — seq-CAS needs none), optimistic version-column CAS maps 1:1 onto agentd's seq-CAS, principal injection via host-resolved ${identity.subject_id}; their gateway proxies ~24–27k QPS so the 50ms p99 is loose; gateway pin from the platform-blessed set (custom static plugins ⇒ self-host cell posture) |
| P3-2 | Server-side tenant fencing + `state.admin.snapshot/restore` | cross-agent key access provably impossible (test); snapshot/restore round-trip | Todo |
| P3-3 | `artifacts.*` façade over content store (S3) | put/get/list with org quotas | Todo |
| P3-4 | Store classes in Agent CRD (`ephemeral/local/managed`) + StatefulSet path | all three render + run | Todo |
| P3-5 | Lifecycle verbs: backup/restore/migrate/stop/start/reset | `agentctl migrate` between nodes with zero run loss (managed) | Todo |
| P3-6 | State capacity benchmark (checkpoint QPS vs fleet size) | numbers in docs/benchmarks.md | Todo |

## P4 — Supervisors + control MCP

| ID | Item | Definition of done | Status |
|---|---|---|---|
| P4-1 | System mcpg deployment + `control.*` bindings over apiserver | tool schemas reviewed; audited | Todo |
| P4-2 | OBO chain for control calls (identity exchange, binding checks) | supervisor of a viewer cannot create (e2e); every call attributed | Todo |
| P4-3 | `Supervisor` CRD + auto-ensure on first login + class profile | login → chat → estate listing | Todo |
| P4-4 | Instruction layering (system→org→user; user layer inert to directives) | fence-injection test red | Todo |
| P4-5 | Approval gates on destructive verbs (addressed to owner) | delete asks its human; operator override recorded | Todo |
| P4-6 | `control.subagents.create` narrowed grant (child Agents, TTL, budgets) | agent spawns governed child; caps enforced | Todo |
| P4-7 | **@mention orchestration** (class workflow: resolve → batch fan-out `a2a.delegate` typed `ask` w/ `args.hops` ceiling → gather → synthesize loop; `concurrency scope: key` per user) | "@a and @b" answered with both accounted for; forbidden handle reported; hop ceiling kills supervisor↔agent loops (own counter — agentd depth does not cross instances) | Todo |

## P5 — Capability plane

| ID | Item | Definition of done | Status |
|---|---|---|---|
| P5-1 | Tenant mcpg per org (operator-provisioned via mcpg CRs) | org create brings its gateway; catalog rendered from MCPService entries | Todo |
| P5-2 | AAuth verification chain at mcpg (identity JWKS) | unsigned/foreign-signed calls refused | Todo |
| P5-3 | Our Apache CredentialIssuer plugin → identity `/exchange` | per-user token injected upstream; cache + refresh observed over simulated days | Todo |
| P5-4 | Connections flow (consent, `connection_required` → HITL card) | user connects a provider once; agent proceeds | Todo |
| P5-5 | `sandbox.*` backend plugin (K8s Jobs + warm pool, RuntimeClass optional) | code run with caps, no network, artifacts I/O; threat-model doc | Todo — baseline: SELF-HOST CELL with our static build (mcpg governance corollary); upgrade to config-against-blessed mcpg plugin-protocol-0022 if upstream ships pre-P5; shape the interface against their draft's provider abstraction now |
| P5-6 | HITL fabric (mcpg approvals + agentd gate bridge + channel registry) | gate raised in agentd answered from Slack under the right identity | Todo |
| P5-7 | `work.*` fabric tools (coordination re-founding) | lease/ack/nack/DLQ + replay e2e | Todo |

## P6 — Fleets & scaling v2

| ID | Item | Definition of done | Status |
|---|---|---|---|
| P6-1 | AgentFleet v1alpha2 + static strategy (vars overlays, singletons) | 3-replica fleet, nightly singleton fires once | Todo |
| P6-2 | Guarded re-partition (drain-first resize) | resize under load loses nothing | Todo |
| P6-3 | Dispatcher strategy (owner + workers + fleet route) | delegation fan-out e2e | Todo |
| P6-4 | Workqueue strategy over `work.*` | crash mid-lease redelivers exactly the leased item | Todo |
| P6-5 | Scaler v2 (inbox_pending/pressure/queue-depth; breaker damping; scale-from-zero) | webhook agent 0→1 on first delivery | Todo |
| P6-6 | Per-fleet budget metering + policies | fleet window breach pauses intake | Todo |

## P7 — Productization

| ID | Item | Definition of done | Status |
|---|---|---|---|
| P7-1 | Webhook exposure GA (`hooks.<domain>`, `agentctl expose`, Gateway-API profile) | external delivery e2e with HMAC + tenant limits | Todo |
| P7-2 | Dashboards + alert rules (fleet health, budgets, gates, exchange latency) | shipped in chart | Todo |
| P7-3 | Audit pipeline end-to-end (gateway/apiserver/identity/mcpg/agentd streams) | one queryable trail for a full OBO tool call | Todo |
| P7-4 | **Billing-ready metering** — versioned event vocabulary (agent-hours, tokens by tier, tool calls by MCPService, OBO exchanges, sandbox CPU-s, state/artifact bytes, webhook/A2A counts, gates, seats, feature enablement), all attributed {org, group, user, agent} | durable events + Prometheus counters + apiserver aggregation/export (CSV/JSON by period); a sample invoice computable from export alone | Todo |
| P7-5 | Managed-service profile docs + API hardening pass (req. 27) | "build on agentctl" guide | Todo |
| P7-6 | Supervisor scale-to-zero (gateway park/wake) | dormant supervisors cost ~0 | Todo |
| P7-7 | OCI bundles for registry sets; docs site refresh; v2 GA checklist | — | Todo |

## PM — Multi-cluster (held at Draft)

| ID | Item | Status |
|---|---|---|
| PM-1 | RFC 0035 design review post-P6 (single-cluster seams re-checked) | Deferred |
| PM-2 | Hub/spoke sync agent + `Cluster` CR | Deferred |
| PM-3 | Gateway federation + cross-cluster migrate | Deferred |
| PM-4 | Supervisor federation (front supervisor ↔ remote/per-cloud supervisors as A2A peers; same `ask` command + `args.hops` ceiling) | Deferred |

## Upstream asks (file in P0-6; re-check each agentd/mcpg release)

| ID | Upstream | Ask | Status |
|---|---|---|---|
| U1 | agentd | A2A admin `reload` verb, or verified `--watch-config` on kubelet ConfigMap symlink swaps | Delivered 2026-08-30 (batch, priority #1) |
| U2 | agentd | A2A TLS hot-rotation (webhook listener already rotates) | Delivered 2026-08-30 (batch) |
| U3 | agentd | `--capabilities`: restore contract/surfaces info; include webhooks + stream/webhook start kinds; tier catalogue | Delivered 2026-08-30 (batch) |
| U4 | agentd | `run_until: auto` webhook/stream misclassification; unify the three long-lived lists | Delivered 2026-08-30 (batch) |
| U5 | agentd | published config schema catch-up ($defs Service.kind/methods/breaker, A2aPeer.service); image license label (says Apache-2.0, is AGPL) | Delivered 2026-08-30 (batch) |
| U6 | mcpg | store-profile MCP tool façade upstreamable; sandbox backend (their RFC 0022) offered back | Todo |
| U7 | aauth/agentd | delegation chains; agentd inbound verification (A2A leg) — carries RFCs 0023–0025 asks forward | Todo |
| U9 | agentd | a stock-image way to stage the binary for out-of-image use | **Done(2026-08-30) — declined upstream** (the binary stays single-purpose; packaging is not its problem). Resolution: chart supports BOTH `binaryValidation.mode: staging` (busybox-derived image) and `mode: imageVolume` (KEP-4639 image-typed volume, beta ~K8s 1.33 + containerd 2.0 — mounts the STOCK signed image, nothing derived). Bonus finding folded into the contract: --validate-config enforces secret resolution ONLY in header maps (intelligence.token passes unresolved — upstream defect raised); admission gained a referenced-Secret existence rung in response |
| U8 | agentd | interface composer `@` completion inserted bare `@name`, which never matched the `@skill:` default prefix (skill never preloaded) and collided with conversational agent handles | **Done(2026-08-30)** — fixed upstream on main (`bc203942`, unreleased): composer inserts the full `@skill:<name>`; `docs/interface.md` corrected (it had documented the broken bare form as working). Bare `@name` is formally free for agent handles. **Caveat**: the prefix is hardcoded in the composer (the daemon never sends `skills.reference_prefix` to clients) — agentctl must not project a custom prefix (renderer rule, RFC 0032 §4.6) |

## Deferred register (decisions consciously not taken now)

| Item | Where noted | Revisit |
|---|---|---|
| Eval/testing service for agent changes (agentic SDLC beyond canary+conformance) | RFC 0026 §2 | post-GA |
| First-party web console (beyond thin agentd-ui) | RFC 0026 §2 | post-GA |
| LLM traffic through a gateway | ADR-0004 | with data |
| Gateway-side DLP/message policy | RFC 0029 §9 | P7+ |
| NATS-backed work fabric | RFC 0031 §8 | v2.1 |
| Per-org KMS keys (BYOK) | RFC 0028 §10 | managed profile |
| Virtual-cluster tenancy | ADR-0010 / RFC 0035 | PM |
| Public vendor-neutral ACC v2 spec | RFC 0026 §6 | post-GA |
| KEDA scaler backend | RFC 0034 §7 | v2.1 |
| Cross-org registry export/import | RFC 0032 §7 | v2.1 |

## Risk register (top 5, watched)

1. **Two pre-1.0 upstreams** (agentd resets, mcpg in-place ABI breaks) —
   mitigation: digest pinning, lockstep CI matrix, contract fixtures per version.
2. **State service on the hot path** (synchronous reactor I/O) — mitigation:
   p99 budget + benchmark gate (P3-6) before `managed` becomes default.
3. **Identity is a crown jewel** — mitigation: smallest surface, strictest
   netpol, envelope crypto, external review before P5 exit.
4. **OBO provider variance** (IdPs differ on 8693/ID-JAG) — mitigation:
   connection-based fallback is always available; capability flags per provider.
5. **Scope breadth** — mitigation: phase gates are demos, not documents; PM held
   in Draft until P6.
