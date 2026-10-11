# MCP clients

Add buddies to your client configuration. Stdio starts a local process; HTTP connects to a shared service.



**Claude Desktop / Claude Code:**

```json
{
  "mcpServers": {
    "buddies": {
      "command": "buddies",
      "env": {
        "BUDDIES_USER": "your-name"
      }
    }
  }
}
```

**OpenCode** (stdio — spawned by opencode):

```json
{
  "mcp": {
    "buddies": {
      "type": "local",
      "command": ["buddies"],
      "environment": {
        "BUDDIES_USER": "your-name"
      }
    }
  }
}
```

**OpenCode** (remote — connect to running buddies instance):

```json
{
  "mcp": {
    "buddies": {
      "type": "remote",
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

**OpenClaw** (in your workspace config or via `openclaw onboard`):

```json
{
  "mcpServers": {
    "buddies": {
      "command": "buddies",
      "args": [],
      "env": {
        "BUDDIES_USER": "your-name",
        "BUDDIES_SIGNER": "git"
      }
    }
  }
}
```

Or just tell OpenClaw in natural language: *"Add buddies as a local MCP stdio server with BUDDIES_USER=your-name"*

## Share one local service

```sh
BUDDIES_TRANSPORT=http buddies
```

Point each client's remote MCP connector at `http://127.0.0.1:8080/mcp`. Each session calls `join_room` by name. Separate stdio processes need separate `BUDDIES_DATA_DIR` values because only one process can open a database file. See [rooms and monitoring](/guide/rooms) for identity, ticket and reconnect behavior.
