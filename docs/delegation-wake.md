# Delegation Result Collection: `wait` + Parent Auto-Wake

Status: **Implemented** — v1 (`wait`) and v2 (wake) both shipped.

> Implementation notes (the sections below remain the original design record):
>
> - `wait` lives in `DelegateTool::execute_inner` in `src/tools/delegate_tool/tool.rs`, with `MAX_WAIT_SECONDS = 60` in `src/tools/delegate_tool/mod.rs`
>   and returns `Ok("still running …")` on timeout — never `Err`, so the
>   circuit breaker is untouched. The shipped timeout text is the v2 wording:
>   *"End your turn — you will be woken with its result when it completes."*
> - Wake lives in `src/delegation/wake.rs` (`DelegationWake`), wired in
>   `start_gateway` in `src/gateway/lifecycle/start.rs`. Instead of a retry loop it uses a **coalescing
>   drain**: near-simultaneous child completions are buffered per parent
>   session and joined into a single wake turn; completions that land while a
>   wake turn is running are picked up by the next drain pass, so no
>   notification is lost. A failed wake is informational — the result stays
>   persisted for `delegate status` / `wait`.
> - Startup sweep: `DelegationTaskStore::fail_orphaned_runs`
>   (`src/delegation/state.rs`) runs once at gateway startup and marks tasks
>   left in `running` / `waiting_handoff` by a previous process as `failed`,
>   so dead executions no longer read as "running" forever.
> - **Push events (TUI agent rows)**: task changes no longer live only in the
>   store. `DelegationTaskStore` carries an event sink (task ids only); the
>   gateway runs `delegation_event_forwarder` in `src/gateway/lifecycle/start.rs`,
>   which re-reads each row and publishes `GatewayEvent::DelegationTaskUpdated`
>   → the WS event `delegation.updated` with a self-sufficient snapshot
>   (`DelegationTaskSnapshot`). Routing rides a new `parent_session` column:
>   the row records the session the `delegate` call ran in (the user session
>   for a tree root, `delegation:<parent_run_id>` for a delegated parent), and
>   `root_session_for_task` climbs the `parent_id` chain to resolve the ROOT
>   USER session the client is subscribed to — the registry's own parent key
>   cannot serve this (a successor's is the handing-off task id, deliberately
>   not a live session). Rows written before the column existed resolve to
>   `None` and are skipped (they are swept to `failed` at startup anyway).
>   Each task's LLM rounds also report usage into the row (`usage_tokens`,
>   accumulated per round by the child's progress callback — the child
>   response's own `usage` is only the last round), so the TUI can render
>   live task rows with `↓ tokens` while they run. Wake semantics are
>   untouched.

This document is the design for fixing a delegation usability bug observed with
DeepSeek (and similar models): intermediate agents early-stop instead of
polling their children, so the root agent never aggregates the results. It
specifies two complementary mechanisms:

1. **`wait`** — a synchronous fast path on the `delegate` tool: block up to a
   bounded budget and return the child result immediately when it completes.
2. **Parent auto-wake** — when a child completes after the parent's turn has
   ended, re-open the parent's conversation with an injected message carrying
   the child result, so the parent continues and aggregates. This is decoupled
   from any tool-call timeout.

The two mechanisms form a single contract: `wait` handles children that finish
within a synchronous window; the timeout of `wait` is a **graceful handoff** to
the wake path, not an error. v1 ships `wait` alone (immediate relief); v2 adds
wake (the long-term fix, no 120 s ceiling). v2 is designed so that `wait`'s
code does not change — only the timeout message wording and the `delegate`
description.

---

## 1. Problem

Current delegation flow (see `src/tools/delegate_tool/`):

```
parent turn:  delegate spawn(child)
              → child runs as a detached tokio task (subagent_registry.rs:249)
              → delegate status(child) → "Running"
              → model decides to stop the turn              ← early stop
child:        completes in background → set_result + complete_run + set_status
parent turn:  already ended — nobody collects the result
```

- The mechanism is intact: children run to completion independently, persist to
  `delegations.db`, and are observable via `delegate status`.
- The usability failure is at the model layer: root/manager agents frequently
  stop their turn ("still in progress") instead of looping on `delegate status`
  to aggregate, so the parent never receives a summary.

