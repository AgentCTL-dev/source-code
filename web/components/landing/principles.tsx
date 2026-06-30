import {
  FileCode2Icon,
  LayersIcon,
  ScaleIcon,
  ShieldCheckIcon,
  type LucideIcon,
} from "lucide-react";

import { Reveal } from "@/components/motion/reveal";
import { SectionHeading } from "@/components/landing/section-heading";
import { LICENSE_CHANGE_DATE } from "@/data/site";

const PRINCIPLES: { icon: LucideIcon; title: string; body: string }[] = [
  {
    icon: ShieldCheckIcon,
    title: "Contract, not a vendor (P0)",
    body: "The data plane is any agent that speaks the Agent Control Contract. agentctl never depends on a specific agent — agentd is the reference implementation, not a dependency.",
  },
  {
    icon: LayersIcon,
    title: "Tiered substrate",
    body: "A stock-unix hostPath socket + DaemonSet is the portable default; a Kata-hybrid vsock tier is the hardened option for hostile multi-tenancy. Pick isolation per workload.",
  },
  {
    icon: FileCode2Icon,
    title: "Contract-as-schema",
    body: "Anti-drift by construction: JSON Schemas + codegen + behavioral conformance fixtures, not a shared runtime crate. The standard and its SDK are open so any vendor can build on them.",
  },
  {
    icon: ScaleIcon,
    title: "Source-available license",
    body: `The contract + SDK are Apache-2.0. The runnable control plane is BUSL-1.1 — free for non-production and internal non-commercial use; each version converts to Apache-2.0 on the Change Date (${LICENSE_CHANGE_DATE}).`,
  },
];

export function Principles() {
  return (
    <section id="principles" className="scroll-mt-20 border-b">
      <div className="mx-auto max-w-6xl px-4 py-20 sm:py-24">
        <Reveal>
          <SectionHeading eyebrow="Principles" title="The decisions it is built on">
            Four load-bearing choices that keep agentctl vendor-neutral, isolatable,
            drift-proof, and open.
          </SectionHeading>
        </Reveal>

        <div className="mt-10 grid gap-4 sm:grid-cols-2">
          {PRINCIPLES.map((p, i) => {
            const Icon = p.icon;
            return (
              <Reveal key={p.title} delay={(i % 2) * 0.06}>
                <div className="bg-card flex h-full gap-4 rounded-xl border p-6">
                  <div className="bg-muted text-foreground ring-border flex size-10 shrink-0 items-center justify-center rounded-lg ring-1">
                    <Icon className="size-5" />
                  </div>
                  <div>
                    <h3 className="font-semibold tracking-tight">{p.title}</h3>
                    <p className="text-muted-foreground mt-1.5 text-sm leading-relaxed">
                      {p.body}
                    </p>
                  </div>
                </div>
              </Reveal>
            );
          })}
        </div>
      </div>
    </section>
  );
}
