#!/usr/bin/env python3
"""End-to-end smoke test for single-specialist workers (1 worker = 1 specialist):
1. With a worker designated as `--orchestrator`
2. Without any orchestrator (peer-to-peer self-organizing workers)
Both modes enforce strict per-specialist resource/scope boundaries across Git and Linear connectors.
"""
import json
import os
from smoke_local_agents import Client
from smoke_room_tickets import Server
import subprocess
import tempfile
import time
from pathlib import Path

FAKE_CODEX = r"""#!/usr/bin/env python3
import json, pathlib, sys

threads = {}
next_id = 1
pending = {}

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def call_tool(turn_id, tool, arguments, state):
    global next_id
    req_id = f"tool-{next_id}"
    next_id += 1
    pending[req_id] = state
    send({"id": req_id, "method": "item/tool/call", "params": {"threadId": state["thread"], "turnId": turn_id, "tool": tool, "arguments": arguments}})

def finish(thread_id, turn_id, text):
    send({"method": "item/completed", "params": {"threadId": thread_id, "turnId": turn_id, "item": {"id": f"msg-{turn_id}", "type": "agentMessage", "text": text}}})
    send({"method": "turn/completed", "params": {"threadId": thread_id, "turn": {"id": turn_id, "status": "completed"}}})

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"id": msg["id"], "result": {"serverInfo": {"name": "fake-codex"}}})
    elif method in ("thread/start", "thread/resume"):
        tid = msg["params"].get("threadId") or f"thread-{next_id}"
        next_id += 1
        ins = msg["params"].get("developerInstructions", "")
        name = "unknown"
        if "Stable participant: " in ins:
            name = ins.split("Stable participant: ", 1)[1].split(".", 1)[0].strip()
        threads[tid] = {"name": name}
        send({"id": msg["id"], "result": {"thread": {"id": tid}}})
    elif method == "turn/start":
        p = msg["params"]
        tid = p["threadId"]
        turn_id = f"turn-{next_id}"
        next_id += 1
        send({"id": msg["id"], "result": {"turn": {"id": turn_id}}})
        payload = json.loads(p["input"][0]["text"])
        name = threads[tid]["name"]
        cwd = pathlib.Path(p["cwd"])
        state = {"thread": tid, "turn": turn_id, "name": name, "payload": payload, "cwd": cwd}

        # =====================================================================
        # MODE 1: Orchestrator worker (`coordinator`) is in the room
        # =====================================================================
        if name == "coordinator":
            assignments = payload["graph"]["assignments"]
            if not assignments:
                # 1. Try violating boundary: assign backend resource to frontend specialist (MUST FAIL)
                call_tool(turn_id, "orchestrator_control", {
                    "action": "assign",
                    "id": "illegal-cross-boundary",
                    "owner": "frontend",
                    "description": "Try touching backend from frontend specialist",
                    "resource": "git:backend",
                    "paths": ["src/api.json"],
                    "dependencies": [],
                    "deadline_secs": 30
                }, {**state, "step": "orch-expect-boundary-reject"})
            elif (
                assignments.get("be-api", {}).get("state") == "completed"
                and assignments.get("fe-ui", {}).get("state") == "completed"
                and assignments.get("linear-101", {}).get("state") == "completed"
            ):
                call_tool(turn_id, "orchestrator_control", {
                    "action": "complete",
                    "summary": "Orchestrated multi-resource goal published"
                }, {**state, "step": "orch-complete"})
            else:
                finish(tid, turn_id, "Waiting for specialist workers")

        # =====================================================================
        # Specialist Workers (`frontend`, `backend`, `linear-bot`)
        # Works in BOTH Mode 1 (with orchestrator) and Mode 2 (peer-to-peer)!
        # =====================================================================
        elif name == "backend":
            assignment = payload.get("assignment")
            has_orch = payload.get("graph", {}).get("orchestrator") is not None
            if assignment and assignment.get("accepted_revision") != assignment.get("revision"):
                # Both Mode 1 (dispatched by orchestrator) and Mode 2 (self-assigned in previous turn):
                # accept -> write in isolated worktree -> report!
                call_tool(turn_id, "orchestrator_control", {
                    "action": "accept",
                    "id": assignment["id"],
                    "revision": assignment["revision"]
                }, {**state, "step": "be-accepted"})
            elif has_orch:
                # Mode 1: answer question from frontend peer
                q = next((m for m in payload["messages"] if m["kind"] == "question"), None)
                if q:
                    call_tool(turn_id, "buddies_send_message", {
                        "to": q["from"],
                        "kind": "answer",
                        "in_reply_to": q["id"],
                        "content": "endpoint=/v2/users"
                    }, {**state, "step": "done-msg"})
                else:
                    finish(tid, turn_id, "Backend idle")
            else:
                # Mode 2 (peer-to-peer, no orchestrator):
                assignments = payload["graph"]["assignments"]
                req = next((m for m in payload["messages"] if m["kind"] == "request"), None)
                if not assignments and req:
                    threads[tid]["requester"] = req["from"]
                    # Self-assign within backend's strict boundary (`git:backend:src`)
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "assign",
                        "id": "be-api",
                        "owner": "backend",
                        "description": "Expose /v2/users in backend repo",
                        "resource": "git:backend",
                        "paths": ["src/api.json"],
                        "dependencies": [],
                        "deadline_secs": 30
                    }, {**state, "step": "p2p-be-assigned", "requester": req["from"], "req_id": req["id"]})
                elif assignments.get("be-api", {}).get("state") == "completed":
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "complete",
                        "summary": "Backend published /v2/users"
                    }, {**state, "step": "p2p-be-completed"})
                else:
                    finish(tid, turn_id, "Backend waiting")

        elif name == "frontend":
            assignment = payload.get("assignment")
            has_orch = payload.get("graph", {}).get("orchestrator") is not None
            if has_orch and assignment:
                # Mode 1 (orchestrated): ask backend first, then implement after answer
                answer = next((m for m in payload["messages"] if m["kind"] == "answer"), None)
                if answer is None:
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "accept",
                        "id": assignment["id"],
                        "revision": assignment["revision"]
                    }, {**state, "step": "fe-ask-backend"})
                else:
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "accept",
                        "id": assignment["id"],
                        "revision": assignment["revision"]
                    }, {**state, "step": "fe-implement"})
            elif not has_orch:
                # Mode 2 (peer-to-peer, no orchestrator):
                assignments = payload["graph"]["assignments"]
                answer = next((m for m in payload["messages"] if m["kind"] == "answer" and m["from"] == "backend"), None)
                linear_done = next((m for m in payload["messages"] if m["kind"] == "answer" and m["from"] == "linear-bot"), None)
                if assignment and assignment.get("accepted_revision") != assignment.get("revision"):
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "accept",
                        "id": assignment["id"],
                        "revision": assignment["revision"]
                    }, {**state, "step": "fe-implement"})
                elif not assignments and answer is None:
                    # First test strict boundary in P2P mode: try self-assigning git:backend (MUST FAIL)
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "assign",
                        "id": "p2p-illegal",
                        "owner": "frontend",
                        "description": "Try self-assigning backend from frontend in P2P mode",
                        "resource": "git:backend",
                        "paths": ["src/api.json"],
                        "dependencies": [],
                        "deadline_secs": 30
                    }, {**state, "step": "p2p-fe-expect-boundary-reject"})
                elif not assignments and answer is not None:
                    # Backend answered! Self-assign frontend UI within git:frontend:src boundary
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "assign",
                        "id": "fe-ui",
                        "owner": "frontend",
                        "description": "Consume /v2/users in frontend UI",
                        "resource": "git:frontend",
                        "paths": ["src/ui.json"],
                        "dependencies": [],
                        "deadline_secs": 30
                    }, {**state, "step": "p2p-fe-assigned"})
                elif assignments.get("fe-ui", {}).get("state") == "completed" and linear_done is None:
                    # Request linear-bot to update ENG-101
                    call_tool(turn_id, "buddies_send_message", {
                        "to": "linear-bot",
                        "kind": "request",
                        "content": "Mark ENG-101 Done"
                    }, {**state, "step": "p2p-fe-asked-linear"})
                elif assignments.get("fe-ui", {}).get("state") == "completed" and linear_done is not None:
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "complete",
                        "summary": "Frontend published UI after P2P coordination"
                    }, {**state, "step": "done-msg"})
                else:
                    finish(tid, turn_id, "Frontend waiting")

        elif name == "linear-bot":
            assignment = payload.get("assignment")
            has_orch = payload.get("graph", {}).get("orchestrator") is not None
            if assignment and assignment.get("accepted_revision") != assignment.get("revision"):
                call_tool(turn_id, "orchestrator_control", {
                    "action": "accept",
                    "id": assignment["id"],
                    "revision": assignment["revision"]
                }, {**state, "step": "linear-accepted"})
            elif not has_orch:
                assignments = payload["graph"]["assignments"]
                req = next((m for m in payload["messages"] if m["kind"] == "request"), None)
                if not assignments and req:
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "assign",
                        "id": "linear-101",
                        "owner": "linear-bot",
                        "description": "Mark ENG-101 Done",
                        "resource": "linear:roadmap",
                        "paths": ["issues/ENG-101.json"],
                        "dependencies": [],
                        "deadline_secs": 30
                    }, {**state, "step": "p2p-linear-assigned"})
                elif assignments.get("linear-101", {}).get("state") == "completed":
                    call_tool(turn_id, "orchestrator_control", {
                        "action": "complete",
                        "summary": "Linear issue ENG-101 published"
                    }, {**state, "step": "p2p-linear-completed"})
                else:
                    finish(tid, turn_id, "Linear waiting")

    elif "id" in msg and msg["id"] in pending:
        state = pending.pop(msg["id"])
        tid = state["thread"]
        turn_id = state["turn"]
        step = state["step"]
        cwd = state["cwd"]
        res = msg.get("result", {})

        # Mode 1 steps
        if step == "orch-expect-boundary-reject":
            assert res.get("success") is False, f"Expected boundary violation to fail, got: {res}"
            call_tool(turn_id, "orchestrator_control", {
                "action": "assign",
                "id": "be-api",
                "owner": "backend",
                "description": "Expose /v2/users in backend repo",
                "resource": "git:backend",
                "paths": ["src/api.json"],
                "dependencies": [],
                "deadline_secs": 30
            }, {**state, "step": "orch-assign-fe"})
        elif step == "orch-assign-fe":
            call_tool(turn_id, "orchestrator_control", {
                "action": "assign",
                "id": "fe-ui",
                "owner": "frontend",
                "description": "Consume /v2/users in frontend repo",
                "resource": "git:frontend",
                "paths": ["src/ui.json"],
                "dependencies": ["be-api"],
                "deadline_secs": 30
            }, {**state, "step": "orch-assign-linear"})
        elif step == "orch-assign-linear":
            call_tool(turn_id, "orchestrator_control", {
                "action": "assign",
                "id": "linear-101",
                "owner": "linear-bot",
                "description": "Update ENG-101 in Linear",
                "resource": "linear:roadmap",
                "paths": ["issues/ENG-101.json"],
                "dependencies": ["fe-ui"],
                "deadline_secs": 30
            }, {**state, "step": "done-msg"})
        elif step == "orch-complete":
            finish(tid, turn_id, "Orchestrator complete")

        # Shared implementation steps (identical in Mode 1 and Mode 2!)
        elif step == "be-accepted":
            (cwd / "src").mkdir(parents=True, exist_ok=True)
            (cwd / "src" / "api.json").write_text('{"endpoint":"/v2/users"}\n')
            aid = state["payload"]["assignment"]["id"]
            rev = state["payload"]["assignment"]["revision"]
            call_tool(turn_id, "orchestrator_control", {
                "action": "report",
                "id": aid,
                "revision": rev,
                "summary": "Backend /v2/users ready",
                "checks": ["test -f src/api.json"]
            }, {**state, "step": "done-msg"})
        elif step == "fe-ask-backend":
            call_tool(turn_id, "buddies_send_message", {
                "to": "backend",
                "kind": "question",
                "content": "What endpoint path did you expose?"
            }, {**state, "step": "fe-checkpoint"})
        elif step == "fe-checkpoint":
            aid = state["payload"]["assignment"]["id"]
            rev = state["payload"]["assignment"]["revision"]
            call_tool(turn_id, "orchestrator_control", {
                "action": "checkpoint",
                "id": aid,
                "revision": rev,
                "detail": "Waiting for backend contract answer"
            }, {**state, "step": "done-msg"})
        elif step == "fe-implement":
            (cwd / "src").mkdir(parents=True, exist_ok=True)
            (cwd / "src" / "ui.json").write_text('{"consumes":"/v2/users"}\n')
            aid = state["payload"]["assignment"]["id"]
            rev = state["payload"]["assignment"]["revision"]
            call_tool(turn_id, "orchestrator_control", {
                "action": "report",
                "id": aid,
                "revision": rev,
                "summary": "Frontend UI updated",
                "checks": ["test -f src/ui.json"]
            }, {**state, "step": "done-msg"})
        elif step == "linear-accepted":
            (cwd / "issues").mkdir(parents=True, exist_ok=True)
            (cwd / "issues" / "ENG-101.json").write_text('{"id":"ENG-101","state":"Done","endpoint":"/v2/users"}\n')
            aid = state["payload"]["assignment"]["id"]
            rev = state["payload"]["assignment"]["revision"]
            call_tool(turn_id, "orchestrator_control", {
                "action": "report",
                "id": aid,
                "revision": rev,
                "summary": "Linear issue ENG-101 Done",
                "checks": ["test -f issues/ENG-101.json"]
            }, {**state, "step": "done-msg"})

        # Mode 2 (P2P without orchestrator) steps
        elif step == "p2p-fe-expect-boundary-reject":
            assert res.get("success") is False, f"Expected P2P boundary violation to fail, got: {res}"
            call_tool(turn_id, "buddies_send_message", {
                "to": "backend",
                "kind": "request",
                "content": "Please expose /v2/users in git:backend:src/api.json"
            }, {**state, "step": "done-msg"})
        elif step in ("p2p-be-assigned", "p2p-fe-assigned", "p2p-linear-assigned", "p2p-fe-asked-linear"):
            finish(tid, turn_id, f"Completed step {step}")
        elif step == "p2p-be-completed":
            call_tool(turn_id, "buddies_send_message", {
                "to": threads[tid].get("requester", "frontend"),
                "kind": "answer",
                "content": "Backend published /v2/users"
            }, {**state, "step": "done-msg"})
        elif step == "p2p-linear-completed":
            call_tool(turn_id, "buddies_send_message", {
                "to": "frontend",
                "kind": "answer",
                "content": "Linear ENG-101 published"
            }, {**state, "step": "done-msg"})
        elif step == "done-msg":
            finish(tid, turn_id, "Turn complete")
"""


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def git(cwd: Path, *args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=cwd, text=True).strip()


