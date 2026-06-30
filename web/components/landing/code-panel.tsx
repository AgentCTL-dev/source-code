"use client";

import { TerminalIcon } from "lucide-react";

import { cn } from "@/lib/utils";
import { CopyButton } from "./copy-button";

// Presentational code card: a faux-terminal title bar (traffic-light dots + an
// optional title) over a pre-highlighted Shiki block, with a copy island. `html`
// is built at build time (lib/highlight); `raw` is what the copy button writes.
export function CodePanel({
  html,
  raw,
  title,
  showDots = true,
  className,
}: {
  html: string;
  raw: string;
  title?: string;
  showDots?: boolean;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "bg-card/70 supports-[backdrop-filter]:bg-card/60 overflow-hidden rounded-xl border shadow-lg backdrop-blur",
        className,
      )}
    >
      <div className="bg-muted/40 text-muted-foreground flex items-center gap-2 border-b px-4 py-2.5 text-xs">
        {showDots ? (
          <span className="flex gap-1.5" aria-hidden>
            <span className="size-2.5 rounded-full bg-red-400/70" />
            <span className="size-2.5 rounded-full bg-amber-400/70" />
            <span className="size-2.5 rounded-full bg-emerald-400/70" />
          </span>
        ) : (
          <TerminalIcon className="size-3.5" />
        )}
        {title ? (
          <span className="ml-1 truncate font-mono">{title}</span>
        ) : null}
        <CopyButton text={raw} className="ml-auto" />
      </div>
      <div
        className="overflow-x-auto p-4 text-[13px] leading-relaxed [&_pre]:!bg-transparent"
        // html is produced at build time by Shiki from trusted repo content.
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </div>
  );
}
