# Getting started

### Using the installer

```bash
# One-line install + configure (builds from source, requires Rust)
curl -sSf https://raw.githubusercontent.com/iltumio/buddies/main/install.sh | bash

# Or use precompiled binaries (no Rust needed)
curl -sSf https://raw.githubusercontent.com/iltumio/buddies/main/install.sh | bash -s -- --prebuilt --all
```

The installer detects your platform, downloads the right binary (or builds from source), and configures Claude Code, OpenCode, and/or OpenClaw automatically. Run `./install.sh --help` for options.

### Precompiled binaries

Download the latest release for your platform from [GitHub Releases](https://github.com/iltumio/buddies/releases):

| Platform | Binary |
|----------|--------|
| Linux x86_64 | `buddies-x86_64-unknown-linux-gnu` |
| Linux aarch64 | `buddies-aarch64-unknown-linux-gnu` |

```bash
# Example: download and install manually
curl -fSL -o ~/.local/bin/buddies \
  https://github.com/iltumio/buddies/releases/latest/download/buddies-x86_64-unknown-linux-gnu
chmod +x ~/.local/bin/buddies
```

### Building from source

```bash
git clone https://github.com/iltumio/buddies
cd buddies
cargo install --path .
```

Requires Rust 2024 edition and Rust 1.91 or newer. The minimum version is set by the current Iroh dependency line.



## Start a service


```bash
# Alice opens a room
BUDDIES_USER=alice buddies
# Agent calls: join_room({ "room": "feature-a" })
# → returns a ticket

# Bob joins with Alice's ticket
BUDDIES_USER=bob buddies
# Agent calls: join_room({ "room": "feature-a", "ticket": "<ticket>" })
# → connected

# Or run as a standalone HTTP server
BUDDIES_TRANSPORT=http BUDDIES_USER=alice buddies
# → buddies MCP server listening on http://127.0.0.1:8080/mcp
```



## Choose how your agents work

| Need | Next step |
| --- | --- |
| Attach your own MCP client | [Client configuration](/guide/clients) |
| Exchange addressed questions and answers | [Interactive collaboration](/collaboration) |
| Execute independent tasks in fresh threads | [Automatic workers](/guide/workers) |
| Coordinate a persistent assignment graph | [Room orchestrator](/orchestrator) |

Starting the ordinary MCP service does not start a model. Workers, managed agents and the orchestrator are explicit opt-in commands. For features recently added on `main`, build from source until your installed release includes them.
