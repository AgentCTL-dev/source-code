import { highlight } from "@/lib/highlight";
import { CodePanel } from "./code-panel";

// Async server wrapper: highlights at build time, then hands the HTML to the
// client CodePanel. Use anywhere in a server tree (e.g. the Hero terminal).
export async function CodeBlock({
  code,
  lang = "bash",
  title,
  showDots = true,
  className,
}: {
  code: string;
  lang?: string;
  title?: string;
  showDots?: boolean;
  className?: string;
}) {
  const html = await highlight(code, lang);
  return (
    <CodePanel
      html={html}
      raw={code}
      title={title}
      showDots={showDots}
      className={className}
    />
  );
}
