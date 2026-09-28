#!/usr/bin/env python3
"""Linux MCP smoke test with temporary data, an active watcher and an open SSE stream.
Run `cargo build --locked` first, then `python3 scripts/smoke_mcp.py`.
"""
import http.client
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import tempfile
import time
import urllib.request
import urllib.parse


def main():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"]
    ))
    binary = Path(metadata["target_directory"]) / "debug/buddies"
    package = next(p for p in metadata["packages"] if p["name"] == "buddies")

    def check_server_info(result):
        assert result["serverInfo"]["name"] == package["name"], result
        assert result["serverInfo"]["version"] == package["version"], result

    with tempfile.TemporaryDirectory(prefix="buddies-smoke-") as temp:
        root = Path(temp)
        repo = root / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        (repo / "untracked.txt").write_text("local fixture\n")
        env = dict(os.environ, BUDDIES_DATA_DIR=str(root / "http-data"),
                   BUDDIES_SIGNER="none", BUDDIES_TRANSPORT="http",
                   BUDDIES_HOST="127.0.0.1", BUDDIES_PORT="0", BUDDIES_USER="smoke-test",
                   BUDDIES_MCP_IDLE_SECS="3")
        process = subprocess.Popen([str(binary)], env=env, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.PIPE)
        sse = None
        try:
            deadline = time.monotonic() + 20
            output = b""
            url = None
            while time.monotonic() < deadline:
                if select.select([process.stderr], [], [], 0.1)[0]:
                    chunk = os.read(process.stderr.fileno(), 65536)
                    if not chunk:
                        raise AssertionError(output.decode())
                    output += chunk
                    for line in output.decode().splitlines():
                        if "listening on http://" in line:
                            url = line.split("listening on ", 1)[1]
                            break
                    if url:
                        break
            assert url, "HTTP startup timeout"
            headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
            next_id = 0

            def rpc(method, params, notification=False):
                nonlocal next_id
                next_id += 1
                body = {"jsonrpc": "2.0", "method": method, "params": params}
                if not notification:
                    body["id"] = next_id
                req = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers)
                with urllib.request.urlopen(req, timeout=10) as response:
                    session = response.headers.get("Mcp-Session-Id")
                    if session:
                        headers["Mcp-Session-Id"] = session
                    if notification:
                        assert response.status == 202
                        return None
                    if "text/event-stream" in response.headers.get("Content-Type", ""):
                        while True:
                            line = response.readline()
                            assert line, "SSE closed before JSON-RPC response"
                            if line.startswith(b"data: ") and line[6:].strip():
                                message = json.loads(line[6:])
                                if message.get("id") == next_id:
                                    break
                    else:
                        message = json.load(response)
                    assert "error" not in message, message
                    assert not message["result"].get("isError"), message
                    if method == "initialize":
                        check_server_info(message["result"])
                    return message["result"]

            rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                               "clientInfo": {"name": "smoke-test", "version": "1"}})
            headers["MCP-Protocol-Version"] = "2025-03-26"
            rpc("notifications/initialized", {}, notification=True)
            assert len(rpc("tools/list", {})["tools"]) == 23
            rpc("tools/call", {"name": "join_room", "arguments": {"room": "smoke"}})
            rpc("tools/call", {"name": "watch_repo", "arguments": {
                "room": "smoke", "repo_path": str(repo), "repo_name": "fixture"}})
            # A second MCP client shares this node and database.
            first_headers = headers.copy()
            headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
            rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                               "clientInfo": {"name": "second-client", "version": "1"}})
            headers["MCP-Protocol-Version"] = "2025-03-26"
            rpc("notifications/initialized", {}, notification=True)
            assert json.loads(rpc("tools/call", {"name": "list_rooms", "arguments": {}})["content"][0]["text"])["rooms"] == []
            rpc("tools/call", {"name": "join_room", "arguments": {"room": "smoke"}})
            result = rpc("tools/call", {"name": "list_rooms", "arguments": {}})
            assert "smoke" in json.loads(result["content"][0]["text"])["rooms"]
            snapshot = json.loads(subprocess.check_output(
                [str(binary), "monitor", "--url", url, "--once"],
                env=dict(env, BUDDIES_DATA_DIR="/nonexistent/monitor-must-not-open-db")))
            assert [r["name"] for r in snapshot["rooms"]] == ["smoke"], snapshot
            assert {c["name"] for c in snapshot["clients"]} == {"smoke-test", "second-client"}, snapshot
            request = urllib.request.Request(url, method="DELETE", headers=headers)
            with urllib.request.urlopen(request, timeout=10) as response:
                assert response.status in (200, 202, 204)
            deadline = time.monotonic() + 5
            while True:
                with urllib.request.urlopen(url.removesuffix("/mcp") + "/status", timeout=3) as response:
                    snapshot = json.load(response)
                if len(snapshot["clients"]) == 1:
                    break
                assert time.monotonic() < deadline, snapshot
                time.sleep(0.05)
            headers = first_headers
            print("Shared MCP clients/status/monitor JSON/session disconnect: OK")
            # An abandoned client must expire even while /status is polled.
            deadline = time.monotonic() + 8
            while True:
                with urllib.request.urlopen(url.removesuffix("/mcp") + "/status", timeout=3) as response:
                    snapshot = json.load(response)
                if not snapshot["clients"]:
                    break
                assert time.monotonic() < deadline, snapshot
                time.sleep(0.1)
            try:
                rpc("tools/list", {})
                raise AssertionError("expired session unexpectedly accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 404, error
            headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
            rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                               "clientInfo": {"name": "reconnected", "version": "1"}})
            headers["MCP-Protocol-Version"] = "2025-03-26"
            rpc("notifications/initialized", {}, notification=True)
            assert json.loads(rpc("tools/call", {"name": "list_rooms", "arguments": {}})["content"][0]["text"])["rooms"] == []
            rpc("tools/call", {"name": "join_room", "arguments": {"room": "smoke"}})
            result = rpc("tools/call", {"name": "list_rooms", "arguments": {}})
            assert "smoke" in json.loads(result["content"][0]["text"])["rooms"]
            print("Abandoned session expiry/404/reinitialize with room retained: OK")
            # Keep a real notification stream open while shutting down.
            sse = http.client.HTTPConnection(urllib.parse.urlparse(url).netloc, timeout=10)
            sse.request("GET", "/mcp", headers={**headers, "Accept": "text/event-stream"})
            response = sse.getresponse()
            assert response.status == 200, response.status
            started = time.monotonic()
            process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=8) == 0
            print(f"HTTP initialize/tools/watch/SSE/SIGTERM: OK ({time.monotonic() - started:.2f}s shutdown)")
        finally:
            if sse:
                sse.close()
            if process.poll() is None:
                process.kill()
                process.wait()
            process.stderr.close()

        # Also retain coverage of the default transport and EOF shutdown.
        env.update(BUDDIES_TRANSPORT="stdio", BUDDIES_DATA_DIR=str(root / "stdio-data"))
        with tempfile.TemporaryFile() as log:
            process = subprocess.Popen([str(binary)], env=env, stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=log)
            try:
                messages = [
                    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                        "protocolVersion": "2025-03-26", "capabilities": {},
                        "clientInfo": {"name": "smoke-test", "version": "1"}}},
                    {"jsonrpc": "2.0", "method": "notifications/initialized"},
                    {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
                ]
                for msg in messages:
                    process.stdin.write(json.dumps(msg).encode() + b"\n")
                    process.stdin.flush()
                    if "id" in msg:
                        assert select.select([process.stdout], [], [], 15)[0], "stdio timeout"
                        result = json.loads(process.stdout.readline())
                        assert result["id"] == msg["id"] and "result" in result, result
                        if msg["method"] == "initialize":
                            check_server_info(result["result"])
                assert len(result["result"]["tools"]) == 23
                process.stdin.close()
                assert process.wait(timeout=10) == 0
                print("stdio initialize/tools/EOF: OK")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                process.stdout.close()
                if not process.stdin.closed:
                    process.stdin.close()


if __name__ == "__main__":
    main()
