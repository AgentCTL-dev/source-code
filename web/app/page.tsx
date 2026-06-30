import { Nav } from "@/components/landing/nav";
import { Hero } from "@/components/landing/hero";
import { Problem } from "@/components/landing/problem";
import { HowItWorks } from "@/components/landing/how-it-works";
import { FeatureGrid } from "@/components/landing/feature-grid";
import { Quickstart } from "@/components/landing/quickstart";
import { UseCases } from "@/components/landing/use-cases";
import { Benchmarks } from "@/components/landing/benchmarks";
import { Principles } from "@/components/landing/principles";
import { FinalCTA } from "@/components/landing/final-cta";
import { SiteFooter } from "@/components/landing/site-footer";
import { BENCHMARKS_ENABLED } from "@/data/benchmarks";

export default function Home() {
  return (
    <>
      <a
        href="#main-content"
        className="focus:bg-background focus:ring-ring sr-only focus:not-sr-only focus:fixed focus:top-3 focus:left-3 focus:z-[100] focus:rounded-md focus:border focus:px-3 focus:py-2 focus:text-sm focus:ring-2"
      >
        Skip to content
      </a>

      <Nav />

      <main id="main-content" className="flex flex-1 flex-col">
        <Hero />
        <Problem />
        <HowItWorks />
        <FeatureGrid />
        <Quickstart />
        <UseCases />
        {BENCHMARKS_ENABLED ? <Benchmarks /> : null}
        <Principles />
        <FinalCTA />
      </main>

      <SiteFooter />
    </>
  );
}
