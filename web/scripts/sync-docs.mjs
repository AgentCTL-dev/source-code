// Single-source the site's docs from the repository's authoritative markdown.
//
// Mirrors ../docs, ../contract, ../rfcs into web/content/docs/(generated)/*.md
// (a gitignored Fumadocs route group — the "(generated)" segment is dropped from
// the URL, so a file maps to /docs/<slug>). Each file gets injected frontmatter,
// inter-doc links rewritten to on-site routes (everything else → GitHub), and
// ```mermaid fences left intact (source.config.ts rewrites them to <Mermaid/>).
// Files are written as CommonMark .md so MDX never tries to parse stray <…>/{…}.
//
// It then builds the static Orama search index (public/search-index.json) with
// Fumadocs' own search server, because `output: 'export'` forbids dynamic route
// handlers. Ported from the agentd-dev/web precedent (lib/docs.js).

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createSearchAPI } from "fumadocs-core/search/server";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const WEB_ROOT = path.resolve(__dirname, "..");
const REPO_ROOT = path.resolve(WEB_ROOT, "..");
const OUT_DIR = path.join(WEB_ROOT, "content", "docs", "(generated)");
const SEARCH_OUT = path.join(WEB_ROOT, "public", "search-index.json");
const GITHUB_BLOB = "https://github.com/agentctl-dev/source-code/blob/main";

// ── manifest ────────────────────────────────────────────────────────────────
// Static entries (repo-relative source path → slug + title + description).
const STATIC_DOCS = [
  { src: "docs/architecture.md", slug: "architecture", title: "Architecture & wiring", description: "The agentctl planes and how they fit together." },
  { src: "docs/security.md", slug: "security", title: "Security & auth model", description: "mTLS, OIDC, attested identities, trusted-proxy, NetworkPolicies." },
  { src: "docs/operations.md", slug: "operations", title: "Operations runbook", description: "Day-2 operations for the agentctl control plane." },
  { src: "docs/cloud-native-roadmap.md", slug: "roadmap", title: "Cloud-native roadmap", description: "The cloud-native productization roadmap." },
  { src: "docs/STATUS.md", slug: "status", title: "Project status", description: "What is built and verified today." },
  { src: "contract/README.md", slug: "contract", title: "Agent Control Contract", description: "The contract the control plane speaks (ACC v1)." },
  { src: "contract/SPEC.md", slug: "contract-spec", title: "ACC specification", description: "The Agent Control Contract specification." },
  { src: "rfcs/README.md", slug: "rfcs", title: "RFC index", description: "Index of agentctl RFCs." },
];

// RFC files (rfcs/NNNN-*.md) → rfc-NNNN, title derived from the filename.
function titleCase(s) {
  return s.replace(/\b\w/g, (c) => c.toUpperCase());
}
function discoverRfcs() {
  const dir = path.join(REPO_ROOT, "rfcs");
  if (!fs.existsSync(dir)) return [];
  return fs
    .readdirSync(dir)
    .filter((f) => /^\d{4}-.*\.md$/.test(f))
    .sort()
    .map((f) => {
      const num = f.slice(0, 4);
      const rest = f.replace(/^\d{4}-/, "").replace(/\.md$/, "").replace(/-/g, " ");
      return {
        src: `rfcs/${f}`,
        slug: `rfc-${num}`,
        title: `RFC ${num} · ${titleCase(rest)}`,
        description: `RFC ${num}: ${rest}.`,
      };
    });
}

const MANIFEST = [...STATIC_DOCS, ...discoverRfcs()].filter((d) =>
  fs.existsSync(path.join(REPO_ROOT, d.src)),
);

// path → slug (exact) and basename → slug (fallback), for link rewriting.
const PATH_TO_SLUG = new Map();
const BASENAME_TO_SLUG = new Map();
for (const d of MANIFEST) {
  PATH_TO_SLUG.set(d.src, d.slug);
  BASENAME_TO_SLUG.set(path.posix.basename(d.src), d.slug);
}

