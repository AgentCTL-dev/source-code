import Link from "next/link";
import {
  ArrowRight,
  Boxes,
  Fingerprint,
  Gauge,
  KeyRound,
  Network,
  ScrollText,
  ShieldCheck,
  Workflow,
} from "lucide-react";
import { Nav } from "@/components/site/nav";
import { Footer } from "@/components/site/footer";
import { CodeBlock } from "@/components/site/code-block";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { GITHUB_URL, REPO_DOCS, AGENTD_IMAGE } from "@/data/site";

export default function Home() {
  return (
    <>
      <a
        href="#main"
        className="focus:bg-background sr-only focus:not-sr-only focus:fixed focus:top-3 focus:left-3 focus:z-[100] focus:rounded-md focus:border focus:px-3 focus:py-2 focus:text-sm"
      >
        Skip to content
      </a>
      <Nav />
      <main id="main" className="flex flex-1 flex-col">
        <Hero />
        <Model />
        <Planes />
        <Quickstart />
        <Benchmarks />
        <Principles />
        <FinalCta />
      </main>
      <Footer />
    </>
  );
}

/* -- layout helpers -------------------------------------------------------- */

function Section({
  id,
  eyebrow,
  title,
  lead,
  children,
}: {
  id?: string;
  eyebrow?: string;
  title?: string;
  lead?: string;
  children: React.ReactNode;
}) {
  return (
    <section id={id} className="border-border/50 border-t">
      <div className="mx-auto max-w-6xl px-4 py-16 sm:px-6 sm:py-20">
        {title ? (
          <div className="mb-10 max-w-2xl">
            {eyebrow ? (
              <div className="text-muted-foreground mb-2 font-mono text-xs tracking-widest uppercase">
                {eyebrow}
              </div>
            ) : null}
            <h2 className="text-2xl font-semibold tracking-tight sm:text-3xl">{title}</h2>
            {lead ? <p className="text-muted-foreground mt-3 text-base">{lead}</p> : null}
          </div>
        ) : null}
        {children}
      </div>
    </section>
  );
}

/* -- hero ------------------------------------------------------------------ */

function Hero() {
  return (
    <section className="relative overflow-hidden">
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0 -z-10 bg-[radial-gradient(60%_50%_at_50%_0%,color-mix(in_oklch,var(--primary)_14%,transparent),transparent)]"
      />
      <div className="mx-auto grid max-w-6xl gap-10 px-4 py-20 sm:px-6 lg:grid-cols-2 lg:py-28">
        <div className="flex flex-col justify-center">
          <Badge variant="outline" className="mb-5 w-fit gap-1.5 font-mono text-xs">
            <span className="size-1.5 rounded-full bg-emerald-400" /> Contract 2.0 · the network is the substrate
          </Badge>
          <h1 className="text-4xl font-semibold tracking-tight text-balance sm:text-5xl">
            The Kubernetes control plane for fleets of conformant agents.
          </h1>
          <p className="text-muted-foreground mt-5 max-w-xl text-lg text-pretty">
            agentctl provisions, secures, scales, and routes fleets of contract-conformant
            agents. Agents <strong className="text-foreground">serve mTLS HTTPS</strong> and
            dial the gateways <strong className="text-foreground">keyless</strong>. Identity is
            the boundary — a verified client cert, or an attested source IP. No node agent, no
            pod-resident secrets.
          </p>
          <div className="mt-8 flex flex-wrap gap-3">
            <Button asChild size="lg">
              <Link href="/install/">
                Get started <ArrowRight className="size-4" />
              </Link>
            </Button>
            <Button asChild size="lg" variant="outline">
              <a href={GITHUB_URL} target="_blank" rel="noreferrer">
                View on GitHub
              </a>
            </Button>
          </div>
          <p className="text-muted-foreground mt-6 font-mono text-xs">
            Rust control plane · BUSL-1.1 · the contract + SDK are Apache-2.0
          </p>
        </div>
        <div className="flex items-center">
          <CodeBlock
            className="w-full shadow-sm"
            lang="agent.yaml — one CR, an mTLS-served agent"
            code={`apiVersion: ${"agents.x-k8s.io/v1alpha1"}
kind: Agent
metadata: { name: researcher, namespace: team-a }
spec:
  image: ${AGENTD_IMAGE}
  mode: reactive
  modelPool: gpt          # keyless — the ModelGateway holds the key
  mcpServerSetRefs: [tools] # brokered — no tool credential on the pod

# the operator renders a restricted-PSS pod that:
#   serves  https://0.0.0.0:8443/mcp   (mTLS, per-workload cert)
#   dials   AGENT_INTELLIGENCE=https://…   (keyless, source-IP attested)
#   holds   zero credentials · no hostPath · runAsNonRoot`}
          />
        </div>
      </div>
    </section>
  );
}

