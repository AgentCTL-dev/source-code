import { BASE_PATH } from "@/lib/utils";

// Static Orama search. `output: 'export'` forbids dynamic route handlers, so the
// search database is pre-built by scripts/sync-docs.mjs into
// public/search-index.json and consumed entirely client-side by Fumadocs'
// built-in `type: 'static'` dialog. `api` is the URL the client fetches the
// exported index from, so it must carry the base path (a raw fetch, not a
// <Link>). Pass this to <RootProvider search={searchOptions} />.
export const SEARCH_INDEX_URL = `${BASE_PATH}/search-index.json`;

export const searchOptions = {
  options: {
    type: "static" as const,
    api: SEARCH_INDEX_URL,
  },
};
