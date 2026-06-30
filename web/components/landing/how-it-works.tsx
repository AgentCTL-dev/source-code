import { Reveal } from "@/components/motion/reveal";
import { SectionHeading } from "@/components/landing/section-heading";
import { ArchitectureViewer } from "@/components/landing/architecture-viewer";

// Curated from README.md (workspace table) + docs/architecture.md.
const PLANES: { name: string; role: string }[] = [
  { name: "operator", role: "render core + reconcile Agent / AgentFleet into Jobs, Deployments, StatefulSets — status, finalizer GC, KEDA-safe replicas" },
  { name: "node-agent", role: "on-node DaemonSet bridge: socket discovery, the management client, mTLS HTTP API, and the /metrics scrape-proxy" },
  { name: "aggregated APIServer", role: "front-proxy mTLS auth + SubjectAccessReview per verb, then forwards drain / lame-duck / cancel to the node-agent" },
  { name: "A2A gateway", role: "public A2A HTTP/JSON-RPC + SSE, JWS-signed Agent Card projection, bridged to the agent over the node-agent" },
  { name: "ModelGateway", role: "ModelPool-driven credential injection, token metering, and budget enforcement — the agent stays secretless" },
  { name: "coordination server", role: "the work.* claim ledger: atomic single-grant, claim_key dedupe, lease re-offer; backs KEDA scale-from-zero" },
];

export function HowItWorks() {
  return (
    <section id="how" className="scroll-mt-20 border-b">
      <div className="mx-auto max-w-6xl px-4 py-20 sm:py-24">
        <Reveal>
          <SectionHeading eyebrow="How it works" title="One control plane, every plane">
            agentctl is a Rust control plane of cooperating components over a{" "}
            <span className="text-foreground">tiered substrate</span> — stock-unix
            hostPath sockets as the portable default, Kata-hybrid vsock as the
            hardened multi-tenant tier. The contract is the only boundary to your
            agents.
          </SectionHeading>
        </Reveal>

        <div className="mt-10 grid gap-px overflow-hidden rounded-xl border bg-border sm:grid-cols-2 lg:grid-cols-3">
          {PLANES.map((p, i) => (
            <Reveal key={p.name} delay={(i % 3) * 0.05} className="bg-card h-full">
              <div className="h-full p-5">
                <div className="font-mono text-sm font-semibold">{p.name}</div>
                <p className="text-muted-foreground mt-2 text-sm leading-relaxed">
                  {p.role}
                </p>
              </div>
            </Reveal>
          ))}
        </div>

        <Reveal delay={0.1} className="mt-12">
          <h3 className="text-lg font-semibold tracking-tight">
            Wiring &amp; communication
          </h3>
          <p className="text-muted-foreground mt-2 mb-6 max-w-2xl text-sm">
            Each tab is one slice of the system — the component topology and the
            per-flow sequence diagrams, straight from the architecture docs.
          </p>
          <ArchitectureViewer />
        </Reveal>
      </div>
    </section>
  );
}
