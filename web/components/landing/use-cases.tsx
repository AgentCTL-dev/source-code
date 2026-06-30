import { CheckIcon } from "lucide-react";

import { Card } from "@/components/ui/card";
import { Reveal } from "@/components/motion/reveal";
import { SectionHeading } from "@/components/landing/section-heading";
import { USE_CASES } from "@/data/usecases";

export function UseCases() {
  return (
    <section id="use-cases" className="scroll-mt-20 border-b">
      <div className="mx-auto max-w-6xl px-4 py-20 sm:py-24">
        <Reveal>
          <SectionHeading eyebrow="Use cases" title="What teams build on it">
            One control plane, many shapes of agent workload — all over the same
            contract.
          </SectionHeading>
        </Reveal>

        <div className="mt-10 grid gap-4 md:grid-cols-2 lg:grid-cols-3">
          {USE_CASES.map((u, i) => {
            const Icon = u.icon;
            return (
              <Reveal key={u.title} delay={(i % 3) * 0.06}>
                <Card className="h-full gap-4 p-6">
                  <div className="flex items-center gap-3">
                    <div className="bg-muted text-foreground ring-border flex size-9 items-center justify-center rounded-lg ring-1">
                      <Icon className="size-4.5" />
                    </div>
                    <h3 className="text-base font-semibold tracking-tight">
                      {u.title}
                    </h3>
                  </div>
                  <p className="text-muted-foreground text-sm leading-relaxed">
                    {u.body}
                  </p>
                  <ul className="mt-1 space-y-2 text-sm">
                    {u.points.map((p) => (
                      <li key={p} className="flex gap-2">
                        <CheckIcon className="text-chart-2 mt-0.5 size-4 shrink-0" />
                        <span className="text-muted-foreground">{p}</span>
                      </li>
                    ))}
                  </ul>
                </Card>
              </Reveal>
            );
          })}
        </div>
      </div>
    </section>
  );
}
