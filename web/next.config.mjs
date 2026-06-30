import { createMDX } from "fumadocs-mdx/next";

// Single base-path knob: '/source-code' for GitHub project pages, '' for an apex
// domain. Drives basePath/assetPrefix and is exposed to the client as
// NEXT_PUBLIC_BASE_PATH (used by the static search index fetch + Mermaid).
const base = process.env.PAGES_BASE_PATH ?? "";

/** @type {import('next').NextConfig} */
const nextConfig = {
  output: "export",
  basePath: base,
  assetPrefix: base,
  trailingSlash: true,
  images: { unoptimized: true },
  env: { NEXT_PUBLIC_BASE_PATH: base },
  reactStrictMode: true,
};

const withMDX = createMDX();

export default withMDX(nextConfig);
