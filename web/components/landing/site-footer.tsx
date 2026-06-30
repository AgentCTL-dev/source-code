import Link from "next/link";
import { GithubIcon } from "lucide-react";

import { CONTACT_EMAIL, GITHUB_URL } from "@/data/site";

const COLUMNS: { heading: string; links: { label: string; href: string; external?: boolean }[] }[] = [
  {
    heading: "Docs",
    links: [
      { label: "Overview", href: "/docs" },
      { label: "Architecture", href: "/docs/architecture" },
      { label: "Security", href: "/docs/security" },
      { label: "Operations", href: "/docs/operations" },
    ],
  },
  {
    heading: "Reference",
    links: [
      { label: "The contract", href: "/docs/contract" },
      { label: "Status", href: "/docs/status" },
      { label: "Roadmap", href: "/docs/roadmap" },
      { label: "RFCs", href: "/docs/rfcs" },
    ],
  },
  {
    heading: "Project",
    links: [
      { label: "GitHub", href: GITHUB_URL, external: true },
      { label: "Quickstart", href: "/#quickstart" },
      { label: "Use cases", href: "/#use-cases" },
    ],
  },
];

export function SiteFooter() {
  return (
    <footer className="bg-background">
      <div className="mx-auto max-w-6xl px-4 py-14">
        <div className="grid gap-10 sm:grid-cols-2 lg:grid-cols-4">
          <div>
            <Link
              href="/"
              className="flex items-center gap-2 font-semibold tracking-tight"
            >
              <span
                aria-hidden
                className="from-chart-1 to-chart-4 inline-block size-5 rounded-md bg-gradient-to-br"
              />
              agentctl
            </Link>
            <p className="text-muted-foreground mt-3 max-w-xs text-sm leading-relaxed">
              The Kubernetes control plane for fleets of conformant agents.
            </p>
            <a
              href={GITHUB_URL}
              target="_blank"
              rel="noreferrer noopener"
              aria-label="agentctl on GitHub"
              className="text-muted-foreground hover:text-foreground mt-4 inline-flex items-center gap-2 text-sm"
            >
              <GithubIcon className="size-4" /> agentctl-dev
            </a>
          </div>

          {COLUMNS.map((col) => (
            <div key={col.heading}>
              <div className="text-sm font-semibold">{col.heading}</div>
              <ul className="mt-3 space-y-2 text-sm">
                {col.links.map((l) => (
                  <li key={l.label}>
                    {l.external ? (
                      <a
                        href={l.href}
                        target="_blank"
                        rel="noreferrer noopener"
                        className="text-muted-foreground hover:text-foreground transition-colors"
                      >
                        {l.label}
                      </a>
                    ) : (
                      <Link
                        href={l.href}
                        className="text-muted-foreground hover:text-foreground transition-colors"
                      >
                        {l.label}
                      </Link>
                    )}
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>

        <div className="text-muted-foreground mt-12 flex flex-col gap-2 border-t pt-6 text-xs sm:flex-row sm:items-center sm:justify-between">
          <span>
            Apache-2.0 (contract + SDK) · BUSL-1.1 (control plane). The data plane
            is any conformant agent.
          </span>
          <a href={`mailto:${CONTACT_EMAIL}`} className="hover:text-foreground">
            Commercial licensing: {CONTACT_EMAIL}
          </a>
        </div>
      </div>
    </footer>
  );
}
