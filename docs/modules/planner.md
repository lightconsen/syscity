# Planner Module

Goal planner for decomposing high-level goals into executable task DAGs.

## Design

The planner takes a user goal (e.g. "deploy this project to a server"), breaks it into a directed acyclic graph of `Task`s, and executes them in topological order with automatic verification and rollback.

- **`GoalDecomposer`** — Breaks goals into subtasks using LLM or rule-based decomposition
- **`DagScheduler`** — Topological execution of task dependencies
- **`TaskExecutor`** — Executes individual tasks with retry and verification
- **`VerificationEngine`** — Checks task success against criteria
- **`RollbackManager`** — Snapshot-based state restoration on failure
- **`ErrorDiagnosisEngine`** — Root cause analysis and remediation suggestions
- **`ToolLearningEngine`** — Learns from successful tool executions for future suggestions
- **`WorkflowRecorder`** / **`WorkflowPlayer`** — Record and replay task sequences
- **`PersistentTaskManager`** — SQLite-backed durable task queue
- **`TaskScheduler`** — Scheduled task execution with cron support
- **`CompositeToolRegistry`** — Registry for multi-step composite tools

### Task Lifecycle

```
Pending ──▶ Running ──▶ Completed
   │           │
   ▼           ▼
Failed    RolledBack
```

## Key Types

```rust
pub struct Task {
    pub id: TaskId,
    pub description: String,
    pub action: DesktopAction,
    pub dependencies: Vec<TaskId>,
    pub verification: Option<VerificationCriteria>,
    pub snapshot_before: bool,
    pub max_retries: u32,
    pub retry_delay: Duration,
    pub status: TaskStatus,
    pub error: Option<String>,
    pub result: Option<String>,
}

pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    RolledBack,
}

pub struct Plan {
    pub goal: String,
    pub tasks: HashMap<TaskId, Task>,
}

pub struct VerificationCriteria {
    pub expected_output: Option<String>,
    pub expected_files: Vec<String>,
    pub timeout: Duration,
}

pub struct ErrorDiagnosisEngine {
    // Root cause analysis and remediation
}

pub struct ToolLearningEngine {
    // Experience-based tool suggestion
}
```

## Data Flow

```
User Goal
    │
    ▼
GoalDecomposer::decompose()
    │
    ▼
Plan (DAG of Tasks)
    │
    ▼
DagScheduler::execute()
    │
    ├──▶ Topological sort
    ├──▶ Execute ready tasks
    ├──▶ Verify results
    ├──▶ Retry on failure
    └──▶ Rollback if needed
```

## Concurrency: reads in parallel, writes in serial

A batch of independent tasks really is spawned concurrently (`max_concurrency`,
default 4). That is safe for perception and unsafe for input, because a machine
has one pointer and one focus: two clicks landing at once do not become two
clicks, they become one click at a place neither task asked for, reported to
both as success.

So the executor asks each action whether it needs the display
(`DesktopAction::needs_input_lock`) and, if it does, holds the shared
`target_lock::DESKTOP` lease for the length of that dispatch only. The
classification is deliberately wide:

| Category | Variants | Takes the display lock |
|----------|----------|------------------------|
| Input | `Click`, `DoubleClick`, `Type`, `KeyPress`, `Drag`, `Scroll` | **yes** |
| Window state | `ActivateWindow`, `CloseWindow`, `MoveWindow`, `ResizeWindow`, `MinimizeWindow`, `MaximizeWindow`, `LaunchApp` | **yes** (they move the focus, so an unaimed keystroke after one lands elsewhere) |
| Process / clipboard | `KillProcess`, `RestartProcess`, `SetProcessPriority`, `ClipboardSet` | **yes** |
| Observation | `Screenshot`, `ReadUiTree`, `ListWindows`, `GetWindowGeometry`, `ListProcesses`, `GetSystemStatus`, `ClipboardGet` | no |
| No effect | `Wait` | no |

`Scroll` and the window actions are the ones most often mistaken for harmless.
Neither types a character, but both change where the next unaimed action lands.
Callers that are unsure belong on the locked side: the cost is waiting, and the
alternative is a wrong action reported as a right one.

Note the boundary with the loop: `ComputerUseLoop` stays a **single-target
serial** loop and is not parallelised internally. Parallelism lives *above* it
— `GoalPlanner` splits a DAG into sub-tasks, each sub-task is its own serial
loop, and only the input steps queue on the display lock. Do not try to make
the loop itself concurrent; it has no way to know what else is on the screen.

## Implemented Features

- Goal decomposition into task DAGs
- Topological task scheduling with dependency resolution
- Task execution with retry and timeout
- Verification engine with multiple criteria types
- Rollback manager with snapshot-based restoration
- Error diagnosis with root cause analysis
- Tool learning from execution history
- Workflow recording and playback
- Persistent task queue with SQLite backend
- Scheduled task execution
- Composite tool registry for reusable multi-step tools
- Task state machine (Pending, Running, Completed, Failed, RolledBack)

