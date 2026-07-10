# Use cases

agentctl turns fleets of conformant agents into declarative Kubernetes resources. These worked examples show how a solo engineer, a startup, a small business, and platform, data, and ops teams put it to work — each with the actual manifests to `kubectl apply`.

Every example runs the reference agent **agentd** (`ghcr.io/agentd-dev/agentd:1.0.0`), but agentctl depends on the [Agent Control Contract](../contract/README.md), not on any one binary — swap in any conformant agent image and the resources are unchanged. The manifests are complete but illustrative: the control plane must already be installed (see [the chart README](../charts/agentctl/README.md)), and provider endpoints, Secrets, and MCP server URLs are placeholders to replace with your own.

**The common thread across all of them:**

- **Secret-free with AAuth** — an agent given a portable AAuth identity signs its own provider and tool requests, so no credential is mounted on the pod; a key-authenticated provider or server means the operator mounts that one key onto the agent (there is no off-pod broker).
- **Scale-from-zero** — claim fleets idle at zero replicas and scale on the work backlog, so you pay only for work actually done.
- **Harness-tracked budgets** — a cumulative `spec.limits.lifetimeTokens` ceiling per instance lets the agent stop itself, so a runaway loop can't run up a surprise bill.
- **Contract, not a vendor** — you program against the contract and `kubectl`, not a proprietary agent framework.

## Pick your starting point

