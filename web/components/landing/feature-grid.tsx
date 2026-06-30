import Link from "next/link";
import { ArrowRightIcon } from "lucide-react";

import { Card } from "@/components/ui/card";
import { Reveal } from "@/components/motion/reveal";
import { SectionHeading } from "@/components/landing/section-heading";
import { FEATURES } from "@/data/features";

export function FeatureGrid() {
  return (
    <section id="features" className="scroll-mt-20 border-b">
      <div className="mx-auto max-w-6xl px-4 py-20 sm:py-24">
        <Reveal>
          <SectionHeading
            eyebrow="Capabilities"
            title="Every plane, gated default-off"
          >
            The reference agent runtime is{" "}
            <span className="text-foreground">agentd</span> — a ~1.3 MB static
            binary and the worked example throughout the docs. Each capability is
            opt-in and independently auditable.
          </SectionHeading>
        </Reveal>

        <div className="mt-10 grid gap-4 md:grid-cols-2 lg:grid-cols-3">
          {FEATURES.map((f, i) => {
            const Icon = f.icon;
            return (
              <Reveal key={f.tag} delay={(i % 3) * 0.06}>
                <Card className="group hover:border-foreground/20 relative h-full gap-4 p-6 transition-colors">
                  <div className="bg-muted text-foreground ring-border flex size-10 items-center justify-center rounded-lg ring-1">
                    <Icon className="size-5" />
                  </div>
                  <div className="text-chart-1 font-mono text-xs">{f.tag}</div>
                  <h3 className="-mt-2 text-lg font-semibold tracking-tight">
                    {f.title}
                  </h3>
                  <p className="text-muted-foreground text-sm leading-relaxed">
                    {f.body}
                  </p>
                  <Link
                    href={f.href}
                    className="text-foreground/80 hover:text-foreground mt-auto inline-flex items-center gap-1 text-sm font-medium"
                  >
                    {f.linkLabel}
                    <ArrowRightIcon className="size-3.5 transition-transform group-hover:translate-x-0.5" />
                  </Link>
                </Card>
              </Reveal>
            );
          })}
        </div>
      </div>
    </section>
  );
}
