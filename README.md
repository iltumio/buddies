<p align="center">
  <img src="docs/public/logo.png" width="160" alt="buddies — two connected speech bubbles" />
</p>

<h1 align="center">buddies</h1>

<p align="center">A local communication and orchestration layer for AI agents.</p>

<p align="center">
  <a href="https://iltumio.github.io/buddies/">Documentation</a> ·
  <a href="https://iltumio.github.io/buddies/guide/getting-started.html">Quick start</a> ·
  <a href="https://github.com/iltumio/buddies/releases">Releases</a> ·
  <a href="https://iltumio.github.io/buddies/agents.html">For AI agents</a>
</p>

Connect your team's agents through rooms: share memories and signed skills, exchange questions and answers, delegate tasks and see overlapping repository edits. Buddies runs locally, stores data in redb and connects peers over encrypted Iroh gossip by default. An optional Apache Iggy transport uses a shared message stream for communication between processes and recovery of signed negotiation events.

For goals with dependencies, `buddies orchestrator` retains a coordinator and specialist conversations, persists assignment revisions and checks results in isolated git worktrees before applying a verified patch.

## Get started

Build from source with **Rust 1.91 or newer**:

```sh
git clone https://github.com/iltumio/buddies.git
cd buddies
cargo install --path . --locked
```

Alternatively, use the [installer](https://iltumio.github.io/buddies/guide/getting-started.html) or download a [Linux release binary](https://github.com/iltumio/buddies/releases). Build from `main` for features that are not in your installed release yet.

Add a local MCP server to your client:

```json
{
  "mcpServers": {
    "buddies": {
      "command": "buddies",
      "env": { "BUDDIES_USER": "your-name" }
    }
  }
}
```

Have your agent call `join_room({"room":"project"})`. To connect another machine, pass the returned ticket to its first `join_room` call. For concurrent local clients, run one HTTP service and configure each connector with `http://127.0.0.1:8080/mcp`:

```sh
BUDDIES_TRANSPORT=http buddies
```

See [client setup](https://iltumio.github.io/buddies/guide/clients.html) for Claude, OpenCode and OpenClaw configurations.

## Choose a runtime

| Command | Purpose |
| --- | --- |
| `buddies` | MCP communication service over stdio or HTTP |
| `buddies monitor` | Terminal view of rooms, participants and network state |
| `buddies channel` | Bridge a shared HTTP service into Claude Code Channels |
| `buddies agent` | Managed Codex conversation with a retained thread and addressed inbox |
| `buddies worker` | Execute independent delegated tasks in separate Codex threads |
| `buddies orchestrator` | Coordinate a persistent goal, ownership, dependencies and verified integration |

The ordinary MCP service starts no model. Managed Codex runtimes require Codex installed and signed in; Claude Channels require activation in the client.

## Coordinate repository changes

Use `watch_repo` to share uncommitted diffs with branch, worktree, agent, task and sprint context. `analyze_file_impact` reports static Rust and TypeScript dependencies; conflict notifications cover both edits to the same file and transitive impact. Agents can use `propose_operation`, `respond_negotiation` and `list_negotiations` to agree on interfaces, file ownership or execution order.

To use a shared Iggy server instead of Iroh, configure every participating process:

```sh
BUDDIES_NET_TRANSPORT=iggy \
BUDDIES_IGGY_URL='iggy://username:password@127.0.0.1:8090' buddies
```

The pinned SDK is tested with `apache/iggy:0.4.300`. See [Iggy streaming and coordination](docs/reference/iggy.md) for server compatibility, signing and replay boundaries, and [repository awareness](docs/guide/repository-awareness.md) for the coordination workflow.

## Run a persistent goal

With the HTTP service running, start from a clean, committed repository:

```sh
buddies orchestrator --room project --repo /path/to/repo \
  --goal 'Implement and verify the producer/consumer contract' \
  --allow-path src/producer --allow-path src/consumer \
  --check 'cargo test --locked' --max-active-specialists 2
```

Assignments retain their owners and revisions. Specialists communicate through durable inboxes and work in separate worktrees. Verified results release dependencies; shared checks gate publication to the original checkout. Use the same command with `--resume` to inspect and recover an interrupted goal. See the [orchestrator guide](docs/orchestrator.md) for budgets and recovery boundaries.

## Documentation

The [VitePress site](https://iltumio.github.io/buddies/) includes setup, rooms, memory, signed skills, delegation, collaboration, orchestration, configuration and architecture. Source Markdown lives in [docs](docs).

Agents can fetch the [topic index](https://iltumio.github.io/buddies/llms.txt), [full Markdown bundle](https://iltumio.github.io/buddies/llms-full.txt), or any published page with a `.md` suffix. These exports are generated from the site's sources on every build.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets

# Documentation (Node.js 22+)
npm ci
npm run docs:dev
npm run docs:build
```

See the [development guide](docs/contributing.md) for smoke tests, wire-format compatibility and release automation. Documentation pull requests are built in CI; pushes to `main` deploy to GitHub Pages.

## License

[MIT](LICENSE)
