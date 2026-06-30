import { Reveal } from "@/components/motion/reveal";

// P0 from README.md — depend on the contract, never on a specific agent.
export function Problem() {
  return (
    <section className="border-b">
      <div className="mx-auto max-w-5xl px-4 py-20 sm:py-24">
        <Reveal>
          <div className="text-chart-1 mb-4 font-mono text-xs tracking-widest uppercase">
            Principle P0
          </div>
          <blockquote className="text-2xl leading-snug font-medium text-balance sm:text-3xl sm:leading-snug">
            Depend on the{" "}
            <span className="text-gradient">contract</span>, never on a specific
            agent.
          </blockquote>
          <div className="text-muted-foreground mt-6 grid gap-6 text-base leading-relaxed sm:grid-cols-2">
            <p>
              The data plane is <em>any</em> agent that conforms to the{" "}
              <span className="text-foreground font-medium">
                Agent Control Contract
              </span>
              : a capabilities manifest, a management MCP profile, a frozen
              metrics &amp; exit-code contract, a config schema, A2A over the
              substrate, and a downward-API env convention.
            </p>
            <p>
              <span className="text-foreground font-medium">agentd</span> is the
              reference agent — the worked example throughout these docs — but it
              is an implementation, not a dependency. Build your own agent against
              the contract and it wires into every plane identically. agentctl is
              Rust-only and vendor-neutral by construction.
            </p>
          </div>
        </Reveal>
      </div>
    </section>
  );
}
