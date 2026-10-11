#!/usr/bin/env python3
"""Addressed conversations, Claude Channel delivery and a persistent Codex runtime."""
from collections import deque
import concurrent.futures
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlparse
from smoke_local_agents import Client
from smoke_room_tickets import Server


class Channel:
    def __init__(self, binary, url, name='claude'):
        self.proc = subprocess.Popen([str(binary), 'channel', '--url', url, '--room', 'shared', '--name', name],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.received = queue.Queue()
        self.events = deque()
        self.sequence = 0
        def read():
            for line in self.proc.stdout:
                self.received.put(json.loads(line))
            self.received.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
        info = self.rpc('initialize', {'protocolVersion': '2025-11-25', 'capabilities': {},
                        'clientInfo': {'name': 'claude-code-fixture', 'version': '1'}})
        assert info['capabilities']['experimental']['claude/channel'] == {}, info
        assert info['protocolVersion'] == '2025-11-25', info
        self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + '\n')
        self.proc.stdin.flush()

    def rpc(self, method, params, allow_error=False):
        self.sequence += 1
        ident = self.sequence
        self.send({'jsonrpc': '2.0', 'id': ident, 'method': method, 'params': params})
        while True:
            msg = self.received.get(timeout=15)
            assert msg is not None, 'channel exited: ' + self.proc.stderr.read()
            if msg.get('id') == ident:
                if allow_error and 'error' in msg:
                    return msg
                assert 'error' not in msg, msg
                return msg['result']
            self.events.append(msg)

    def call(self, tool_name, **arguments):
        value = self.rpc('tools/call', {'name': tool_name, 'arguments': arguments})
        assert not value.get('isError'), value
        return json.loads(value['content'][0]['text'])

    def event(self, kind='message'):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            msg = self.events.popleft() if self.events else self.received.get(timeout=15)
            assert msg is not None, 'channel exited'
            if msg.get('method') == 'notifications/claude/channel' and msg['params']['meta']['event'] == kind:
                return json.loads(msg['params']['content'])
        raise AssertionError('expected channel event ' + kind)

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
            raise
        self.reader.join(timeout=3)
        self.proc.stdout.close()
        self.proc.stderr.close()


FAKE = r'''#!/usr/bin/env python3
import json,sys
from pathlib import Path
thread='persistent-fixture'
turn=0
current=[]
pending={}
def emit(value): print(json.dumps(value),flush=True)
def final(text):
    emit({'method':'item/completed','params':{'threadId':thread,'item':{'type':'agentMessage','text':text}}})
    emit({'method':'turn/completed','params':{'threadId':thread,'turn':{'id':f'turn-{turn}','status':'completed'}}})
def tool(name,args,then):
    ident=1000+turn
    pending[ident]=then
    emit({'id':ident,'method':'item/tool/call','params':{'threadId':thread,'turnId':f'turn-{turn}','tool':name,'arguments':args}})
for line in sys.stdin:
    msg=json.loads(line)
    if 'method' not in msg:
        ident=msg['id']
        assert msg['result']['success'],msg
        assert msg['result']['contentItems'][0]['type']=='inputText'
        pending.pop(ident)()
        continue
    if 'id' not in msg: continue
    method=msg['method']; p=msg['params']
    with open('rpc.log','a') as f: f.write(method+'\n')
    if method=='initialize':
        assert p['capabilities']['experimentalApi'] is True
        result={}
    elif method=='thread/start':
        assert p['approvalPolicy']=='never'
        assert any(t['name']=='buddies_send_message' for t in p['dynamicTools'])
        result={'thread':{'id':thread}}
    elif method=='turn/start':
        assert p['threadId']==thread
        turn+=1; current=json.loads(p['input'][0]['text']); m=current[0]
        result={'turn':{'id':f'turn-{turn}'}}
    elif method=='turn/steer':
        assert p['expectedTurnId']==f'turn-{turn}'
        current.extend(json.loads(p['input'][0]['text']))
        result={'turnId':f'turn-{turn}'}
    else: raise Exception(method)
    emit({'id':msg['id'],'result':result})
    if method=='turn/start':
        if m['content']=='hold': continue
        if m['kind']=='question':
            tool('buddies_send_message',{'room':'shared','to':m['from'],'kind':'question',
                'content':'Which field should I use?','in_reply_to':m['id']},lambda:final('Waiting for the contract.'))
        elif m['kind']=='answer':
            Path('contract.txt').write_text(m['content'])
            tool('buddies_send_message',{'room':'shared','to':m['from'],'kind':'update',
                'content':'Implemented '+m['content'],'in_reply_to':m['id']},lambda:final('Implementation verified.'))
        else: final('done')
    elif method=='turn/steer':
        Path('steered.txt').write_text(current[-1]['content'])
        final('Updated the active work.')
'''


