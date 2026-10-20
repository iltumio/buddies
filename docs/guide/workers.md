# Automatic Codex workers

A normal MCP connection does not guarantee that the host starts an agent turn
when it receives `notifications/buddies/taskArrived`. Start an executor to handle
tasks without asking the interactive agent to poll:

```bash
buddies worker --agent codex --room my-project --cwd /path/to/repo
```

The worker connects to the running buddies HTTP service, joins by name, and polls
its durable queue automatically. It uses Codex App Server to start a separate
thread for each task and submits the final result. Codex must already be installed
and signed in. Use `--codex-bin /absolute/path/to/codex` if PATH uses a wrapper.
This does not wake or take over an existing interactive Codex conversation.

`--sandbox workspace-write` is the default; use `--sandbox read-only` for review
or testing. The worker runs without interactive approvals and fails requests
that require user input. `--model` is optional; otherwise Codex uses its configured
model. Only start a worker in a room whose delegated work you intend to execute
in that repository. The child Codex process has the buddies MCP connector disabled
to avoid recursively accepting tasks.

Call `delegate_task` from a room member as usual: live workers are preferred over
interactive local agents. Use `target_agent: "codex"` (or the stable worker ID from
`get_room_status.workers`) to require a worker. With `background: true`, the call
returns `task_id` immediately; call `get_task_status({room, task_id})` for its state
and outcome. Without `background`, the call waits and returns the output to the
requesting agent. The timeout is the whole task deadline, including queue time
(1–300 seconds). Worker routing currently handles delegations submitted to that
same buddies service; incoming P2P tasks retain their existing MCP polling path.

Worker tasks have durable `queued`, `running`, `completed` and `failed` states.
The server commits an exclusive claim before delivery; claim retries return the
same task and completed results cannot be overwritten. Queued tasks and outcomes
survive MCP disconnects and server restarts. A worker renews its running lease;
expired leases and deadlines fail the task. An interrupted execution is never
automatically repeated because it may already have changed files. Failure is not
a rollback of side effects. Finished tasks are retained for seven days; the queue
is bounded to 128 unfinished tasks and 512 total records.

The worker keeps a private identity and execution/result journal under
`~/.local/share/buddies-workers/` by default, separate from the server database.
`--data-dir` selects an explicit worker directory; only one process may open it.
Keep this directory to retain identity and retry unacknowledged results after a
restart. Its identity is bound to the server URL, room and repository. `--once`
exits after one acknowledged task. The monitor labels these participants `worker`.

