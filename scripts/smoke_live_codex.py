#!/usr/bin/env python3
"""Opt-in live-model check: uses the signed-in Codex account and consumes model usage.

The peer is simulated. No commands/files are requested from the model; all server
and test files are temporary. This is intentionally not part of CI.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
from smoke_local_agents import Client
from smoke_room_tickets import Server


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex-bin', default='codex')
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = Path(metadata['target_directory'])/'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-live-codex-') as directory:
        root = Path(directory)
        subprocess.run(['git', 'init', '-q', str(root)], check=True)
        server = Server(binary, str(root/'server'), 'live-check')
        agent = None
        try:
            client = Client(server.url)
            client.call('join_room', room='shared')
            client.call('register_participant', room='shared', name='claude-peer')
            with open(root/'agent.log', 'w') as log:
                agent = subprocess.Popen([str(binary), 'agent', '--url', server.url, '--room', 'shared',
                    '--name', 'codex-live', '--cwd', str(root), '--codex-bin', args.codex_bin], stdout=log, stderr=log)

            def wait_for(fn, timeout=120):
                deadline = time.monotonic()+timeout
                while time.monotonic()<deadline:
                    result = fn()
                    if result:
                        return result
                    assert agent.poll() is None, 'Codex runtime exited'
                    time.sleep(.3)
                raise AssertionError('live coordination timed out')

            wait_for(lambda: any(p['name']=='codex-live' for p in
                client.call('get_room_status', room='shared')['participants']), timeout=60)
            client.call('send_message', room='shared', to='codex-live', kind='question', content=
                'This is a bounded communication integration test. Do not inspect files or execute commands. '
                'I am responsible for the API contract. Use buddies_send_message to ask me which field to use, '
                'with kind question and in_reply_to this message ID. Do not guess a field name. '
                'After sending the clarification, finish your turn and wait for my answer.')
            clarification = wait_for(lambda: next((m for m in client.call('poll_messages', room='shared')['messages']
                if m['from']=='codex-live' and m['kind']=='question'), None))
            answer = client.call('send_message', room='shared', to='codex-live', kind='answer',
                in_reply_to=clarification['id'], content='Use account_id. Continue the original coordination test: '
                'send me a kind update with in_reply_to this answer ID containing EXACTLY VERIFIED_ACCOUNT_ID, '
                'then finish. Do not execute commands.')
            wait_for(lambda: next((m for m in client.call('poll_messages', room='shared')['messages']
                if m['from']=='codex-live' and m['in_reply_to']==answer['id'] and m['content']=='VERIFIED_ACCOUNT_ID'), None))
            client.close()
            print('Real Codex clarification -> answer -> retained-context dynamic tool response: OK (simulated peer)')
        except Exception:
            if (root/'agent.log').exists():
                print((root/'agent.log').read_text()[-9000:])
            raise
        finally:
            if agent is not None and agent.poll() is None:
                agent.terminate()
                agent.wait(timeout=10)
            server.stop()


if __name__ == '__main__':
    main()
