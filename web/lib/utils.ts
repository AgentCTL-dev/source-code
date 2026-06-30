import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

// Base path knob, inlined at build time. Empty for an apex domain, '/source-code'
// for GitHub project pages. Use only for raw asset URLs (fetch/img/src) — Next's
// <Link>/next-image already prefix basePath automatically.
export const BASE_PATH = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

export function withBase(path: string) {
  if (!path.startsWith("/")) return path;
  return `${BASE_PATH}${path}`;
}
