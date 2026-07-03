"use client";

import { Check, Copy } from "lucide-react";
import { useState } from "react";
import { cn } from "@/lib/utils";

export function CodeBlock({
  code,
  lang,
  className,
}: {
  code: string;
  lang?: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable */
    }
  };
  return (
    <div
      className={cn(
        "group border-border/60 bg-muted/40 relative overflow-hidden rounded-lg border",
        className,
      )}
    >
      {lang ? (
        <div className="text-muted-foreground border-border/60 border-b px-4 py-1.5 font-mono text-xs">
          {lang}
        </div>
      ) : null}
      <button
        type="button"
        onClick={copy}
        aria-label="Copy code"
        className="text-muted-foreground hover:text-foreground hover:bg-background/80 absolute top-2 right-2 rounded-md border border-transparent p-1.5 opacity-0 transition group-hover:opacity-100 focus-visible:opacity-100"
      >
        {copied ? (
          <Check className="size-3.5 text-emerald-400" />
        ) : (
          <Copy className="size-3.5" />
        )}
      </button>
      <pre className="overflow-x-auto px-4 py-3 text-[13px] leading-relaxed">
        <code className="font-mono">{code}</code>
      </pre>
    </div>
  );
}
