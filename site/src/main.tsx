import { StrictMode } from "react";
import { createRoot, hydrateRoot } from "react-dom/client";
import App from "./App";
import "./index.css";

// The production HTML is prerendered (see `scripts/postbuild.mjs`), so the
// container arrives with markup for React to adopt; development serves an
// empty one. Hydrating the prerendered case keeps its first paint instead of
// throwing it away and rendering from scratch.
const container = document.getElementById("root")!;
const tree = (
  <StrictMode>
    <App />
  </StrictMode>
);

if (container.hasChildNodes()) {
  hydrateRoot(container, tree);
} else {
  createRoot(container).render(tree);
}
