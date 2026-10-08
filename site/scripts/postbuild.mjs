/**
 * Post-build: prerender the page into `dist/index.html` and inline its CSS.
 *
 * Both steps exist to remove round trips from the critical path of a page
 * that is, in the end, static:
 *
 * 1. **Prerender** — the client build leaves `<div id="root"></div>`, so the
 *    deployed HTML contains no text at all: a visitor (or a crawler) sees
 *    nothing until the ~80 KB bundle downloads, parses and runs. Rendering the
 *    same React tree to a string here puts the real content in the file.
 * 2. **Inline CSS** — the stylesheet is one 5.7 KB (gzip) file, and a
 *    `<link>` to it blocks the first paint for a full round trip. Inlined, the
 *    page is paintable the moment the HTML lands.
 *
 * Run by `pnpm build` after `vite build`.
 */
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const DIST = "dist";
const SSR_DIST = "dist-ssr";
const INDEX = join(DIST, "index.html");

// ── 1. Build the server bundle and render the page ────────────────────────
execFileSync(
  "npx",
  ["vite", "build", "--ssr", "src/entry-server.tsx", "--outDir", SSR_DIST],
  { stdio: "inherit" },
);

const { render } = await import(`../${SSR_DIST}/entry-server.js`);
const appHtml = render();

let html = readFileSync(INDEX, "utf8");

const placeholder = '<div id="root"></div>';
if (!html.includes(placeholder)) {
  throw new Error(
    `expected ${placeholder} in ${INDEX} — the prerender would silently do nothing`,
  );
}
html = html.replace(placeholder, `<div id="root">${appHtml}</div>`);

// ── 2. Inline the stylesheet ──────────────────────────────────────────────
// `cssCodeSplit: false` in vite.config.ts means the whole page has exactly one
// stylesheet, wherever Vite decided to name it (`style-<hash>.css` in
// practice). Anything other than exactly one is a configuration change this
// script must not guess about.
const cssNames = readdirSync(join(DIST, "assets")).filter((n) => n.endsWith(".css"));
if (cssNames.length !== 1) {
  throw new Error(
    `expected exactly one stylesheet to inline, found ${cssNames.length}: ${cssNames.join(", ")}`,
  );
}
const cssFile = cssNames[0];
const css = readFileSync(join(DIST, "assets", cssFile), "utf8");

const linkTag = new RegExp(
  `<link[^>]*rel="stylesheet"[^>]*href="[^"]*${cssFile}"[^>]*>`,
);
if (!linkTag.test(html)) {
  throw new Error(`no <link> for ${cssFile} in ${INDEX} — nothing to inline`);
}
html = html.replace(linkTag, `<style>${css}</style>`);

// The inlined stylesheet is referenced by nothing now; leaving it would ship
// the same bytes twice and advertise a file no visitor requests.
rmSync(join(DIST, "assets", cssFile));

writeFileSync(INDEX, html);
rmSync(SSR_DIST, { recursive: true, force: true });

const kb = (bytes) => (bytes / 1024).toFixed(1);
console.log(
  `prerendered ${kb(appHtml.length)} kB of markup; inlined ${kb(css.length)} kB of CSS`,
);
