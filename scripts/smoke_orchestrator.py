#!/usr/bin/env python3
"""Real transport/worktrees with a deterministic multi-thread App Server fixture.

No model usage. Exercises the design's producer/consumer experiment, then restart
recovery while a specialist has unintegrated effects.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from smoke_collaboration import eventually
from smoke_local_agents import Client
from smoke_room_tickets import Server


FAKE = r'''#!/usr/bin/env python3
import json, os, re, sys
from pathlib import Path
assert 'features.multi_agent=false' in sys.argv
assert 'features.multi_agent_v2=false' in sys.argv
assert 'features.goals=false' in sys.argv
threads = {}
pending = {}
sequence = 10000
changed = False
log = Path(os.environ['BUDDIES_FIXTURE_LOG'])
def record(**value):
    with log.open('a') as f: f.write(json.dumps(value)+'\n')
def emit(value): print(json.dumps(value), flush=True)
def final(tid, text, status='completed'):
    emit({'method':'item/completed','params':{'threadId':tid,'item':{'type':'agentMessage','text':text}}})
    emit({'method':'turn/completed','params':{'threadId':tid,'turn':{'id':threads[tid]['turn'],'status':status}}})
def tool(tid, name, arguments, done):
    global sequence
    sequence += 1
    pending[sequence] = done
    record(event='tool', thread=tid, name=name, arguments=arguments)
    emit({'id':sequence,'method':'item/tool/call','params':{'threadId':tid,'turnId':threads[tid]['turn'],'tool':name,'arguments':arguments}})
def control(tid, arguments, done): tool(tid,'orchestrator_control',arguments,done)
def send(tid, arguments, done): tool(tid,'buddies_send_message',{'room':'shared',**arguments},done)
def accept(tid, a, done): control(tid,{'action':'accept','id':a['id'],'revision':a['revision']},lambda _:done())
def report(tid, a, done):
    control(tid,{'action':'report','id':a['id'],'revision':a['revision'],
        'summary':'Implemented '+a['description'],'checks':['test -f '+a['paths'][0]]},lambda _:done())
def coordinator(tid, data):
    global changed
    g = data['graph']; assignments = g['assignments']; messages = data['messages']
    if any(a['uncertain'] for a in assignments.values()):
        final(tid,'Inspect interrupted effects before retrying.'); return
    if not assignments:
        control(tid,{'action':'assign','id':'producer','owner':'specialist-a','description':'Write producer fields',
            'paths':['producer.json'],'dependencies':[],'deadline_secs':90},lambda _:
            control(tid,{'action':'assign','id':'consumer','owner':'specialist-b','description':'Consume the unspecified producer field',
                'paths':['consumer.txt'],'dependencies':['producer'],'deadline_secs':90},lambda _:final(tid,'Assignments recorded.')))
    elif all(a['state']=='completed' for a in assignments.values()):
        control(tid,{'action':'complete','summary':'Revised producer/consumer contract verified.'},lambda _:final(tid,'Done.'))
    elif any(m['content']=='Change the consumer contract to account_id.' for m in messages):
        changed = True
        def checkpoint(g):
            a = g['assignments']['consumer']
            send(tid,{'to':'specialist-b','kind':'update','content':'Checkpoint before changing to account_id.',
                'in_reply_to':a['message_id']},lambda _:final(tid,'Checkpoint requested.'))
        control(tid,{'action':'inspect'},checkpoint)
    elif changed and assignments['consumer']['revision']==1 and assignments['consumer']['state']=='waiting_for_input' and 'specialist-b' not in g['inflight']:
        control(tid,{'action':'revise','id':'consumer','description':'Consume account_id from the unchanged producer'},lambda _:final(tid,'Revision 2 requires acceptance.'))
    else: final(tid,'Waiting for a meaningful result or clarification.')
def specialist(tid, data):
    t = threads[tid]; a = data['assignment']; messages = data['messages']; cwd = Path(t['cwd'])
    if t['name']=='specialist-a':
        if a:
            def implement():
                (cwd/'producer.json').write_text(json.dumps({'id':1,'account_id':42}))
                report(tid,a,lambda:final(tid,'Producer verified.'))
            accept(tid,a,implement)
        else:
            assert t['sandbox']['type']=='readOnly',t
            m = messages[0]
            send(tid,{'to':m['from'],'kind':'answer','content':'id','in_reply_to':m['id']},lambda _:final(tid,'Recorded clarification: id.'))
    elif a['revision']==2:
        def implement():
            (cwd/'consumer.txt').write_text('account_id')
            # A late old-revision result remains evidence and cannot complete r2.
            control(tid,{'action':'report','id':'consumer','revision':1,'summary':'Late legacy result','checks':['true']},lambda _:
                report(tid,a,lambda:final(tid,'Consumer revision 2 verified.')))
        accept(tid,a,implement)
    elif messages[0]['kind']=='answer':
        assert a['accepted_revision']==1,a
        (cwd/'consumer.txt').write_text(messages[0]['content'])
        record(event='holding',thread=tid,cwd=str(cwd),revision=a['revision'])
        # Remain active until the orchestrator steers a checkpoint.
    else:
        def ask():
            send(tid,{'to':'specialist-a','kind':'question','content':'Which field should the consumer use?'},lambda _:
                control(tid,{'action':'checkpoint','id':'consumer','revision':1,'detail':'Waiting for the missing contract field'},lambda _:final(tid,'Waiting for clarification.')))
        accept(tid,a,ask)
for line in sys.stdin:
    msg = json.loads(line)
    if 'method' not in msg:
        assert msg['result']['success'],msg
        value = json.loads(msg['result']['contentItems'][0]['text'])
        if 'conversation_id' in value: record(event='message', value=value)
        pending.pop(msg['id'])(value)
        continue
    if 'id' not in msg: continue
    method = msg['method']; p = msg['params']; tid = p.get('threadId')
    record(event='rpc',method=method,thread=tid)
    if method=='initialize':
        assert p['capabilities']['experimentalApi']; result={}
    elif method in ('thread/start','thread/resume'):
        name = re.search(r'Stable participant: (\S+)\. Goal:',p['developerInstructions']).group(1)
        tid = tid or 'thread-'+name
        assert tid=='thread-'+name
        assert p['approvalPolicy']=='never'
        if method=='thread/start':
            tools = p['dynamicTools']
            assert next(t for t in tools if t['name']=='orchestrator_control')['inputSchema']['type']=='object'
        threads[tid] = {'name':name,'cwd':p['cwd'],'turns':0}
        result={'thread':{'id':tid}}
    elif method=='turn/start':
        t=threads[tid]; t['turns']+=1; t['turn']='turn-'+tid+'-'+str(t['turns'])
        t['cwd']=p['cwd']; t['sandbox']=p['sandboxPolicy']
        data=json.loads(p['input'][0]['text']); result={'turn':{'id':t['turn']}}
    elif method=='turn/steer':
        t=threads[tid]; assert p['expectedTurnId']==t['turn']
        data=json.loads(p['input'][0]['text']); result={'turnId':t['turn']}
    elif method=='turn/interrupt': result={}
    else: raise AssertionError(method)
    emit({'id':msg['id'],'result':result})
    if method=='turn/start':
        if threads[tid]['name']=='orchestrator': coordinator(tid,data)
        else: specialist(tid,data)
    elif method=='turn/steer':
        a=data['assignment']; assert a['id']=='consumer' and a['revision']==1
        control(tid,{'action':'checkpoint','id':'consumer','revision':1,'detail':'Retained legacy effects before material update'},lambda _:final(tid,'Checkpoint ready.'))
    elif method=='turn/interrupt': final(tid,'Interrupted.','interrupted')
'''


def events(log):
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text().splitlines()]


def snapshot(repo):
    files = list((repo/'.git/buddies-orchestrator').glob('*/state.json'))
    return json.loads(files[0].read_text()) if files else None


def seed(repo):
    repo.mkdir()
    (repo/'producer.json').write_text('{}')
    (repo/'consumer.txt').write_text('missing')
    (repo/'check.py').write_text("import json\nfrom pathlib import Path\nassert Path('consumer.txt').read_text() == 'account_id'\nassert json.loads(Path('producer.json').read_text())['account_id'] == 42\n")
    subprocess.run(['git', 'init', '-q', str(repo)], check=True)
    subprocess.run(['git', '-C', str(repo), 'add', '.'], check=True)
    subprocess.run(['git', '-C', str(repo), '-c', 'user.name=test', '-c', 'user.email=test@test',
                    '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/dev/null', 'commit', '-qm', 'base'], check=True)


def launch(binary, server, repo, fake, log, stderr, resume=False):
    args = [str(binary), 'orchestrator', '--room', 'shared', '--repo', str(repo),
            '--goal', 'Implement the producer/consumer contract', '--allow-path', 'producer.json',
            '--allow-path', 'consumer.txt', '--check', 'python3 check.py', '--url', server.url,
            '--codex-bin', str(fake), '--debounce-ms', '50', '--max-elapsed-secs', '120',
            '--max-model-calls', '32', '--check-timeout-secs', '5']
    if resume:
        args.append('--resume')
    return subprocess.Popen(args, env={**os.environ, 'BUDDIES_FIXTURE_LOG': str(log)},
                            stdout=subprocess.PIPE, stderr=stderr, text=True)


def stop(proc):
    if proc.poll() is None:
        proc.terminate()
    proc.communicate(timeout=10)


def experiment(binary, root, recovery=False):
    seed(root/'repo')
    repo = root/'repo'
    fake = root/'codex-fixture'
    fake.write_text(FAKE)
    fake.chmod(0o755)
    log = root/'rpc.jsonl'
    server = Server(binary, str(root/'service'), 'service')
    client = None
    proc = None
    with (root/'stderr.log').open('w') as stderr:
        try:
            client = Client(server.url)
            client.call('join_room', room='shared')
            client.call('register_participant', room='shared', name='user')
            proc = launch(binary, server, repo, fake, log, stderr)
            def holding():
                assert proc.poll() is None, (root/'stderr.log').read_text()
                return any(e['event']=='holding' for e in events(log))
            eventually(holding, timeout=25)
            before = snapshot(repo)
            assert before['assignments']['producer']['state']=='completed', before
            assert before['assignments']['consumer']['accepted_revision']==1, before
            # Nothing is published while the consumer is active/unverified.
            assert (repo/'producer.json').read_text()=='{}'
            if recovery:
                stop(proc)
                stopped = snapshot(repo)
                assert stopped['assignments']['consumer']['uncertain'], stopped
                offset = len(events(log))
                proc = launch(binary, server, repo, fake, log, stderr, resume=True)
                eventually(lambda: any(e.get('method')=='thread/resume' and e.get('thread')=='thread-specialist-b'
                                       for e in events(log)[offset:]))
                time.sleep(1)
                recovered = snapshot(repo)
                assert recovered['assignments']['consumer']['uncertain'], recovered
                assert recovered['assignments']['consumer']['revision']==1
                assert not any(e.get('method')=='turn/start' and e.get('thread')=='thread-specialist-b'
                               for e in events(log)[offset:]), events(log)[offset:]
                assert recovered['calls'] >= stopped['calls'] and recovered['elapsed_ms'] >= stopped['elapsed_ms']
                stop(proc)
                proc = None
            else:
                client.call('send_message', room='shared', to='orchestrator', kind='request',
                            content='Change the consumer contract to account_id.')
                out, _ = proc.communicate(timeout=30)
                assert proc.returncode==0, (out, (root/'stderr.log').read_text())
                proc = None
                state = snapshot(repo)
                assert state['completed'] and not state['publishing'], state
                assert state['assignments']['producer']['revision']==1
                consumer = state['assignments']['consumer']
                assert consumer['revision']==consumer['accepted_revision']==2, consumer
                assert consumer['state']=='completed' and consumer['evidence'][-1]['verified'], consumer
                assert any(e['revision']==1 and not e['verified'] for e in consumer['evidence'])
                assert (repo/'consumer.txt').read_text()=='account_id'
                subprocess.run(['python3', 'check.py'], cwd=repo, check=True)
                rpc = events(log)
                assert sum(e.get('method')=='thread/start' for e in rpc)==3, rpc
                assert sum(e.get('method')=='turn/steer' for e in rpc)==1, rpc
                assert sum(e.get('arguments',{}).get('action')=='accept' and e['thread']=='thread-specialist-b'
                           for e in rpc)==2, rpc
                question = next(e for e in rpc if e.get('arguments',{}).get('kind')=='question')
                answer = next(e for e in rpc if e.get('arguments',{}).get('kind')=='answer')
                assert question['arguments']['to']=='specialist-a' and answer['arguments']['content']=='id'
                recorded = next(e['value'] for e in rpc if e['event']=='message' and e['value']['kind']=='question')
                inspector = Client(server.url)
                try:
                    inspector.call('join_room', room='shared')
                    inspector.call('register_participant', room='shared', name='specialist-b')
                    history = inspector.call('get_conversation', room='shared', conversation_id=recorded['conversation_id'])['messages']
                    assert any(m['kind']=='question' and m['state']=='handled' for m in history), history
                    assert any(m['kind']=='answer' and m['content']=='id' and m['state']=='handled' for m in history), history
                finally:
                    inspector.close()
                # Completed resume prints the stored result without starting Codex.
                offset = len(rpc)
                proc = launch(binary, server, repo, fake, log, stderr, resume=True)
                proc.communicate(timeout=10)
                assert proc.returncode==0 and len(events(log))==offset
                proc = None
                print(f"orchestrator experiment: {state['calls']} dispatches, {state['elapsed_ms']} ms, revision 2 verified")
        except Exception:
            stderr.flush()
            print((root/'stderr.log').read_text())
            print(events(log)[-12:])
            raise
        finally:
            if proc is not None:
                stop(proc)
            if client is not None:
                client.close()
            server.stop()


def main():
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = Path(metadata['target_directory'])/'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-orchestrator-') as directory:
        root = Path(directory)
        for name, recovery in [('acceptance', False), ('recovery', True)]:
            (root/name).mkdir()
            experiment(binary, root/name, recovery)
    print('orchestrator smoke: acceptance and interrupted recovery passed')


if __name__ == '__main__':
    main()
