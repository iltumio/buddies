#!/usr/bin/env python3
"""Exercise automatic execution with a deterministic Codex App Server fixture."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from urllib.parse import urlparse
from smoke_local_agents import Client
from smoke_room_tickets import Server

FAKE = r'''#!/usr/bin/env python3
import json,sys,time,os
from pathlib import Path
for line in sys.stdin:
    msg=json.loads(line)
    if 'id' not in msg: continue
    method=msg['method']
    if method=='initialize': result={}
    elif method=='thread/start':
        assert msg['params']['approvalPolicy']=='never'
        assert msg['params']['sandbox']=='workspace-write'
        result={'thread':{'id':'thread-fixture'}}
    elif method=='turn/start':
        text=msg['params']['input'][0]['text']
        with open('executions.log','a') as f: f.write(text+'\n')
        result={'turn':{'id':'turn-fixture'}}
        # Events preceding the request response must not get lost.
        print(json.dumps({'method':'item/completed','params':{'threadId':'thread-fixture','item':{'type':'agentMessage','text':'AUTOMATIC_RESULT'}}}),flush=True)
    else: raise Exception(method)
    print(json.dumps({'id':msg['id'],'result':result}),flush=True)
    if method=='turn/start':
        if text=='hang':
            Path('child.pid').write_text(str(os.getpid()))
            time.sleep(60)
        print(json.dumps({'method':'turn/completed','params':{'threadId':'thread-fixture','turn':{'id':'turn-fixture','status':'completed'}}}),flush=True)
'''


def eventually(fn, seconds=15):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        result=fn()
        if result: return result
        time.sleep(.1)
    raise AssertionError('timed out waiting for worker')


def main():
    metadata=json.loads(subprocess.check_output(['cargo','metadata','--no-deps','--locked','--format-version','1']))
    binary=Path(metadata['target_directory'])/'debug/buddies'
    with tempfile.TemporaryDirectory(prefix='buddies-worker-') as directory:
        root=Path(directory)
        fake=root/'codex-fixture';fake.write_text(FAKE);fake.chmod(0o700)
        server=Server(binary,str(root/'server'),'server')
        processes=[]
        def start_worker(once=False):
            log=open(root/'worker.log','ab')
            proc=subprocess.Popen([str(binary),'worker','--room','tasks','--cwd',str(root),'--url',server.url,
                '--data-dir',str(root/'worker'),'--codex-bin',str(fake)]+(['--once'] if once else []),stdout=log,stderr=log)
            log.close(); processes.append(proc);return proc
        try:
            client=Client(server.url);client.call('join_room',room='tasks')
            worker=start_worker()
            identity=eventually(lambda: client.call('get_room_status',room='tasks').get('workers'))[0]['id']
            result=client.call('delegate_task',room='tasks',description='first',timeout_secs=30,target_agent='codex')
            assert result['status']=='completed' and result['output']=='AUTOMATIC_RESULT',result
            assert (root/'executions.log').read_text().splitlines()==['first']
            worker.terminate();assert worker.wait(timeout=10)==0
            # Accepted tasks survive the requester session and a server restart.
            pending=client.call('delegate_task',room='tasks',description='second',timeout_secs=120,target_agent=identity,background=True)
            port=urlparse(server.url).port
            client.close(); server.stop()
            server=Server(binary,str(root/'server'),'server',port=port)
            worker=start_worker(once=True)
            client=Client(server.url);client.call('join_room',room='tasks')
            done=eventually(lambda: (lambda j: j if j['state']=='completed' else None)(client.call('get_task_status',room='tasks',task_id=pending['task_id'])),seconds=45)
            assert done['outcome']['output']=='AUTOMATIC_RESULT'
            assert worker.wait(timeout=10)==0
            assert (root/'executions.log').read_text().splitlines()==['first','second']
            assert client.call('get_room_status',room='tasks')['workers'][0]['id']==identity
            # A timed out execution is killed and stays failed, never silently repeated.
            worker=start_worker(once=True)
            pending=client.call('delegate_task',room='tasks',description='hang',timeout_secs=40,target_agent=identity,background=True)
            eventually(lambda: (root/'child.pid').exists(),seconds=35)
            worker.terminate();assert worker.wait(timeout=10)==0
            failed=client.call('get_task_status',room='tasks',task_id=pending['task_id'])
            assert failed['state']=='failed',failed
            pid=int((root/'child.pid').read_text())
            try: os.kill(pid,0)
            except ProcessLookupError: pass
            else: raise AssertionError('Codex process survived worker shutdown')
            (root/'child.pid').unlink()
            worker=start_worker(once=True)
            pending=client.call('delegate_task',room='tasks',description='hang',timeout_secs=3,target_agent=identity,background=True)
            eventually(lambda: (root/'child.pid').exists())
            assert worker.wait(timeout=10)==0
            failed=client.call('get_task_status',room='tasks',task_id=pending['task_id'])
            assert failed['state']=='failed'
            try: os.kill(int((root/'child.pid').read_text()),0)
            except ProcessLookupError: pass
            else: raise AssertionError('Codex process survived deadline')
            client.close()
            print('Automatic delegation, stable identity, durable queue/result, restart, deadline and process cleanup: OK')
        except Exception:
            print((root/'worker.log').read_text()[-6000:])
            raise
        finally:
            for p in processes:
                if p.poll() is None: p.terminate();p.wait(timeout=10)
            server.stop()


if __name__=='__main__': main()
