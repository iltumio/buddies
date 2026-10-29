# Configuration

| Environment variable | Default | Description |
|---------------------|---------|-------------|
| `BUDDIES_USER` | OS username | Your display name in rooms |
| `BUDDIES_AGENT` | `unknown-agent` | Which agent you're using |
| `BUDDIES_DATA_DIR` | `~/.local/share/buddies` | Where local memories are stored |
| `RUST_LOG` | `warn` | Log level (logs go to stderr, never pollutes MCP stdio) |
| `BUDDIES_TRANSPORT` | `stdio` | Transport mode: `stdio` (default, for MCP clients that spawn the process) or `http` (standalone HTTP server) |
| `BUDDIES_PORT` | `8080` | HTTP listen port (only used when `BUDDIES_TRANSPORT=http`) |
| `BUDDIES_HOST` | `127.0.0.1` | HTTP bind address (only used when `BUDDIES_TRANSPORT=http`) |
| `BUDDIES_HEARTBEAT_SECS` | `10` | Presence announcement interval |
| `BUDDIES_SUSPECT_SECS` | `30` | Elapsed time before a peer becomes unreachable |
| `BUDDIES_OFFLINE_SECS` | `90` | Elapsed time before a peer becomes offline |
| `BUDDIES_MCP_IDLE_SECS` | `300` | HTTP session inactivity limit; expired clients must initialize again |
| `BUDDIES_SIGNER` | `git` | Signing identity source: `git`, `none`, `gpg`, `ssh`, `generated` |
| `BUDDIES_GPG_KEY_ID` | unset | GPG key ID when `BUDDIES_SIGNER=gpg` (or use `BUDDIES_SIGNING_KEY`) |
| `BUDDIES_SSH_PRIVATE_KEY` | unset | SSH private key path when `BUDDIES_SIGNER=ssh` |
| `BUDDIES_SSH_PUBLIC_KEY` | inferred | SSH public key value or path when `BUDDIES_SIGNER=ssh` |
| `BUDDIES_SIGNING_KEY` | unset | Generic fallback for `BUDDIES_GPG_KEY_ID` or `BUDDIES_SSH_PRIVATE_KEY` |

## Room transport

`BUDDIES_NET_TRANSPORT` selects communication between peers independently of the
stdio/HTTP MCP setting `BUDDIES_TRANSPORT`.

| Environment variable | Default | Description |
| --- | --- | --- |
| `BUDDIES_NET_TRANSPORT` | `iroh` | `iroh`, `iggy`, or process-local `local` |
| `BUDDIES_IGGY_URL` | required for Iggy | Connection URL; `memory` explicitly selects the nonpersistent in-process test broker |
| `BUDDIES_IGGY_STREAM` | `buddies` | Shared stream containing room topics |
| `BUDDIES_IGGY_PARTITIONS` | `1` | Exactly one partition per room is supported |
| `BUDDIES_IGGY_POLL_MS` | `40` | Poll interval, clamped to 5–5000 ms |
| `BUDDIES_IGGY_REPLAY` | `true` | `false`, `0`, or `latest` disables historical replay |
| `BUDDIES_IGGY_USERNAME` | `iggy` | Username when the URL is supplied as a bare host and port |
| `BUDDIES_IGGY_PASSWORD` | `iggy` | Password when the URL is supplied as a bare host and port |
| `BUDDIES_IGGY_PROTOCOL` | TCP | Protocol suffix when constructing a URL from a bare host and port |

URL lookup falls back to `BUDDIES_IGGY_CONNECTION_STRING`, then
`IGGY_CONNECTION_STRING`. A full URL supplies its own credentials and protocol.
See [Iggy streaming and coordination](./iggy.md) for the compatible server version
and which signed events may be recovered from history.

### Startup identity options

- Use existing git signing identity (default):
  - `BUDDIES_SIGNER=git`
  - Reads `git config user.signingkey` and `git config gpg.format`
- Provide explicit key at startup:
  - GPG: `BUDDIES_SIGNER=gpg BUDDIES_GPG_KEY_ID=<key-id> buddies`
  - SSH: `BUDDIES_SIGNER=ssh BUDDIES_SSH_PRIVATE_KEY=~/.ssh/id_ed25519 buddies`
- Generate identity on first start and reuse it:
  - `BUDDIES_SIGNER=generated buddies`
  - Generates `identity_ed25519` in `BUDDIES_DATA_DIR` and uses it for signing

## Resource limits and shutdown

Distributed searches accept at most 30 seconds; task delegation accepts 1–300
seconds. Long polling is capped at 30 seconds. Each outgoing request registry
allows 128 concurrent requests, and merged searches retain at most 50 results.
Cancellation and broadcast errors release request registrations immediately.

Database work runs outside the async runtime with four concurrent worker slots.
Signing and verification use at most four child processes, with a 10-second
queue/execution deadline; timed-out or cancelled commands are terminated.
Configured signing failures are errors, never an automatic unsigned fallback.

SIGINT/SIGTERM stops HTTP admission, cancels MCP sessions and notifications, and
allows up to five seconds for HTTP connections to finish before stopping watchers,
room receivers, and the Iroh router. The stdio transport also cleans up on EOF.