### 1.1 Root causes

1. **The tool contract pushes polling onto the model.** `delegate spawn`
   returns only `child_id` (`DelegateTool::execute_inner` in
   `src/tools/delegate_tool/tool.rs`); there is no
   synchronous way to wait for the outcome, so the model must decide how many
   times to poll `status`.
2. **Misleading description.** `delegate`'s description claims *"Progress and
   results are relayed to the parent"* (`DelegateTool::description` in
   `src/tools/delegate_tool/tool.rs`) — results
   are **not** auto-relayed; the parent must poll.
3. **`status` output gives no next-step guidance.** The model sees
   `Child <id> status: Running` with no instruction on what to do.
4. **Model discipline varies.** Some models (observed: DeepSeek) early-stop
   reliably; this makes the delegation tree a multi-turn coordination problem
   instead of a single turn.

---

## 2. Design overview

```
wait  = synchronous fast path.  Child completes inside the wait window
        (≤ ~60 s) → parent gets the result in the same tool call.
wake  = asynchronous slow path. Child outlives the window → wait returns
        Ok("still running"); the parent ends its turn; when the child
        completes, a detached turn on the parent's session is opened with
        the child result, and the parent continues and aggregates.
```

The timeout of `wait` is the **seam** where wake plugs in. Nothing in `wait`'s
code changes when wake lands; only the timeout message wording and the
`delegate` description change between v1 and v2.

---

## 3. Constraints that shape the design

### 3.1 The 120 s tool-call hard ceiling

Every tool call is wrapped in `tokio::time::timeout(context.timeout(), …)`
(`ToolRegistry::execute_call` in `src/tools/registry/streaming.rs`); the turn engine applies
`with_timeout(Duration::from_secs(120))` (`src/agent/agent_engine.rs:1437` and
`:1894`), and the HTTP/SSE layer has a matching 120 s window
(`src/gateway/handlers/openai.rs:106/174`).

**Consequence:** a synchronous `wait` must cap its internal budget **well under
120 s** (clamp to ~60 s). Otherwise the outer timeout trips and the tool call
fails.

### 3.2 The circuit breaker

`CIRCUIT_BREAKER_THRESHOLD = 3` (the associated const on `ToolRegistry` in
`src/tools/registry/mod.rs`). An `Err` from a
tool call triggers `ToolRegistry::record_failure` in `src/tools/registry/mod.rs`; three consecutive
failures degrade the tool and disable **all** `delegate` actions.

**Consequence:** `wait` must never return `Err` on timeout. Its timeout returns
`Ok("still running …")` (a normal, informative result), which takes the
`reset_failure` path (`agent_engine.rs:1457`) and leaves the breaker untouched.
A failed child likewise returns `Ok` with the failure text — information for
the model, not a tool fault.

### 3.3 Deadlock red line

The `DelegationTracker` is `Arc<RwLock<HashMap<String, ChildAgent>>>`
(the `DelegationTracker` struct in `src/tools/delegate_tool/mod.rs`). A poll loop must **never hold the lock across a
sleep/await**: snapshot → release lock → sleep → snapshot.

### 3.4 The tracker is shared between parent and child

`spawn_child` clones the parent's tracker into the child task
(`DelegateTool::spawn_child` in `src/tools/delegate_tool/tool.rs`); the child's `execute_child_task` writes status via
`DelegationTracker::set_result` in `src/tools/delegate_tool/mod.rs` into the **same** map the
parent's tool instance reads. So `wait` can poll `tracker.get_child(id)` and
reliably observe transitions to `Completed`/`Failed`.

### 3.5 Who a child may run as (`target_agent`)

`TaskSpec.target_agent` is parsed out of the **model's** tool arguments (the
`task_json` handling in `DelegateTool::execute_inner` in `src/tools/delegate_tool/tool.rs`), which makes it a privilege
selector rather than a routing hint from the operator: the child runs as the
agent that name resolves to, and therefore with that agent's workspace, secrets
and skill trust.

It is default-deny: a delegation may only name the agent it delegates for, and
any other name is refused, logged, and the child runs as that agent
(`spawn_child`). Default-deny costs no supported use — nothing else in the tree
sets the field, and no configuration exposes it. If cross-agent delegation is
wanted, the allowlist belongs in operator configuration (a gateway-level list of
delegatable agents), never in a name the model chose.

