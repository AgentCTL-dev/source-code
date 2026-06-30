import defaultMdxComponents from "fumadocs-ui/mdx";
import type { MDXComponents } from "mdx/types";
import { Mermaid } from "@/components/mdx/Mermaid";

// Components made available to every compiled MDX page. `Mermaid` is referenced
// by the remark transform in source.config.ts.
export function getMDXComponents(components?: MDXComponents): MDXComponents {
  return {
    ...defaultMdxComponents,
    Mermaid,
    ...components,
  };
}
