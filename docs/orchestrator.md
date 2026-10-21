# Room orchestrator

`buddies orchestrator` explicitly starts one persistent coordinator for a room.
Running the ordinary buddies service still starts no model. The coordinator owns
the goal and assignment graph; specialists retain their own Codex conversations
and write in separate git worktrees.

Start the HTTP service, then run against a **clean, committed** repository:

```sh
BUDDIES_TRANSPORT=http buddies

buddies orchestrator --room project --repo /path/to/repo \
  --model YOUR_MODEL --goal 'Implement and verify the producer/consumer contract' \
  --allow-path src/producer --allow-path src/consumer \
  --check 'cargo test --locked' --max-active-specialists 2
```

`--model` is optional and otherwise uses Codex's configured model. Codex must be
installed and signed in; `--codex-bin` can select its real executable. `--url`
selects the shared HTTP service. Paths are explicit repository-relative files or
directories, without globs, `.` or parent components. Repeat `--allow-path` and
`--check` for additional paths and shared checks.

The stable participants are `orchestrator`, `specialist-a`, and `specialist-b`.
The model may create additional named specialists only for independent
responsibilities, with at most 16 registered roles. The service rejects live
duplicate names. All participants use addressed inboxes on the same HTTP service;
this command does not replicate assignments across P2P nodes.

## Machine configuration and autonomous service mode

To configure a machine as an autonomous orchestrator that accepts delegated tasks
and creates specialists with scoped access to local machine resources, pass
`--config <path>` (see [`contrib/orchestrator.toml`](../contrib/orchestrator.toml)):

```toml
room = "Machine-Orchestrator"
url = "http://127.0.0.1:8080"
max_active_specialists = 4
max_model_calls = 128

[[resources]]
name = "backend"
repo = "/srv/repos/backend"
description = "Rust backend API and storage engine"
allow_paths = ["src", "tests", "Cargo.toml"]
checks = ["cargo test --locked"]

[[resources]]
name = "frontend"
repo = "/srv/repos/frontend"
description = "Web UI application"
allow_paths = ["src", "package.json"]
checks = ["npm test"]
```

Start the machine orchestrator in listening service mode (when `--goal` is omitted):

```sh
buddies orchestrator --config ~/.config/buddies/orchestrator.toml
```

Or install it as a user `systemd` service using
[`contrib/systemd/buddies-orchestrator.service`](../contrib/systemd/buddies-orchestrator.service):

```sh
mkdir -p ~/.config/systemd/user ~/.config/buddies
cp contrib/orchestrator.toml ~/.config/buddies/orchestrator.toml
cp contrib/systemd/buddies-orchestrator.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now buddies-orchestrator.service
```

In service mode, the orchestrator:
1. Joins `room` as `orchestrator` and registers on the local durable `/worker` queue.
2. Accepts delegated tasks from:
   - `delegate_task` (`wait: false` durable queue or `wait: true` synchronous/P2P delegation),
   - addressed `request` messages sent to `orchestrator` in the room (`send_message` with `to: "orchestrator"` and `kind: "request"`).
3. For each delegated task, autonomously creates specialist roles via `orchestrator_control` (`action: "create_specialist"`) and grants each specialist access to specific machine `resources` and `allowed_paths`.
4. Dispatches each assignment in an isolated git worktree of the target resource (`resource: "<name>"`), verifies acceptance checks, and publishes verified patches to the touched repositories.

Pass `--once` with `--config` to process a single delegated task and exit.

## Assignment protocol

The dynamic `orchestrator_control` tool proposes state transitions. The scheduler
validates actor identity, ownership, dependencies and revisions before accepting
them. The coordinator can `create_specialist`, `assign`, `revise`, `cancel`,
`recover`, `inspect` and `complete`. Specialists can `accept`, `checkpoint`,
`report`, `fail` and `heartbeat` their own assignments. `inspect` is available to
every role.

Each assignment stores an ID, owner, revision, target resource, owned paths,
dependencies, deadline, acceptance, worktree, baseline and result evidence.
Dependencies must already exist, so the graph remains acyclic. Shared paths within
the same resource have one explicit owner. Reuse that assignment through revisions
when its contract changes.

The states are `queued`, `running`, `waiting_for_input`, `completed`, `failed`
and `cancelled`. A report does not complete an assignment: the runtime waits for
the turn to finish, checks all changed paths, runs its reported acceptance checks
and integrates the patch. Only verified completion releases dependencies.
`handled` inbox receipts still mean that a message was processed.

