# AGENTS.md

Guidance for agentic coding assistants operating in this repository.

## Project Snapshot

- Name: `smemo`
- Language: Rust (edition 2024)
- Runtime: Tokio async runtime
- Purpose: MCP server for P2P shared memory and delegation over iroh/iroh-gossip
- Entry point: `src/main.rs`

## Repo Layout

- `src/main.rs`: process bootstrap, env config, server startup/shutdown
- `src/server.rs`: MCP tool surface (`join_room`, `search_memory`,
  `delegate_task`, identity policy tools, etc.)
- `src/identity.rs`: local signer discovery (git/env/generated),
  SSH/GPG signing and verification helpers
- `src/room.rs`: room lifecycle, gossip receive loop, distributed search,
  delegated task queue, signature verification, whitelist enforcement
- `src/node.rs`: endpoint/router/gossip/storage assembly
- `src/storage.rs`: redb-backed memory persistence and search
- `src/protocol.rs`: P2P wire message enums and topic derivation
- `src/memory.rs`: memory models, filtering, kind parsing
- `src/ticket.rs`: room ticket serialization/parsing

## Build, Run, Lint, Test Commands

This repo uses Cargo directly (no Makefile, no npm scripts,
no CI workflow file in repo).

### Core Commands

- Build (dev): `cargo build`
- Build (release): `cargo build --release`
- Run local server: `cargo run`
- Install binary from local path: `cargo install --path .`
- Format check: `cargo fmt --all -- --check`
- Format write: `cargo fmt --all`
- Lint (strict): `cargo clippy --all-targets --all-features -- -D warnings`
- Test all: `cargo test`

### Single Test Execution

Current unit tests live in `src/protocol.rs` and `src/storage.rs`.
Use these command forms:

- Run tests matching a name: `cargo test <test_name_substring>`
- Run one integration test target:
  `cargo test --test <integration_test_file_stem>`
- Run one exact test and show stdout:
  `cargo test <exact_test_name> -- --exact --nocapture`

Examples:

- `cargo test signer_identity_parse_and_label_roundtrip -- --exact`
- `cargo test storage::tests::search_applies_query_and_filters -- --exact`

### Useful Verification Commands

- Typecheck all targets: `cargo check --all-targets`
- Dependency lock/update check: `cargo check --locked`
- Optional docs build: `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`

## Configuration and Runtime Notes

- Required Rust toolchain capability: Rust 1.85+ (edition 2024)
- Logging is configured via `tracing_subscriber` and writes to stderr
- Important env vars:
  - `SMEMO_USER` (default: OS username)
  - `SMEMO_AGENT` (default: `unknown-agent`)
  - `SMEMO_DATA_DIR` (default: `~/.local/share/smemo` equivalent)
  - `RUST_LOG` (default filter fallback: `warn`)
  - `SMEMO_TRANSPORT` (`stdio` default, or `http` for standalone HTTP server)
  - `SMEMO_PORT` (default: `8080`, HTTP listen port when `SMEMO_TRANSPORT=http`)
  - `SMEMO_HOST` (default: `127.0.0.1`, HTTP bind address when `SMEMO_TRANSPORT=http`)
  - `SMEMO_SIGNER` (`git`, `none`, `gpg`, `ssh`, `generated`)
  - `SMEMO_GPG_KEY_ID` / `SMEMO_SIGNING_KEY` (for `SMEMO_SIGNER=gpg`)
  - `SMEMO_SSH_PRIVATE_KEY` / `SMEMO_SIGNING_KEY` (for `SMEMO_SIGNER=ssh`)
  - `SMEMO_SSH_PUBLIC_KEY` (optional pubkey value/path for SSH mode)

## Coding Style and Patterns

Follow existing style in `src/*.rs`.

### Naming

- Types/traits/enums: PascalCase (`SmemoNode`, `MemoryKind`, `P2PMessageBody`)
- Functions/methods/fields/modules: snake_case
  (`search_distributed`, `room_manager`)
- Constants: UPPER_SNAKE_CASE (`MAX_PENDING_TASKS`, `MEMORIES_TABLE`)
- Enum variants: PascalCase (`TaskResult::Success`, `MemoryKind::Decision`)

