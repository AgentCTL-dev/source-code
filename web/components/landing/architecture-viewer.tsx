"use client";

import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Mermaid } from "@/components/mdx/Mermaid";

// The diagrams are lifted verbatim from docs/architecture.md so the landing and
// the docs render the exact same topology + per-flow sequence diagrams through
// the shared static-export-safe <Mermaid> client component.
type Diagram = { value: string; label: string; caption: string; chart: string };

const DIAGRAMS: Diagram[] = [
  {
    value: "topology",
    label: "Topology",
    caption: "Who talks to whom — control plane, data plane, and the edges between them.",
    chart: `flowchart LR
  subgraph ext[External / users]
    kubectl[kubectl / kube-apiserver]
    prod[Producers]
    peer[Other agents / A2A clients]
    prov[Model provider]
    prom[Prometheus / Grafana]
  end

  subgraph cp[Control plane: agentctl-system]
    adm[admission webhook]
    api[aggregated APIServer]
    op[operator]
    na[node-agent DaemonSet]
    gw[A2A gateway]
    mg[ModelGateway]
    coord[coordination server]
    scaler[KEDA external scaler]
    keda[KEDA]
    cm[cert-manager]
    pg[(Postgres)]
  end

  subgraph dp[Data plane: your agents]
    agent[agentd pod]
  end

  kubectl -->|apply Agent/Fleet/ModelPool| api
  kubectl -->|drain / status verbs| api
  api -->|mutate then validate| adm
  op -->|watch CRDs, render workload| agent
  api -->|mTLS| na
  na -->|unix socket MCP mgmt| agent
  peer -->|A2A HTTP + SSE| gw
  gw -->|mTLS| na
  gw <-->|durable tasks| pg
  mg <-->|usage / tasks| pg
  agent -->|infer: no key| mg
  mg -->|provider call: key injected| prov
  prod -->|work.submit MCP| coord
  agent -->|work.claim/ack MCP egress| coord
  keda -->|IsActive/GetMetrics gRPC| scaler
  scaler -->|work.stats| coord
  keda -->|scale 0..N| agent
  na -->|scrape-proxy /metrics| prom
  cm -.->|certs + caBundle| api
  cm -.->|cert| adm
  cm -.->|mTLS certs| na`,
  },
  {
    value: "provisioning",
    label: "Provisioning",
    caption: "Apply a CR → admission → operator → a confined, running agent.",
    chart: `sequenceDiagram
  actor U as kubectl
  participant API as kube-apiserver
  participant ADM as admission webhook
  participant OP as operator
  participant AG as agentd pod
  U->>API: apply Agent (image, mode, modelPool, caps)
  API->>ADM: mutate (defaults) then validate (trifecta + registry)
  ADM-->>API: patched + admitted
  OP->>API: watch Agents
  OP->>API: apply Deployment/Job/StatefulSet (confined + downward env + mgmt socket)
  API-->>AG: scheduled + started
  AG->>AG: serve mgmt MCP on unix socket; idle / reactive`,
  },
  {
    value: "management",
    label: "Management",
    caption: "kubectl drain → SAR → mTLS → kernel-attested unix socket.",
    chart: `sequenceDiagram
  actor U as kubectl
  participant KA as kube-apiserver
  participant API as aggregated APIServer
  participant NA as node-agent
  participant AG as agentd
  U->>KA: create --raw .../agents/x/drain
  KA->>API: proxy (front-proxy mTLS + identity)
  API->>API: SubjectAccessReview (RBAC)
  API->>NA: POST /drain (mTLS client cert)
  NA->>NA: SO_PEERCRED attest pod uid
  NA->>AG: tools/call drain (unix socket MCP)
  AG-->>NA: draining -> proc.exit reason=drain
  NA-->>API: ok
  API-->>U: Success`,
  },
  {
    value: "intelligence",
    label: "Intelligence",
    caption: "Secretless inference: the gateway injects the key and enforces the budget.",
    chart: `sequenceDiagram
  participant AG as agentd
  participant MG as ModelGateway
  participant K8 as kube Secret via ModelPool
  participant P as Provider
  Note over AG: networkless + secretless
  AG->>MG: infer (X-Agent identity, no key)
  MG->>K8: read ModelPool credentialSecretRef
  MG->>MG: meter tokens; check budget
  alt within budget
    MG->>P: provider call (real key injected)
    P-->>MG: completion + usage
    MG-->>AG: completion
  else over budget
    MG-->>AG: HTTP 429
  end`,
  },
  {
    value: "a2a",
    label: "A2A",
    caption: "Agents reachable by other agents over a JWS-verified, durable mesh.",
    chart: `sequenceDiagram
  participant C as A2A client / peer agent
  participant GW as A2A gateway
  participant PG as Postgres
  participant NA as node-agent
  participant AG as agentd
  C->>GW: GET /.well-known/agent-card.json
  GW-->>C: JWS-signed Agent Card
  C->>GW: message/send or message/stream
  GW->>PG: create Task (durable)
  GW->>NA: forward (mTLS)
  NA->>AG: a2a (unix socket)
  AG-->>NA: result / SSE frames
  NA-->>GW: relay
  GW->>PG: update Task -> completed
  GW-->>C: result / SSE: working -> artifact -> completed`,
  },
  {
    value: "claim",
    label: "Claim-mode",
    caption: "Elastic from zero: KEDA scales on backlog; one winner per item.",
    chart: `sequenceDiagram
  participant PR as Producer
  participant CO as coordination server
  participant SC as scaler
  participant KE as KEDA
  participant FL as fleet 0..N agentd
  participant MG as ModelGateway
  PR->>CO: work.submit(item, claim_key)
  loop polling
    KE->>SC: IsActive / GetMetrics (gRPC)
    SC->>CO: work.stats -> pending
  end
  KE->>FL: scale 0 -> N (from zero)
  FL->>CO: work.claim(item) + claim_key  (N agents race)
  CO-->>FL: granted=true to ONE; held_by to the rest
  FL->>MG: infer (process the item)
  FL->>CO: work.ack(lease, claim_key)
  Note over CO: claim_key recorded -> redelivery deduped; lease expiry re-offers on crash
  SC->>CO: work.stats -> 0
  KE->>FL: scale N -> 0`,
  },
];

export function ArchitectureViewer() {
  return (
    <Tabs defaultValue="topology" className="w-full gap-4">
      <div className="overflow-x-auto">
        <TabsList className="h-auto flex-nowrap">
          {DIAGRAMS.map((d) => (
            <TabsTrigger key={d.value} value={d.value} className="whitespace-nowrap">
              {d.label}
            </TabsTrigger>
          ))}
        </TabsList>
      </div>
      {DIAGRAMS.map((d) => (
        <TabsContent key={d.value} value={d.value}>
          <div className="bg-card/60 rounded-xl border p-4 sm:p-6">
            <p className="text-muted-foreground mb-4 text-sm">{d.caption}</p>
            <Mermaid chart={d.chart} />
          </div>
        </TabsContent>
      ))}
    </Tabs>
  );
}
