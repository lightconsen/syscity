# UI tests

Playwright specs that drive the **real** gateway in a browser: a scripted fake
model, a throwaway `SYSCITY_HOME`, and assertions on what is on screen.

The Rust e2e suite (`tests/e2e/`) proves the event sequence. It cannot prove the
screen — the two worst bugs in this UI had a perfectly correct event stream
behind them: a hook-order error that blanked the message list, and a reloaded
page showing a running turn as stopped.

## Running

```sh
cargo build            # the tests spawn target/debug/syscity
pnpm build             # the gateway serves the UI out of dist/
pnpm test:ui           # or: pnpm test:ui --headed, or --grep "approval"
```

Only Chrome is needed (`channel: "chrome"` in `playwright.config.ts`), so there
is no browser download. A run takes ~30 s; most of that is per-test gateway
startup, not the tests themselves.

## How a test is set up

```js
const app = await ui.launch(shellApproval());   // script the model
await app.send("run the probe");                // drive the UI
await expect(app.page.getByTestId("approval-dialog")).toBeVisible();
```

`ui.launch(script, gatewayOpts)` starts a fake provider, a gateway pointed at
it, and navigates a page past the first-launch gates. Everything is torn down
whether the test passed or not, and every test gets its own home and port, so
specs run in parallel and share no state.

Fixtures:

| file | what it is |
| --- | --- |
| `fixtures/fake-provider.mjs` | scriptable OpenAI-compatible SSE server |
| `fixtures/gateway.mjs` | spawns `syscity start` against a private home |
| `fixtures/scripts.mjs` | named model behaviours (plain text, tool call, approval) |
| `fixtures/app.mjs` | `ui` fixture + `App` helper (`send`, `lastAssistant`, …) |

## What to assert on

Add a `data-testid` rather than matching text where the text is translated or
likely to change. The ids currently in use: `composer-input`, `composer-send`,
`composer-stop`, `message-list`, `message-user`, `message-assistant`,
`message-skeleton`, `live-status` (+ `data-status`), `turn-duration`,
`internals-toggle`, `internals-panel`, `tool-call` (+ `data-tool-name`,
`data-status`), `approval-dialog`, `approval-risk`, `approval-approve`,
`approval-always`, `approval-deny`, `approval-deny-dots`, `approval-close`,
`approval-badge` (+ `data-count`), `session-row` (+ `data-session-id`,
`data-active`), `session-running`.

When a test needs the gateway's own view of a turn (rather than what the page
happens to have rendered), assert on the live bar's `data-status` or the
sidebar's `session-running` — those come from `sessions.status`, not from an
event the page might have missed.

## Adding a model behaviour

Add it to `fixtures/scripts.mjs` so a scenario is defined once. A script is an
array of steps, one assistant turn each: the first completion returns step 1,
the follow-up (after the tool result is fed back) returns step 2, and the last
step repeats.