The delegation task row and its events record the id of the agent that actually
executes the child, not the requested string: a lookup that fell back to the
parent used to leave the row naming an agent that never ran.

---

## 4. Component 1: the `wait` action (v1)

### 4.1 Semantics

- **On time:** child reaches `Completed` within the window → return the child's
  result immediately (no second `status` call needed).
- **On child failure:** return `Ok` with the failure text.
- **On timeout:** return `Ok("still running …")`. The exact wording is the
  v1/v2 switch:

  | Phase | Timeout text | Model behavior |
  |---|---|---|
  | v1 (wait only) | `still running — poll again with wait/status` | keeps the polling habit (today's behavior, less blind) |
  | v2 (wait + wake) | `still running — end your turn, you will be resumed` | stops cleanly; wake re-opens the turn |

### 4.2 Spec (pseudo-code)

```
wait(child_id, [seconds]):
  budget = clamp(seconds.unwrap_or(60), 1, 60)        # ≤ outer 120 s ceiling
  deadline = now + budget
  loop:
    child = tracker.get_child(child_id)                # snapshot, then drop lock
    match child.status:
      Completed => return Ok(child.result)             # fast path
      Failed    => return Ok("child failed: …")        # info, not a fault
      _         => {}                                   # keep waiting
    remaining = deadline - now
    if remaining == 0: return Ok("still running …")    # ← handoff to wake
    sleep(min(remaining, 1 s)).await                    # lock already released
```

### 4.3 Supporting changes (v1)

- **Fix the `delegate` description** (`DelegateTool::description` in `src/tools/delegate_tool/tool.rs`): remove the
  false "Progress and results are relayed to the parent" claim. State instead:
  *spawn returns a child id; call `wait` to block for the result; if it reports
  still running, do not blindly poll — follow the timeout guidance.*
- **`status` output guidance**: append a one-line next-step hint (e.g. "call
  wait to block for this child, or poll status again").

### 4.4 Why wait is wake-proof

`wait`'s code does not reference wake at all. The only v1→v2 diff is the timeout
message string and the description. The `Ok`-not-`Err` contract, the 60 s clamp,
and the snapshot/sleep loop are identical in both phases.

---

## 5. Component 2: parent auto-wake (v2)

### 5.1 Data sources (all already present)

| Need | Source |
|---|---|
| Trigger point | child completion paths in `execute_child_task` in `src/tools/delegate_tool/child_task.rs`, alongside `coordinator.maybe_advance` |
| Parent session key | `registry.get_run(child_id).parent_session` (`subagent_registry.rs:364`, field at `:38`); recorded at spawn as `context.user_id` — for the root it is the user session, for a delegated parent it is `delegation:<parent_id>` (already a full session key) |
| Parent `Arc<Agent>` | **the one structural gap** — needs a session → agent bridge (§5.2) |

`ToolContext` carries identity/sandbox/model/delegation only
(`src/tools/types.rs:149-159`) — no agent handle. The delegation layer cannot
derive the parent agent by itself.

### 5.2 Session → agent bridge (the only cross-layer change)

The parent's agent resolves one of two ways:

- **Root agent** (user session, router-bound): `router.resolve_by_session(session)`
  (`src/inbound/router.rs:603`) → `agent_id` → `GatewayState.agents.agents.get(agent_id)`.
- **Delegated parent** (session = `delegation:<parent_id>`, **not** router-bound,
  because delegated turns run `process_message_with_progress` directly): look up
  `delegation_tasks.agent_id` for the parent task, then `AgentResolver.resolve(agent_id)`.

Inject a resolver into the delegation layer, assembled in
`start_gateway` in `src/gateway/lifecycle/start.rs`:

```
type WakeResolver = Arc<dyn Fn(&str) -> Option<Arc<Agent>> + Send + Sync>;
// 1. if session starts with "delegation:" → delegation_tasks.agent_id → AgentResolver
// 2. else → router.resolve_by_session(session) → agents.get(agent_id)
```

This is the same seam where the existing `AgentResolver` is injected today
(in `start_gateway`, `src/gateway/lifecycle/start.rs`, "Register delegation tool with agent resolver").

### 5.3 Wake action

Mirror the heartbeat runner's wake pattern (`src/heartbeat/runner.rs:338-409`):

```
notify_parent(child_id, result):
    run = registry.get_run(child_id)                     # parent_session
    agent = bridge(run.parent_session)?                  # session → agent
    if agent busy: schedule a retry                      # heartbeat mpsc+Retry (runner.rs:286-304)
    msg = IncomingMessage::new("system", parent_session,
          "Your delegated child {child} completed.\nResult: {result}\n"
          "Summarize/aggregate and continue your task.")
          .with_provenance(InternalSystem { source: "delegation" })
    tokio::spawn(agent.process_message_with_progress(msg, cb))   # detached turn
```

Key properties:

- The woken turn is a **detached task**, not a tool call — so the 120 s tool
  ceiling, the circuit breaker, and the tracker deadlock red line do not apply.
- `process_message` keys history by the message's session id; passing the
  parent's session key **resumes** that conversation (history continuity)
  rather than starting a fresh one (which is why the heartbeat-style "new
  session each wake" does not apply here).
- If the parent is already running a turn (`busy`), defer via a retry — the
  exact busy/retry machinery heartbeat already has (`runner.rs:286-304`).

### 5.4 Termination and merge semantics

Without guards, wake could loop or duplicate turns:

1. **Wake condition:** only wake when the tree root still has children that
   are not `completed` — otherwise the parent has already aggregated and a
   wake would be noise. Query the store under `root_id`.
2. **Merge / dedup:** if two children finish near-simultaneously, two notifies
   would open two parent turns. Keep a `Mutex<HashSet<session>>` of sessions
   with a pending wake; if a wake is already scheduled for a session, append
   the extra result to a pending buffer instead of opening another turn.
3. **Termination:** the model decides "done" → the woken turn ends naturally.
   `max_iterations` (`Context.set_max_tool_iterations`, `src/agent/context.rs:251`)
   is the backstop. If the parent session has been closed/removed → no-op.
4. **Context growth (v3):** every wake appends the child result to the parent's
   context; deep trees inflate it. Summarization/pruning is out of scope for v2.

### 5.5 Background output (v3)

If the original HTTP/SSE request has already returned, the woken turn runs in
the background; its output is persisted to session history and surfaced via the
gateway event bus (the model heartbeat already demonstrates this
background-turn + event pattern). Whether/how the client is notified of a
woken-turn completion is a UX decision, not a mechanism blocker.

---

## 6. Phased roadmap

### Phase 1 — `wait` (v1): synchronous fast path

| # | Task | File(s) |
|---|---|---|
| 1.1 | Add `wait` action: budgeted poll on the shared tracker; `Ok`-not-`Err` on timeout; clamp ≤ 60 s | `DelegateTool::execute_inner` in `src/tools/delegate_tool/tool.rs` |
| 1.2 | Fix `delegate` description (remove false relay claim; document `wait`) | `DelegateTool::description` in `src/tools/delegate_tool/tool.rs` |
| 1.3 | `status` output: one-line next-step hint | `src/tools/delegate_tool/tool.rs` |
| 1.4 | Unit tests (§8) | `src/tools/delegate_tool/tests.rs` |

**Acceptance:** the model can get an in-turn result for children that complete
within ~60 s; on timeout the model receives a clear "still running" result and
the circuit breaker is never tripped by a `wait`.

### Phase 2 — wake (v2): async slow path

| # | Task | File(s) |
|---|---|---|
| 2.1 | `DelegationWake` module: notify + busy retry + pending-buffer merge | `src/delegation/wake.rs` (new) |
| 2.2 | Hook notify into `execute_child_task` completion/error paths | `execute_child_task` in `src/tools/delegate_tool/child_task.rs` |
| 2.3 | Session → agent bridge (root via router, delegated via `delegation_tasks.agent_id`) | `start_gateway` in `src/gateway/lifecycle/start.rs` |
| 2.4 | Flip timeout text + description to v2 wording | `src/tools/delegate_tool/tool.rs` |
| 2.5 | Wake condition (root still has non-completed children); unit tests | `src/delegation/wake.rs` |

**Acceptance:** child completes after the parent's turn ended → the parent is
woken with the child result and produces an aggregated answer; no infinite
wake loop; no duplicate concurrent turns.

### Phase 3 — polish (v3, optional)

| # | Task | Notes |
|---|---|---|
| 3.1 | Background-output UX: surfaced woken-turn completions (event/stream) | heartbeat-event precedent |
| 3.2 | Context summarization for repeated wakes on deep trees | out of scope for v2 |
| 3.3 | Configurable `wait` budget / wake enablement | agent config |

---

## 7. Files touched (summary)

| File | v1 | v2 |
|---|---|---|
| `src/tools/delegate_tool/` | `wait` action, description fix, status hint | notify hook, resolver field |
| `src/delegation/wake.rs` (new) | — | notify + busy retry + merge |
| `src/gateway/lifecycle/start.rs` | — | session → agent closure assembly |
| `src/agent/agent_engine.rs` | — | none (turns reuse `process_message_with_progress`) |

---

## 8. Testing plan

`wait` (v1):

- child completes within budget → returns the result directly.
- child slow → returns `Ok("still running …")` before the outer 120 s timeout.
- child fails → returns `Ok` with the failure text.
- circuit breaker: after a timeout, `is_degraded` is false and a subsequent
  `delegate status` still works.
- budget clamp: `seconds=300` → capped at 60; `seconds=0`/missing → default.

`wake` (v2, mock bridge/store/registry):

- child completion invokes notify; woken turn opens on `parent_session`.
- `busy` parent → notify deferred and retried.
- two near-simultaneous completions → one wake turn, both results present.
- root has no non-completed children → no wake.
- parent session closed → no-op.

Run before each commit:

```
cargo fmt --check && cargo clippy --lib -- -D warnings
cargo test --lib tools::delegate_tool
cargo test --lib delegation::
```

---

## 9. Risk register

| Risk | Impact | Mitigation |
|---|---|---|
| Model ignores `wait` / still early-stops | v1 benefit reduced | Description + status guidance; wake (v2) removes the polling decision entirely |
| `wait` trips the 120 s outer timeout | tool call fails | clamp budget ≤ 60 s |
| `wait` timeout counted as failure | breaker degrades all `delegate` actions | `Ok`-not-`Err` contract (§3.2) |
| Poll loop holds tracker lock across sleep | deadlock | snapshot → drop lock → sleep (§3.3) |
| Two children complete at once | duplicate parent turns | pending-buffer merge (§5.4) |
| Infinite wake loop | runaway turns | wake condition + `max_iterations` backstop (§5.4) |
| Deep trees inflate parent context | cost / quality | summarization/pruning (v3) |
| Woken turn output invisible to client | confusion | background-output UX (v3, heartbeat-event precedent) |

---

## 10. References

- Delegate tool + tracker: `src/tools/delegate_tool/`
  - description overpromise: `DelegateTool::description` in `tool.rs`; completion/error paths: `execute_child_task` in `child_task.rs`;
    tracker clone into child: `DelegateTool::spawn_child` in `tool.rs`; `set_result`: `DelegationTracker::set_result` in `mod.rs`
- Registry / timeouts / breaker: `src/tools/registry/`
  - `execute_call` timeout wrap: `ToolRegistry::execute_call` in `streaming.rs`; `CIRCUIT_BREAKER_THRESHOLD = 3`: the associated const in `mod.rs`
- Agent turn engine: `src/agent/agent_engine.rs` (`with_timeout(120 s)`: `:1437`/`:1894`)
- Subagent registry: `src/agent/subagent_registry.rs` (`get_run`: `:364`,
  `parent_session`: `:38`, detached `tokio::spawn`: `:249`)
- Session → agent routing: `src/inbound/router.rs` (`resolve_by_session`: `:603`,
  `bind_session`: `:519`)
- Wake precedent: `src/heartbeat/runner.rs` (agent wake: `:338-409`, busy retry: `:286-304`)
- Iteration backstop: `src/agent/context.rs:251` (`set_max_tool_iterations`)
- Shared task state / task rows: `src/delegation/` (`task_state_tool.rs`,
  `state.rs`, `scope.rs`)

---

*Both phases are implemented (see the status header). This document remains as
the design record: Phase 1 (`wait`) and Phase 2 (wake) landed as specified,
with the wake path realized as a coalescing drain rather than a busy-retry.*
