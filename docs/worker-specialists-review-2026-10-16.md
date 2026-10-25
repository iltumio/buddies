# Adversarial review: worker specialists

Status: all seven findings below and the WorkerState duplication have been addressed
in the working tree. The original findings are retained as the rationale; see
“Resolution and regression coverage” for the resulting contract.

## Scope and evidence

`main` and `HEAD` both resolve to `6ef4079b97e0ae80424e0cda7e906d0b5cb4cfeb`.
Reviewed the working-tree changes against that commit, including untracked
connector/specialist modules and the two smoke scripts. There are no branch-only
commits. The Tier 2 graph describes the main checkout, generation
`2026-10-11T10:14:53Z`; coverage was checked, but current changes and new modules
were inspected directly. This is a targeted review, not a completeness guarantee.

The existing orchestrator design/docs provide a baseline; no dedicated new
specialist specification was identified. New behavior was assessed against the
changelog and smoke-test claims, particularly strict boundaries and multi-resource
coordination. Findings below are source-verified failure paths; the existing smoke
tests exercise successful paths, not these adversarial scenarios.

## Standards

One maintainability finding (heuristic, not a hard violation):
`specialist_worker.rs` repeatedly builds the large `WireEvent::WorkerState`
payload across start, completion, error and control paths. This is possible
Duplicated Code/Data Clumps: centralize the runtime snapshot to prevent fields or
state transitions from diverging. No additional hard standards violation is
claimed. Formatting and lint checks pass.

## Original functional/spec findings

1. **P1 — Control messages have no coordinator ownership check.**
   `specialist_worker.rs`, `poll`: a parseable Dispatch immediately replaces
   `orchestrator_peer` before boundary validation; accepted dispatch replaces the
   current assignment. Cancel and Publish likewise lack sender authorization.
   An ordinary participant can issue privileged controls; two coordinators can
   redirect a busy worker's reports. Validate sender role, assignment ownership
   and busy state before mutation, and reject or queue competing dispatches.

2. **P1 — Heads from distinct repositories are mixed.**
   `runtime.rs`, remote dispatch: absent a resource-specific head, assignment.base
   falls back to the local `--repo` integration head. A remote worker then tries
   to check out that foreign commit. WorkerState/verification also overwrite the
   local integration head using remote Git heads. Trigger: orchestrator with
   repository A and a specialist on unrelated repository B. Keep heads keyed by
   resource and never substitute a different repository's head.

3. **P1 — Opening resources can destroy state before acquiring the room lock.**
   `mod.rs` opens ManagedResource before Store::open and resume validation.
   `connector.rs`, ManagedResource::open, performs reset --hard/clean -fdx on an
   existing integration when resume is false. A duplicate invocation can mutate
   the active integration before failing the lock or saved-state check. Acquire
   ownership and validate resume before destructive initialization.

4. **P1 — Acceptance checks bypass the advertised resource boundary.**
   `connector.rs`, run_checks executes report-provided commands through host
   `sh -c`; current_dir is not isolation. A check can write outside the workspace
   and still pass the local diff validation. The preexisting Git check runner
   shares this design; the new state connector extends it. Sandbox checks or
   explicitly limit them to user-approved commands and narrow the boundary claim.

5. **P2 — The worker ignores --sandbox read-only.**
   `specialist_worker.rs`, thread/turn startup hardcodes workspace-write for
   assignments instead of honoring the worker option. Selecting specialist mode
   changes the meaning of an existing user constraint. Preserve read-only or
   reject an incompatible option explicitly before startup.

6. **P2 — Wildcard Git ownership rejects valid root deletions.**
   `connector.rs`, expand_git_scopes derives `*` from names currently in the
   workspace. A deleted root file/directory disappears from that allowlist, then
   verification reports an ownership violation. Include the base tree's paths or
   validate wildcard ownership directly.

7. **P2 — State publication can partially apply before reporting a conflict.**
   `connector.rs`, StateConnector::publish validates and writes in the same loop.
   If a later file changed externally after preparation, earlier files are already
   published when the operation errors. Preflight all conflicts before writes;
   atomicity also needs rollback or a transactional target. Across resources,
   prepare-then-sequential-publish alone does not provide distributed atomicity.

## Utility assessment

Bounded persistent specialists, explicit dependencies and verified integration
are useful for coordinated work across repositories. Direct peer communication
is useful for independent tasks without a central scheduler. The initial review found blocking holes in ownership, lifecycle and
multi-repository isolation; the fixes below address those failure paths. The `linear` and `gdrive` kinds currently operate on local
directory snapshots; they do not synchronize with those services' APIs. Describe
them as local state adapters until real service integrations exist.

