#!/usr/bin/env python3
"""Process-owned tickets survive clients, leaves and service restarts."""
import base64
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time
from smoke_local_agents import Client


def decode(ticket):
    return json.loads(base64.b32decode(ticket.upper() + '=' * (-len(ticket) % 8)))


class Server:
    def __init__(self, binary, directory, name):
        self.proc = subprocess.Popen([str(binary)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            env=dict(os.environ, BUDDIES_DATA_DIR=directory, BUDDIES_SIGNER='none',
                     BUDDIES_USER=name, BUDDIES_TRANSPORT='http', BUDDIES_HOST='127.0.0.1', BUDDIES_PORT='0'))
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if select.select([self.proc.stderr], [], [], .1)[0]:
                line = self.proc.stderr.readline().decode()
                if 'listening on http://' in line:
                    self.url = line.split('listening on ', 1)[1].strip()
                    return
        self.stop()
        raise AssertionError('server startup failed')

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
            raise
        self.proc.stderr.close()


def connected(client):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if client.call('get_room_status', room='shared')['connection']['neighbors'] > 0:
            return
        time.sleep(.1)
    raise AssertionError('stored ticket did not reconnect to external room')


def main():
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = Path(metadata['target_directory']) / 'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-tickets-') as root:
        remote = Server(binary, root + '/remote', 'remote')
        local = Server(binary, root + '/local', 'local')
        try:
            r = Client(remote.url)
            external = r.call('join_room', room='shared')
            a, b = Client(local.url), Client(local.url)
            initial = a.call('join_room', room='shared')
            b.call('join_room', room='shared', ticket=external['ticket'])
            remembered = a.call('join_room', room='shared')
            assert decode(external['ticket'])['endpoints'][0] in decode(remembered['ticket'])['endpoints'], 'external ticket was not retained by the process'
            assert remembered['ticket'] == b.call('join_room', room='shared')['ticket'], 'clients received different room tickets'
            connected(a)
            for c in (a, b):
                c.call('leave_room', room='shared')
                c.close()
            local.stop()
            local = Server(binary, root + '/local', 'local')
            a = Client(local.url)
            restored = a.call('join_room', room='shared')
            assert restored['endpoint_id'] == initial['endpoint_id'], 'node identity changed after restart'
            assert decode(external['ticket'])['endpoints'][0] in decode(restored['ticket'])['endpoints'], 'external ticket lost after restart'
            connected(a)
            invalid_topic = decode(external['ticket'])
            invalid_topic['topic'] = [0] * 32
            encoded = base64.b32encode(json.dumps(invalid_topic).encode()).decode().rstrip('=').lower()
            result = a.rpc('tools/call', {'name': 'join_room', 'arguments': {'room': 'shared', 'ticket': encoded}}, allow_error=True)
            assert result.get('isError'), 'mismatched topic accepted'
            for bad in ('not-a-ticket', external['ticket']):
                result = a.rpc('tools/call', {'name': 'join_room', 'arguments': {'room': 'wrong-room', 'ticket': bad}}, allow_error=True)
                assert result.get('isError'), 'invalid or mismatched ticket accepted'
            assert a.call('join_room', room='shared')['ticket'] == restored['ticket']
            a.close(); r.close()
            print('Shared tickets, late import, P2P reconnect, persistence and validation: OK')
        finally:
            local.stop()
            remote.stop()


if __name__ == '__main__':
    main()