// ── link rewriting ──────────────────────────────────────────────────────────
function resolveRepoPath(currentSrc, href) {
  if (href.startsWith("/")) return href.slice(1);
  const dir = path.posix.dirname(currentSrc);
  return path.posix.normalize(path.posix.join(dir, href)).replace(/^\.\//, "");
}

function rewriteHref(href, currentSrc) {
  if (/^(https?:|mailto:|tel:|#)/i.test(href)) return href;
  const [rawPath, hash] = href.split("#");
  if (!rawPath) return href;
  const resolved = resolveRepoPath(currentSrc, rawPath);
  const suffix = hash ? `#${hash}` : "";
  let slug =
    PATH_TO_SLUG.get(resolved) ?? PATH_TO_SLUG.get(resolved.toLowerCase());
  if (!slug) {
    const base = path.posix.basename(resolved);
    slug = BASENAME_TO_SLUG.get(base) ?? BASENAME_TO_SLUG.get(base.toLowerCase());
  }
  if (slug) return `/docs/${slug}${suffix}`;
  return `${GITHUB_BLOB}/${resolved}${suffix}`;
}

// Rewrite [text](href) links, skipping fenced code blocks (literal content).
const LINK_RE = /(\]\()([^)\s]+)(\))/g;
function rewriteLinks(body, currentSrc) {
  const lines = body.split("\n");
  let inFence = false;
  return lines
    .map((line) => {
      const fence = line.match(/^\s*(```|~~~)/);
      if (fence) {
        inFence = !inFence;
        return line;
      }
      if (inFence) return line;
      return line.replace(LINK_RE, (_m, open, href, close) => {
        return `${open}${rewriteHref(href, currentSrc)}${close}`;
      });
    })
    .join("\n");
}

// ── content shaping ─────────────────────────────────────────────────────────
function stripLeadingFrontmatter(raw) {
  if (raw.startsWith("---\n")) {
    const end = raw.indexOf("\n---", 4);
    if (end !== -1) {
      const after = raw.indexOf("\n", end + 1);
      return raw.slice(after + 1);
    }
  }
  return raw;
}

function dropFirstH1(body) {
  const lines = body.split("\n");
  const idx = lines.findIndex((l) => /^#\s+/.test(l));
  if (idx !== -1 && idx < 5) lines.splice(idx, 1);
  return lines.join("\n").replace(/^\n+/, "");
}

function yamlQuote(s) {
  return `"${String(s).replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

// Plain-text extraction for the search index.
function toPlainText(body) {
  return body
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/~~~[\s\S]*?~~~/g, " ")
    .replace(/`([^`]+)`/g, "$1")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/^#{1,6}\s+/gm, "")
    .replace(/[*_>#|]/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 12000);
}

// ── committed (hand-authored) pages → search index ──────────────────────────
// The (generated)/ mirror is indexed from MANIFEST above; the IA pages under
// content/docs/** (committed .mdx, never in the route group) are indexed here so
// static search covers every page in the tree, not just the synced deep content.
const DOCS_ROOT = path.join(WEB_ROOT, "content", "docs");

function parseFrontmatterField(raw, field) {
  if (!raw.startsWith("---\n")) return undefined;
  const end = raw.indexOf("\n---", 4);
  if (end === -1) return undefined;
  const fm = raw.slice(4, end);
  const m = fm.match(new RegExp(`^${field}:\\s*(.+)$`, "m"));
  if (!m) return undefined;
  let v = m[1].trim();
  if (
    (v.startsWith('"') && v.endsWith('"')) ||
    (v.startsWith("'") && v.endsWith("'"))
  ) {
    v = v.slice(1, -1);
  }
  return v;
}

// Map a file path (relative to content/docs, sans extension) to its /docs URL,
// dropping a trailing `index` segment (folder index pages).
function pageUrlFromRel(relNoExt) {
  const parts = relNoExt.split(path.sep).filter(Boolean);
  if (parts[parts.length - 1] === "index") parts.pop();
  return parts.length ? `/docs/${parts.join("/")}` : "/docs";
}

function collectAuthoredIndexes() {
  const out = [];
  function walk(dir) {
    for (const name of fs.readdirSync(dir)) {
      const full = path.join(dir, name);
      const stat = fs.statSync(full);
      if (stat.isDirectory()) {
        // skip the gitignored (generated)/ route group — indexed via MANIFEST.
        if (name === "(generated)") continue;
        walk(full);
        continue;
      }
      if (!/\.mdx?$/.test(name)) continue;
      const raw = fs.readFileSync(full, "utf8");
      const rel = path.relative(DOCS_ROOT, full).replace(/\.mdx?$/, "");
      const url = pageUrlFromRel(rel);
      const title = parseFrontmatterField(raw, "title") ?? path.basename(rel);
      const description = parseFrontmatterField(raw, "description") ?? "";
      const body = dropFirstH1(stripLeadingFrontmatter(raw));
      out.push({ title, description, content: toPlainText(body), url });
    }
  }
  walk(DOCS_ROOT);
  return out;
}

// ── run ─────────────────────────────────────────────────────────────────────
function syncDocs() {
  fs.rmSync(OUT_DIR, { recursive: true, force: true });
  fs.mkdirSync(OUT_DIR, { recursive: true });

  const indexes = [];

  for (const entry of MANIFEST) {
    const raw = fs.readFileSync(path.join(REPO_ROOT, entry.src), "utf8");
    let body = stripLeadingFrontmatter(raw);
    body = dropFirstH1(body);
    body = rewriteLinks(body, entry.src);

    const frontmatter =
      `---\n` +
      `title: ${yamlQuote(entry.title)}\n` +
      `description: ${yamlQuote(entry.description)}\n` +
      `---\n\n`;

    fs.writeFileSync(path.join(OUT_DIR, `${entry.slug}.md`), frontmatter + body);

    indexes.push({
      title: entry.title,
      description: entry.description,
      content: toPlainText(body),
      url: `/docs/${entry.slug}`,
    });
  }

  // Include every committed hand-authored IA page (index + the new tree) so
  // static search covers the whole docs site, not just the synced mirror.
  indexes.push(...collectAuthoredIndexes());

  return indexes;
}

async function buildSearchIndex(indexes) {
  const server = createSearchAPI("simple", { indexes });
  const exported = await server.export();
  fs.mkdirSync(path.dirname(SEARCH_OUT), { recursive: true });
  fs.writeFileSync(SEARCH_OUT, JSON.stringify(exported));
}

async function main() {
  const indexes = syncDocs();
  await buildSearchIndex(indexes);
  console.log(
    `[sync-docs] mirrored ${MANIFEST.length} docs → ${path.relative(
      WEB_ROOT,
      OUT_DIR,
    )} and wrote ${path.relative(WEB_ROOT, SEARCH_OUT)} (${indexes.length} entries)`,
  );
}

main().catch((err) => {
  console.error("[sync-docs] failed:", err);
  process.exit(1);
});
