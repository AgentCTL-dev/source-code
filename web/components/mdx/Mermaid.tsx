"use client";

import { useEffect, useId, useRef, useState } from "react";
import { useTheme } from "next-themes";

// Static-export-safe Mermaid: `mermaid` is dynamically imported in the browser
// (never bundled into the prerendered HTML) and re-renders when the theme flips.
// The remark transform in source.config.ts rewrites ```mermaid fences to this.
export function Mermaid({ chart }: { chart: string }) {
  const rawId = useId();
  const id = `mmd-${rawId.replace(/[^a-zA-Z0-9]/g, "")}`;
  const { resolvedTheme } = useTheme();
  const [svg, setSvg] = useState<string>("");
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;

    async function render() {
      const mermaid = (await import("mermaid")).default;
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        theme: resolvedTheme === "light" ? "default" : "dark",
        fontFamily: "inherit",
      });
      try {
        const { svg: out } = await mermaid.render(id, chart.trim());
        if (!cancelled) setSvg(out);
      } catch (err) {
        if (!cancelled) {
          setSvg(
            `<pre class="text-fd-muted-foreground text-xs whitespace-pre-wrap">${String(
              err,
            )}</pre>`,
          );
        }
      }
    }

    void render();
    return () => {
      cancelled = true;
    };
  }, [chart, id, resolvedTheme]);

  return (
    <div
      ref={containerRef}
      className="my-6 flex justify-center overflow-x-auto"
      // mermaid output is generated locally from trusted repo content
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  );
}

export default Mermaid;
