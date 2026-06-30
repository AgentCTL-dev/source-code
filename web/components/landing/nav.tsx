"use client";

import Link from "next/link";
import { GithubIcon, MenuIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetClose,
  SheetContent,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from "@/components/ui/sheet";
import { ThemeToggle } from "@/components/landing/theme-toggle";
import { GITHUB_URL } from "@/data/site";

const ANCHORS = [
  { href: "#how", label: "How it works" },
  { href: "#features", label: "Features" },
  { href: "#quickstart", label: "Quickstart" },
  { href: "#use-cases", label: "Use cases" },
];

export function Nav() {
  return (
    <header className="bg-background/70 supports-[backdrop-filter]:bg-background/55 sticky top-0 z-50 border-b backdrop-blur">
      <nav
        aria-label="Primary"
        className="mx-auto flex h-14 max-w-6xl items-center gap-6 px-4"
      >
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

        <div className="text-muted-foreground hidden items-center gap-5 text-sm md:flex">
          {ANCHORS.map((a) => (
            <a key={a.href} href={a.href} className="hover:text-foreground transition-colors">
              {a.label}
            </a>
          ))}
        </div>

        <div className="ml-auto flex items-center gap-1.5">
          <Button asChild variant="ghost" size="sm" className="hidden sm:inline-flex">
            <Link href="/docs">Docs</Link>
          </Button>
          <Button asChild variant="ghost" size="icon" aria-label="agentctl on GitHub">
            <a href={GITHUB_URL} target="_blank" rel="noreferrer noopener">
              <GithubIcon className="size-4" />
            </a>
          </Button>
          <ThemeToggle />

          {/* mobile menu */}
          <Sheet>
            <SheetTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                aria-label="Open menu"
                className="md:hidden"
              >
                <MenuIcon className="size-4" />
              </Button>
            </SheetTrigger>
            <SheetContent side="right" className="w-72">
              <SheetHeader>
                <SheetTitle>agentctl</SheetTitle>
              </SheetHeader>
              <nav className="flex flex-col gap-1 px-3 text-sm">
                {ANCHORS.map((a) => (
                  <SheetClose asChild key={a.href}>
                    <a
                      href={a.href}
                      className="hover:bg-accent rounded-md px-3 py-2 transition-colors"
                    >
                      {a.label}
                    </a>
                  </SheetClose>
                ))}
                <SheetClose asChild>
                  <Link
                    href="/docs"
                    className="hover:bg-accent rounded-md px-3 py-2 transition-colors"
                  >
                    Docs
                  </Link>
                </SheetClose>
                <SheetClose asChild>
                  <a
                    href={GITHUB_URL}
                    target="_blank"
                    rel="noreferrer noopener"
                    className="hover:bg-accent inline-flex items-center gap-2 rounded-md px-3 py-2 transition-colors"
                  >
                    <GithubIcon className="size-4" /> GitHub
                  </a>
                </SheetClose>
              </nav>
            </SheetContent>
          </Sheet>
        </div>
      </nav>
    </header>
  );
}