For continuous operation, adapt
[`contrib/systemd/buddies-worker.service`](https://github.com/iltumio/buddies/blob/main/contrib/systemd/buddies-worker.service)
and install it as a user service. The service terminates the execution process
group on shutdown. The HTTP service's existing loopback/trust assumptions also
apply to its `/worker` endpoint; worker credentials identify their own claims.


## Addressed specialists

Providing `--name` or `--resource` selects the persistent specialist runtime:

```sh
buddies worker --room my-project --name backend \
  --resource git:backend=/path/to/backend --allow-scope src --check 'cargo test'
```

These workers collaborate through addressed messages and isolated assignments,
with a coordinator (`worker --orchestrator`) or directly with peers. See
[resource specialists and publication](../orchestrator.md#resource-specialists-and-publication)
for ownership, check execution, resume and publication guarantees. Restart with
the original arguments plus `--resume`; a second process cannot open the same
resource state. Non-Git resource kinds operate on local directory snapshots.

## Live Linear with Claude Code

A Claude worker reuses the Linear tools and authentication already configured in
Claude Code. It adds only a Buddies channel; it does not replace existing MCP
servers, copy credentials, or bypass tool permissions.

```sh
buddies worker --agent claude --room zyphe-prod --name linear \
  --linear-team ENG --linear-team ZYP --cwd /path/to/your/project
```

Run this in an interactive terminal and accept Claude's development-channel
confirmation. Channels must be enabled by your organization. Keep this terminal
open to receive room messages. Use `--claude-bin /absolute/path/to/claude` if your
PATH contains a wrapper. `--cwd` selects the project whose MCP configuration and
instructions Claude should load; it defaults to the current directory.

`--linear-access auto` (default) follows the session's existing tools and project
instructions. Choose `--linear-access mcp` for existing Linear MCP tools, or
`--linear-access cli` for an authenticated `linear` CLI. Startup asks Claude to
verify access with a read-only lookup and report a blocker if unavailable. No
Linear credentials are requested or stored by Buddies.

Restart with the same arguments plus `--resume` to resume the exact saved Claude
session. Session identifiers and a process lock live under
`~/.local/share/buddies-claude-workers/` (or `--data-dir`). A resume with no saved
session fails; omit `--resume` on first start. If Claude exits before creating its
conversation (for example, you decline channel consent), a saved identifier may
exist without a Claude conversation. Use a fresh `--data-dir` without `--resume`
in that case; keep the old directory for inspection.

Send addressed requests to `linear` using `send_message`, or use delegated tasks.
The participant advertises `linear-live` resources, with team keys as scopes.
It is not a staged orchestrator specialist: `Dispatch`, `PreparePublish`, and
`Publish` are unsupported. Writes use Claude's native tools and take effect
immediately. The worker is instructed to verify the actual issue team, inspect
receipts before retries, and read back changes before acknowledging success.
Team scopes are model instructions, not enforced API permissions; use appropriately
scoped credentials and Claude tool permissions. `--resource`, local acceptance
checks, `--once`, and the Codex sandbox are not used in this mode.

### Use an existing Claude configuration

To use the same integration in a Claude session you start yourself, add this
entry alongside your existing Linear MCP configuration, then restart Claude:

```json
{
  "mcpServers": {
    "buddies-linear": {
      "command": "/absolute/path/to/buddies",
      "args": ["channel", "--room", "zyphe-prod", "--name", "linear", "--linear-team", "ENG"]
    }
  }
}
```

```sh
claude --dangerously-load-development-channels server:buddies-linear
```

This integrates the tools available in that Claude session. Buddies cannot attach
tools from another already-running Claude process. Do not run both launch methods
with the same room/name simultaneously. See the [Claude Channels reference](https://code.claude.com/docs/en/channels-reference).

## Start a group from TOML

Define the group in `buddies.toml`, then validate and launch it:

```sh
buddies up --config buddies.toml --check
buddies up --config buddies.toml
# Subsequent starts, after each worker has saved its session:
buddies up --config buddies.toml --resume
```

```toml
room = "my-project"
url = "http://127.0.0.1:8080"

[defaults]
codex-bin = "codex"

[[specialists]]
name = "backend"
resource = ["git:backend=./backend"]
check = ["cargo test"]

[[specialists]]
name = "frontend"
resource = ["git:frontend=./frontend"]
check = ["pnpm test"]

[[specialists]]
name = "linear"
agent = "claude"
cwd = "./backend"
linear-team = ["ENG"]
```

The HTTP service must already be running. `up` requires **tmux** and stays in the
foreground as the group's supervisor. Each worker gets a separate terminal in a
private tmux server, including Claude's interactive channel consent and tool
permissions. Run the printed `tmux -L ... attach -t specialists` command in another
terminal; use `Ctrl-b n` to switch workers and `Ctrl-b d` to detach. Detaching does
not stop the workers. Process startup is not a claim that authentication or room
registration has succeeded; inspect each worker's output and the monitor.

Ctrl-C or SIGTERM to `buddies up` interrupts every worker, allows up to ten seconds
for shutdown, then closes the private tmux server. If any worker exits (even with
status zero, including `once = true`), the supervisor prints its last terminal
output, stops the remaining workers, and exits with an error. It never restarts
workers automatically or replays work. Other tmux servers are unaffected.

Every `[[specialists]]` entry requires `name`. Worker settings use exactly the
long option names from `buddies worker --help`, without `--`: for example
`allow-scope = ["src", "tests"]`, `check-timeout-secs = 120`, `model = "..."`,
`orchestrator = true`, `goal = "..."`, and `data-dir = "./state/backend"`.
Repeatable options (`resource`, `check`, `allow-scope`, `linear-team`) accept
arrays. Booleans use TOML `true`/`false`. Unknown options and invalid CLI values
are errors. `[defaults]` accepts worker options except `name`; per-specialist
values replace defaults, including entire arrays. Top-level `room` is required;
`url` is optional (otherwise `BUDDIES_URL` or the usual CLI default applies).
An entry can override room and URL. Names must be unique per server and room.
Give workers distinct `data-dir` values if setting them explicitly.

Paths are relative to the **configuration file**, regardless of the invocation
directory; `~/` is expanded. Bare executable names use PATH. Without `cwd`, the
configuration directory is used. Environment-variable and shell interpolation
are not performed in configuration values. Acceptance commands in `check` keep
the normal worker semantics and are executed only by the worker when required.

`--check` parses every entry, checks paths and worker option compatibility, and
prints the resolved arguments without launching tmux, models, or connections.
It does not verify authentication, server availability, saved-session bindings,
or that a participant name is free. `--resume` forces resume for every entry;
for a mix of existing and new workers, set `resume = true` only on existing
entries instead. Existing individual `buddies worker` commands remain supported.
