# Iggy streaming and file coordination

Use a shared Iggy server to communicate between processes. With the currently
pinned Rust SDK (`iggy = 0.6.210`), the verified server image is
`apache/iggy:0.4.300`. Newer server releases change the binary protocol; do not
substitute `latest` without upgrading and testing the SDK together.

```sh
BUDDIES_NET_TRANSPORT=iggy \
BUDDIES_IGGY_URL='iggy://username:password@127.0.0.1:8090' buddies
```

Supply these environment variables to the Buddies process. `BUDDIES_IGGY_STREAM`
defaults to `buddies`. Exactly one partition per room is supported.
`BUDDIES_IGGY_REPLAY=false` starts after the partition's current high-water mark.
Connection errors redact credentials and connection attempts have a timeout.

The URL is required when selecting Iggy. Setting it explicitly to `memory`
selects a process-local test broker: it neither connects processes nor persists
after process exit. `BUDDIES_IGGY_CONNECTION_STRING` and `IGGY_CONNECTION_STRING`
are accepted aliases for the URL.

Run the real-server smoke test with Docker available:

```sh
bash scripts/smoke_iggy.sh
```

The script creates and removes a disposable server. It verifies publication,
history replay, and live-only subscription offsets. Normal `cargo test` does not
require Docker; its real-server test is ignored unless explicitly selected.

## File activity

`watch_repo` and `set_watch_context` accept agent, worktree, task and sprint
metadata. Empty context values clear overrides. Changing branch reannounces
unchanged dirty files. Failed publications retry during watcher reconciliation.

The watcher caches source contents and dependency results and performs graph
refreshes on the blocking executor. Deleted tracked files remain in the graph
using their `HEAD` contents, including on the first scan after startup.

Rust and TypeScript resolvers are separate modules. Reports expose `limitations`:
this is static module analysis, not a complete compiler or language-server graph.
Generated code, macro expansion, dynamic specifiers, package exports and nested
or inherited TypeScript configurations may need additional analysis.

Large activity metadata is capped; `impact_truncated=true` means consumers must
query `analyze_file_impact` for the full static report. A capped list must not be
interpreted as proof that other files are unaffected.

The original Postcard file-activity variant retains its exact field order.
Buddies publishes that basic variant plus an appended V2 variant, so older peers
still see ordinary diffs and upgraded peers receive the additional metadata.
Older peers do not understand negotiation messages or V2 metadata.

## Negotiation

1. Call `propose_operation` with a branch, or a local checkout from which it can
   be detected. The returned object includes its `revision`.
2. Call `respond_negotiation` with that exact revision. A counter requires a
   nonempty `counter_plan`; accepts cannot introduce a different plan.
3. Reload through `list_negotiations` before deciding on a changed revision.

Proposals are immutable and their IDs are bound to their original owner,
identity and room. Responses have unique IDs. State is derived in a stable order
from the event set, and an accept applies only to the addressed plan hash.
Only the proposer can resolve a proposal. Histories have count and byte limits;
start a new proposal when its history is full.

Tools report publication failures as errors, including the locally saved event
ID. Retry a proposal with the same `proposal_id` and immutable inputs, or retry
a response with the same `response_id` and inputs. A local save is not evidence
that another peer received or accepted an event. Reusing an ID with different
content is rejected.

Signed negotiation events can be recovered from Iggy history for seven days.
They still undergo signature verification, room/owner checks and nonce checks.
Unsigned history, presence, tasks, votes and other live actions are not replayed;
the normal ten-minute freshness window remains in force for live signed traffic.
Configure signing to recover agreements after all participating agents stop.