### Imports

- Keep imports grouped by std / external crate / local crate
- Use explicit `use` lines; avoid wildcard/glob imports
- Internal modules should use `crate::...` paths
- Preserve readability over micro-optimizing import grouping

### Formatting

- Use `rustfmt` defaults (no repo-local rustfmt config present)
- Favor short, readable lines and trailing commas in multiline structs/enums
- Keep chained calls vertically aligned when they span lines

### Types and API Shapes

- Use concrete structs for request/response payloads in MCP tools
- Derive traits explicitly (`Debug`, `Clone`, `Serialize`, `Deserialize`,
  `JsonSchema`) as needed
- Prefer domain types (`Uuid`, enums) over plain strings when practical
- Keep public API fields `pub` only where external access is required

### Async and Concurrency

- Tokio is the default async model; use `async/await` idiomatically
- Shared mutable state should use `Arc<...>` + `RwLock`/`Mutex`
  as established in `room.rs`
- Use channels intentionally:
  - `oneshot` for one-response waiters
  - `mpsc` for streamed/aggregated responses
  - `Notify` for wake-up signals

### Error Handling

- Primary error type is `anyhow::Result` in internal layers
- Use `?` for propagation; attach context when it improves debuggability
- Convert to MCP-facing errors at boundaries (see helpers in `src/server.rs`)
- Avoid `unwrap()` in non-obviously-safe paths;
  prefer propagating or mapping errors
- `expect()` is acceptable only for truly infallible assumptions
  with clear messages

### Logging and Observability

- Use `tracing` macros (`debug!`, `info!`, `warn!`) with structured fields
- Include identifiers in logs (room name, task ID, peer/source) for traceability
- Keep logs concise and useful for distributed debugging

### Serialization and Wire Protocol

- Gossip payloads use postcard byte serialization (`protocol.rs`)
- Tool I/O returned to MCP clients is JSON text (pretty-printed in helper)
- `P2PMessage` now carries optional signature metadata
  (`signed_by`, `signature`)
- Preserve backward compatibility of P2P message enum variants when possible

### Identity and Signature Policy

- Incoming room messages are validated in `RoomManager::verify_incoming_message`
- Whitelist identities are represented by `SignerIdentity`
  (`gpg:<key-id>`, `ssh:<public-key>`)
- If whitelist is non-empty, non-whitelisted identities are rejected
- If `require_signed=true` for a room, unsigned messages are rejected
- Outgoing room messages are signed when local signer discovery succeeds

### Storage Patterns

- redb is the persistence backend (`Storage` in `src/storage.rs`)
- Maintain table definitions as constants
- Keep tx lifecycle explicit: begin -> open table -> read/write -> commit
- Search/list behavior currently sorts by descending timestamp

## MCP Tool Implementation Conventions

- Tool handlers are implemented in `SmemoServer` with rmcp macros
- Add new tools via `#[tool(...)]` under the existing
  `#[tool_router]` impl block
- Request structs should include `JsonSchema` and clear field docs
  when semantics are non-obvious
- Use central JSON/error helpers (`ok_json`, `err`) for consistency
- Identity policy tools currently include:
  - `set_identity_policy`
  - `add_whitelisted_identity`
  - `get_identity_policy`

## Testing Guidance for New Code

New work should include targeted tests when feasible:

- Unit tests near modules (`#[cfg(test)] mod tests`) for pure logic
- Async behavior tests with `#[tokio::test]`
- Storage behavior tests should avoid flakiness and assert ordering/filtering
- Run at minimum: `cargo test` and
  `cargo clippy --all-targets --all-features -- -D warnings`

## Rules Files Check

Checked and found no repository-specific rule files at analysis time:

- `.cursorrules`: not present
- `.cursor/rules/`: not present
- `.github/copilot-instructions.md`: not present

If these files are added later, treat them as higher-priority
local instructions and update this AGENTS.md accordingly.

## Agent Working Agreement for This Repo

- Keep changes minimal and scoped; avoid broad refactors during bugfixes
- Match existing abstractions before introducing new layers
- Do not add dependencies unless clearly justified
- Preserve MCP tool contract compatibility and message semantics
- Verify with build/lint/test commands before finalizing work
