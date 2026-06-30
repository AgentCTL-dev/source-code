import Link from "next/link";
import { ArrowRightIcon, GithubIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Reveal } from "@/components/motion/reveal";
import { GITHUB_URL } from "@/data/site";

export function FinalCTA() {
  return (
    <section className="bg-grid-fade relative overflow-hidden border-b">
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0 -z-10 bg-[radial-gradient(60%_60%_at_50%_120%,color-mix(in_oklch,var(--color-chart-1)_22%,transparent),transparent_70%)]"
      />
      <div className="mx-auto max-w-3xl px-4 py-24 text-center sm:py-28">
        <Reveal>
          <h2 className="text-3xl font-bold tracking-tight text-balance sm:text-4xl">
            Run fleets of agents the Kubernetes way
          </h2>
          <p className="text-muted-foreground mx-auto mt-4 max-w-xl text-pretty">
            Declarative, secretless, elastic, and observable — over a contract,
            not a vendor. Start with a kind cluster and the agentd reference agent.
          </p>
          <div className="mt-8 flex flex-wrap justify-center gap-3">
            <Button asChild size="lg">
              <Link href="/docs">
                Read the docs <ArrowRightIcon className="size-4" />
              </Link>
            </Button>
            <Button asChild size="lg" variant="outline">
              <a href={GITHUB_URL} target="_blank" rel="noreferrer noopener">
                <GithubIcon className="size-4" /> Star on GitHub
              </a>
            </Button>
          </div>
        </Reveal>
      </div>
    </section>
  );
}
