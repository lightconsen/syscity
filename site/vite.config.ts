import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Relative base so assets resolve under any deployment root: the custom
// domain (https://syscity.net/, served at "/") and the project site
// (https://lightconsen.github.io/syscity/). The site is a single static
// landing page with no client-side routing, so relative paths are safe.
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: {
    // The bundle was being downlevelled for browsers this project does not
    // target: the trace flagged 10.6 KB of polyfills and syntax transforms a
    // modern engine never needs. `es2022` keeps optional chaining, class
    // fields and async iteration as written, and still covers every browser
    // that supports the features the page itself uses (WebP/AVIF, `<video>`
    // autoplay, `matchMedia`).
    target: "es2022",
    // One stylesheet for a one-page site: `postbuild.mjs` inlines it, which it
    // can only do if Vite does not split it per-chunk.
    cssCodeSplit: false,
  },
});