def init_git_repo(path: Path, initial_file: str, initial_content: str) -> None:
    path.mkdir(parents=True, exist_ok=True)
    git(path, "init", "-b", "main")
    git(path, "config", "user.email", "test@example.com")
    git(path, "config", "user.name", "Test")
    git(path, "config", "commit.gpgsign", "false")
    f = path / initial_file
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text(initial_content)
    git(path, "add", ".")
    git(path, "commit", "-m", "initial")


def init_linear_dir(path: Path) -> None:
    (path / "issues").mkdir(parents=True, exist_ok=True)
    (path / "issues" / "ENG-101.json").write_text('{"id":"ENG-101","state":"Todo"}\n')


def main() -> None:
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"]))
    buddies = Path(metadata["target_directory"]) / "debug/buddies"

    with tempfile.TemporaryDirectory(prefix="buddies-worker-spec-") as tmp_str:
        tmp = Path(tmp_str)
        fake_codex = tmp / "fake-codex"
        fake_codex.write_text(FAKE_CODEX)
        fake_codex.chmod(0o755)

        server = Server(buddies, str(tmp / "server-data"), "service")
        url = server.url
        env = dict(os.environ, BUDDIES_SIGNER="none")
        try:

            # =================================================================
            # TEST 1: Single workers (`1 worker = 1 specialist`) WITH `--orchestrator`
            # =================================================================
            fe_repo1 = tmp / "mode1-fe"
            be_repo1 = tmp / "mode1-be"
            linear1 = tmp / "mode1-linear"
            coordinator_repo = tmp / "mode1-coordinator"
            init_git_repo(coordinator_repo, "README", 'Independent coordinator repository\n')
            init_git_repo(fe_repo1, "src/ui.json", '{"consumes":null}\n')
            init_git_repo(be_repo1, "src/api.json", '{"endpoint":null}\n')
            init_linear_dir(linear1)

            w_fe1 = subprocess.Popen([
                str(buddies), "worker",
                "--room", "room-with-orch",
                "--name", "frontend",
                "--resource", f"git:frontend={fe_repo1}",
                "--allow-scope", "src",
                "--check", "test -f src/ui.json",
                "--debounce-ms", "50",
                "--url", url,
                "--codex-bin", str(fake_codex),
                "--once",
            ], env=env)
            w_be1 = subprocess.Popen([
                str(buddies), "worker",
                "--room", "room-with-orch",
                "--name", "backend",
                "--resource", f"git:backend={be_repo1}",
                "--allow-scope", "src",
                "--check", "test -f src/api.json",
                "--debounce-ms", "50",
                "--url", url,
                "--codex-bin", str(fake_codex),
                "--once",
            ], env=env)
            w_lin1 = subprocess.Popen([
                str(buddies), "worker",
                "--room", "room-with-orch",
                "--name", "linear-bot",
                "--resource", f"linear:roadmap={linear1}",
                "--allow-scope", "issues",
                "--check", "test -f issues/ENG-101.json",
                "--debounce-ms", "50",
                "--url", url,
                "--codex-bin", str(fake_codex),
                "--once",
            ], env=env)
            w_orch = subprocess.Popen([
                str(buddies), "worker",
                "--room", "room-with-orch",
                "--name", "coordinator",
                "--cwd", str(coordinator_repo),
                "--orchestrator",
                "--data-dir", str(tmp / "orch-state"),
                "--goal", "Coordinate frontend, backend, and linear-bot to ship /v2/users",
                "--debounce-ms", "80",
                "--url", url,
                "--codex-bin", str(fake_codex),
            ], env=env)

            assert w_orch.wait(timeout=30) == 0, "orchestrator worker failed"
            assert w_fe1.wait(timeout=15) == 0, "frontend worker (mode 1) failed"
            assert w_be1.wait(timeout=15) == 0, "backend worker (mode 1) failed"
            assert w_lin1.wait(timeout=15) == 0, "linear worker (mode 1) failed"

            assert json.loads((be_repo1 / "src" / "api.json").read_text()) == {"endpoint": "/v2/users"}
            assert json.loads((fe_repo1 / "src" / "ui.json").read_text()) == {"consumes": "/v2/users"}
            assert json.loads((linear1 / "issues" / "ENG-101.json").read_text()) == {
                "id": "ENG-101",
                "state": "Done",
                "endpoint": "/v2/users",
            }
            print("mode 1 (1 worker = 1 specialist + --orchestrator worker): passed")

            # =================================================================
            # TEST 2: Single workers (`1 worker = 1 specialist`) WITHOUT orchestrator
            #         (Peer-to-peer self-organizing workers in the room)
            # =================================================================
            fe_repo2 = tmp / "mode2-fe"
            be_repo2 = tmp / "mode2-be"
            linear2 = tmp / "mode2-linear"
            init_git_repo(fe_repo2, "src/ui.json", '{"consumes":null}\n')
            init_git_repo(be_repo2, "src/api.json", '{"endpoint":null}\n')
            init_linear_dir(linear2)

            w_be2 = subprocess.Popen([
                str(buddies), "worker",
                "--room", "room-no-orch",
                "--name", "backend",
                "--resource", f"git:backend={be_repo2}",
                "--allow-scope", "src",
                "--check", "test -f src/api.json",
                "--debounce-ms", "50",
                "--url", url,
                "--codex-bin", str(fake_codex),
                "--once",
            ], env=env)
            w_lin2 = subprocess.Popen([
                str(buddies), "worker",
                "--room", "room-no-orch",
                "--name", "linear-bot",
                "--resource", f"linear:roadmap={linear2}",
                "--allow-scope", "issues",
                "--check", "test -f issues/ENG-101.json",
                "--debounce-ms", "50",
                "--url", url,
                "--codex-bin", str(fake_codex),
                "--once",
            ], env=env)
            w_fe2 = subprocess.Popen([
                str(buddies), "worker",
                "--room", "room-no-orch",
                "--name", "frontend",
                "--resource", f"git:frontend={fe_repo2}",
                "--allow-scope", "src",
                "--check", "test -f src/ui.json",
                "--goal", "Coordinate directly with peer workers in the room to deliver /v2/users",
                "--debounce-ms", "50",
                "--url", url,
                "--codex-bin", str(fake_codex),
                "--once",
            ], env=env)

            assert w_be2.wait(timeout=30) == 0, "backend worker (mode 2 P2P) failed"
            assert w_lin2.wait(timeout=30) == 0, "linear worker (mode 2 P2P) failed"
            assert w_fe2.wait(timeout=30) == 0, "frontend worker (mode 2 P2P) failed"

            assert json.loads((be_repo2 / "src" / "api.json").read_text()) == {"endpoint": "/v2/users"}
            assert json.loads((fe_repo2 / "src" / "ui.json").read_text()) == {"consumes": "/v2/users"}
            assert json.loads((linear2 / "issues" / "ENG-101.json").read_text()) == {
                "id": "ENG-101",
                "state": "Done",
                "endpoint": "/v2/users",
            }
            print("mode 2 (1 worker = 1 specialist WITHOUT orchestrator, peer-to-peer): passed")

            # =================================================================
            # TEST 3: Human-managed MCP agent (`human-lead`) collaborating in
            #         the same room with a specialist worker (`backend`)
            # =================================================================
            be_repo3 = tmp / "mode3-be"
            init_git_repo(be_repo3, "src/api.json", '{"endpoint":null}\n')

            human = Client(url)
            human.call("join_room", room="room-human-mixed")
            human.call("register_participant", room="room-human-mixed", name="human-lead", delivery="poll", accepts_tasks=False)

            w_be3 = subprocess.Popen([
                str(buddies), "worker",
                "--room", "room-human-mixed",
                "--name", "backend",
                "--resource", f"git:backend={be_repo3}",
                "--allow-scope", "src",
                "--check", "test -f src/api.json",
                "--debounce-ms", "50",
                "--url", url,
                "--codex-bin", str(fake_codex),
                "--once",
            ], env=env)

            # Wait until `backend` specialist worker registers in the room
            for _ in range(60):
                st = human.call("get_room_status", room="room-human-mixed")
                parts = {p["name"]: p for p in st.get("participants", [])}
                if "backend" in parts and "human-lead" in parts:
                    assert parts["human-lead"]["specialist"] is False
                    assert parts["human-lead"]["orchestrator"] is False
                    assert parts["backend"]["specialist"] is True
                    break
                time.sleep(0.05)
            else:
                raise AssertionError("backend worker did not register in room-human-mixed")

            # Human-managed agent sends a normal request to the specialist worker
            human.call(
                "send_message",
                room="room-human-mixed",
                to="backend",
                kind="request",
                content="Please expose /v2/users in git:backend:src/api.json",
            )

            assert w_be3.wait(timeout=30) == 0, "backend worker (mode 3 human-mixed) failed"
            assert json.loads((be_repo3 / "src" / "api.json").read_text()) == {"endpoint": "/v2/users"}

            inbox = human.call("poll_messages", room="room-human-mixed")
            answers = [m for m in inbox["messages"] if m["from"] == "backend" and m["kind"] == "answer"]
            assert any("Backend published /v2/users" in m["content"] for m in answers), inbox
            monitor_snap = json.loads(
                subprocess.check_output([str(buddies), "monitor", "--url", url, "--once"], env=env)
            )
            rooms_by_name = {r["name"]: r for r in monitor_snap.get("rooms", [])}
            assert len(rooms_by_name["room-with-orch"]["messages"]) > 0, rooms_by_name["room-with-orch"]
            assert len(rooms_by_name["room-no-orch"]["messages"]) > 0, rooms_by_name["room-no-orch"]
            assert any(
                m["from"] == "human-lead" and m["to"] == "backend"
                for m in rooms_by_name["room-human-mixed"]["messages"]
            ), rooms_by_name["room-human-mixed"]["messages"]
            print("mode 3 (human-managed MCP agent + autonomous specialist worker + monitor messages): passed")
        finally:
            server.stop()


if __name__ == "__main__":
    main()
