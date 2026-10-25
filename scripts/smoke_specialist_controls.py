#!/usr/bin/env python3
"""Exercise coordinator ownership and read-only policy over the actual MCP transport."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from smoke_local_agents import Client
from smoke_room_tickets import Server
from smoke_worker_specialists import init_git_repo

FAKE_CODEX = r'''#!/usr/bin/env python3
import json, os, sys

def emit(value):
    print(json.dumps(value), flush=True)

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method')
    if method == 'initialize':
        emit({'id': msg['id'], 'result': {}})
    elif method in ('thread/start', 'thread/resume'):
        with open(os.environ['BUDDIES_CONTROL_LOG'], 'a') as f:
            f.write(json.dumps({'method': method, 'params': msg['params']}) + '\n')
        emit({'id': msg['id'], 'result': {'thread': {'id': 'worker-thread'}}})
    elif method == 'turn/start':
        with open(os.environ['BUDDIES_CONTROL_LOG'], 'a') as f:
            f.write(json.dumps({'method': method, 'params': msg['params']}) + '\n')
        emit({'id': msg['id'], 'result': {'turn': {'id': 'active-turn'}}})
    elif method == 'turn/interrupt':
        emit({'id': msg['id'], 'result': {}})
        emit({'method': 'turn/completed', 'params': {'threadId': 'worker-thread', 'turn': {'id': 'active-turn', 'status': 'interrupted'}}})
'''


def eventually(check, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError('condition did not become true')


def main():
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = Path(metadata['target_directory']) / 'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-controls-') as directory:
        tmp = Path(directory)
        repo = tmp / 'repo'
        init_git_repo(repo, 'src/file', 'base\n')
        fake = tmp / 'codex'
        fake.write_text(FAKE_CODEX)
        fake.chmod(0o755)
        log = tmp / 'turns.jsonl'
        server = Server(binary, str(tmp / 'server'), 'service')
        worker = None
        try:
            clients = {}
            for name in ['lead', 'second', 'peer']:
                client = Client(server.url)
                client.call('join_room', room='controls')
                client.call('register_participant', room='controls', name=name,
                            delivery='poll', orchestrator=name != 'peer')
                clients[name] = client
            worker = subprocess.Popen([
                str(binary), 'worker', '--room', 'controls', '--name', 'worker',
                '--resource', f'git:target={repo}', '--allow-scope', 'src',
                '--sandbox', 'read-only', '--debounce-ms', '50',
                '--url', server.url, '--codex-bin', str(fake),
            ], env=dict(os.environ, BUDDIES_SIGNER='none', BUDDIES_CONTROL_LOG=str(log)))
            lead = clients['lead']
            eventually(lambda: any(p['name'] == 'worker' for p in lead.call('get_room_status', room='controls')['participants']))
            assignment = dict(id='task', owner='worker', revision=1, description='inspect', paths=['src'],
                              resources=[dict(resource='git:target', scopes=['src'])], dependencies=[],
                              deadline=int(time.time())+60, timeout_secs=60, last_heartbeat=int(time.time()),
                              depth=0, state='running', uncertain=False, evidence=[])
            dispatch = dict(orchestrator_event='dispatch', assignment=assignment, resource_heads={})

            def send(sender, event):
                return clients[sender].call('send_message', room='controls', to='worker', kind='request', content=json.dumps(event))

            def receipt(sender, sent, terminal=True):
                def check():
                    # Acknowledge WorkerState RPCs so the worker can start/finish its turn.
                    for m in lead.call('poll_messages', room='controls')['messages']:
                        if m['from'] == 'worker':
                            lead.call('acknowledge_message', room='controls', message_id=m['id'], state='handled', detail='{}')
                    messages = clients[sender].call('get_conversation', room='controls', conversation_id=sent['conversation_id'])['messages']
                    return next((m for m in messages if m['id'] == sent['id'] and m['state'] in
                                 (['handled', 'failed'] if terminal else ['read', 'handled', 'failed'])), None)
                return eventually(check)

            assert receipt('peer', send('peer', dispatch))['state'] == 'failed'
            genuine = send('lead', dispatch)
            assert receipt('lead', genuine, terminal=False)['state'] == 'read'
            eventually(lambda: log.exists() and 'turn/start' in log.read_text())
            for sender in ['second', 'peer']:
                for event in [dispatch, dict(orchestrator_event='cancel', id='task', revision=1),
                              dict(orchestrator_event='prepare_publish', resource_heads={}),
                              dict(orchestrator_event='publish', resource_heads={})]:
                    assert receipt(sender, send(sender, event))['state'] == 'failed'
            assert receipt('lead', send('lead', dispatch))['state'] == 'failed'
            assert receipt('lead', send('lead', dict(orchestrator_event='publish', resource_heads={})))['state'] == 'failed'
            entries = [json.loads(line) for line in log.read_text().splitlines()]
            assert entries[0]['params']['sandbox'] == 'read-only'
            turns = [e for e in entries if e['method'] == 'turn/start']
            assert len(turns) == 1
            assert turns[0]['params']['sandboxPolicy'] == {'type': 'readOnly'}
            assert (repo / 'src/file').read_text() == 'base\n'
            assert worker.poll() is None
            print('specialist controls: forged commands, competing coordinator, busy dispatch, premature publish and read-only policy passed')
        finally:
            if worker is not None:
                worker.terminate()
                try:
                    worker.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    worker.kill()
                    worker.wait()
            server.stop()


if __name__ == '__main__':
    main()