## Monitor repair and validation

The live systemd service was running an obsolete, deleted executable. Its /status
rooms omitted messages; serde defaults silently turned this into empty history.
After the user-authorized restart, /status and monitor --once returned 17 messages
in zyphe-prod. Both workers were resumed and registered again. Their local Codex
wrapper polluted the JSON stream with mise output, so resumption used the direct
installed Codex executable.

The source now rejects room snapshots missing messages with an update/restart
instruction, preserves successful completed turns in the default message view,
and abbreviates resource heads safely at Unicode character boundaries. Added
regression coverage and updated monitor usage/restart documentation.

Validation: 94 unit/integration-style tests passed; cargo build --locked,
cargo fmt --check, cargo clippy --all-targets -- -D warnings, git diff --check,
smoke_worker_specialists.py and smoke_multi_orchestrator_specialists.py passed.
Live JSON output was checked; interactive terminal rendering was not exercised.
This was the validation at the end of the monitor repair. The subsequent
specialist fixes and their verification are recorded below. No commits or PRs
were created.


## Resolution and regression coverage

1. Coordinator role, worker ownership, revision and idle-state validation now run
   before any dispatch mutation. The first valid coordinator remains pinned in
   persistent state. Cancel/prepare/publish require that same coordinator;
   publication requires completed, verified work and an idle worker. Unknown
   senders and resource-head updates outside a remote worker's advertised
   resources are rejected. Cancellation is not undone by a late turn completion.
2. Remote dispatch only uses a head for its exact resource. Remote events cannot
   overwrite the local repository's integration head. Head mismatches fail
   without checking out another commit or discarding retained integration work.
3. The orchestrator locks/loads its store and validates resume/config before
   opening resources. Every managed resource also holds an exclusive filesystem
   lock for its lifetime. Non-resume reopening fails rather than resetting or
   cleaning integration; incomplete resume metadata and changed scopes/checks
   fail closed.
4. Git and state verification execute only reported checks that exactly match
   user-configured commands. All configured checks are mandatory at publication,
   allowing partial assignments to complete before whole-goal checks can pass.
   Reported commands never become host execution authority.
   Configured checks and invoked scripts remain trusted host code, explicitly
   documented; this is not a hostile-code sandbox guarantee.
5. Specialist thread and turn policies honor read-only, including assignment
   turns. Idle turns remain read-only even in workspace-write mode.
6. Git validation uses wildcard containment directly, covering deleted paths;
   changed paths are validated independently rather than inferred from surviving
   filesystem entries.
7. State publication binds to the prepared base/integration snapshots and checks
   the full conflict set before writes. Writes use staged files; runtime write
   failures attempt rollback. A recovery snapshot remains on interruption or
   failed rollback. Worker publication state is persisted before writes and
   blocks unsafe retries/resume. No distributed atomicity is claimed across
   resources. Stale state assignments fail verification rather than overwriting
   later integration changes.

WorkerState construction is centralized in one snapshot helper with explicit
turn-progress data. The worker guide, orchestrator guide and smoke-test claims
now distinguish local directory adapters from actual Linear/Drive API connectors.

Four regression tests were run red before their fixes: duplicate destructive
open, root deletion with wildcard ownership, model-supplied host commands, and a
late publication conflict. Additional tests cover configured-check failure,
rollback after a write error, stale prepared snapshots, coordinator ownership,
resource-specific heads and read-only turn policy. The MCP control smoke test
checks rejection receipts, a single actual Codex turn and its read-only policy.
The worker smoke now uses a separate local coordinator repository alongside the
remote resource repositories. All three specialist smoke scripts are included
in CI alongside the existing transport/collaboration tests.

Final validation after the fixes: 104 tests passed; locked build, formatter,
Clippy with warnings denied, Rust 1.91 all-target check and diff whitespace check
passed. All nine smoke scripts passed: MCP, local agents, room tickets, legacy
worker, collaboration, legacy orchestrator (including interrupted recovery),
worker specialists, multiple orchestrators and specialist control rejection.
The legacy orchestrator smoke caught an early whole-goal check regression during
implementation; the final policy runs only authorized selected checks per
assignment and all configured checks at publication, preserving dependencies.
Changes are in the working tree; no installation, commit or PR was performed in
the specialist-fix pass.
