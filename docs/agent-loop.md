# The nightly agent loop

A scheduled GitHub Action that turns one queued issue into one reviewed pull
request per night. It is how the project can be improved continuously without a
human driving every change: the agent implements, the repository's own checks
verify, and a human reviews the PR.

The agent **never** pushes to `main` and **never** merges. Its output is always a
pull request.

## How it works

```
issue labeled agent:ready
        │
        ▼
.github/workflows/agent-nightly.yml  (nightly, or manual dispatch)
        │  select oldest issue (or the one named in the dispatch input)
        │  guard: at most MAX_OPEN_AGENT_PRS agent PRs open at once
        ▼
branch agent/issue-<n>   ──►  headless `claude -p` with .github/agent-task-prompt.md
        │
        ▼
./scripts/agent-verify.sh   (ci.yml's `check` job + tests scoped to the diff)
        │
   pass ├─► commit, push, open PR (label agent-pr) ──► human reviews & merges
        └─ fail ─► comment the failure on the issue, label agent:blocked
```

The workflow itself runs only a *bounded* check so a bad change is caught before
a PR is opened. The full CI matrix (`ci.yml`) still runs on the PR and remains
the real gate.

## Labels

| Label | Meaning |
|---|---|
| `agent:ready` | Queued. The loop picks the lowest-numbered open one. |
| `agent:in-progress` | Claimed by the current run. |
| `agent:review` | A PR is open; awaiting human review. |
| `agent:blocked` | The agent could not produce a verified change; needs a human. |
| `agent-pr` | Applied to the PR the loop opens. |

Create them once:

```bash
gh label create agent:ready       --color 0E8A16 --description "Queued for the nightly agent"
gh label create agent:in-progress --color FBCA04 --description "Claimed by a running agent"
gh label create agent:review      --color 1D76DB --description "Agent opened a PR"
gh label create agent:blocked     --color B60205 --description "Agent could not verify a change"
gh label create agent-pr          --color 5319E7 --description "Opened by the nightly agent"
```

## Filing tasks

A good task is small, verifiable, and self-contained. Its body must carry a
**"Done when"** line naming the command or test that proves it — that line is the
agent's success criterion.

Two structured sources can be filed automatically:

```bash
# Preview what would be filed:
./scripts/seed-agent-issues.sh --dry-run

# File them (idempotent — existing titles are skipped):
./scripts/seed-agent-issues.sh
```

- `evals/actions/actions.json` — regenerate with
  `syscity eval action-items --generate`, then `./scripts/seed-agent-issues.sh --source actions`.
- `docs/risk-register.md` — open rows (marked ⏳) become issues with
  `--source risk`.

The gaps in `hermes-diffs.local.md` are filed by hand: that file is gitignored,
so CI and the seeder never see it.

## Running it by hand

```bash
# Select and prepare only — no agent, no PR:
gh workflow run agent-nightly.yml -f dry_run=true

# Work one specific issue now:
gh workflow run agent-nightly.yml -f issue=123
```

## Configuration

| Name | Kind | Purpose |
|---|---|---|
| `EVAL_API_KEY` | secret | DeepSeek key, passed as `ANTHROPIC_AUTH_TOKEN`; the agent talks to DeepSeek's Anthropic-compatible endpoint. |
| `AGENT_PAT` | secret | Fine-grained PAT with **Contents + Issues + Pull requests** (all read/write). It is also `GH_TOKEN`, so Issues write is needed for the label/comment steps. With it, the PR triggers `ci.yml`; without it the PR is opened by `GITHUB_TOKEN` and CI does not run on it automatically. |
| `AGENT_ANTHROPIC_BASE_URL`, `AGENT_MODEL`, `AGENT_HAIKU_MODEL` | variables (optional) | Override the endpoint and models. Defaults: `https://api.deepseek.com/anthropic`, `deepseek-v4-pro[1m]`, `deepseek-v4-flash`. |
| `MAX_OPEN_AGENT_PRS` | env in the workflow | Cap on agent PRs open at once (default `1`). |
| `AGENT_MAX_BUDGET_USD` | env in the workflow | Client-side cost ceiling per run (default `5`; it is an estimate, so leave headroom). |

The agent authenticates the way Claude Code does in CI: `ANTHROPIC_BASE_URL` +
`ANTHROPIC_AUTH_TOKEN` point it at a provider that speaks the Anthropic Messages
wire format. Swapping providers is therefore a variables-only change.

## Cost and safety

- **One task per night**, and at most `MAX_OPEN_AGENT_PRS` open agent PRs — the
  spend and the review load are both bounded.
- The agent's tool surface is restricted (`--allowedTools` / `--disallowedTools`);
  `git push`, `git merge`, `git rebase` and `gh pr merge` are denied.
- `main` is protected: force-push and deletion are disabled, and the loop only
  ever pushes `agent/issue-<n>`.
- `.claude/settings.json` carries a matching `permissions.deny` block as defence
  in depth for local runs.
- `./scripts/gate-integrity.sh` runs in the verify step, so the agent cannot lower
  an eval threshold to make its change pass.

## Turning it off

Disable the workflow (Actions → Agent Nightly → ⋯ → Disable), or delete
`.github/workflows/agent-nightly.yml`. Queued `agent:ready` issues simply stop
being consumed.
