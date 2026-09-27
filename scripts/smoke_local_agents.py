#!/usr/bin/env python3
"""Two MCP sessions on one node must be distinct, isolated room participants."""
import concurrent.futures
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import tempfile
import time
import urllib.request
import uuid


class Client:
    def __init__(self, url):
        self.url = url
        self.headers = {'Content-Type': 'application/json', 'Accept': 'application/json, text/event-stream'}
        self.rpc('initialize', {'protocolVersion': '2025-03-26', 'capabilities': {},
                               'clientInfo': {'name': 'same-client-name', 'version': '1'}})
        self.rpc('notifications/initialized', {}, notification=True)

    def rpc(self, method, params, notification=False, allow_error=False):
        ident = str(uuid.uuid4())
        body = {'jsonrpc': '2.0', 'method': method, 'params': params}
        if not notification:
            body['id'] = ident
        req = urllib.request.Request(self.url, data=json.dumps(body).encode(), headers=self.headers)
        with urllib.request.urlopen(req, timeout=15) as response:
            session = response.headers.get('Mcp-Session-Id')
            if session:
                self.headers['Mcp-Session-Id'] = session
            if notification:
                return
            if 'text/event-stream' in response.headers.get('Content-Type', ''):
                while True:
                    line = response.readline()
                    assert line, 'SSE closed before response'
                    if line.startswith(b'data: ') and line[6:].strip():
                        msg = json.loads(line[6:])
                        if msg.get('id') == ident:
                            break
            else:
                msg = json.load(response)
        if allow_error and 'error' in msg:
            return {'isError': True, 'error': msg['error']}
        assert 'error' not in msg, msg
        return msg['result']

    def call(self, name, **arguments):
        result = self.rpc('tools/call', {'name': name, 'arguments': arguments})
        assert not result.get('isError'), result
        return json.loads(result['content'][0]['text'])

    def close(self):
        urllib.request.urlopen(urllib.request.Request(self.url, method='DELETE', headers=self.headers), timeout=3).close()


def main():
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = Path(metadata['target_directory']) / 'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-local-agents-') as directory:
        env = dict(os.environ, BUDDIES_DATA_DIR=directory, BUDDIES_SIGNER='none',
                   BUDDIES_TRANSPORT='http', BUDDIES_HOST='127.0.0.1', BUDDIES_PORT='0')
        proc = subprocess.Popen([str(binary)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 15
            url = None
            while time.monotonic() < deadline:
                if select.select([proc.stderr], [], [], .1)[0]:
                    line = proc.stderr.readline().decode()
                    if 'listening on http://' in line:
                        url = line.split('listening on ', 1)[1].strip()
                        break
            assert url, 'server startup failed'
            a, b, outsider = Client(url), Client(url), Client(url)
            ja = a.call('join_room', room='shared')
            jb = b.call('join_room', room='shared')
            outsider.call('join_room', room='elsewhere')
            peers = a.call('get_room_status', room='shared')['peers']
            assert len(peers) == 1, f'Expected other local participant, got {peers}'
            assert ja['agent_id'] != jb['agent_id']
            assert peers[0]['name'] == jb['agent_id'] and peers[0]['scope'] == 'local'
            assert outsider.call('list_rooms')['rooms'] == ['elsewhere']
            with urllib.request.urlopen(urllib.request.Request(url, headers={**a.headers, 'Accept': 'text/event-stream'}), timeout=3) as stream:
                b.call('notify_peers', room='shared', text='working locally')
                while True:
                    line = stream.readline()
                    assert line, 'notification stream closed'
                    if line.startswith(b'data: ') and line[6:].strip():
                        event = json.loads(line[6:])
                        if event.get('method') == 'notifications/buddies/status':
                            assert event['params']['author'] == jb['agent_id']
                            assert event['params']['text'] == 'working locally'
                            break
            assert a.call('get_room_status', room='shared')['peers'][0]['last_status'] == 'working locally'
            with concurrent.futures.ThreadPoolExecutor() as executor:
                delegated = executor.submit(a.call, 'delegate_task', room='shared', description='local task', timeout_secs=8)
                deadline = time.monotonic() + 4
                while True:
                    tasks = b.call('poll_pending_tasks', room='shared', wait_secs=0)['tasks']
                    if tasks:
                        break
                    assert time.monotonic() < deadline, 'local task was never delivered'
                    time.sleep(.05)
                task = tasks[0]
                assert task['source_peer'] == ja['agent_id']
                assert a.call('poll_pending_tasks', room='shared', wait_secs=0)['tasks'] == []
                assert outsider.call('poll_pending_tasks', wait_secs=0)['tasks'] == []
                forged = outsider.rpc('tools/call', {'name': 'submit_task_result', 'arguments': {
                    'task_id': task['task_id'], 'room': 'shared', 'source_peer': task['source_peer'],
                    'success': True, 'output': 'forged'}}, allow_error=True)
                assert forged.get('isError'), forged
                b.call('submit_task_result', task_id=task['task_id'], room='shared',
                       source_peer=task['source_peer'], success=True, output='done locally')
                assert delegated.result(timeout=3) == {'status': 'completed', 'output': 'done locally'}
            duplicate = b.rpc('tools/call', {'name': 'submit_task_result', 'arguments': {
                'task_id': task['task_id'], 'room': 'shared', 'source_peer': task['source_peer'],
                'success': True, 'output': 'duplicate'}}, allow_error=True)
            assert duplicate.get('isError'), duplicate
            with concurrent.futures.ThreadPoolExecutor() as executor:
                pending = executor.submit(a.call, 'delegate_task', room='shared', description='disconnect task', timeout_secs=8)
                deadline = time.monotonic() + 4
                while not b.call('poll_pending_tasks', room='shared', wait_secs=0)['tasks']:
                    assert time.monotonic() < deadline
                    time.sleep(.05)
                b.close()
                assert pending.result(timeout=4)['status'] == 'error'
            b = Client(url)
            assert b.call('join_room', room='shared')['agent_id'] != jb['agent_id']
            assert a.call('delegate_task', room='shared', description='timeout task', timeout_secs=1)['status'] == 'error'
            assert b.call('poll_pending_tasks', room='shared', wait_secs=0)['tasks'] == []
            b.call('leave_room', room='shared')
            assert a.call('list_rooms')['rooms'] == ['shared']
            assert a.call('get_room_status', room='shared')['peers'] == []
            b.call('join_room', room='shared')
            b.close()
            deadline = time.monotonic() + 4
            while a.call('get_room_status', room='shared')['peers']:
                assert time.monotonic() < deadline, 'closed local participant retained'
                time.sleep(.05)
            a.close()
            outsider.close()
            print('Local participants/status/task routing/isolation/leave/disconnect: OK')
        finally:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            proc.stderr.close()


if __name__ == '__main__':
    main()
