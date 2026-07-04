import type { ReactNode } from "react";
import { ExternalLink } from "lucide-react";
import { Nav } from "@/components/site/nav";
import { Footer } from "@/components/site/footer";
import { cn } from "@/lib/utils";

export function DocShell({
  title,
  lead,
  editHref,
  children,
}: {
  title: string;
  lead: string;
  editHref?: string;
  children: ReactNode;
}) {
  return (
    <>
      <Nav />
      <main className="mx-auto w-full max-w-3xl flex-1 px-4 py-14 sm:px-6">
        <header className="mb-10">
          <h1 className="text-3xl font-semibold tracking-tight sm:text-4xl">{title}</h1>
          <p className="text-muted-foreground mt-3 text-lg text-pretty">{lead}</p>
          {editHref ? (
            <a
              href={editHref}
              target="_blank"
              rel="noreferrer"
              className="text-muted-foreground hover:text-foreground mt-4 inline-flex items-center gap-1.5 font-mono text-xs transition"
            >
              source on GitHub <ExternalLink className="size-3" />
            </a>
          ) : null}
        </header>
        <div className="flex flex-col gap-5">{children}</div>
      </main>
      <Footer />
    </>
  );
}

/* -- prose primitives (hand-authored pages, no MDX) ------------------------ */

export function H2({ id, children }: { id?: string; children: ReactNode }) {
  return (
    <h2 id={id} className="mt-6 scroll-mt-20 text-xl font-semibold tracking-tight">
      {children}
    </h2>
  );
}

export function P({ children }: { children: ReactNode }) {
  return <p className="text-muted-foreground leading-relaxed">{children}</p>;
}

export function Ul({ children }: { children: ReactNode }) {
  return (
    <ul className="text-muted-foreground ml-5 flex list-disc flex-col gap-1.5 leading-relaxed">
      {children}
    </ul>
  );
}

export function C({ children }: { children: ReactNode }) {
  return (
    <code className="bg-muted/60 rounded px-1 py-0.5 font-mono text-[0.85em] break-words">{children}</code>
  );
}

export function Note({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div
      className={cn(
        "border-primary/40 bg-primary/5 text-muted-foreground rounded-lg border-l-2 px-4 py-3 text-sm leading-relaxed",
        className,
      )}
    >
      {children}
    </div>
  );
}