Specialists must accept each revision before reporting its result. Results from
older revisions remain unverified evidence. Revising a producer also increments
and invalidates its dependent assignments; unrelated assignments keep their
revision, worktree and thread. Material changes to active work require a recorded
checkpoint and completion of the active turn before revision or cancellation.
A new revision gets a fresh worktree; earlier effects remain inspectable.

Questions and answers travel through the durable inbox and resume the same
specialist thread. An idle specialist can answer a clarification in a read-only
turn after its assignment completes. Messages queue while a turn is busy;
related `update` messages can steer the active turn. A confirmed steering rejection
queues the message; uncertain dispatch stops execution rather than replaying it.
Ordinary transport status does not wake the model. `heartbeat` updates stored
activity without requesting a coordinator decision.

## Integration and recovery

State lives under the repository's common git directory:
`buddies-orchestrator/<room-hash>/state.redb` (or `--state-dir` when configured).
A transactional database lock prevents two local coordinators from owning that
state. `state.json` is an atomic, reviewable snapshot, not an editable
configuration. This directory also retains specialist worktrees, per-revision
patches, acceptance logs and the integration worktree. Internal commits occur
only in the detached integration worktree; repository hooks are disabled for the
runtime's git operations.

The coordinator completes the goal only after every required assignment is
verified and specialists are idle. The runtime then runs the resource's `checks`
commands on the combined changes, validates the authorized scope again, and
applies `verified.patch` to the original checkout. It does not commit that
checkout. Its HEAD and cleanliness must still match the starting repository;
otherwise the patch remains available for inspection and integration.

Restart with the same command and add `--resume`. Configuration and aggregate
usage persist. Threads resume using their saved IDs. Interrupted work becomes
uncertain and cannot run again until the coordinator inspects effects and records
an explicit `recover` decision. Read inbox messages from an interrupted dispatch
become failed receipts. Completed goals return their stored result without
starting Codex.

An interrupted integration/publication can require manual inspection of retained
worktrees, the saved integration HEAD, and `verified.patch`. A saved publication
boundary with an uncertain outcome refuses automatic resume. Do not edit the
state snapshot or resend work without reviewing effects. Acceptance failures
before publication leave the checkout unchanged and can be addressed by revising
the relevant assignments.

These worktrees and scope checks guard integration and coordination; they are not
a security boundary for hostile code. Reported acceptance commands execute with
the local user's permissions in an isolated worktree. Use a repository and goals
you trust, as for the existing managed-agent/worker runtimes.

## Budgets

Defaults are two active specialist turns, 64 model dispatches, 1 MiB of generated
text, one hour of aggregate running time, dependency depth eight and 120 seconds
per acceptance command. Configure them with `--max-active-specialists`,
`--max-model-calls`, `--max-output-bytes`, `--max-elapsed-secs`, `--max-depth` and
`--check-timeout-secs`. `--debounce-ms` defaults to 1000 and combines bursts before
dispatch. Specialist turns waiting for input release an active slot, while their
assignment stays associated with its owner.

The call budget counts host `turn/start` and `turn/steer` dispatches, including
failed/uncertain dispatches. It is not a billing/token measurement of internal
model requests. Output tracks generated reply/reasoning text and tool arguments. Time and usage do
not reset on resume; budget exhaustion stops subprocesses and retains evidence.
Graphs, role counts, revisions, input text, patches and check logs are also bounded.
The App Server's native multi-agent delegation and autonomous goal continuations
are disabled so specialist creation and turn dispatch stay with the scheduler.

## Verification

```sh
cargo test --locked --all-targets
cargo build --locked
PYTHONDONTWRITEBYTECODE=1 python3 scripts/smoke_orchestrator.py
```

The deterministic experiment uses the real HTTP service, durable inbox and git
worktrees with an App Server fixture. A consumer asks about a missing field,
receives a recorded answer in the same thread, then accepts a material contract
change after an active-turn checkpoint. Its revision-two result passes the shared
check; the producer remains on revision one. The test also checks stale evidence,
thread reuse, single dispatch of each revision, publication, completed resume,
and interrupted recovery without duplicate execution.

This validates the runtime protocol. It does not measure a real model's cost or
compare completion rates with direct peer coordination. That comparison remains
an opt-in acceptance experiment using the same models and task described in
[the design](orchestrator-design.md).