/* -- the model ------------------------------------------------------------- */

function Model() {
  return (
    <Section
      id="model"
      eyebrow="The model"
      title="The network is the substrate; identity is the boundary."
      lead="Contract 2.0 removed every non-HTTP transport and the local exec surface. Agents are reached the way Kubernetes reaches anything else — over the network, with a verified identity — which keeps the control plane out of the agent's execution layer while managing it completely."
    >
      <div className="grid gap-4 md:grid-cols-2">
        <Card className="p-6">
          <div className="mb-3 flex items-center gap-2">
            <ShieldCheck className="text-primary size-5" />
            <h3 className="font-semibold">Into the agent — mTLS client cert</h3>
          </div>
          <p className="text-muted-foreground text-sm">
            The APIServer and A2A gateway dial the agent&apos;s{" "}
            <code className="font-mono">https://&lt;podIP&gt;:8443/mcp</code> presenting the
            control-plane client cert. A cert that chains to the pinned CA is{" "}
            <code className="font-mono">Management</code>; no cert is refused, never downgraded.
          </p>
        </Card>
        <Card className="p-6">
          <div className="mb-3 flex items-center gap-2">
            <Fingerprint className="text-primary size-5" />
            <h3 className="font-semibold">Out of the agent — attested source IP</h3>
          </div>
          <p className="text-muted-foreground text-sm">
            The agent dials the ModelGateway and MCPGateway <strong>keyless</strong>. Each gateway
            attests the caller by source IP (a kube pod lookup), scopes it, injects the real
            credential it holds off-pod, meters, and forwards. No secret ever lands on the pod.
          </p>
        </Card>
      </div>
      <div className="text-muted-foreground mt-6 grid gap-2 font-mono text-xs sm:grid-cols-3">
        <div className="border-border/60 bg-muted/30 rounded-md border px-3 py-2">
          ✓ node-agent retired
        </div>
        <div className="border-border/60 bg-muted/30 rounded-md border px-3 py-2">
          ✓ zero pod credentials
        </div>
        <div className="border-border/60 bg-muted/30 rounded-md border px-3 py-2">
          ✓ restricted-PSS · no hostPath
        </div>
      </div>
    </Section>
  );
}

/* -- the planes ------------------------------------------------------------ */

const PLANES = [
  {
    icon: Boxes,
    title: "Provisioning & PKI",
    body: "Declare an Agent or AgentFleet; the operator renders an mTLS-serving pod and mints its identity via cert-manager — a per-workload serving cert plus the per-namespace CA. Restricted-PSS, zero credentials, live cert rotation.",
  },
  {
    icon: ScrollText,
    title: "Management",
    body: "An aggregated APIServer exposes drain / lame-duck / cancel / pause / resume as SAR-gated verbs, forwarded direct to the agent pod as a2a.* admin JSON-RPC over mTLS. No proxy, no host socket.",
  },
  {
    icon: KeyRound,
    title: "Intelligence",
    body: "The ModelGateway is the secretless inference path: agents dial it keyless, it attests by source IP, injects the ModelPool credential, meters tokens, and enforces the budget — a 429 on exhaustion. The pod never holds a provider key.",
  },
  {
    icon: Network,
    title: "Tool plane (MCP)",
    body: "Agents work only through operator-provided MCP tools. An MCPServerSet binds servers; the MCPGateway attests, scopes to what the agent may reach, injects the credential off-pod, and forwards. No stdio, no tool secret on the pod.",
  },
  {
    icon: Workflow,
    title: "A2A mesh",
    body: "The gateway fronts each agent's public A2A surface — forwarding direct to the pod with the contract-2.0 wire (bare PascalCase methods, SSE streaming), signing the Agent Card, and holding the durable task store.",
  },
  {
    icon: Gauge,
    title: "Scaling",
    body: "Elastic fleets by claim (Deployment + a KEDA external scaler, scale-from-zero on an off-pod backlog) or shard (StatefulSet with keyed partitioning). Exactly one replica-field writer.",
  },
];

function Planes() {
  return (
    <Section
      id="planes"
      eyebrow="The planes"
      title="One control plane, six planes — every capability gated default-off."
      lead="Each plane programs against the published Agent Control Contract, never a specific agent. agentd is the reference implementation, not a dependency."
    >
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {PLANES.map((p) => (
          <Card key={p.title} className="p-6">
            <p.icon className="text-primary mb-3 size-5" />
            <h3 className="font-semibold">{p.title}</h3>
            <p className="text-muted-foreground mt-2 text-sm">{p.body}</p>
          </Card>
        ))}
      </div>
    </Section>
  );
}

