import { defineDocs, defineConfig } from "fumadocs-mdx/config";
import remarkGfm from "remark-gfm";
import { visit } from "unist-util-visit";

// Single source of truth: content/docs holds hand-authored IA pages (committed)
// plus the gitignored (generated)/ mirror of ../docs, ../contract, ../rfcs that
// scripts/sync-docs.mjs writes as CommonMark .md (parsed literally — MDX-safe).
export const docs = defineDocs({
  dir: "content/docs",
});

// Turn ```mermaid fenced code blocks into <Mermaid chart="..." /> MDX nodes so
// they render through the static-export-safe client component (see
// components/mdx/Mermaid.tsx) instead of rehype-mermaid, which needs a headless
// browser and breaks `output: 'export'`.
function remarkMermaid() {
  return (tree: unknown) => {
    visit(tree as never, "code", (node: any, index: any, parent: any) => {
      if (!parent || index == null || node.lang !== "mermaid") return;
      parent.children[index] = {
        type: "mdxJsxFlowElement",
        name: "Mermaid",
        attributes: [
          { type: "mdxJsxAttribute", name: "chart", value: node.value },
        ],
        children: [],
      };
    });
  };
}

export default defineConfig({
  mdxOptions: {
    // preset 'fumadocs' (default) already wires rehype-code (shiki) + structure;
    // we prepend gfm and the mermaid transform.
    remarkPlugins: (v) => [remarkGfm, remarkMermaid, ...v],
  },
});
