import Link from "next/link";
import { ArrowRightIcon, BookOpenIcon, GithubIcon } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Reveal } from "@/components/motion/reveal";
import { CodeBlock } from "@/components/landing/code-block";
import { GITHUB_URL } from "@/data/site";

// Real kind quickstart from README.md (Quickstart (kind)).
const KIND_QUICKSTART = `# 1. spin up a local cluster
kind create cluster --name agentctl

# 2. install the contract CRDs + control plane
cargo run -p agentctl-crdgen && kubectl apply -f deploy/crds/

# 3. provision your first agent
kubectl apply -f deploy/examples/agent-once.yaml
kubectl get agents`;

export function Hero() {
  return (
    <section className="bg-grid-fade relative overflow-hidden border-b">
      {/* ambient gradient wash behind the grid */}
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0 -z-10 bg-[radial-gradient(60%_50%_at_50%_-10%,color-mix(in_oklch,var(--color-chart-1)_22%,transparent),transparent_70%)]"
      />
      <div className="from-background pointer-events-none absolute inset-x-0 bottom-0 h-32 bg-gradient-to-t to-transparent" />

      <div className="relative mx-auto grid max-w-6xl items-center gap-12 px-4 py-20 lg:grid-cols-[1.05fr_0.95fr] lg:py-28">
        <Reveal>
          <Badge variant="secondary" className="mb-5 gap-2">
            <span className="bg-chart-2 size-1.5 rounded-full" />
            Kubernetes-native · contract-first · Rust
          </Badge>
          <h1 className="text-4xl font-bold tracking-tight text-balance sm:text-5xl lg:text-6xl">
            The Kubernetes control plane for fleets of{" "}
            <span className="text-gradient">conformant agents</span>
          </h1>
          <p className="text-muted-foreground mt-6 max-w-xl text-lg text-pretty">
            agentctl provisions, supplies intelligence to, scales, secures, and
            observes fleets of contract-conformant AI agents. A control plane
            that speaks the{" "}
            <span className="text-foreground font-medium">
              Agent Control Contract
            </span>{" "}
            — not a vendor.
          </p>
          <div className="mt-8 flex flex-wrap gap-3">
            <Button asChild size="lg">
              <Link href="/docs">
                Get started <ArrowRightIcon className="size-4" />
              </Link>
            </Button>
            <Button asChild size="lg" variant="outline">
              <a href="#quickstart">
                <BookOpenIcon className="size-4" /> Quickstart
              </a>
            </Button>
            <Button asChild size="lg" variant="ghost">
              <a href={GITHUB_URL} target="_blank" rel="noreferrer noopener">
                <GithubIcon className="size-4" /> GitHub
              </a>
            </Button>
          </div>
          <dl className="text-muted-foreground mt-10 flex flex-wrap gap-x-8 gap-y-3 text-sm">
            <div className="flex items-baseline gap-2">
              <dt className="text-foreground font-mono font-semibold">7</dt>
              <dd>control-plane components</dd>
            </div>
            <div className="flex items-baseline gap-2">
              <dt className="text-foreground font-mono font-semibold">~1.3 MB</dt>
              <dd>reference agent binary</dd>
            </div>
            <div className="flex items-baseline gap-2">
              <dt className="text-foreground font-mono font-semibold">default-off</dt>
              <dd>every capability gated</dd>
            </div>
          </dl>
        </Reveal>

        <Reveal delay={0.1}>
          <CodeBlock
            title="quickstart.sh"
            lang="bash"
            code={KIND_QUICKSTART}
            className="shadow-xl"
          />
        </Reveal>
      </div>
    </section>
  );
}