def eventually(fn, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = fn()
        if value:
            return value
        time.sleep(.1)
    raise AssertionError('condition never became true')


def main():
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = Path(metadata['target_directory']) / 'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-collaboration-') as directory:
        root = Path(directory)
        server = Server(binary, str(root/'server'), 'service')
        channels = []
        agent = None
        try:
            alice = Client(server.url)
            alice.call('join_room', room='shared')
            alice.call('register_participant', room='shared', name='alice')
            # Presence without an executor declaration must fail immediately.
            idle = Client(server.url)
            idle.call('join_room', room='shared')
            result = alice.rpc('tools/call', {'name':'delegate_task','arguments':{
                'room':'shared','description':'must not go to an idle session','timeout_secs':1}}, allow_error=True)
            assert result.get('isError'), result
            idle.close()
            claude = Channel(binary, server.url)
            channels.append(claude)
            tools = claude.rpc('tools/list', {})['tools']
            assert any(t['name']=='send_message' for t in tools)
            question = alice.call('send_message', room='shared', to='claude', kind='question', content='Which contract?')
            received = claude.event()
            assert received['id'] == question['id'] and received['state'] == 'pending'
            claude.call('acknowledge_message', room='shared', message_id=received['id'], state='read')
            reply = claude.call('send_message', room='shared', to='alice', kind='answer', content='user_id', in_reply_to=received['id'])
            assert reply['conversation_id'] == question['conversation_id']
            assert alice.call('poll_messages', room='shared')['messages'][0]['id'] == reply['id']
            forged = alice.rpc('tools/call', {'name':'acknowledge_message','arguments':{
                'room':'shared','message_id':question['id'],'state':'handled'}}, allow_error=True)
            assert forged.get('isError'), forged
            claude.call('acknowledge_message', room='shared', message_id=received['id'], state='handled')
            assert claude.call('poll_messages', room='shared')['messages'] == []
            with concurrent.futures.ThreadPoolExecutor() as pool:
                future = pool.submit(alice.call, 'delegate_task', room='shared', description='Channel task', timeout_secs=10)
                task = claude.event('task')
                claude.call('submit_task_result', task_id=task['task_id'], room='shared', source_peer=task['source_peer'], success=True, output='Channel task done')
                assert future.result(timeout=5)['output'] == 'Channel task done'
            # One model thread asks for clarification, resumes on the answer, then accepts steering.
            fake = root/'codex-fixture'
            fake.write_text(FAKE)
            fake.chmod(0o700)
            with open(root/'agent.log', 'w') as log:
                agent = subprocess.Popen([str(binary),'agent','--url',server.url,'--room','shared','--name','codex',
                    '--cwd',str(root),'--codex-bin',str(fake)],stdout=log,stderr=log)
            eventually(lambda: any(p['name']=='codex' for p in alice.call('get_room_status',room='shared')['participants']))
            initial = claude.call('send_message', room='shared', to='codex', kind='question', content='Implement the client')
            clarification = claude.event()
            assert clarification['from']=='codex' and clarification['in_reply_to']==initial['id']
            claude.call('send_message', room='shared', to='codex', kind='answer', content='user_id', in_reply_to=clarification['id'])
            update = claude.event()
            assert update['content']=='Implemented user_id'
            assert (root/'contract.txt').read_text()=='user_id'
            claude.call('acknowledge_message', room='shared', message_id=clarification['id'], state='handled')
            claude.call('acknowledge_message', room='shared', message_id=update['id'], state='handled')
            held = claude.call('send_message', room='shared', to='codex', kind='request', content='hold')
            eventually(lambda: any(m['id']==held['id'] and m['state']=='read' for m in
                claude.call('get_conversation',room='shared',conversation_id=held['conversation_id'])['messages']))
            claude.call('send_message', room='shared', to='codex', kind='update', content='use account_id', in_reply_to=held['id'])
            completion = claude.event()
            assert completion['in_reply_to']==held['id'] and completion['kind']=='answer'
            claude.call('acknowledge_message', room='shared', message_id=completion['id'], state='handled')
            assert (root/'steered.txt').read_text()=='use account_id'
            methods=(root/'rpc.log').read_text().splitlines()
            assert methods.count('thread/start')==1 and methods.count('turn/start')==3 and methods.count('turn/steer')==1,methods
            agent.terminate()
            assert agent.wait(timeout=8)==0
            claude.close()
            channels.remove(claude)
            # Offline inbox entries and receipt history survive a service restart.
            offline=alice.call('send_message',room='shared',to='claude',kind='question',content='After restart')
            port=urlparse(server.url).port
            alice.close()
            server.stop()
            server=Server(binary,str(root/'server'),'service',port=port)
            alice=Client(server.url)
            alice.call('join_room',room='shared')
            alice.call('register_participant',room='shared',name='alice')
            claude=Channel(binary,server.url)
            channels.append(claude)
            assert claude.event()['id']==offline['id']
            history=alice.call('get_conversation',room='shared',conversation_id=question['conversation_id'])['messages']
            assert history[0]['state']=='handled'
            claude.close()
            channels.remove(claude)
            alice.close()
            print('Inbox/receipts/restart, Claude Channel task and reply, Codex clarification/action/context/steering: OK')
        except Exception:
            if (root/'agent.log').exists(): print((root/'agent.log').read_text()[-6000:])
            raise
        finally:
            if agent is not None and agent.poll() is None:
                agent.terminate()
                agent.wait(timeout=8)
            for channel in channels:
                channel.close()
            server.stop()


if __name__ == '__main__':
    main()
