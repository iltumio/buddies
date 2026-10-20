#!/usr/bin/env python3
"""End-to-end test for multiple local orchestrators in the same room, each with
bounded specialists (--specialist) and multi-resource connectors (git + linear).

Verifies:
1. Specialist boundary enforcement: `frontend` specialist is rejected if assigned
   `backend` resources/scopes.
2. Multi-resource execution: `orchestrator-app` coordinates `frontend` (on `git:frontend`)
   and `backend` (on `git:backend`), while `orchestrator-pm` coordinates `linear-bot`
   (on `linear:roadmap` non-git connector) in the same room.
3. Inter-orchestrator room coordination and prepare-then-publish success across
   both Git repositories and the stateful Linear connector.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile

from smoke_collaboration import eventually
from smoke_room_tickets import Server


FAKE_CODEX = r'''#!/usr/bin/env python3
import json, os, re, sys
from pathlib import Path

threads = {}
pending = {}
sequence = 20000
boundary_tested = False
pm_notified = False
log = Path(os.environ['BUDDIES_FIXTURE_LOG'])

def record(**value):
    with log.open('a') as f:
        f.write(json.dumps(value) + '\n')

def emit(value):
    print(json.dumps(value), flush=True)

def final(tid, text, status='completed'):
    emit({'method': 'item/completed', 'params': {'threadId': tid, 'item': {'type': 'agentMessage', 'text': text}}})
    emit({'method': 'turn/completed', 'params': {'threadId': tid, 'turn': {'id': threads[tid]['turn'], 'status': status}}})

def tool(tid, name, arguments, done):
    global sequence
    sequence += 1
    pending[sequence] = done
    record(event='tool', thread=tid, name=name, arguments=arguments)
    emit({'id': sequence, 'method': 'item/tool/call', 'params': {
        'threadId': tid,
        'turnId': threads[tid]['turn'],
        'tool': name,
        'arguments': arguments,
    }})

def control(tid, arguments, done):
    tool(tid, 'orchestrator_control', arguments, done)

def send(tid, arguments, done):
    tool(tid, 'buddies_send_message', {'room': 'shared', **arguments}, done)

def accept(tid, a, done):
    control(tid, {'action': 'accept', 'id': a['id'], 'revision': a['revision']}, lambda _: done())

def report(tid, a, check_cmd, done):
    control(tid, {
        'action': 'report',
        'id': a['id'],
        'revision': a['revision'],
        'summary': 'Completed ' + a['id'],
        'checks': [check_cmd],
    }, lambda _: done())

def handle_app_orchestrator(tid, data):
    global boundary_tested, pm_notified
    g = data['graph']
    assignments = g['assignments']
    messages = data['messages']
    if not assignments:
        # Step 1: Try assigning backend work to frontend specialist -> MUST fail boundary check!
        def after_bad_assign(res):
            global boundary_tested
            assert not res['success'], f"Expected boundary rejection, got: {res}"
            assert "boundary" in res['text'], f"Expected boundary error message, got: {res['text']}"
            boundary_tested = True
            record(event='boundary_rejected', detail=res['text'])
            # Step 2: Assign backend work to `backend` and frontend work to `frontend` within their boundaries.
            control(tid, {
                'action': 'assign',
                'id': 'be-api',
                'owner': 'backend',
                'description': 'Create backend API schema',
                'resource': 'git:backend',
                'paths': ['src/api.json'],
                'dependencies': [],
                'deadline_secs': 90,
            }, lambda r1: (
                assert_ok(r1),
                control(tid, {
                    'action': 'assign',
                    'id': 'fe-ui',
                    'owner': 'frontend',
                    'description': 'Consume backend API schema in frontend UI',
                    'resource': 'git:frontend',
                    'paths': ['src/ui.json'],
                    'dependencies': ['be-api'],
                    'deadline_secs': 90,
                }, lambda r2: (
                    assert_ok(r2),
                    final(tid, 'Bounded assignments scheduled.')
                ))
            ))
        control(tid, {
            'action': 'assign',
            'id': 'illegal-cross-boundary',
            'owner': 'frontend',
            'description': 'Attempt to touch backend from frontend specialist',
            'resource': 'git:backend',
            'paths': ['src/api.json'],
            'dependencies': [],
            'deadline_secs': 90,
        }, after_bad_assign)
    elif all(a['state'] == 'completed' for a in assignments.values()):
        if not pm_notified:
            pm_notified = True
            send(tid, {
                'to': 'orchestrator-pm',
                'kind': 'request',
                'content': 'API and UI ready: endpoint=/v2/users',
            }, lambda _: final(tid, 'Notified orchestrator-pm.'))
        elif any(m['from'] == 'orchestrator-pm' and 'ENG-101' in m['content'] for m in messages):
            control(tid, {
                'action': 'complete',
                'summary': 'Frontend and backend repositories verified and synchronized with PM.',
            }, lambda r: (assert_ok(r), final(tid, 'App orchestrator complete.')))
        else:
            final(tid, 'Waiting for orchestrator-pm confirmation.')
    else:
        final(tid, 'Waiting for bounded specialists.')

def handle_pm_orchestrator(tid, data):
    g = data['graph']
    assignments = g['assignments']
    messages = data['messages']
    if not assignments:
        if any(m['from'] == 'orchestrator-app' and 'endpoint=/v2/users' in m['content'] for m in messages):
            control(tid, {
                'action': 'assign',
                'id': 'linear-sync',
                'owner': 'linear-bot',
                'description': 'Update Linear issue ENG-101 with /v2/users',
                'resource': 'linear:roadmap',
                'paths': ['issues/ENG-101.json'],
                'dependencies': [],
                'deadline_secs': 90,
            }, lambda r: (assert_ok(r), final(tid, 'Assigned Linear sync.')))
        else:
            final(tid, 'Waiting for orchestrator-app contract notification.')
    elif all(a['state'] == 'completed' for a in assignments.values()):
        send(tid, {
            'to': 'orchestrator-app',
            'kind': 'update',
            'content': 'Linear issue ENG-101 synced.',
        }, lambda _: control(tid, {
            'action': 'complete',
            'summary': 'Linear roadmap connector published.',
        }, lambda r: (assert_ok(r), final(tid, 'PM orchestrator complete.'))))
    else:
        final(tid, 'Waiting for linear-bot specialist.')

def handle_specialist(tid, data):
    t = threads[tid]
    a = data['assignment']
    cwd = Path(t['cwd'])
    name = t['name']
    if name == 'backend' and a:
        def impl():
            (cwd / 'src/api.json').write_text(json.dumps({'endpoint': '/v2/users', 'version': 2}))
            report(tid, a, 'test -f src/api.json', lambda: final(tid, 'Backend API implemented.'))
        accept(tid, a, impl)
    elif name == 'frontend' and a:
        def impl():
            (cwd / 'src/ui.json').write_text(json.dumps({'consumes': '/v2/users', 'component': 'UsersView'}))
            report(tid, a, 'test -f src/ui.json', lambda: final(tid, 'Frontend UI implemented.'))
        accept(tid, a, impl)
    elif name == 'linear-bot' and a:
        def impl():
            issue_file = cwd / 'issues/ENG-101.json'
            issue_file.parent.mkdir(parents=True, exist_ok=True)
            issue_file.write_text(json.dumps({'id': 'ENG-101', 'status': 'Done', 'endpoint': '/v2/users'}))
            report(tid, a, 'test -f issues/ENG-101.json', lambda: final(tid, 'Linear ENG-101 updated.'))
        accept(tid, a, impl)
    else:
        final(tid, 'Idle.')

def assert_ok(res):
    assert res['success'], f"Tool call failed unexpectedly: {res}"

for line in sys.stdin:
    msg = json.loads(line)
    if 'method' not in msg:
        res_obj = msg['result']
        text = res_obj['contentItems'][0]['text']
        try:
            val = json.loads(text)
        except Exception:
            val = text
        pending.pop(msg['id'])({'success': res_obj['success'], 'text': text, 'value': val})
        continue
    if 'id' not in msg:
        continue
    method = msg['method']
    p = msg['params']
    tid = p.get('threadId')
    if method == 'initialize':
        result = {}
    elif method in ('thread/start', 'thread/resume'):
        name = re.search(r'Stable participant: (\S+)\. Goal:', p['developerInstructions']).group(1)
        tid = tid or 'thread-' + name
        threads[tid] = {'name': name, 'cwd': p['cwd'], 'turns': 0}
        result = {'thread': {'id': tid}}
    elif method == 'turn/start':
        threads[tid]['turns'] += 1
        threads[tid]['cwd'] = p['cwd']
        threads[tid]['sandbox'] = p['sandboxPolicy']
        turn = f"{tid}-turn-{threads[tid]['turns']}"
        threads[tid]['turn'] = turn
        emit({'id': msg['id'], 'result': {'turn': {'id': turn}}})
        data = json.loads(p['input'][0]['text'])
        name = threads[tid]['name']
        if name == 'orchestrator-app':
            handle_app_orchestrator(tid, data)
        elif name == 'orchestrator-pm':
            handle_pm_orchestrator(tid, data)
        else:
            handle_specialist(tid, data)
        continue
    elif method == 'turn/steer':
        emit({'id': msg['id'], 'result': {'turn': {'id': p['expectedTurnId']}}})
        continue
    emit({'id': msg['id'], 'result': result})
'''


def init_git_repo(path: Path, initial_files: dict[str, str]):
    path.mkdir(parents=True)
    subprocess.run(['git', 'init', '-q', str(path)], check=True)
    subprocess.run(['git', '-C', str(path), 'config', 'user.email', 'test@example.com'], check=True)
    subprocess.run(['git', '-C', str(path), 'config', 'user.name', 'Test'], check=True)
    subprocess.run(['git', '-C', str(path), 'config', 'commit.gpgsign', 'false'], check=True)
    for rel, content in initial_files.items():
        fp = path / rel
        fp.parent.mkdir(parents=True, exist_ok=True)
        fp.write_text(content)
    subprocess.run(['git', '-C', str(path), 'add', '.'], check=True)
    subprocess.run(['git', '-C', str(path), 'commit', '-q', '-m', 'init'], check=True)


def main():
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = Path(metadata['target_directory']) / 'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-multi-orch-') as directory:
        root = Path(directory)
        fe_repo = root / 'fe-repo'
        be_repo = root / 'be-repo'
        linear_dir = root / 'linear-roadmap'
        init_git_repo(fe_repo, {'src/ui.json': '{"consumes": null}\n'})
        init_git_repo(be_repo, {'src/api.json': '{"endpoint": null}\n'})
        (linear_dir / 'issues').mkdir(parents=True)
        (linear_dir / 'issues/ENG-101.json').write_text('{"id": "ENG-101", "status": "Todo"}\n')

        fake = root / 'fake-codex'
        fake.write_text(FAKE_CODEX)
        fake.chmod(0o755)
        log = root / 'fixture.jsonl'
        log.touch()

        server = Server(binary, str(root / 'service'), 'service')
        env = dict(os.environ, BUDDIES_SIGNER='none', BUDDIES_FIXTURE_LOG=str(log))
        app_proc = None
        pm_proc = None
        try:
            app_proc = subprocess.Popen(
                [
                    str(binary), 'orchestrator',
                    '--room', 'shared',
                    '--name', 'orchestrator-app',
                    '--resource', f'git:frontend={fe_repo}',
                    '--resource', f'git:backend={be_repo}',
                    '--specialist', 'frontend=git:frontend:src',
                    '--specialist', 'backend=git:backend:src',
                    '--goal', 'Coordinate frontend and backend repos with strict specialist boundaries',
                    '--check', 'git:frontend:test -f src/ui.json',
                    '--check', 'git:backend:test -f src/api.json',
                    '--url', server.url,
                    '--codex-bin', str(fake),
                    '--debounce-ms', '100',
                    '--max-active-specialists', '2',
                ],
                stdout=open(root / 'out.log', 'a'),
                stderr=open(root / 'err.log', 'a'),
                text=True,
                env=env,
            )
            pm_proc = subprocess.Popen(
                [
                    str(binary), 'orchestrator',
                    '--room', 'shared',
                    '--name', 'orchestrator-pm',
                    '--resource', f'linear:roadmap={linear_dir}',
                    '--specialist', 'linear-bot=linear:roadmap:issues',
                    '--goal', 'Track contract rollout in Linear roadmap connector',
                    '--check', 'linear:roadmap:test -f issues/ENG-101.json',
                    '--url', server.url,
                    '--codex-bin', str(fake),
                    '--debounce-ms', '100',
                ],
                stdout=open(root / 'out.log', 'a'),
                stderr=open(root / 'err.log', 'a'),
                text=True,
                env=env,
            )

            try:
                app_proc.wait(timeout=10)
                pm_proc.wait(timeout=10)
            except Exception:
                app_proc.kill(); pm_proc.kill()
                print("ERR LOG:\n", (root / 'err.log').read_text())
                print("FIXTURE LOG:\n", log.read_text())
                raise
            pm_out, pm_err = pm_proc.communicate(timeout=25)
            assert app_proc.returncode == 0, (root / "err.log").read_text()
            assert pm_proc.returncode == 0, (root / "err.log").read_text()

            # Verify boundary rejection occurred in fixture log
            events = [json.loads(line) for line in log.read_text().splitlines() if line.strip()]
            assert any(e.get('event') == 'boundary_rejected' for e in events), "Expected boundary rejection event"

            # Verify published state across all 3 resources (2 git repos + 1 Linear state connector)
            fe_data = json.loads((fe_repo / 'src/ui.json').read_text())
            be_data = json.loads((be_repo / 'src/api.json').read_text())
            linear_data = json.loads((linear_dir / 'issues/ENG-101.json').read_text())

            assert be_data == {'endpoint': '/v2/users', 'version': 2}, be_data
            assert fe_data == {'consumes': '/v2/users', 'component': 'UsersView'}, fe_data
            assert linear_data == {'id': 'ENG-101', 'status': 'Done', 'endpoint': '/v2/users'}, linear_data
            print("multi-orchestrator smoke: boundary enforcement, multi-repo git, and Linear connector publication passed")
        finally:
            for p in (app_proc, pm_proc):
                if p is not None and p.poll() is None:
                    p.terminate()
            server.stop()


if __name__ == '__main__':
    main()
