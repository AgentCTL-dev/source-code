import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

// Consistent eyebrow + title + lead for each landing section.
export function SectionHeading({
  eyebrow,
  title,
  children,
  align = "left",
  className,
}: {
  eyebrow?: string;
  title: ReactNode;
  children?: ReactNode;
  align?: "left" | "center";
  className?: string;
}) {
  return (
    <div
      className={cn(
        align === "center" && "mx-auto max-w-2xl text-center",
        className,
      )}
    >
      {eyebrow ? (
        <div className="text-chart-1 mb-3 font-mono text-xs tracking-widest uppercase">
          {eyebrow}
        </div>
      ) : null}
      <h2 className="text-2xl font-bold tracking-tight text-balance sm:text-3xl">
        {title}
      </h2>
      {children ? (
        <p className="text-muted-foreground mt-3 text-pretty">{children}</p>
      ) : null}
    </div>
  );
}
