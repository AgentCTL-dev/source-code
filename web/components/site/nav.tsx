import Link from "next/link";
import { Github } from "lucide-react";
import { ThemeToggle } from "@/components/site/theme-toggle";
import { GITHUB_URL } from "@/data/site";

const LINKS = [
  { href: "/architecture/", label: "Architecture" },
  { href: "/contract/", label: "Contract" },
  { href: "/install/", label: "Install" },
];

export function Nav() {
  return (
    <header className="border-border/60 bg-background/80 sticky top-0 z-50 border-b backdrop-blur">
      <div className="mx-auto flex h-14 max-w-6xl items-center gap-6 px-4 sm:px-6">
        <Link href="/" className="flex items-center gap-2 font-semibold tracking-tight">
          <span className="bg-foreground text-background grid size-6 place-items-center rounded font-mono text-xs">
            a
          </span>
          agentctl
        </Link>
        <nav className="text-muted-foreground hidden items-center gap-5 text-sm md:flex">
          {LINKS.map((l) => (
            <Link key={l.href} href={l.href} className="hover:text-foreground transition">
              {l.label}
            </Link>
          ))}
        </nav>
        <div className="ml-auto flex items-center gap-1">
          <ThemeToggle />
          <a
            href={GITHUB_URL}
            target="_blank"
            rel="noreferrer"
            aria-label="GitHub"
            className="text-muted-foreground hover:text-foreground hover:border-border inline-flex size-9 items-center justify-center rounded-md border border-transparent transition"
          >
            <Github className="size-4" />
          </a>
        </div>
      </div>
    </header>
  );
}
