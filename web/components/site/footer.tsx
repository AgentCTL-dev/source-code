import Link from "next/link";
import { GITHUB_URL, REPO_RFCS, REPO_DOCS, CONTRACT_VERSION } from "@/data/site";

export function Footer() {
  return (
    <footer className="border-border/60 border-t">
      <div className="text-muted-foreground mx-auto flex max-w-6xl flex-col gap-4 px-4 py-8 text-sm sm:flex-row sm:items-center sm:justify-between sm:px-6">
        <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
          <Link href="/architecture/" className="hover:text-foreground transition">
            Architecture
          </Link>
          <Link href="/contract/" className="hover:text-foreground transition">
            Contract
          </Link>
          <Link href="/install/" className="hover:text-foreground transition">
            Install
          </Link>
          <a href={REPO_DOCS} className="hover:text-foreground transition" target="_blank" rel="noreferrer">
            Docs
          </a>
          <a href={REPO_RFCS} className="hover:text-foreground transition" target="_blank" rel="noreferrer">
            RFCs
          </a>
          <a href={GITHUB_URL} className="hover:text-foreground transition" target="_blank" rel="noreferrer">
            GitHub
          </a>
        </div>
        <p className="font-mono text-xs">
          contract {CONTRACT_VERSION} · control plane BUSL-1.1 · contract + SDK Apache-2.0
        </p>
      </div>
    </footer>
  );
}
