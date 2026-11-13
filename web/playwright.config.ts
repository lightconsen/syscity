import { defineConfig, devices } from "@playwright/test";

/**
 * UI tests drive the *real* gateway in a browser.
 *
 * The Rust e2e suite (`tests/e2e/`) proves the event sequence. It cannot prove
 * the screen: the two worst bugs in this UI — a hook-order error that blanked
 * the message list, and a reloaded page showing a running turn as stopped —
 * had a perfectly correct event stream behind them.
 *
 * Each spec starts its own gateway (private SYSCITY_HOME, random port) and its
 * own scripted fake provider, so tests run in parallel and share no state.
 * The gateway serves the UI out of `dist/`, so `pnpm build` must have run.
 *
 * Run:  pnpm test:ui
 * Watch a run:  pnpm test:ui --headed
 */
export default defineConfig({
  testDir: "./e2e",
  // The fixtures are modules, not tests.
  testMatch: /.*\.spec\.(ts|mjs)$/,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  // Gateway startup is the expensive part and the CPU is shared; a couple of
  // workers keeps runs honest without thrashing.
  workers: 2,
  fullyParallel: false,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : [["list"]],
  use: {
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "off",
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        // Use the Chrome already on the machine rather than downloading a
        // browser: the app only needs a modern Chromium, and a 150 MB download
        // in CI for a UI that ships in a WebView is not worth it.
        channel: "chrome",
      },
    },
  ],
});