| Persona | Use case | Core building block |
| --- | --- | --- |
| Solo AI engineer / indie hacker | [Event-driven personal automation on a tiny cluster](#event-driven-personal-automation-on-a-tiny-cluster) | reactive `Agent` |
| Seed-stage SaaS startup with a 4-person engineering team and no dedicated ops | [Elastic support-ticket triage that costs $0 while you sleep](#elastic-support-ticket-triage-that-costs-0-while-you-sleep) | claim `AgentFleet` (scale-from-zero) |
| Software / platform AI engineer | [Automated pull-request review at scale with a claim AgentFleet](#automated-pull-request-review-at-scale-with-a-claim-agentfleet) | claim `AgentFleet` + inline `mcpServers` |
| Small digital agency ops lead | [Nightly SEO drafts + CRM enrichment on a hard budget, in three YAML files](#nightly-seo-drafts--crm-enrichment-on-a-hard-budget-in-three-yaml-files) | schedule-mode `Agent` (CronJob) |
| Data / ops engineering team | [Sharded enrichment: burst-backfill a record backlog, then own it in steady state](#sharded-enrichment-burst-backfill-a-record-backlog-then-own-it-in-steady-state) | claim + shard `AgentFleet` |
| Ops / RevOps automation team | [A least-privilege deal-desk pipeline: four stages chained over the work fabric, with per-stage token ceilings, scaling from zero](#a-least-privilege-deal-desk-pipeline-four-stages-chained-over-the-work-fabric-with-per-stage-token-ceilings-scaling-from-zero) | chained fleets on the work fabric |
| SaaS platform team | [Hosting untrusted per-tenant agents as a customer-facing SaaS feature](#hosting-untrusted-per-tenant-agents-as-a-customer-facing-saas-feature) | per-namespace isolation + per-tenant `ModelPool` |

---

## Event-driven personal automation on a tiny cluster

**Who:** Solo AI engineer / indie hacker

I run a one-node cluster in a closet and I want a personal assistant that wakes up when something lands — a webhook, a new inbox message, a dropped file — triages it, and takes one small action through a tool. I don't want a service idling and billing me 24/7, and I really don't want a provider API key or a tool token sitting on a pod I forget to lock down. It should be one small YAML I can read in one sitting, and it should cost basically nothing when nothing is happening.

**How it works**

1. Define one ModelPool pointing at your LLM provider. The agent dials that endpoint directly (the operator renders it into the pod as INTELLIGENCE). Give the agent a secret-free AAuth identity, or reference a provider-key Secret via credentialSecretRef — the operator then mounts it onto the pod as INTELLIGENCE_TOKEN. Cap spend with the harness-tracked spec.limits.lifetimeTokens so a runaway loop stops itself instead of billing your card.
2. Declare the one or two tools the assistant needs inline on the Agent's spec.mcpServers (e.g. an inbox/reader server with auth.mode: none and a notify/write server with auth.mode: staticToken). The agent dials each server directly; a staticToken bearer is mounted onto the pod (or use auth.mode: aauth to keep the pod secret-free).
3. Declare a reactive Agent (mode: reactive) that binds the pool via spec.model.pool and lists those tools inline under mcpServers, and lists its event sources in subscribe (MCP resource URIs like inbox://unread or hook://intake). The operator renders it to a single-replica Deployment that idles and wakes on those subscribed resources.
4. The reactive agent triages each event with its instruction, then acts through exactly one or two inline MCP tools it dials directly — no exec, no declared egress capability, no secrets list, so it never trips the lethal-trifecta admission gate.
5. Add a second on-demand Agent in mode: once for deeper research. The operator renders it to a Job; kubectl apply it (or delete + re-apply) when you want a one-shot run, and it terminates to zero when done.
6. mTLS on the Management surface, restricted-PSS, and a harness token ceiling; on the AAuth path the pod holds no provider or tool key at all. At rest the reactive Deployment is a single idle pod and the once Job is nothing at all — near-zero cost until an event actually arrives.

**Sample deployment**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: provider-credentials
  namespace: indie
type: Opaque
stringData:
  api-key: sk-your-provider-key-here
---
apiVersion: v1
kind: Secret
metadata:
  name: notify-token
  namespace: indie
type: Opaque
stringData:
  token: xoxb-your-notify-token-here
---
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata:
  name: home-pool
  namespace: indie
spec:
  provider: anthropic
  endpoint: https://api.anthropic.com
  credentialSecretRef:
    name: provider-credentials
    key: api-key
  models: ["claude-sonnet-4-5"]
  defaultModel: claude-sonnet-4-5
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata:
  name: triage
  namespace: indie
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Triage each incoming item. Classify urgency, draft a one-line action, and post a summary via the notify tool. Do nothing else."
  model:
    pool: home-pool
  mcpServers:                       # inline; the agent dials each directly
    - name: inbox
      endpoint: https://inbox-mcp.indie.svc.cluster.local./mcp
      auth: { mode: none }
    - name: notify
      endpoint: https://notify-mcp.indie.svc.cluster.local./mcp
      auth:
        mode: staticToken           # bearer mounted onto the agent pod
        tokenSecretRef: { name: notify-token, key: token }
  subscribe: ["inbox://unread", "hook://intake"]
  surfaces:
    metrics: true
    management: true
  limits:
    lifetimeTokens: 500000          # cumulative ceiling; the harness stops the agent here
    maxTokens: 20000                # per-reaction bound
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata:
  name: research
  namespace: indie
spec:
  mode: once
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Research the topic in /data/topic.txt and write a cited brief to /data/brief.md"
  model:
    pool: home-pool
  mcpServers:                       # inline; the agent dials each directly
    - name: inbox
      endpoint: https://inbox-mcp.indie.svc.cluster.local./mcp
      auth: { mode: none }
    - name: notify
      endpoint: https://notify-mcp.indie.svc.cluster.local./mcp
      auth:
        mode: staticToken
        tokenSecretRef: { name: notify-token, key: token }
  limits:
    maxTokens: 120000
```

**Why agentctl**

- Secret-free on the AAuth path: give each agent an AAuth identity and it signs its own provider and MCP calls, so nothing is mounted to leak. The mounted-key path shown here instead puts the provider key and notify token on the agent (the operator mounts them) — there is no off-pod broker, so choose AAuth when the pod is untrusted.
- Near-zero cost at rest: the reactive Agent is one idle Deployment pod that wakes only on subscribed events, and the once research Agent is a Job that runs and terminates — nothing idles billing you.
- Budget as a hard stop, not a dashboard: spec.limits.lifetimeTokens is a cumulative ceiling the harness enforces (a clean exit or drain on exhaustion), capping spend even if an instruction misbehaves.
- One small, readable YAML: two Secrets, a ModelPool, and two Agents with their tools inline — declarative, kubectl-native, diffable in git.
- Safe by default: no exec/egress/secrets declared means the lethal-trifecta admission gate is never tripped, and the agent can reach only the two MCP servers you inlined.

**Operating it.** Watch token spend (the agent's `agent_budget_tokens_remaining` gauge) and event throughput via Prometheus /metrics (surfaces.metrics: true on the reactive agent). Because the triage agent also exposes surfaces.management: true, you can pause/resume, cancel, or drain it through the aggregated Management API with kubectl to stop it acting without deleting config; re-apply the once research Agent whenever you want a fresh one-shot run, and delete the reactive Agent to remove the last idle pod entirely.

---

## Elastic support-ticket triage that costs $0 while you sleep

**Who:** Seed-stage SaaS startup with a 4-person engineering team and no dedicated ops

Support tickets pour in during business hours and go quiet overnight, but our human queue can't keep up with first-response SLAs. We want AI to classify, tag, draft a reply, and escalate the genuinely hard tickets — without standing up a 24/7 service, without pasting our OpenAI key and Zendesk token into yet another pod, and without paying for idle compute at 3am. We're two founders deep and can't babysit infrastructure.

**How it works**

1. Your ingest webhook drops each new ticket onto the coordination work fabric as a work item on queue://tickets (one work.submit per ticket), which becomes the backlog signal.
2. A claim-mode AgentFleet of agentd workers leases tickets with exactly-one-owner claims; each worker classifies, tags, drafts a reply, and either resolves or escalates, then reports terminal state with work.result/ack.
3. The KEDA external scaler reads the coordination backlog and scales the worker Deployment elastically — up during the daytime burst, back down to zero replicas overnight, so idle compute is $0.
4. Workers dial the OpenAI endpoint directly: give them an AAuth identity to stay secret-free, or mount the provider key via ModelPool.credentialSecretRef (the operator sets it on each pod as INTELLIGENCE_TOKEN). Each worker caps its own cumulative spend with spec.limits.lifetimeTokens and stops itself when exhausted — no runaway invoice.
5. Workers reach Zendesk and your CRM by dialing those inline mcpServers directly: Zendesk with auth.mode: staticToken (its bearer mounted onto the worker) and the CRM with auth.mode: none; use auth.mode: aauth for a server that verifies the agent's signature instead. No gateway sits in between.
6. A poison ticket that fails maxAttempts redeliveries is dead-lettered (surfaced at dlq://items) for a human to inspect instead of hot-looping and burning tokens.
7. Restricted-PSS pods plus default-deny NetworkPolicies mean a worker can egress only to DNS, the control plane, and public HTTPS (its providers + MCP servers) — nothing else, including the tenant next door.

**Sample deployment**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: openai-credentials
  namespace: support
type: Opaque
stringData:
  api-key: sk-REPLACE-with-your-openai-key
---
apiVersion: v1
kind: Secret
metadata:
  name: zendesk-token
  namespace: support
type: Opaque
stringData:
  token: REPLACE-with-your-zendesk-api-token
---
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata:
  name: support-llm
  namespace: support
spec:
  provider: openai
  endpoint: https://api.openai.com/v1
  credentialSecretRef:
    name: openai-credentials
    key: api-key
  models: ["gpt-4o-mini", "gpt-4o"]
  defaultModel: gpt-4o-mini
---
apiVersion: agentctl.dev/v1alpha1
kind: AgentFleet
metadata:
  name: ticket-triage
  namespace: support
spec:
  template:
    mode: reactive
    image: ghcr.io/agentd-dev/agentd:1.0.0
    instruction: "Classify the ticket, apply tags, draft a customer reply, and escalate anything you are not confident resolving."
    model:
      pool: support-llm
    mcpServers:                     # inline; each worker dials these directly
      - name: zendesk
        endpoint: https://mcp.zendesk.example.com
        auth:
          mode: staticToken         # bearer mounted onto the worker
          tokenSecretRef: { name: zendesk-token, key: token }
        tags: ["egress"]
      - name: crm
        endpoint: https://mcp.crm.internal
        auth: { mode: none }
        tags: ["egress"]
    subscribe: ["queue://tickets"]
    surfaces:
      metrics: true
      management: true   # exposes drain/pause/cancel verbs over mTLS (kubectl)
    limits:
      lifetimeTokens: 2000000   # per-worker cumulative ceiling; the harness stops it here
      maxTokens: 20000          # per-reaction bound
  scaling:
    mode: claim
    minReplicas: 0    # scale to zero overnight
    maxReplicas: 20
    target:
      metric: pending_events
      value: "5"      # ~5 backlogged tickets per worker
  work:
    source: "queue://tickets"
    maxAttempts: 3    # dead-letter poison tickets for a human
    claimTtl: "2m"
```

**Why agentctl**

- Secret-free on the AAuth path: give the workers an AAuth identity and neither your OpenAI key nor your Zendesk token is mounted anywhere — each worker signs its own calls. The mounted-key path here instead puts those keys on the worker (the operator mounts them); there is no off-pod broker, so prefer AAuth for untrusted work.
- $0 when idle: claim-mode minReplicas:0 lets KEDA scale the Deployment to zero replicas overnight and back up on the daytime backlog — you pay only for tickets actually worked.
- A ceiling per worker, one bill you control: each worker's spec.limits.lifetimeTokens is a cumulative cap the harness enforces (a clean drain on exhaustion), so an agent loop can't run up a surprise invoice.
- No framework lock-in: you depend on the Agent Control Contract 1.0, not a vendor — agentd is the reference agent and swappable for any conformant image.
- kubectl-native and ops-light: the whole triage system is four declarative resources a 4-person team can review, diff, and GitOps — no bespoke autoscaler or secret-plumbing to maintain.

**Operating it.** Tune throughput with scaling.target.value (tickets per worker) and maxReplicas; watch the per-agent Prometheus /metrics for backlog, token spend against each worker's lifetime ceiling, and dead-letter rate. With the management surface enabled you can kubectl-drain or pause the fleet during a provider incident or bad-prompt rollback, and requeue or drop dead-lettered tickets (dlq://items) on the work fabric. To lift the ceiling as volume grows, raise the template's spec.limits.lifetimeTokens and re-apply the fleet.

---

## Automated pull-request review at scale with a claim AgentFleet

**Who:** Software / platform AI engineer

I want every pull request across our org reviewed automatically: read the diff, run static checks and code search, and post findings — plus chew through a backlog of repos on demand. Traffic is spiky (nothing at 3am, hundreds of PRs after a merge storm), the reviewer needs a git/code-search tool but I refuse to bake a GitHub token or a model key into the pod, and I have to be able to roll the fleet cleanly during a deploy without dropping in-flight reviews.

**How it works**

1. A webhook receiver (your small producer, out of scope of agentctl) turns each PR event into a work item and calls `work.submit` on the coordination server (the `work.*` MCP fabric); a backlog job can submit one item per repo to review at scale.
2. The `pr-reviewers` AgentFleet runs `scaling.mode: claim` with `minReplicas: 0`, so with `coordination` + `scaler` enabled the KEDA external scaler reads the `queue://pr-review` backlog and scales the Deployment from zero; each worker leases exactly-one-owner claims off `work.source` with a `work.claimTtl` lease.
3. Each worker (mode `reactive`, subscribed to `queue://pr-review`) reviews the claimed diff, calls the git/code-search MCP server it declares inline under `mcpServers` — dialing it directly with the git token mounted onto the worker (`auth.mode: staticToken`) — then records findings via `work.ack`/`work.result`.
4. Model calls dial `review-pool`'s endpoint directly, authenticated by the worker's AAuth identity or a mounted provider key; per-worker `limits` cap a single review (`maxTokens`/`maxSteps`) and the worker's cumulative spend (`lifetimeTokens`).
5. A poison PR that fails `work.maxAttempts` times is dead-lettered (surfaced at `dlq://items`) instead of looping forever, so you can requeue or drop it.
6. During a deploy, `kubectl create --raw /apis/management.agentctl.dev/v1alpha1/namespaces/ci/agentfleets/pr-reviewers/drain -f /dev/null` fans the drain verb to every replica over mTLS: workers stop claiming new items and finish in-flight reviews, so you can `kubectl apply` the new fleet spec (or bump the image) without cutting a review mid-flight.
7. Because `surfaces.a2a` is on, the gateway projects and JWS-signs a fleet Agent Card, so other systems (a release bot, an IDE) can call the fleet with `message/send` and get delegated review work — the fleet is one addressable A2A endpoint.

**Sample deployment**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: model-credentials
  namespace: ci
type: Opaque
stringData:
  api-key: sk-REDACTED
---
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata:
  name: review-pool
  namespace: ci
spec:
  provider: anthropic
  endpoint: https://api.anthropic.com
  credentialSecretRef:
    name: model-credentials
    key: api-key
  models: ["claude-sonnet-4-5"]
  defaultModel: claude-sonnet-4-5
---
apiVersion: v1
kind: Secret
metadata:
  name: codetools-token
  namespace: ci
type: Opaque
stringData:
  token: ghp-REDACTED
---
apiVersion: agentctl.dev/v1alpha1
kind: AgentFleet
metadata:
  name: pr-reviewers
  namespace: ci
spec:
  template:
    mode: reactive
    image: ghcr.io/agentd-dev/agentd:1.0.0
    instruction: "Review the claimed PR diff; run code-search and static checks via the git-codesearch tool; post findings as the work result."
    model:
      pool: review-pool
    mcpServers:                     # inline; each worker dials the server directly
      - name: git-codesearch
        endpoint: https://mcp.internal.svc/git
        auth:
          mode: staticToken         # git token mounted onto the worker
          tokenSecretRef: { name: codetools-token, key: token }
          header: Authorization
        tags: ["egress"]
    subscribe: ["queue://pr-review"]
    surfaces:
      a2a: true
      management: true
      metrics: true
    limits:
      lifetimeTokens: 50000000  # per-worker cumulative ceiling (harness-enforced)
      maxTokens: 200000         # per-review bound
      maxSteps: 40
  scaling:
    mode: claim
    minReplicas: 0
    maxReplicas: 25
    target:
      metric: pending_events
      value: "4"
  work:
    source: "queue://pr-review"
    maxAttempts: 3
    claimTtl: "5m"
```

**Why agentctl**

- Secret-free on the AAuth path: give the reviewers an AAuth identity and neither the GitHub/code-search token nor a model key is mounted — each signs its own calls. The mounted-key path here holds the git token on the worker (`auth.mode: staticToken`); there is no off-pod broker, so prefer AAuth for a compromised-pod threat model.
- Scale-from-zero economics: claim mode idles at zero replicas and KEDA scales on the actual PR backlog, so a merge storm gets 25 workers and a quiet night costs nothing.
- Budgets as guardrails, not surprises: per-worker `limits` (`lifetimeTokens` cumulative, plus `maxTokens`/`maxSteps` per review) bound spend, harness-enforced with a clean exit/drain instead of an open-ended bill.
- Clean rolls with no dropped work: the Management `drain` verb finishes in-flight reviews before you redeploy, and dead-lettering keeps one poison PR from wedging the fleet.
- Contract, not a vendor: workers are `ghcr.io/agentd-dev/agentd:1.0.0`, but any agent honoring the Agent Control Contract drops in unchanged — and the fleet is itself a signed A2A endpoint other systems can call.

**Operating it.** Requires the opt-in `coordination` + `scaler` planes (and KEDA) for elastic claim scaling; tune `target.value` (backlog per replica) and `maxReplicas` to trade latency against provider rate limits, and watch the per-agent Prometheus series for token burn and claim throughput. Use `drain` before deploys and `pause`/`resume` to freeze the fleet during an incident; poison PRs surface on the dead-letter channel (`dlq://items`) for requeue or drop.

---

## Nightly SEO drafts + CRM enrichment on a hard budget, in three YAML files

**Who:** Small digital agency ops lead (comfortable with kubectl, not a Kubernetes expert)

We run content + CRM chores for a dozen clients. Every night I want SEO drafts written into our CMS, new leads summarized, and CRM records enriched — without paying for idle compute all day, and with a hard ceiling so a runaway loop can never blow the LLM budget. I don't want to babysit servers or scatter API keys across pods. I want a couple of YAML files I apply and forget.

**How it works**

1. Declare a ModelPool `content-llm` pointing at your provider. The agent dials that endpoint directly; reference a provider-key Secret via `credentialSecretRef` (the operator mounts it onto the pod as `INTELLIGENCE_TOKEN`), or give the agent an AAuth identity to keep the pod secret-free.
2. Declare the two tools inline on each Agent's `spec.mcpServers`: your CMS MCP server and your CRM MCP server, each with `auth.mode: staticToken` and a `tokenSecretRef` (the operator mounts each bearer onto the agent). The agent dials each server directly.
3. Declare schedule-mode Agents (one per job) with `mode: schedule`, `image: ghcr.io/agentd-dev/agentd:1.0.0`, an `instruction`, `model: { pool: content-llm }`, and the tools inline under `mcpServers`. The operator renders each to a Kubernetes CronJob.
4. Each night the CronJob fires a fresh Pod, the agent runs its instruction — dialing the CMS/CRM tools and the model directly — then exits. A CronJob runs no Pods between fires, so idle cost is nothing beyond the control plane.
5. Set a per-run cap with `limits.maxTokens` (and `maxSteps` to bound tool-call loops) and a cumulative `limits.lifetimeTokens` ceiling for the fire; the harness enforces both, so a misbehaving run self-limits.
6. When the lifetime ceiling is hit, the harness ends the run cleanly (`EXIT_BUDGET`) and the next night's fire starts a fresh instance; raise `limits.lifetimeTokens` to lift the ceiling. Watch the agent's `agent_budget_tokens_remaining` gauge on `/metrics`.

**Sample deployment**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: content-provider-key
  namespace: agency
type: Opaque
stringData:
  api-key: sk-your-provider-key
---
apiVersion: v1
kind: Secret
metadata:
  name: cms-crm-tokens
  namespace: agency
type: Opaque
stringData:
  cms-token: cms-mcp-token
  crm-token: crm-mcp-token
---
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata:
  name: content-llm
  namespace: agency
spec:
  provider: anthropic
  endpoint: https://api.anthropic.com
  credentialSecretRef:
    name: content-provider-key
    key: api-key
  models: ["claude-sonnet-4-5"]
  defaultModel: claude-sonnet-4-5
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata:
  name: seo-drafts
  namespace: agency
spec:
  mode: schedule
  schedule: { cron: "0 2 * * *", timezone: "Europe/Berlin" }
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "For each client topic queue in the CMS, draft an SEO article and save it as a draft post."
  model:
    pool: content-llm
  mcpServers:                       # inline; the agent dials each directly
    - name: cms
      endpoint: https://cms.internal.example.com/mcp
      auth:
        mode: staticToken
        tokenSecretRef: { name: cms-crm-tokens, key: cms-token }
    - name: crm
      endpoint: https://crm.internal.example.com/mcp
      auth:
        mode: staticToken
        tokenSecretRef: { name: cms-crm-tokens, key: crm-token }
  limits: { lifetimeTokens: 4000000, maxTokens: 2000000, maxSteps: 60 }
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata:
  name: crm-enrich
  namespace: agency
spec:
  mode: schedule
  schedule: { cron: "30 2 * * *", timezone: "Europe/Berlin" }
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Summarize leads created since yesterday and enrich each CRM record with the summary and firmographics."
  model:
    pool: content-llm
  mcpServers:                       # inline; the agent dials each directly
    - name: cms
      endpoint: https://cms.internal.example.com/mcp
      auth:
        mode: staticToken
        tokenSecretRef: { name: cms-crm-tokens, key: cms-token }
    - name: crm
      endpoint: https://crm.internal.example.com/mcp
      auth:
        mode: staticToken
        tokenSecretRef: { name: cms-crm-tokens, key: crm-token }
  limits: { lifetimeTokens: 4000000, maxTokens: 2000000, maxSteps: 60 }
```

**Why agentctl**

- Zero cost when idle: a schedule-mode Agent renders to a CronJob — nothing runs (and nothing bills) until the cron fires, then the Pod exits.
- A real spend ceiling, not a dashboard alert: `limits.lifetimeTokens` is enforced by the harness, which ends the run cleanly the instant the fire's ceiling is crossed — a runaway run cannot overspend.
- Secret-free on the AAuth path: give the agents an AAuth identity and no provider key or CMS/CRM token is mounted at all. The mounted-key path here holds those keys on the agent (the operator mounts them); there is no off-pod broker.
- Scoped tools: each agent reaches only the CMS + CRM servers it declares inline under mcpServers and dials directly — no ambient network access to anything else in-cluster.
- It's just declarative YAML applied with kubectl — no agent framework to write, no servers to run, and day-2 checks are `kubectl get agents` / `kubectl get mp`.

**Operating it.** Watch spend on each agent's Prometheus `/metrics` (the `agent_budget_tokens_remaining` gauge); raise the agents' `limits.lifetimeTokens` to lift the per-fire ceiling. There is no shared pool cap now — each nightly fire is its own instance with its own lifetime box, and per-agent `limits.maxTokens`/`maxSteps` bound any single run. `kubectl get mp content-llm` shows the pool's provider/endpoint and Ready status.

---

## Sharded enrichment: burst-backfill a record backlog, then own it in steady state

**Who:** Data / ops engineering team

We have a 20M-row backlog to classify and embed, and once it's caught up we need N data sources kept continuously monitored with deterministic, no-double-work ownership. We don't want to hand every worker a warehouse token or a provider API key, and we want poison records quarantined instead of retried forever. We already run Kubernetes and want to drive all of this with kubectl, not a bespoke queue-worker service.

**How it works**

1. Define one ModelPool (provider + endpoint + defaultModel). Each worker dials that endpoint directly — with an AAuth identity (secret-free) or the provider key mounted via credentialSecretRef (set on the pod as INTELLIGENCE_TOKEN). Cap each worker's cumulative spend with the template's spec.limits.lifetimeTokens.
2. Declare your datastore/warehouse MCP endpoints inline on each fleet's template.mcpServers, each with auth.mode: staticToken + tokenSecretRef (the operator mounts each bearer onto the worker). Each worker dials them directly — no gateway in between.
3. For the backfill, deploy a CLAIM AgentFleet: scaling.mode: claim, minReplicas: 0, maxReplicas: 40, target{metric,value}. The operator renders a Deployment with replicas omitted and a KEDA external scaler that scales the pool from zero off the work.source backlog; each worker leases exactly-one-owner claims from the coordination work fabric.
4. Bound the backfill's failure behavior with work.claimTtl (lease TTL so a crashed worker's item is redelivered) and work.maxAttempts (dead-letter a poison record after N redeliveries — it moves to the deadletter state, surfaced at dlq://items, instead of cycling forever). Each worker's template.limits.lifetimeTokens caps this run's per-worker spend.
5. For steady state, deploy a SHARD AgentFleet: scaling.mode: shard, shards: 8. The operator renders a StatefulSet of 8 keyed partitions (FNV-1a/64 modulus); each ordinal deterministically owns its slice of the work.source, so every source has exactly one live owner and no two pods touch the same record.
6. Both fleets share the same ModelPool by name and declare the same tools inline, and expose surfaces.metrics for Prometheus. When the backfill drains, KEDA parks it at minReplicas: 0 and the shard fleet carries ongoing monitoring; resize shards via a guarded stop-the-world rebalance.
7. Operate with kubectl: get agentfleet plus the aggregated Management API's drain / pause / resume verbs (mTLS, RBAC-gated) to quiesce a worker before node maintenance.

**Sample deployment**

```yaml
apiVersion: v1
kind: Secret
metadata: { name: provider-credentials, namespace: data }
type: Opaque
stringData: { api-key: sk-REPLACE }
---
apiVersion: v1
kind: Secret
metadata: { name: warehouse-token, namespace: data }
type: Opaque
stringData: { token: whse-REPLACE }
---
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata: { name: enrich-pool, namespace: data }
spec:
  provider: openai
  endpoint: https://api.openai.com/v1
  credentialSecretRef: { name: provider-credentials, key: api-key }
  models: ["gpt-4o-mini", "text-embedding-3-small"]
  defaultModel: gpt-4o-mini
---
# BACKFILL — elastic claim fleet, scales from zero on the backlog
apiVersion: agentctl.dev/v1alpha1
kind: AgentFleet
metadata: { name: enrich-backfill, namespace: data }
spec:
  template:
    mode: reactive
    image: ghcr.io/agentd-dev/agentd:1.0.0
    instruction: "Claim a record, classify + embed it, write results back, ack."
    model:
      pool: enrich-pool
    mcpServers:                     # inline; each worker dials the server directly
      - name: warehouse
        endpoint: https://warehouse-mcp.data.svc:8443
        auth:
          mode: staticToken         # bearer mounted onto the worker
          tokenSecretRef: { name: warehouse-token, key: token }
        tags: ["read", "write"]
    subscribe: ["queue://records-backfill"]
    surfaces: { metrics: true }
    limits: { lifetimeTokens: 10000000, maxTokens: 8000 }  # per-worker cumulative + per-run
  scaling:
    mode: claim
    minReplicas: 0
    maxReplicas: 40
    target: { metric: pending_events, value: "50" }
  work:
    source: "queue://records-backfill"
    maxAttempts: 5
    claimTtl: "1m"
---
# STEADY STATE — 8 keyed shards, deterministic one-owner monitoring
apiVersion: agentctl.dev/v1alpha1
kind: AgentFleet
metadata: { name: source-monitor, namespace: data }
spec:
  template:
    mode: reactive
    image: ghcr.io/agentd-dev/agentd:1.0.0
    instruction: "Own your partition of sources; enrich new records as they arrive."
    model:
      pool: enrich-pool
    mcpServers:                     # inline; each worker dials the server directly
      - name: warehouse
        endpoint: https://warehouse-mcp.data.svc:8443
        auth:
          mode: staticToken
          tokenSecretRef: { name: warehouse-token, key: token }
        tags: ["read", "write"]
    subscribe: ["queue://sources"]
    surfaces: { metrics: true }
  scaling:
    mode: shard
    shards: 8
  work:
    source: "queue://sources"
```

**Why agentctl**

- Secret-free on the AAuth path: give the workers an AAuth identity and neither the provider key nor the warehouse token lands on a pod — each signs its own calls. The mounted-key path here holds those keys on the worker (the operator mounts them); there is no off-pod broker.
- Scale-from-zero backfill: a claim fleet is a Deployment + KEDA external scaler that parks at zero and bursts to max on the real backlog, so you pay only while the queue is deep.
- Exactly-one-owner semantics two ways: claim leases (with TTL redelivery) for elastic backfill, FNV-1a keyed StatefulSet shards for deterministic steady-state ownership — no double-processing either way.
- Poison-record safety: work.maxAttempts dead-letters bad rows to dlq://items instead of retrying forever, and each worker's limits.lifetimeTokens caps its cumulative spend.
- kubectl-native operations: fleets are CRDs and drain/pause/resume are aggregated-API verbs, not a bespoke worker service you build and babysit.

**Operating it.** The claim fleet auto-parks at minReplicas: 0 when the backlog drains and re-bursts on new work; watch pending_events and each worker's token spend (the agent_budget_tokens_remaining gauge; a worker draining early signals its lifetime ceiling) on Prometheus /metrics. Resizing scaling.shards is a guarded stop-the-world rebalance, so pick N up front; before node maintenance, use the Management drain/pause verbs (kubectl, mTLS + RBAC-gated) to quiesce a shard cleanly rather than deleting the pod.

---

## A least-privilege deal-desk pipeline: four stages chained over the work fabric, with per-stage token ceilings, scaling from zero

**Who:** Ops / RevOps automation team

Our deal-desk process has four stages — intake, research, draft, compliance-review — and today it's a pile of brittle scripts and human hand-offs. We want each stage to be its own agent that holds ONLY the tools for its job, the stages composed into a durable pipeline, and a hard token ceiling on each stage so a runaway case can't blow the budget.

**How it works**

1. Model each stage as its own agent, its tools declared inline on spec.mcpServers, so the drafting agent literally cannot reach CRM or policy tools — least privilege is a binding, not a convention.
2. Chain the stages over the coordination work fabric: each stage subscribes to its input queue (queue://cases -> queue://research -> queue://drafting -> queue://compliance) and, when done, submits the next stage's work item via work.submit. The queue topology IS the process graph — no orchestrator script.
3. Run stage 1 (intake) as a CLAIM-mode AgentFleet with minReplicas 0: KEDA scales workers from ZERO on the case backlog and back to zero when idle; workers lease exactly-one-owner claims (work.claimTtl) and poison cases dead-letter to dlq://items after maxAttempts. Leave work.source UNSET so the scaler reads the coordination backlog (a queue:// work.source would break scaler activation).
4. Run stages 2-4 (research/draft/compliance) as long-lived reactive Agents, each declaring ONLY its own tools inline under mcpServers; the agent dials each directly. crm's staticToken is mounted onto that stage's pod (or use auth.mode: aauth to keep the pod secret-free).
5. Point every stage at the SAME ModelPool process-brain — one provider endpoint every stage dials directly. Token budgets are per-instance now: give each stage a spec.limits.lifetimeTokens ceiling so no single stage can run away, and the harness stops that stage on exhaustion.
6. Give the intake fleet's template its own limits.lifetimeTokens so each intake worker caps its cumulative spend even under a burst.
7. Every agent can be secret-free: give each an AAuth identity and it signs its own provider and MCP calls — no key on any pod, so the pipeline is safe for hostile multi-tenancy. The mounted-key path (crm's staticToken, or a provider credentialSecretRef) instead puts that one key on the stage's pod; there is no off-pod broker.

**Sample deployment**

```yaml
apiVersion: v1
kind: Secret
metadata: { name: provider-credentials, namespace: revops }
type: Opaque
stringData: { api-key: sk-REPLACE-ME }
---
apiVersion: v1
kind: Secret
metadata: { name: crm-token, namespace: revops }
type: Opaque
stringData: { token: REPLACE-ME }   # mounted onto the research stage's pod
---
# One pool every stage dials directly; each stage caps its own spend via limits.lifetimeTokens.
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata: { name: process-brain, namespace: revops }
spec:
  provider: openai
  endpoint: https://api.provider.example/v1
  credentialSecretRef: { name: provider-credentials, key: api-key }
  models: ["gpt-strong", "gpt-fast"]
  defaultModel: gpt-strong
---
# Stage 1 - intake: claim workers scale from ZERO on backlog, normalize each
# case, and submit it onward to queue://research via the work fabric.
apiVersion: agentctl.dev/v1alpha1
kind: AgentFleet
metadata: { name: case-intake, namespace: revops }
spec:
  work:
    maxAttempts: 5
    claimTtl: "1m"
  scaling:
    mode: claim
    minReplicas: 0
    maxReplicas: 8
    target: { metric: pending_events, value: "10" }
  # work.source intentionally UNSET: the KEDA scaler reads the coordination
  # backlog; a queue:// work.source would break scaler activation.
  template:
    mode: reactive
    image: ghcr.io/agentd-dev/agentd:1.0.0
    instruction: "Normalize each inbound case and submit it to queue://research."
    subscribe: ["queue://cases"]
    model:
      pool: process-brain
    surfaces: { a2a: true, metrics: true }
    limits: { lifetimeTokens: 1000000 }   # per-worker cumulative ceiling (intake tier)
---
# Stages 2-4 - specialists: each reactive, bound to ONLY its own tools,
# chained by the work fabric (research -> drafting -> compliance).
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata: { name: research-specialist, namespace: revops }
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Research the case with CRM tools; submit the result to queue://drafting."
  subscribe: ["queue://research"]
  model:
    pool: process-brain
  mcpServers:                       # inline; dialed directly (crm token mounted on this pod)
    - name: crm
      endpoint: https://crm.internal/mcp
      auth: { mode: staticToken, tokenSecretRef: { name: crm-token, key: token } }
  surfaces: { a2a: true, metrics: true }
  limits: { lifetimeTokens: 2000000 }   # this stage's ceiling
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata: { name: draft-specialist, namespace: revops }
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Draft the deal doc from the research; submit it to queue://compliance."
  subscribe: ["queue://drafting"]
  model:
    pool: process-brain
  mcpServers:                       # inline; dialed directly
    - { name: doc-store, endpoint: https://docs.internal/mcp, auth: { mode: none } }
  surfaces: { a2a: true, metrics: true }
  limits: { lifetimeTokens: 2000000 }   # this stage's ceiling
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata: { name: compliance-specialist, namespace: revops }
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Compliance-review the draft against policy; emit the terminal result."
  subscribe: ["queue://compliance"]
  model:
    pool: process-brain
  mcpServers:                       # inline; dialed directly
    - { name: policy-kb, endpoint: https://policy.internal/mcp, auth: { mode: none } }
  surfaces: { a2a: true, metrics: true }
  limits: { lifetimeTokens: 2000000 }   # this stage's ceiling
```

**Why agentctl**

- The process graph is the work-fabric queue topology, not a pile of scripts — each stage claims its input and submits the next, with exactly-one-owner claims, TTL redelivery, and dead-lettering (dlq://items) built in.
- Per-stage token ceilings, harness-enforced: each stage's spec.limits.lifetimeTokens caps its cumulative spend, so a runaway case exhausts one stage cleanly instead of blowing an open-ended bill. (There is no shared pool ceiling now — budgets live on each instance.)
- Least privilege by construction: each stage declares only its own tools inline under mcpServers and dials them directly, so the drafting agent literally cannot reach CRM or policy tools.
- Every agent can be secret-free — with an AAuth identity each stage signs its own calls and no provider or tool key lands on a pod — so the whole pipeline is safe for hostile multi-tenancy. (The mounted-key path holds that one key on the stage's pod; there is no off-pod broker.)
- The intake tier scales from zero on backlog (KEDA claim mode) while the specialist stages stay warm — the pipeline costs nothing when idle and absorbs bursts without re-plumbing.

**Operating it.** Let KEDA drive the intake tier from the case backlog (leave work.source unset so the scaler reads the coordination backlog; a queue:// work.source breaks scaler activation), or pin it with kubectl scale agentfleet/case-intake. Drain or pause any stage for a controlled cutover via the Management API (kubectl against the aggregated API, RBAC/SubjectAccessReview-gated); poison cases dead-letter to dlq://items after maxAttempts. Watch each agent's Prometheus /metrics (the agent_budget_tokens_remaining gauge per stage) to see each stage's spend against its own ceiling. If one stage must fan a single case across many workers, make that stage its own AgentFleet with a coordinator + distribution: a2a (which fans work to that fleet's OWN worker pool over A2A).

---

## Hosting untrusted per-tenant agents as a customer-facing SaaS feature

**Who:** SaaS platform team (agents as a customer-facing feature)

We ship agents as a product feature: every customer authors their own instruction and we run it for them. The instructions are effectively untrusted — a tenant will try to exfiltrate our provider keys, reach another tenant's tools, or burn the whole model budget. We need one agent per tenant, in the tenant's own namespace, hard-isolated, with no way to touch another tenant's models, tools, or credentials, and per-tenant cost attribution.

**How it works**

1. Give each customer their own namespace (e.g. tenant-acme). On every Agent/AgentFleet reconcile the operator ensures the four tenant NetworkPolicies (agent-default-deny in+out; agent-allow-controlplane-and-dns egress to DNS + the A2A gateway and coordination pods; agent-ingress-controlplane-only; agent-internet-egress = public-HTTPS only, every private/link-local/CGNAT range carved out) in the workload's own namespace, so a namespace created after install is still isolated — no cross-tenant pod-to-pod path exists.
2. Give each tenant agent its own AAuth identity so it signs its own provider and tool calls and holds NO key — the secret-free path, and the right default for untrusted tenants. (If a provider needs a key, put it in a per-tenant Secret and reference it from that tenant's ModelPool; the operator mounts it onto the tenant's pod as INTELLIGENCE_TOKEN — but then a hostile pod holds the key and could exfiltrate it over the public-HTTPS egress, so prefer AAuth here.)
3. Cap each tenant with the agent's spec.limits.lifetimeTokens; the harness stops that tenant's agent on exhaustion. model.pool is namespace-scoped and admission-validated, so a tenant cannot even select another tenant's pool; the coordination server still attests each tenant by source IP, so one tenant cannot ack or release another's work claim.
4. Declare each tenant's tools inline on its Agent's spec.mcpServers — auth.mode: aauth (secret-free) or auth.mode: staticToken pointing at the tenant's own tool-token Secret (mounted onto that tenant's pod). A tenant reaches only the servers it declares and cannot reach another tenant's.
5. Render the untrusted instruction as a plain Agent that declares NO exec/egress/secrets. The validating admission webhook denies the lethal trifecta (exec+egress+secrets together) unless annotated, and enforces the image-registry allow-list so a tenant can only run approved agent images (ghcr.io/agentd-dev/...).
6. Every pod self-confines to restricted PSS (nonroot, drop ALL caps, read-only rootfs, no auto-mounted SA token, RuntimeDefault seccomp). drop:[ALL] removes CAP_NET_RAW, so a tenant cannot spoof its source IP — the attested tenant identity holds even against a hostile pod.
7. Expose surfaces.management so the control plane reaches the agent over mTLS on 8443 (Management client cert = the sole Management origin); management verbs (pause/resume/drain/cancel) are RBAC + SubjectAccessReview gated, so your platform can pause or drain a misbehaving tenant via kubectl without the tenant being able to drive itself.

**Sample deployment**

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: tenant-acme
  labels:
    # namespace runs at baseline; each workload self-confines to restricted PSS
    pod-security.kubernetes.io/enforce: baseline
---
apiVersion: v1
kind: Secret
metadata:
  name: acme-provider-creds
  namespace: tenant-acme
type: Opaque
stringData:
  api-key: sk-acme-tenant-key      # mounted onto the tenant's agent as INTELLIGENCE_TOKEN
---
apiVersion: v1
kind: Secret
metadata:
  name: acme-crm-token
  namespace: tenant-acme
type: Opaque
stringData:
  token: crm-bearer-acme           # mounted onto the tenant's agent
---
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata:
  name: acme-pool
  namespace: tenant-acme
spec:
  provider: openai
  endpoint: https://api.openai.com/v1
  credentialSecretRef:
    name: acme-provider-creds
    key: api-key
  models: ["gpt-4o-mini"]
  defaultModel: gpt-4o-mini
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata:
  name: acme-assistant
  namespace: tenant-acme
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0   # must match admission allow-list
  instruction: "Customer-authored, untrusted."
  model:
    pool: acme-pool
  mcpServers:                       # inline; the agent dials each directly
    - name: acme-crm
      endpoint: https://crm.acme.example/mcp
      auth:
        mode: staticToken           # bearer mounted onto this tenant's pod (prefer aauth here)
        tokenSecretRef:
          name: acme-crm-token
          key: token
  subscribe: ["mcp://acme-crm/tickets"]
  surfaces:
    a2a: true
    metrics: true
    management: true              # RBAC/SAR-gated pause/drain/cancel over mTLS :8443
  limits:
    lifetimeTokens: 2000000       # per-tenant cumulative ceiling (harness-enforced)
    maxTokens: 100000
    maxSteps: 40
  # NO exec / egress / secrets declared — no trifecta
```

**Why agentctl**

- Secret-free with AAuth: give the tenant agent an AAuth identity and its untrusted instruction runs in a pod that holds no provider key and no tool token — it signs its own calls, so there is nothing on the pod to exfiltrate. (Mount a key only for a tenant you trust not to leak it — there is no off-pod broker now, and public-HTTPS egress is open.)
- Hostile-multi-tenant isolation is the default, not a bolt-on: default-deny NetworkPolicies (egress only to DNS, the control plane, and public HTTPS) + attested source-IP identity on the work fabric mean a tenant cannot reach another tenant's namespace or borrow its work claims; model.pool and tools are namespace-scoped.
- Per-tenant token ceilings give real cost attribution and a blast-radius cap — one runaway tenant exhausts its own spec.limits.lifetimeTokens (a clean drain), not your whole model bill.
- Admission gates (registry allow-list + lethal-trifecta) let you accept arbitrary customer instructions while forbidding arbitrary code, egress, and secret reads.
- kubectl-native operations: pause, drain, or cancel a misbehaving tenant through RBAC/SAR-gated management verbs — the tenant can't drive those on itself.

**Operating it.** Templatize the namespace + (optional) Secrets + ModelPool + Agent (tools inline) block per customer (one apply per onboarding); prefer per-tenant AAuth so no key is mounted on an untrusted pod. Isolation requires a policy-enforcing CNI (Calico/Cilium) with networkPolicies.enabled=true — kindnet ignores NetworkPolicies. Watch each tenant agent's agent_budget_tokens_remaining gauge to spot a tenant nearing its lifetime ceiling; pause a hot tenant with kubectl management verbs while you investigate.

---

## Next steps

- **[Install the control plane](../charts/agentctl/README.md)** — cert-manager + `helm install`, then apply any example above.
- **[Architecture](architecture.md)** — how the operator, A2A gateway, and coordination fit together.
- **[Security](security.md)** — the identity model, per-namespace isolation, and the lethal-trifecta gate behind the multi-tenant example.
- **[Operations](operations.md)** — day-2: the management verbs, budgets, upgrades, and tuning.
- **[The Agent Control Contract](../contract/README.md)** — what makes an agent conformant, so you can bring your own.

