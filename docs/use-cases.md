# Use cases

agentctl turns fleets of conformant agents into declarative Kubernetes resources. These worked examples show how a solo engineer, a startup, a small business, and platform, data, and ops teams put it to work — each with the actual manifests to `kubectl apply`.

Every example runs the reference agent **agentd** (`ghcr.io/agentd-dev/agentd:1.0.0`), but agentctl depends on the [Agent Control Contract](../contract/README.md), not on any one binary — swap in any conformant agent image and the resources are unchanged. The manifests are complete but illustrative: the control plane must already be installed (see [the chart README](../charts/agentctl/README.md)), and provider endpoints, Secrets, and MCP server URLs are placeholders to replace with your own.

**The common thread across all of them:**

- **Secret-free agents** — no model-provider or tool-server credential is ever mounted on an agent pod; the gateways attest the caller and inject credentials off-pod.
- **Scale-from-zero** — claim fleets idle at zero replicas and scale on the work backlog, so you pay only for work actually done.
- **Hard budgets** — `ModelPool` and per-fleet token caps are enforced with a `429`, so an agent loop can't run up a surprise bill.
- **Contract, not a vendor** — you program against the contract and `kubectl`, not a proprietary agent framework.

## Pick your starting point

| Persona | Use case | Core building block |
| --- | --- | --- |
| Solo AI engineer / indie hacker | [Event-driven personal automation on a tiny cluster](#event-driven-personal-automation-on-a-tiny-cluster) | reactive `Agent` |
| Seed-stage SaaS startup with a 4-person engineering team and no dedicated ops | [Elastic support-ticket triage that costs $0 while you sleep](#elastic-support-ticket-triage-that-costs-0-while-you-sleep) | claim `AgentFleet` (scale-from-zero) |
| Software / platform AI engineer | [Automated pull-request review at scale with a claim AgentFleet](#automated-pull-request-review-at-scale-with-a-claim-agentfleet) | claim `AgentFleet` + `MCPServerSet` |
| Small digital agency ops lead | [Nightly SEO drafts + CRM enrichment on a hard budget, in three YAML files](#nightly-seo-drafts--crm-enrichment-on-a-hard-budget-in-three-yaml-files) | schedule-mode `Agent` (CronJob) |
| Data / ops engineering team | [Sharded enrichment: burst-backfill a record backlog, then own it in steady state](#sharded-enrichment-burst-backfill-a-record-backlog-then-own-it-in-steady-state) | claim + shard `AgentFleet` |
| Ops / RevOps automation team | [A least-privilege deal-desk pipeline: four stages chained over the work fabric, under one token budget, scaling from zero](#a-least-privilege-deal-desk-pipeline-four-stages-chained-over-the-work-fabric-under-one-token-budget-scaling-from-zero) | chained fleets on the work fabric |
| SaaS platform team | [Hosting untrusted per-tenant agents as a customer-facing SaaS feature](#hosting-untrusted-per-tenant-agents-as-a-customer-facing-saas-feature) | per-namespace isolation + `ModelPool` budgets |

---

## Event-driven personal automation on a tiny cluster

**Who:** Solo AI engineer / indie hacker

I run a one-node cluster in a closet and I want a personal assistant that wakes up when something lands — a webhook, a new inbox message, a dropped file — triages it, and takes one small action through a tool. I don't want a service idling and billing me 24/7, and I really don't want a provider API key or a tool token sitting on a pod I forget to lock down. It should be one small YAML I can read in one sitting, and it should cost basically nothing when nothing is happening.

**How it works**

1. Define one ModelPool pointing at your LLM provider, with a tight budget.maxTokens. The modelgateway attests the agent, injects the provider key OFF-POD on each call, meters tokens, and returns a 429 when the budget is spent — so a runaway loop caps your spend instead of your credit card.
2. Define one MCPServerSet with the one or two tools the assistant needs (e.g. an inbox/reader server and a notify/write server). Each server's token lives in a Secret the mcpgateway reads; the agent is scoped to only these servers and holds NO tool credential.
3. Declare a reactive Agent (mode: reactive) that binds the pool via spec.modelPool and the tools via mcpServerSetRefs, and lists its event sources in subscribe (MCP resource URIs like inbox://unread or hook://intake). The operator renders it to a single-replica Deployment that idles and wakes on those subscribed resources.
4. The reactive agent triages each event with its instruction, then acts through exactly one or two bound MCP tools via the gateway — no exec, no egress, no secrets declared, so it never trips the lethal-trifecta admission gate.
5. Add a second on-demand Agent in mode: once for deeper research. The operator renders it to a Job; kubectl apply it (or delete + re-apply) when you want a one-shot run, and it terminates to zero when done.
6. Everything is secret-free on the pod, mTLS on the Management surface, restricted-PSS, and metered by budget. At rest the reactive Deployment is a single idle pod and the once Job is nothing at all — near-zero cost until an event actually arrives.

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
  budget:
    maxTokens: 500000
---
apiVersion: agentctl.dev/v1alpha1
kind: MCPServerSet
metadata:
  name: home-tools
  namespace: indie
spec:
  servers:
    - name: inbox
      endpoint: https://inbox-mcp.indie.svc.cluster.local./mcp
      auth:
        mode: none
    - name: notify
      endpoint: https://notify-mcp.indie.svc.cluster.local./mcp
      auth:
        mode: staticToken
        tokenSecretRef:
          name: notify-token
          key: token
      budget:
        maxTokens: 100000
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
  modelPool: home-pool
  mcpServerSetRefs:
    - name: home-tools
  subscribe: ["inbox://unread", "hook://intake"]
  surfaces:
    metrics: true
    management: true
  limits:
    maxTokens: 20000
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
  modelPool: home-pool
  mcpServerSetRefs:
    - name: home-tools
  limits:
    maxTokens: 120000
```

**Why agentctl**

- Secret-free by construction: neither agent holds the provider key or the notify token — the modelgateway and mcpgateway inject them off-pod, so a compromised or misconfigured pod leaks nothing.
- Near-zero cost at rest: the reactive Agent is one idle Deployment pod that wakes only on subscribed events, and the once research Agent is a Job that runs and terminates — nothing idles billing you.
- Budget as a hard stop, not a dashboard: ModelPool budget.maxTokens plus per-agent limits.maxTokens return a 429 on exhaustion, capping spend even if an instruction misbehaves.
- One small, readable YAML: two Secrets, a ModelPool, an MCPServerSet, and two Agents — declarative, kubectl-native, diffable in git.
- Safe by default: no exec/egress/secrets declared means the lethal-trifecta admission gate is never tripped, and the agent is scoped to only the two tools you bound.

**Operating it.** Watch spend and event throughput via each component's Prometheus /metrics (surfaces.metrics: true on the reactive agent). Because the triage agent also exposes surfaces.management: true, you can pause/resume, cancel, or drain it through the aggregated Management API with kubectl to stop it acting without deleting config; re-apply the once research Agent whenever you want a fresh one-shot run, and delete the reactive Agent to remove the last idle pod entirely.

---

## Elastic support-ticket triage that costs $0 while you sleep

**Who:** Seed-stage SaaS startup with a 4-person engineering team and no dedicated ops

Support tickets pour in during business hours and go quiet overnight, but our human queue can't keep up with first-response SLAs. We want AI to classify, tag, draft a reply, and escalate the genuinely hard tickets — without standing up a 24/7 service, without pasting our OpenAI key and Zendesk token into yet another pod, and without paying for idle compute at 3am. We're two founders deep and can't babysit infrastructure.

**How it works**

1. Your ingest webhook drops each new ticket onto the coordination work fabric as a work item on queue://tickets (one work.submit per ticket), which becomes the backlog signal.
2. A claim-mode AgentFleet of agentd workers leases tickets with exactly-one-owner claims; each worker classifies, tags, drafts a reply, and either resolves or escalates, then reports terminal state with work.result/ack.
3. The KEDA external scaler reads the coordination backlog and scales the worker Deployment elastically — up during the daytime burst, back down to zero replicas overnight, so idle compute is $0.
4. Workers dial the modelgateway keyless: it attests each pod by its attested source IP, injects the OpenAI credential off-pod from the ModelPool's Secret, meters tokens, and returns a 429 once the token budget is exhausted — no provider key ever lands on a worker.
5. Workers reach Zendesk and your CRM only through the mcpgateway, which scopes them to the bound MCPServerSet, injects each server's token off-pod, and forwards MCP — no helpdesk credential on any pod.
6. A poison ticket that fails maxAttempts redeliveries is dead-lettered (surfaced at dlq://items) for a human to inspect instead of hot-looping and burning budget.
7. Restricted-PSS pods plus default-deny NetworkPolicies mean a worker can only talk to DNS and the control-plane gateways — nothing else, including the tenant next door.

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
  budget:
    maxTokens: 50000000   # total cumulative cap; modelgateway 429s when spent. Raise/reset via kubectl.
---
apiVersion: agentctl.dev/v1alpha1
kind: MCPServerSet
metadata:
  name: helpdesk-tools
  namespace: support
spec:
  servers:
    - name: zendesk
      endpoint: https://mcp.zendesk.example.com
      auth:
        mode: staticToken
        tokenSecretRef:
          name: zendesk-token
          key: token
      tags: ["egress"]
    - name: crm
      endpoint: https://mcp.crm.internal
      auth:
        mode: none
      tags: ["egress"]
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
    modelPool: support-llm
    mcpServerSetRefs:
      - name: helpdesk-tools
    subscribe: ["queue://tickets"]
    surfaces:
      metrics: true
      management: true   # exposes drain/pause/cancel verbs over mTLS (kubectl)
    limits:
      maxTokens: 20000
  scaling:
    mode: claim
    min: 0            # scale to zero overnight
    max: 20
    target:
      signal: pending_events
      value: "5"      # ~5 backlogged tickets per worker
  workSource: "queue://tickets"
  workPolicy:
    claimTtlMs: 120000
    maxAttempts: 3    # dead-letter poison tickets for a human
  budget:
    maxTokens: 40000000   # per-fleet cap, enforced on top of the pool cap
```

**Why agentctl**

- Secret-free pods: neither your OpenAI key nor your Zendesk token is ever mounted on a worker — the modelgateway and mcpgateway inject them off-pod after attesting the caller by its attested source IP.
- $0 when idle: claim-mode min:0 lets KEDA scale the Deployment to zero replicas overnight and back up on the daytime backlog — you pay only for tickets actually worked.
- Two budgets, one bill you control: a pool-wide total maxTokens plus a per-fleet cap, both enforced with a hard 429, so an agent loop can't run up a surprise invoice.
- No framework lock-in: you depend on the Agent Control Contract 1.0, not a vendor — agentd is the reference agent and swappable for any conformant image.
- kubectl-native and ops-light: the whole triage system is four declarative resources a 4-person team can review, diff, and GitOps — no bespoke autoscaler or secret-plumbing to maintain.

**Operating it.** Tune throughput with scaling.target.value (tickets per worker) and max; watch the per-component and per-agent Prometheus /metrics for backlog, token spend against budget, and dead-letter rate. With the management surface enabled you can kubectl-drain or pause the fleet during a provider incident or bad-prompt rollback, and requeue or drop dead-lettered tickets (dlq://items) on the work fabric. The pool budget is a cumulative token cap with no auto-reset — raise ModelPool.budget.maxTokens (or reset it) as volume grows, without touching the fleet.

---

## Automated pull-request review at scale with a claim AgentFleet

**Who:** Software / platform AI engineer

I want every pull request across our org reviewed automatically: read the diff, run static checks and code search, and post findings — plus chew through a backlog of repos on demand. Traffic is spiky (nothing at 3am, hundreds of PRs after a merge storm), the reviewer needs a git/code-search tool but I refuse to bake a GitHub token or a model key into the pod, and I have to be able to roll the fleet cleanly during a deploy without dropping in-flight reviews.

**How it works**

1. A webhook receiver (your small producer, out of scope of agentctl) turns each PR event into a work item and calls `work.submit` on the coordination server (the `work.*` MCP fabric); a backlog job can submit one item per repo to review at scale.
2. The `pr-reviewers` AgentFleet runs `scaling.mode: claim` with `min: 0`, so with `coordination` + `scaler` enabled the KEDA external scaler reads the `queue://pr-review` backlog and scales the Deployment from zero; each worker leases exactly-one-owner claims off `workSource` with a `claimTtlMs` lease.
3. Each worker (mode `reactive`, subscribed to `queue://pr-review`) reviews the claimed diff, calls the git/code-search MCP server through the mcpgateway — which attests the pod by source IP, scopes it to only the `code-tools` MCPServerSet, and injects the git token off-pod — then records findings via `work.ack`/`work.result`.
4. Model calls go keyless to the modelgateway, which resolves `review-pool`, injects the provider key from the referenced Secret, meters tokens, and enforces both the per-fleet `budget.maxTokens` and the pool cap (a 429 when either is exhausted); per-worker `limits` cap a single review's spend and steps.
5. A poison PR that fails `workPolicy.maxAttempts` times is dead-lettered (surfaced at `dlq://items`) instead of looping forever, so you can requeue or drop it.
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
  budget:
    maxTokens: 20000000
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
kind: MCPServerSet
metadata:
  name: code-tools
  namespace: ci
spec:
  servers:
    - name: git-codesearch
      endpoint: https://mcp.internal.svc/git
      auth:
        mode: staticToken
        tokenSecretRef:
          name: codetools-token
          key: token
        header: Authorization
      tags: ["egress"]
      budget:
        maxTokens: 2000000
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
    modelPool: review-pool
    mcpServerSetRefs:
      - name: code-tools
    subscribe: ["queue://pr-review"]
    surfaces:
      a2a: true
      management: true
      metrics: true
    limits:
      maxTokens: 200000
      maxSteps: 40
  scaling:
    mode: claim
    min: 0
    max: 25
    target:
      signal: pending_events
      value: "4"
  workSource: "queue://pr-review"
  workPolicy:
    claimTtlMs: 300000
    maxAttempts: 3
  budget:
    maxTokens: 50000000
```

**Why agentctl**

- Secret-free workers: the GitHub/code-search token and the model key are injected off-pod by the mcpgateway and modelgateway; a compromised reviewer pod holds no credential to exfiltrate.
- Scale-from-zero economics: claim mode idles at zero replicas and KEDA scales on the actual PR backlog, so a merge storm gets 25 workers and a quiet night costs nothing.
- Budgets as guardrails, not surprises: a per-fleet `budget` plus per-worker `limits` and the pool cap bound spend, returning a 429 on exhaustion instead of an open-ended bill.
- Clean rolls with no dropped work: the Management `drain` verb finishes in-flight reviews before you redeploy, and dead-lettering keeps one poison PR from wedging the fleet.
- Contract, not a vendor: workers are `ghcr.io/agentd-dev/agentd:1.0.0`, but any agent honoring the Agent Control Contract drops in unchanged — and the fleet is itself a signed A2A endpoint other systems can call.

**Operating it.** Requires the opt-in `coordination` + `scaler` planes (and KEDA) for elastic claim scaling; tune `target.value` (backlog per replica) and `max` to trade latency against provider rate limits, and watch the modelgateway and per-agent Prometheus series for token burn and claim throughput. Use `drain` before deploys and `pause`/`resume` to freeze the fleet during an incident; poison PRs surface on the dead-letter channel (`dlq://items`) for requeue or drop.

---

## Nightly SEO drafts + CRM enrichment on a hard budget, in three YAML files

**Who:** Small digital agency ops lead (comfortable with kubectl, not a Kubernetes expert)

We run content + CRM chores for a dozen clients. Every night I want SEO drafts written into our CMS, new leads summarized, and CRM records enriched — without paying for idle compute all day, and with a hard ceiling so a runaway loop can never blow the LLM budget. I don't want to babysit servers or scatter API keys across pods. I want a couple of YAML files I apply and forget.

**How it works**

1. Declare a ModelPool `content-llm` pointing at your provider, with `credentialSecretRef` naming a Secret and `budget.maxTokens` set to a hard cumulative cap. The modelgateway holds the key, meters every call, and returns a 429 the moment the pool crosses the cap — agents dial it keyless and never mount the provider Secret.
2. Declare one MCPServerSet `content-tools` bundling two `servers[]`: your CMS MCP server and your CRM MCP server, each with `auth.mode: staticToken` and a `tokenSecretRef`. The mcpgateway injects each server's token off-pod and scopes each agent to only the servers it binds.
3. Declare schedule-mode Agents (one per job) with `mode: schedule`, `image: ghcr.io/agentd-dev/agentd:1.0.0`, an `instruction`, `modelPool: content-llm`, and `mcpServerSetRefs: [{ name: content-tools }]`. The operator renders each to a Kubernetes CronJob.
4. Each night the CronJob fires a fresh Pod, the agent runs its instruction against the CMS/CRM tools through the mcpgateway and the model through the modelgateway, then exits. A CronJob runs no Pods between fires, so idle cost is nothing beyond the control plane.
5. Set a per-agent belt-and-suspenders cap with `limits.maxTokens` (and `maxSteps` to bound tool-call loops) so a single misbehaving run self-limits before it even touches the pool ceiling.
6. When the pool budget is exhausted, the modelgateway returns 429; the in-flight run fails cleanly and the next night's run resumes once you raise the cap or the budget window resets. Watch `ModelPool.status.usedTokens` against `spec.budget.maxTokens` via `kubectl get mp`.

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
  budget:
    maxTokens: 20000000   # hard cumulative cap (no auto-reset); 429 past this. Raise/reset via kubectl.
---
apiVersion: agentctl.dev/v1alpha1
kind: MCPServerSet
metadata:
  name: content-tools
  namespace: agency
spec:
  servers:
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
  modelPool: content-llm
  mcpServerSetRefs: [{ name: content-tools }]
  limits: { maxTokens: 2000000, maxSteps: 60 }
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
  modelPool: content-llm
  mcpServerSetRefs: [{ name: content-tools }]
  limits: { maxTokens: 2000000, maxSteps: 60 }
```

**Why agentctl**

- Zero cost when idle: a schedule-mode Agent renders to a CronJob — nothing runs (and nothing bills) until the cron fires, then the Pod exits.
- A real spend ceiling, not a dashboard alert: ModelPool `budget.maxTokens` is enforced in the modelgateway, which returns a 429 the instant the cap is crossed — a runaway run cannot overspend.
- Secret-free pods: the provider key and the CMS/CRM tokens live in Secrets the gateways read off-pod; the agent containers never mount a credential, so a compromised draft job leaks nothing.
- Scoped tools: each agent reaches only the CMS + CRM servers bound via its MCPServerSet, brokered and attested by the mcpgateway — no ambient network access to anything else.
- It's just declarative YAML applied with kubectl — no agent framework to write, no servers to run, and day-2 checks are `kubectl get mp` / `kubectl get agents`.

**Operating it.** Watch spend with `kubectl get mp content-llm` (Budget vs Used printer columns, from spec.budget.maxTokens and status.usedTokens); raise `budget.maxTokens` to lift the ceiling. Stagger the crons (02:00 / 02:30) so both nightly jobs draw from the shared cap in sequence, and per-agent `limits.maxTokens`/`maxSteps` bound any single run. Each component and agent exposes a Prometheus `/metrics` endpoint if you want per-run token and latency series.

---

## Sharded enrichment: burst-backfill a record backlog, then own it in steady state

**Who:** Data / ops engineering team

We have a 20M-row backlog to classify and embed, and once it's caught up we need N data sources kept continuously monitored with deterministic, no-double-work ownership. We don't want to hand every worker a warehouse token or a provider API key, and we want poison records quarantined instead of retried forever. We already run Kubernetes and want to drive all of this with kubectl, not a bespoke queue-worker service.

**How it works**

1. Define one ModelPool (provider + credentialSecretRef + defaultModel) with a budget.maxTokens cap. The modelgateway attests each worker, injects the provider key off-pod, meters tokens, and returns 429 when the cap is hit — workers dial keyless.
2. Define one MCPServerSet whose servers[] point at your datastore/warehouse MCP endpoints, each with auth.mode: staticToken + tokenSecretRef. The mcpgateway holds those tokens and scopes each worker to only these servers; the pods hold no tool credential.
3. For the backfill, deploy a CLAIM AgentFleet: scaling.mode: claim, min: 0, max: 40, target{signal,value}. The operator renders a Deployment with replicas omitted and a KEDA external scaler that scales the pool from zero off the workSource backlog; each worker leases exactly-one-owner claims from the coordination work fabric.
4. Bound the backfill's failure behavior with workPolicy: claimTtlMs (lease TTL so a crashed worker's item is redelivered) and maxAttempts (dead-letter a poison record after N redeliveries — it moves to the deadletter state, surfaced at dlq://items, instead of cycling forever). Add fleet-level budget.maxTokens to isolate this run's spend.
5. For steady state, deploy a SHARD AgentFleet: scaling.mode: shard, shards: 8. The operator renders a StatefulSet of 8 keyed partitions (FNV-1a/64 modulus); each ordinal deterministically owns its slice of the workSource, so every source has exactly one live owner and no two pods touch the same record.
6. Both fleets reference the same ModelPool and MCPServerSet by name and expose surfaces.metrics for Prometheus. When the backfill drains, KEDA parks it at min: 0 and the shard fleet carries ongoing monitoring; resize shards via a guarded stop-the-world rebalance.
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
  budget: { maxTokens: 500000000 }
---
apiVersion: agentctl.dev/v1alpha1
kind: MCPServerSet
metadata: { name: datastore-tools, namespace: data }
spec:
  servers:
    - name: warehouse
      endpoint: https://warehouse-mcp.data.svc:8443
      auth:
        mode: staticToken
        tokenSecretRef: { name: warehouse-token, key: token }
      tags: ["read", "write"]
      budget: { maxTokens: 200000000 }
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
    modelPool: enrich-pool
    mcpServerSetRefs: [{ name: datastore-tools }]
    subscribe: ["queue://records-backfill"]
    surfaces: { metrics: true }
    limits: { maxTokens: 8000 }
  scaling:
    mode: claim
    min: 0
    max: 40
    target: { signal: pending_events, value: "50" }
  workSource: "queue://records-backfill"
  workPolicy: { claimTtlMs: 60000, maxAttempts: 5 }
  budget: { maxTokens: 400000000 }
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
    modelPool: enrich-pool
    mcpServerSetRefs: [{ name: datastore-tools }]
    subscribe: ["queue://sources"]
    surfaces: { metrics: true }
  scaling:
    mode: shard
    shards: 8
  workSource: "queue://sources"
```

**Why agentctl**

- Secret-free workers: neither the provider key nor the warehouse token ever lands on a pod — the modelgateway and mcpgateway inject them off-pod and attest the caller.
- Scale-from-zero backfill: a claim fleet is a Deployment + KEDA external scaler that parks at zero and bursts to max on the real backlog, so you pay only while the queue is deep.
- Exactly-one-owner semantics two ways: claim leases (with TTL redelivery) for elastic backfill, FNV-1a keyed StatefulSet shards for deterministic steady-state ownership — no double-processing either way.
- Poison-record safety: workPolicy.maxAttempts dead-letters bad rows to dlq://items instead of retrying forever, and per-fleet budget.maxTokens caps the run's spend independently of the pool.
- kubectl-native operations: fleets are CRDs and drain/pause/resume are aggregated-API verbs, not a bespoke worker service you build and babysit.

**Operating it.** The claim fleet auto-parks at min: 0 when the backlog drains and re-bursts on new work; watch pending_events and per-fleet token spend (429s from the gateway signal budget exhaustion) on Prometheus /metrics. Resizing scaling.shards is a guarded stop-the-world rebalance, so pick N up front; before node maintenance, use the Management drain/pause verbs (kubectl, mTLS + RBAC-gated) to quiesce a shard cleanly rather than deleting the pod.

---

## A least-privilege deal-desk pipeline: four stages chained over the work fabric, under one token budget, scaling from zero

**Who:** Ops / RevOps automation team

Our deal-desk process has four stages — intake, research, draft, compliance-review — and today it's a pile of brittle scripts and human hand-offs. We want each stage to be its own agent that holds ONLY the tools for its job, the stages composed into a durable pipeline, and one hard token ceiling for the whole process so a runaway case can't blow the budget.

**How it works**

1. Model each stage as its own agent bound to ONLY the tools that stage needs (via mcpServerSetRefs), so the drafting agent literally cannot reach CRM or policy tools — least privilege is a binding, not a convention.
2. Chain the stages over the coordination work fabric: each stage subscribes to its input queue (queue://cases -> queue://research -> queue://drafting -> queue://compliance) and, when done, submits the next stage's work item via work.submit. The queue topology IS the process graph — no orchestrator script.
3. Run stage 1 (intake) as a CLAIM-mode AgentFleet with min 0: KEDA scales workers from ZERO on the case backlog and back to zero when idle; workers lease exactly-one-owner claims (claimTtlMs) and poison cases dead-letter to dlq://items after maxAttempts. Leave workSource UNSET so the scaler reads the coordination backlog (a queue:// workSource would break scaler activation).
4. Run stages 2-4 (research/draft/compliance) as long-lived reactive Agents, each bound to its own MCPServerSet; the mcpgateway attests each agent, scopes it to only its bound servers, and injects each server's credential OFF-POD (crm's staticToken lives at the gateway, never on the pod).
5. Point every stage at the SAME ModelPool process-brain; its budget.maxTokens is the single ceiling for the whole process — the modelgateway meters every stage's tokens and returns 429 when the pool is exhausted, no matter which stage spent it.
6. Add budget.maxTokens on the intake AgentFleet as a nested cap (enforced by the modelgateway keyed by namespace, pool, fleet) that isolates the intake tier's spend on top of the pool-wide ceiling.
7. Every agent is secret-free: agents dial the modelgateway and mcpgateway keyless over mTLS and the gateways inject provider/tool credentials off-pod — no key ever lands on a pod, so the pipeline is safe for hostile multi-tenancy.

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
stringData: { token: REPLACE-ME }   # read only by the mcpgateway, never on a pod
---
# One pool = one token ceiling for every stage of the process.
apiVersion: agentctl.dev/v1alpha1
kind: ModelPool
metadata: { name: process-brain, namespace: revops }
spec:
  provider: openai
  endpoint: https://api.provider.example/v1
  credentialSecretRef: { name: provider-credentials, key: api-key }
  models: ["gpt-strong", "gpt-fast"]
  defaultModel: gpt-strong
  budget: { maxTokens: 5000000 }   # hard ceiling across ALL stages
---
apiVersion: agentctl.dev/v1alpha1
kind: MCPServerSet
metadata: { name: research-tools, namespace: revops }
spec:
  servers:
    - name: crm
      endpoint: https://crm.internal/mcp
      auth: { mode: staticToken, tokenSecretRef: { name: crm-token, key: token } }
---
apiVersion: agentctl.dev/v1alpha1
kind: MCPServerSet
metadata: { name: drafting-tools, namespace: revops }
spec:
  servers:
    - { name: doc-store, endpoint: https://docs.internal/mcp, auth: { mode: none } }
---
apiVersion: agentctl.dev/v1alpha1
kind: MCPServerSet
metadata: { name: compliance-tools, namespace: revops }
spec:
  servers:
    - { name: policy-kb, endpoint: https://policy.internal/mcp, auth: { mode: none } }
---
# Stage 1 - intake: claim workers scale from ZERO on backlog, normalize each
# case, and submit it onward to queue://research via the work fabric.
apiVersion: agentctl.dev/v1alpha1
kind: AgentFleet
metadata: { name: case-intake, namespace: revops }
spec:
  budget: { maxTokens: 1000000 }          # fleet-scoped nested cap (intake only)
  workPolicy: { claimTtlMs: 60000, maxAttempts: 5 }
  scaling:
    mode: claim
    min: 0
    max: 8
    target: { signal: pending_events, value: "10" }
  # workSource intentionally UNSET: the KEDA scaler reads the coordination
  # backlog; a queue:// workSource would break scaler activation.
  template:
    mode: reactive
    image: ghcr.io/agentd-dev/agentd:1.0.0
    instruction: "Normalize each inbound case and submit it to queue://research."
    subscribe: ["queue://cases"]
    modelPool: process-brain
    surfaces: { a2a: true, metrics: true }
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
  modelPool: process-brain
  mcpServerSetRefs: [{ name: research-tools }]
  surfaces: { a2a: true, metrics: true }
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata: { name: draft-specialist, namespace: revops }
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Draft the deal doc from the research; submit it to queue://compliance."
  subscribe: ["queue://drafting"]
  modelPool: process-brain
  mcpServerSetRefs: [{ name: drafting-tools }]
  surfaces: { a2a: true, metrics: true }
---
apiVersion: agentctl.dev/v1alpha1
kind: Agent
metadata: { name: compliance-specialist, namespace: revops }
spec:
  mode: reactive
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Compliance-review the draft against policy; emit the terminal result."
  subscribe: ["queue://compliance"]
  modelPool: process-brain
  mcpServerSetRefs: [{ name: compliance-tools }]
  surfaces: { a2a: true, metrics: true }
```

**Why agentctl**

- The process graph is the work-fabric queue topology, not a pile of scripts — each stage claims its input and submits the next, with exactly-one-owner claims, TTL redelivery, and dead-lettering (dlq://items) built in.
- One ModelPool.budget.maxTokens is a single gateway-enforced ceiling for the entire multi-stage process; a runaway case gets a 429, not a surprise bill — and the intake fleet's own budget adds a nested isolation cap keyed by (namespace, pool, fleet).
- Least privilege by construction: each stage binds only its own MCPServerSet, so the drafting agent literally cannot reach CRM or policy tools; the mcpgateway scopes and injects every tool credential off-pod.
- Every agent is secret-free — no provider or tool key ever lands on a pod; agents dial the gateways keyless over mTLS — so the whole pipeline is safe for hostile multi-tenancy.
- The intake tier scales from zero on backlog (KEDA claim mode) while the specialist stages stay warm — the pipeline costs nothing when idle and absorbs bursts without re-plumbing.

**Operating it.** Let KEDA drive the intake tier from the case backlog (leave workSource unset so the scaler reads the coordination backlog; a queue:// workSource breaks scaler activation), or pin it with kubectl scale agentfleet/case-intake. Drain or pause any stage for a controlled cutover via the Management API (kubectl against the aggregated API, RBAC/SubjectAccessReview-gated); poison cases dead-letter to dlq://items after maxAttempts. Watch each agent's Prometheus /metrics plus the ModelPool's status.usedTokens meter to see whole-process spend against the single budget. If one stage must fan a single case across many workers, make that stage its own AgentFleet with a coordinator + distribution: a2a (which fans work to that fleet's OWN worker pool over A2A).

---

## Hosting untrusted per-tenant agents as a customer-facing SaaS feature

**Who:** SaaS platform team (agents as a customer-facing feature)

We ship agents as a product feature: every customer authors their own instruction and we run it for them. The instructions are effectively untrusted — a tenant will try to exfiltrate our provider keys, reach another tenant's tools, or burn the whole model budget. We need one agent per tenant, in the tenant's own namespace, hard-isolated, with no way to touch another tenant's models, tools, or credentials, and per-tenant cost attribution.

**How it works**

1. Give each customer their own namespace (e.g. tenant-acme). On every Agent/AgentFleet reconcile the operator ensures the three tenant NetworkPolicies (agent-default-deny in+out; agent-allow-controlplane-and-dns egress only to DNS + the four control-plane gateway pods — ModelGateway, MCPGateway, A2A gateway, coordination; agent-ingress-controlplane-only) in the workload's own namespace, so a namespace created after install is still isolated — no cross-tenant pod-to-pod path exists.
2. Put the provider credential in a Secret in each tenant namespace and reference it from a per-tenant ModelPool. Only the modelgateway ever reads it; the agent pod dials the gateway keyless and holds no key. Set modelgateway.secretsNamespaces / mcpgateway.secretsNamespaces to the tenant namespaces so the gateways drop the cluster-wide secrets grant.
3. Set ModelPool.spec.budget.maxTokens per tenant. The modelgateway attests the caller by source IP, resolves it to the pod's namespace (the authoritative tenant), meters tokens against that tenant's pool, and returns 429 on exhaustion — one tenant cannot spend, or even select, another tenant's pool.
4. Bind tools via a per-tenant MCPServerSet with auth.mode: staticToken pointing at the tenant's own tool-token Secret (distinct from the provider key). The mcpgateway attests the agent, scopes it to only its bound servers, and injects each server's token off-pod — a tenant agent holds no tool credential and cannot reach a server it isn't bound to.
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
  api-key: sk-acme-tenant-key      # only the modelgateway ever reads this
---
apiVersion: v1
kind: Secret
metadata:
  name: acme-crm-token
  namespace: tenant-acme
type: Opaque
stringData:
  token: crm-bearer-acme           # only the mcpgateway ever reads this
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
  budget:
    maxTokens: 2000000             # per-tenant cap; 429 on exhaustion
---
apiVersion: agentctl.dev/v1alpha1
kind: MCPServerSet
metadata:
  name: acme-tools
  namespace: tenant-acme
spec:
  servers:
    - name: acme-crm
      endpoint: https://crm.acme.example/mcp
      auth:
        mode: staticToken           # token held at the gateway, never on the pod
        tokenSecretRef:
          name: acme-crm-token
          key: token
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
  modelPool: acme-pool
  mcpServerSetRefs:
    - name: acme-tools
  subscribe: ["mcp://acme-crm/tickets"]
  surfaces:
    a2a: true
    metrics: true
    management: true              # RBAC/SAR-gated pause/drain/cancel over mTLS :8443
  limits:
    maxTokens: 100000
    maxSteps: 40
  # NO exec / egress / secrets — no trifecta, stays secret-free
```

**Why agentctl**

- Secret-free by construction: the tenant's untrusted instruction runs in a pod that holds no provider key and no tool token — both are injected off-pod by the gateways, so there is nothing on the pod to exfiltrate.
- Hostile-multi-tenant isolation is the default, not a bolt-on: default-deny NetworkPolicies + attested source-IP identity mean a tenant cannot reach, bill, or borrow another tenant's model pool, tools, or namespace.
- Per-tenant budgets give real cost attribution and a blast-radius cap — one runaway tenant hits a 429, not your whole model bill.
- Admission gates (registry allow-list + lethal-trifecta) let you accept arbitrary customer instructions while forbidding arbitrary code, egress, and secret reads.
- kubectl-native operations: pause, drain, or cancel a misbehaving tenant through RBAC/SAR-gated management verbs — the tenant can't drive those on itself.

**Operating it.** Templatize the namespace + Secrets + ModelPool + MCPServerSet + Agent block per customer (one apply per onboarding), and scope modelgateway.secretsNamespaces/mcpgateway.secretsNamespaces to the tenant namespaces to shrink the gateways' secret blast radius. Isolation requires a policy-enforcing CNI (Calico/Cilium) with networkPolicies.enabled=true — kindnet ignores NetworkPolicies. Watch per-tenant token metrics on the modelgateway and 429 rates to spot a tenant hitting its ModelPool budget; pause a hot tenant with kubectl management verbs while you investigate.

---

## Next steps

- **[Install the control plane](../charts/agentctl/README.md)** — cert-manager + `helm install`, then apply any example above.
- **[Architecture](architecture.md)** — how the operator, gateways, and coordination fit together.
- **[Security](security.md)** — the identity model, per-namespace isolation, and the lethal-trifecta gate behind the multi-tenant example.
- **[Operations](operations.md)** — day-2: the management verbs, budgets, upgrades, and tuning.
- **[The Agent Control Contract](../contract/README.md)** — what makes an agent conformant, so you can bring your own.