/* -- quickstart ------------------------------------------------------------ */

function Quickstart() {
  return (
    <Section
      id="quickstart"
      eyebrow="Quickstart"
      title="Install the control plane, apply a CR."
      lead="cert-manager is the only hard prerequisite. Postgres is bundled; KEDA is optional (claim-mode autoscaling)."
    >
      <div className="grid gap-4 lg:grid-cols-2">
        <CodeBlock
          lang="1 · install"
          code={`# cert-manager (the one hard prerequisite)
kubectl apply -f https://github.com/cert-manager/\\
  cert-manager/releases/latest/download/cert-manager.yaml

# the control plane
kubectl create namespace agentctl-system
helm install agentctl ./charts/agentctl -n agentctl-system

kubectl -n agentctl-system get pods     # all Running
kubectl -n agentctl-system get certificate  # all READY`}
        />
        <CodeBlock
          lang="2 · run an agent"
          code={`kubectl apply -f - <<'EOF'
apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata: { name: hello, namespace: team-a }
spec:
  image: ${AGENTD_IMAGE}
  mode: reactive
EOF

kubectl get agents -n team-a   # READY=True
# the pod now serves mTLS :8443/mcp and dials keyless.`}
        />
      </div>
    </Section>
  );
}

/* -- benchmarks ------------------------------------------------------------ */

const STATS = [
  { value: "~1m", label: "CPU / idle agent", sub: "sub-MiB working set" },
  { value: "0", label: "pod credentials", sub: "keyless dials, mTLS serve" },
  { value: "~18m", label: "control plane CPU", sub: "~79 MiB across 9 pods" },
  { value: "0", label: "per-node cost", sub: "node-agent retired" },
];

function Benchmarks() {
  return (
    <Section
      id="benchmarks"
      eyebrow="Measured"
      title="Light data plane, negligible control plane."
      lead="Live kubectl-top readings from a running v2 stack: a full control plane plus a reactive agent bound to a brokered MCP tool, all Ready."
    >
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        {STATS.map((s) => (
          <Card key={s.label} className="p-6">
            <div className="text-3xl font-semibold tracking-tight tabular-nums">{s.value}</div>
            <div className="mt-1 text-sm font-medium">{s.label}</div>
            <div className="text-muted-foreground mt-0.5 font-mono text-xs">{s.sub}</div>
          </Card>
        ))}
      </div>
      <p className="text-muted-foreground mt-4 text-xs">
        Point-in-time readings of one idle agent — the eight Rust components together idle at
        ~10m / ~25 MiB; Postgres is the single largest line. Full density / throughput / latency
        methodology in the repo benchmarks.
      </p>
    </Section>
  );
}

/* -- principles ------------------------------------------------------------ */

const PRINCIPLES = [
  {
    title: "Depend on the contract, never on an agent",
    body: "agentctl consumes only the published, language-neutral Agent Control Contract. Any binary that emits a conformant manifest, serves mTLS /mcp, and dials keyless is managed unchanged. agentd is the reference, not a dependency.",
  },
  {
    title: "Identity is the boundary",
    body: "A verified mTLS client cert into agents; an attested source IP into the gateways. Reachability is never authority. mTLS-only — the control plane never puts a bearer on the pod.",
  },
  {
    title: "The pod holds no power it doesn't need",
    body: "No provider key, no tool credential, no bearer, no hostPath, no privilege — only a rotatable serving identity. Credentials live off-pod in the gateways that inject them.",
  },
];

function Principles() {
  return (
    <Section eyebrow="Principles" title="The load-bearing rules.">
      <div className="grid gap-4 md:grid-cols-3">
        {PRINCIPLES.map((p) => (
          <div key={p.title} className="border-border/60 rounded-lg border p-6">
            <h3 className="font-semibold">{p.title}</h3>
            <p className="text-muted-foreground mt-2 text-sm">{p.body}</p>
          </div>
        ))}
      </div>
      <p className="text-muted-foreground mt-6 text-sm">
        The full architecture, contract specification, and operational guides live in the{" "}
        <a href={REPO_DOCS} className="text-foreground underline underline-offset-4" target="_blank" rel="noreferrer">
          repository documentation
        </a>.
      </p>
    </Section>
  );
}

/* -- final cta ------------------------------------------------------------- */

function FinalCta() {
  return (
    <Section title="Run a fleet in a few minutes.">
      <div className="flex flex-wrap gap-3">
        <Button asChild size="lg">
          <Link href="/install/">
            Install guide <ArrowRight className="size-4" />
          </Link>
        </Button>
        <Button asChild size="lg" variant="outline">
          <Link href="/architecture/">Read the architecture</Link>
        </Button>
      </div>
    </Section>
  );
}
