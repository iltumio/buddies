#!/usr/bin/env python3
"""Live Linear channel contract and Claude launch/resume; no remote writes."""
import json
import os
from pathlib import Path
import pty
import subprocess
import tempfile
from smoke_collaboration import Channel, eventually
from smoke_local_agents import Client
from smoke_room_tickets import Server

FAKE = '''#!/usr/bin/env python3
import json,sys
from pathlib import Path
args=sys.argv[1:]
assert '--strict-mcp-config' not in args
assert '--dangerously-skip-permissions' not in args
assert '--print' not in args
assert args[args.index('--dangerously-load-development-channels')+1]=='server:buddies-worker'
config=json.loads(args[args.index('--mcp-config')+1])
assert list(config['mcpServers'])==['buddies-worker']
bridge=config['mcpServers']['buddies-worker']
assert '--linear-team' in bridge['args'] and 'ENG' in bridge['args']
assert '--linear-access' in bridge['args'] and 'mcp' in bridge['args']
prompt=args[args.index('--append-system-prompt')+1]
assert 'LIVE Linear teams: ENG' in prompt and 'MCP tools already available' in prompt
assert 'effect immediately' in prompt
Path('captured.json').write_text(json.dumps(args))
'''


def main():
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = Path(metadata['target_directory']) / 'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-linear-') as directory:
        root = Path(directory)
        fake = root/'claude'
        fake.write_text(FAKE)
        fake.chmod(0o700)
        args = [str(binary), 'worker', '--agent', 'claude', '--room', 'shared', '--name', 'linear',
                '--linear-team', 'ENG', '--linear-access', 'mcp', '--cwd', str(root),
                '--data-dir', str(root/'worker'), '--claude-bin', str(fake)]
        master, slave = pty.openpty()
        try:
            first = subprocess.run(args, stdin=slave, capture_output=True, timeout=15)
            assert first.returncode == 0, first.stderr
            captured = json.loads((root/'captured.json').read_text())
            session = captured[captured.index('--session-id')+1]
            duplicate = subprocess.run(args, stdin=slave, capture_output=True, timeout=15)
            assert duplicate.returncode != 0 and b'use --resume' in duplicate.stderr
            resumed = subprocess.run(args+['--resume'], stdin=slave, capture_output=True, timeout=15)
            assert resumed.returncode == 0, resumed.stderr
            captured = json.loads((root/'captured.json').read_text())
            assert captured[captured.index('--resume')+1] == session
            headless = subprocess.run(args+['--resume'], stdin=subprocess.DEVNULL, capture_output=True, timeout=15)
            assert headless.returncode != 0 and b'interactive terminal' in headless.stderr
        finally:
            os.close(master)
            os.close(slave)
        server = Server(binary, str(root/'server'), 'linear-fixture')
        channel = client = None
        try:
            client = Client(server.url)
            client.call('join_room', room='shared')
            client.call('register_participant', room='shared', name='requester')
            channel = Channel(binary, server.url, 'linear', ['--linear-team', 'ENG', '--linear-access', 'mcp'])
            assert 'LIVE Linear teams: ENG' in channel.info['instructions']
            participant = eventually(lambda: next((p for p in client.call('get_room_status', room='shared')['participants']
                                                   if p['name']=='linear' and p['accepts_tasks']), None))
            assert not participant['specialist'], participant
            assert participant['resources'] == [{'id':'linear:ENG', 'kind':'linear-live', 'scopes':['ENG'], 'checks':[]}]
            message = client.call('send_message', room='shared', to='linear', kind='request', content='Read ENG-123; do not modify it.')
            assert channel.event()['id'] == message['id']
            channel.call('acknowledge_message', room='shared', message_id=message['id'], state='handled')
            assert channel.call('poll_messages', room='shared')['messages'] == []
        finally:
            if channel: channel.close()
            if client: client.close()
            server.stop()
    print('Linear live registration, inbox, Claude config inheritance, exact session resume: OK')


if __name__ == '__main__':
    main()
