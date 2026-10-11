# Interactive collaboration

A room provides transport and shared context. Autonomous interaction also needs
a host that starts a model turn, a stable recipient and instructions for handling
the message. A connected MCP client is not necessarily an executor.

## Shared service and addressed messages

Build and install this version before configuring the adapters:

```sh
cargo install --path . --locked
```

Restart the running buddies service after upgrading and reconnect existing MCP
clients so they discover the new tools. Use the newly installed executable in
the channel configuration; an older HTTP service does not expose the inbox tools.

Run one HTTP service for the local clients:

```sh
BUDDIES_TRANSPORT=http buddies
```

Normal MCP sessions join and register explicitly:

```json
{"tool":"join_room","args":{"room":"project"}}
{"tool":"register_participant","args":{"room":"project","name":"codex-client","delivery":"poll","accepts_tasks":false}}
{"tool":"send_message","args":{"room":"project","to":"claude-backend","kind":"question","content":"Does the endpoint return user_id?"}}
```

Use a name unique among live sessions in that room. `get_room_status.participants`
shows named participants, their declared delivery mode and task acceptance. These
are declarations, not proof that a model has read a message. An inbox recipient
must register at least once before messages can be queued for it while offline.

Messages have a server-generated ID, sender, recipient, kind and conversation ID.
Use `in_reply_to` when replying; the service validates the parties and room and
retains the conversation ID. `poll_messages` returns pending/read messages for
your participant. `get_conversation` shows your sent/received messages and states.
Only the recipient can call `acknowledge_message` with `read`, `handled` or
`failed`; terminal receipts cannot be reversed. `handled` means the message was
processed, not necessarily that the whole project task is finished. A clarification
can handle one message while the larger task remains blocked.

The inbox, participant names and receipts survive service restarts. Names are
reclaimed on reconnect after the old session disconnects, while live duplicates
are rejected. History is capped at 512 messages and retained for seven days;
expired entries are removed when sending new messages. A full history returns an
error rather than evicting recent messages. Registration is capped at 512 names.
These names follow the shared service's existing local trust model; they are not
new authentication identities or separate security tenants.

Addressed conversations currently work between clients of the **same HTTP
service**. Existing P2P memories and tasks remain available; addressed inboxes are
not yet replicated across remote nodes.

## Claude Code Channels

Configure a stdio bridge that connects to the HTTP service, rather than opening a
second buddies database. For example, add this entry to the MCP configuration
Claude Code uses:

```json
{
  "mcpServers": {
    "buddies-channel": {
      "command": "/absolute/path/to/buddies",
      "args": ["channel", "--url", "http://127.0.0.1:8080", "--room", "project", "--name", "claude-backend"]
    }
  }
}
```

Start the interactive session with the custom development channel enabled:

```sh
claude --dangerously-load-development-channels server:buddies-channel
```

Claude Code Channels are a research preview. The development flag skips the
plugin allowlist for this entry and presents an interactive confirmation; it does
not bypass organization policy or tool approvals. Use the trusted local bridge
you built. An ordinary MCP registration without channel activation does not
deliver events to Claude's context. See the
[official Channels reference](https://code.claude.com/docs/en/channels-reference).

The bridge declares `claude/channel`, negotiates up to MCP 2025-11-25, and emits
`notifications/claude/channel` over stdio. It automatically joins/registers, polls
the durable inbox once per second and forwards tools to the service using the
same participant session. It also delivers delegated tasks, status updates and
file-conflict notifications. Room/participant membership is owned by the bridge.
It does not acknowledge inbox messages on transport delivery: Claude must use
the receipt tool. Unhandled messages can be delivered again after bridge restart;
inspect conversation history before repeating actions that may have side effects.
Transient status/conflict events have a bounded buffer; durable inbox messages
remain available if transient notifications are lost.

## Managed Codex conversation

For automatic reception, run an App Server conversation:

```sh
buddies agent --room project --name codex-client --cwd /path/to/repo \
  --url http://127.0.0.1:8080 --sandbox workspace-write
```

The default sandbox is `read-only`; select `workspace-write` for implementation.
Codex must be installed and signed in. `--codex-bin` selects the real executable
when PATH contains a wrapper. `--model` is optional. The command prints its Codex
thread ID; use `--thread-id ID` to resume it after restarting. `--once` exits after
one completed turn.

One thread receives successive messages, one new message per turn. Questions,
requests and answers queue for the next turn. `update` messages in the active
conversation steer the current turn; reference an earlier message with
`in_reply_to`. Unrelated updates queue. A confirmed steering rejection queues the
update for the following turn; an uncertain dispatch is failed rather than repeated.
The runtime records `read` when it consumes an inbox entry, before dispatching to
App Server, and `handled` after turn completion. A runtime read receipt therefore
does not prove the model has already reasoned about the message. It sends the final response back to question or
request senders unless the model already replied with a communication tool.
Failed turns produce failed receipts. A read message found after restart is
marked failed rather than automatically repeating an ambiguous interrupted
execution; review possible effects before resending.

The model gets a bounded set of communication tools for the configured room,
including addressed replies, history and background worker delegation. Dynamic
tools require App Server's experimental API capability. Interactive approval/input
requests are unavailable in this unattended runtime. A task needing them must
report the limitation. The runtime and its subprocesses stop on shutdown.

This opens or resumes a **managed conversation**; it does not take over an
unrelated Codex CLI already open in another terminal. A normal Codex MCP session
can use the same messages with explicit polling while active. Attaching automatic
delivery to an existing interactive UI requires that UI's integration surface.
See [Codex App Server](https://learn.chatgpt.com/docs/app-server).

## Collaboration protocol

1. Identify the shared goal, your responsibility and dependencies.
2. Ask the responsible participant before changing a shared contract.
3. Read the relevant conversation before acting on a reply.
4. Reply to questions/requests or explain the blocker; mark the message's outcome.
5. Apply answers to the existing task and verify the effect.
6. Treat status updates as context. Avoid acknowledgement ping-pong.

For a manual client, check the inbox before work, at dependency changes and before
finishing. These instructions improve active turns; they cannot wake an idle
client without host support.

## Verification

```sh
cargo build --locked
PYTHONDONTWRITEBYTECODE=1 python3 scripts/smoke_collaboration.py
```

The deterministic test exercises actual service/bridge processes and an App
Server fixture: a question triggers clarification, the answer continues the same
thread and changes an artifact, and an update steers active work. It also checks
task delivery, receipt authorization and inbox/history persistence after restart.
It does not establish the behavior of a particular real model or demonstrate
that Claude Code's channel has been enabled in a user's interactive session.

An opt-in check uses the signed-in Codex account, consumes model usage, and tests
real dynamic tool calls plus retained context against a simulated peer:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/smoke_live_codex.py --codex-bin /absolute/path/to/codex
```

For a real-client acceptance check, give Claude responsibility for the API contract
and the managed Codex participant responsibility for its consumer in a temporary
repository. Require clarification about a missing field. Completion requires the
question, reply, retained context and a verified implementation, with no human
relaying messages. Record outcomes and tool/turn timestamps, not just delivery.
