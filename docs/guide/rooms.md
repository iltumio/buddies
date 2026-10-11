# Rooms and monitoring

Run `buddies monitor` to inspect a running HTTP server in a Ratatui terminal UI:

```sh
BUDDIES_TRANSPORT=http buddies
# In another terminal:
buddies monitor
# Custom endpoint, or a single machine-readable snapshot:
buddies monitor --url http://127.0.0.1:8080/mcp
buddies monitor --once
```

The monitor shows joined rooms, known P2P agents and their last reported status,
plus active local MCP sessions (client name/version). Use ↑/↓ or j/k to select a
room, PgUp/PgDn to browse its agents, and q, Esc or Ctrl-C to exit. It refreshes
every second and retries automatically if the server is unavailable, marking
previous data as stale. P2P membership is last known state, not proof that a peer
is still online; MCP sessions are separate from P2P identities.

Timing values accept 1–86400 seconds, with heartbeat < suspect < offline.

Presence is refreshed by periodic `Join` announcements (10 seconds by default),
without resetting the agent's last status. The monitor and `get_room_status` show
`online`, `unreachable` after 30 seconds, and `offline` after 90 seconds without
an accepted announcement/message. Elapsed time is measured locally. Signed
messages must pass signature and replay checks; unsigned rooms still allow
unauthenticated presence. Offline entries are retained for at least one hour.
Older peers without periodic announcements can appear offline while idle.

Rooms show `waiting`, `connected`, or `reconnecting`, direct-neighbor counts,
retry counts and the last transport error. A room without neighbors can be
valid (for example, its first member). Joining registers the room immediately;
bootstrap discovery continues in the background. Failed subscriptions are
recreated, and known peers retried with exponential delays of 1–30 seconds plus
up to 500 ms of jitter. Leaving a room or shutting down cancels these retries.

HTTP sessions expire after five minutes without an incoming MCP request.
Polling `/status` does not extend a session. Expired session IDs return HTTP 404;
the MCP client must initialize a new session. Session cleanup also releases
notification listeners, even when no further notifications arrive. Configure a
longer idle timeout for clients that only listen for notifications for long
periods. Reinitializing a client keeps the running node's rooms and database.
Restarting the **server** still requires clients to rejoin rooms. Saved tickets
retain connection information; generic P2P task replay is not provided by this
recovery layer. Durable local worker tasks have separate recovery semantics.

`--url` also accepts `BUDDIES_URL`; the default is `http://127.0.0.1:8080`.
Monitoring reads `GET /status` and never opens a database or generates an
identity. Keep the HTTP server bound to loopback for local use: both `/mcp` and
`/status` are available on its configured listener.

To share buddies between Codex, Claude and concurrent sessions, run **one** HTTP
server and configure every client with its `http://127.0.0.1:8080/mcp` URL.
Each initialized session receives a distinct `local:<uuid>` agent ID, including
clients with identical names. Each agent must call `join_room`; the response
includes its `agent_id`. `list_rooms` lists that session's memberships, and
`get_room_status` lists other local participants plus remote P2P peers. The
monitor includes everyone and labels each participant `local` or `remote`.
A room can have active local participants and zero network neighbors.

Room tickets belong to the buddies process, not to individual agents. Use
`join_room({"room":"my-project"})` on every local client; they all receive the
same process-owned ticket. To connect an external room, one client supplies
`join_room({"room":"my-project","ticket":"<external-ticket>"})` once.
Buddies validates the room and topic, merges the external endpoints, and saves
the ticket in its database. All subsequent clients join by name, even after
leaving the room or restarting the service. A new external ticket can update
peer addresses while the room is already active. Importing a ticket does not
replace the memberships of other local agents.

The transport identity is now stored in the same database, so endpoint IDs
survive restarts. Ticket addresses are refreshed for the local endpoint after
restart; remote relay and direct addresses from imported tickets are retained.
The first start with this version creates a persistent transport identity;
previously issued tickets may need to be exchanged once. Keep the data directory
to retain tickets and identity. Rooms are rejoined on demand, and new MCP
sessions still call `join_room` by name. A saved ticket supplies connection
information; it cannot make an offline external process reachable.

`notify_peers` updates the sender's local status and sends
`notifications/buddies/status` to other local members. `delegate_task` assigns
one other local member that explicitly accepts tasks, excluding the sender;
otherwise it uses P2P delegation when no other local members are connected.
Connected local members without an executor declaration produce an immediate
error rather than a silent assignment to an idle session.
The assignee receives `notifications/buddies/taskArrived` and
can also use `poll_pending_tasks`. Results are checked against assignee, room
and source agent; late or duplicate local results are rejected. Local tasks
are cancelled on timeout, departure, disconnection or request cancellation.
They are not automatically re-executed after failure.

Leaving a room removes only that session; the network subscription is closed
when the last explicit local member leaves. Disconnects remove local presence
without tearing down shared room subscriptions. A new session must rejoin.
`watch_repo` also registers its caller in the actual watched room. The node's
cryptographic identity, database and repository watchers remain shared: these
session IDs are participant identities, not separate security tenants. Remote
nodes still see the shared node's P2P identity.

Separate stdio processes must use separate data directories because the database
permits only one open instance per file. Back up existing directories before
switching; changing the client URL does not merge their previous databases.

For Linux/systemd user sessions, a service template is provided (adjust `ExecStart`
if the binary is installed outside `~/.cargo/bin`). Stop existing stdio instances
using the same database before starting the service:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/systemd/buddies.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now buddies.service
buddies monitor
```

The service starts with your user session. Check logs with
`journalctl --user -u buddies.service`; restart it after upgrading the binary.
