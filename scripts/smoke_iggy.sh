#!/usr/bin/env bash
# Disposable real-server regression for the pinned Rust SDK's wire protocol.
set -euo pipefail
cd "$(dirname "$0")/.."
image="${BUDDIES_TEST_IGGY_IMAGE:-apache/iggy:0.4.300}"
container="buddies-iggy-smoke-$$"
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
docker run --detach --rm --name "$container" \
  -e IGGY_TCP_ADDRESS=0.0.0.0:8090 \
  -e IGGY_ROOT_USERNAME=iggy -e IGGY_ROOT_PASSWORD=iggy \
  -p 127.0.0.1::8090 "$image" >/dev/null
port="$(docker port "$container" 8090/tcp | cut -d: -f2)"
python3 - "$port" <<'PY'
import socket, sys, time
for _ in range(100):
    try:
        with socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=.2):
            break
    except OSError:
        time.sleep(.1)
else:
    raise SystemExit('Iggy server did not become ready')
PY
BUDDIES_TEST_IGGY_URL="iggy://iggy:iggy@127.0.0.1:$port" \
  cargo test --locked real_server_publish_replay_and_live_offsets -- --ignored
