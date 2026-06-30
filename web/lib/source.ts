import { docs } from "@/.source";
import { loader } from "fumadocs-core/source";

// The fumadocs loader: turns the generated .source collection into a page tree
// + page accessors, all rooted at /docs.
export const source = loader({
  baseUrl: "/docs",
  source: docs.toFumadocsSource(),
});
