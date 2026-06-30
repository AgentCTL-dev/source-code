"use client";

import { useState } from "react";
import { CheckIcon, CopyIcon } from "lucide-react";

import { cn } from "@/lib/utils";

// Small clipboard island used inside otherwise-static code panels.
export function CopyButton({
  text,
  className,
}: {
  text: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1800);
    } catch {
      // clipboard may be unavailable (insecure context) — fail quietly.
    }
  }

  return (
    <button
      type="button"
      onClick={copy}
      aria-label={copied ? "Copied" : "Copy code"}
      className={cn(
        "text-muted-foreground hover:text-foreground hover:bg-accent focus-visible:ring-ring focus-visible:ring-2 inline-flex size-7 items-center justify-center rounded-md transition-colors outline-none",
        className,
      )}
    >
      {copied ? (
        <CheckIcon className="size-3.5 text-emerald-500" />
      ) : (
        <CopyIcon className="size-3.5" />
      )}
    </button>
  );
}
