// Shared scripts for the fake provider.
//
// A UI test says what the model does, then asserts on what the screen does.
// Keeping the scripts here means "a turn that calls a tool" is defined once
// and can be reused by the tool, approval and run-state specs.
//
// Each entry is one assistant turn; the second entry answers after the tool
// result has been fed back to the model.

/** Plain streaming text — the ordinary case. */
export function plainText(text = "chunk1 chunk2 chunk3 chunk4 chunk5", chunkDelayMs = 20) {
  return [{ text, chunkCount: 5, chunkDelayMs }];
}

/** Slow text, long enough to reload the page or hit Stop mid-turn. */
export function slowText({ chunkDelayMs = 1500, chunkCount = 12 } = {}) {
  const text = Array.from({ length: chunkCount }, (_, i) => `chunk${i + 1} `).join("").trim();
  return [{ text, chunkCount, chunkDelayMs }];
}

/**
 * A tool call that needs no approval — a read-only glob — then a final answer.
 *
 * This is the "the agent used a tool and the UI shows it" case: the tool card
 * appears in the internals panel with the tool's name and a done badge.
 */
export function readOnlyTool({ name = "glob", arguments: args = { pattern: "*.md" } } = {}) {
  return [
    { toolCalls: [{ name, arguments: args }] },
    { text: "found the files" },
  ];
}

/**
 * A shell call that stops at the approval gate, then a final answer once the
 * call has been decided.
 *
 * `shell` declares `requires_approval`, so under the default `[permissions]`
 * mode every shell call asks — the test drives the dialog rather than relying
 * on the command being dangerous. The command itself is inert on purpose:
 * approving it must run something, and a test must never `sudo`.
 */
export function shellApproval({ command = "echo approval-probe-ok" } = {}) {
  return [
    { toolCalls: [{ name: "shell", arguments: { command } }] },
    { text: "the command was decided" },
  ];
}

/** The provider fails the completion. */
export function providerError() {
  return [{ errorStatus: 500 }];
}
