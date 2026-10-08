/**
 * Server entry, used only at build time by `scripts/postbuild.mjs`.
 *
 * The landing page is entirely static — no data fetching, no routing — so
 * prerendering it is a single `renderToString` call. That turns the deployed
 * `index.html` from an empty shell into the real page: text present before any
 * JavaScript runs (which is what search engines and slow connections see), and
 * a first paint that no longer waits on the bundle.
 */
import { renderToString } from "react-dom/server";
import App from "./App";

export function render(): string {
  return renderToString(<App />);
}
