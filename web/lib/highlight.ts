import { codeToHtml } from "shiki";

// Build-time syntax highlighting for the landing code blocks. Dual-theme output
// (defaultColor:false) emits --shiki-light / --shiki-dark CSS variables per token;
// globals.css picks the right one off the `.dark` class so highlighted code flips
// with the theme toggle without re-running Shiki in the browser. Safe under
// `output: 'export'` because it runs on the server at build time.
export async function highlight(code: string, lang: string): Promise<string> {
  return codeToHtml(code, {
    lang,
    themes: { light: "github-light", dark: "github-dark-default" },
    defaultColor: false,
  });
}
