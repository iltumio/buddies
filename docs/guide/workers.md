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
