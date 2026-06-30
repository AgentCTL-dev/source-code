import { Reveal } from "@/components/motion/reveal";
import { SectionHeading } from "@/components/landing/section-heading";
import { CountUp } from "@/components/landing/count-up";
import { BENCHMARKS, BENCHMARKS_CAVEAT } from "@/data/benchmarks";

export function Benchmarks() {
  return (
    <section id="benchmarks" className="scroll-mt-20 border-b">
      <div className="mx-auto max-w-6xl px-4 py-20 sm:py-24">
        <Reveal>
          <SectionHeading
            eyebrow="Benchmarks"
            title="Verified on the e2e + scale harness"
            align="center"
          >
            Coordination correctness under contention and elastic scale-from-zero,
            driven by the agentd v1.0.0 reference agent.
          </SectionHeading>
        </Reveal>

        <div className="mt-12 grid grid-cols-2 gap-px overflow-hidden rounded-xl border bg-border lg:grid-cols-4">
          {BENCHMARKS.map((s, i) => (
            <Reveal key={s.label} delay={(i % 4) * 0.06} className="bg-card">
              <div className="flex h-full flex-col items-center p-6 text-center sm:p-8">
                <div className="text-gradient text-4xl font-bold tracking-tight tabular-nums sm:text-5xl">
                  {s.display ? (
                    s.display
                  ) : (
                    <CountUp
                      value={s.value ?? 0}
                      decimals={s.decimals}
                      prefix={s.prefix}
                      suffix={s.suffix}
                    />
                  )}
                </div>
                <div className="text-foreground mt-3 text-sm font-medium">
                  {s.label}
                </div>
                <p className="text-muted-foreground mt-1.5 text-xs leading-relaxed">
                  {s.detail}
                </p>
              </div>
            </Reveal>
          ))}
        </div>

        <Reveal delay={0.1}>
          <p className="text-muted-foreground mx-auto mt-8 max-w-3xl text-center text-xs leading-relaxed text-balance">
            {BENCHMARKS_CAVEAT}
          </p>
        </Reveal>
      </div>
    </section>
  );
}
