"use client";

import { useState } from "react";
import Link from "next/link";
import { Github, Menu, X } from "lucide-react";
import { ThemeToggle } from "@/components/site/theme-toggle";
import { GITHUB_URL } from "@/data/site";

const LINKS = [
  { href: "/architecture/", label: "Architecture" },
  { href: "/contract/", label: "Contract" },
  { href: "/install/", label: "Install" },
];

export function Nav() {
  const [open, setOpen] = useState(false);
  return (
    <header className="border-border/60 bg-background/80 sticky top-0 z-50 border-b backdrop-blur">
      <div className="mx-auto flex h-14 max-w-6xl items-center gap-6 px-4 sm:px-6">
        <Link
          href="/"
          className="flex items-center gap-2 font-semibold tracking-tight"
          onClick={() => setOpen(false)}
        >
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
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            aria-label={open ? "Close menu" : "Open menu"}
            aria-expanded={open}
            className="text-muted-foreground hover:text-foreground hover:border-border inline-flex size-9 items-center justify-center rounded-md border border-transparent transition md:hidden"
          >
            {open ? <X className="size-5" /> : <Menu className="size-5" />}
          </button>
        </div>
      </div>
      {open ? (
        <nav className="border-border/60 bg-background/95 border-t px-4 py-2 backdrop-blur md:hidden">
          <ul className="flex flex-col">
            {LINKS.map((l) => (
              <li key={l.href}>
                <Link
                  href={l.href}
                  onClick={() => setOpen(false)}
                  className="text-muted-foreground hover:text-foreground block rounded-md px-2 py-2.5 text-sm transition"
                >
                  {l.label}
                </Link>
              </li>
            ))}
          </ul>
        </nav>
      ) : null}
    </header>
  );
}
