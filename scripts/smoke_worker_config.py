#!/usr/bin/env python3
"""Exercise real tmux PTYs, group cleanup and Claude resume without model/API calls."""
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

FAKE = '''#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
assert sys.stdin.isatty()
Path('started.json').write_text(json.dumps({'pid': os.getpid(), 'args': sys.argv[1:]}))
while not Path('exit-now').exists():
    time.sleep(.05)
print('fixture requested exit', flush=True)
sys.exit(7)
'''


def eventually(fn):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        value = fn()
        if value:
            return value
        time.sleep(.1)
    raise AssertionError('timeout waiting for workers')


def main():
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1']))
    binary = str(Path(metadata['target_directory']) / 'debug/buddies')
    with tempfile.TemporaryDirectory(prefix='buddies-config-') as directory:
        root = Path(directory)
        fake = root / "claude ' fixture"
        fake.write_text(FAKE)
        fake.chmod(0o700)
        for name in ['one', 'two']:
            (root / name).mkdir()
        config = root / 'buddies.toml'
        config.write_text(f'''room = "fixture"
[defaults]
agent = "claude"
claude-bin = {json.dumps(str(fake))}
linear-team = ["ENG"]
[[specialists]]
name = "one"
cwd = "./one"
data-dir = "./state-one"
[[specialists]]
name = "two"
cwd = "./two"
data-dir = "./state-two"
''')
        args = [binary, 'up', '--config', str(config)]
        # Invoking elsewhere must still resolve every path relative to the TOML.
        checked = subprocess.run(args + ['--check'], cwd='/', capture_output=True, text=True, timeout=15)
        assert checked.returncode == 0, checked.stderr
        assert not (root / 'state-one').exists()
        sessions = {}
        for attempt in range(2):
            process = subprocess.Popen(args + (['--resume'] if attempt else []), cwd='/', stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            pids = []
            try:
                for name in ['one', 'two']:
                    marker = root / name / 'started.json'
                    eventually(marker.exists)
                    data = json.loads(marker.read_text())
                    pids.append(data['pid'])
                    flag = '--resume' if attempt else '--session-id'
                    session = data['args'][data['args'].index(flag) + 1]
                    if attempt:
                        assert session == sessions[name]
                    else:
                        sessions[name] = session
                    marker.unlink()
                if attempt:
                    (root / 'one' / 'exit-now').touch()
                else:
                    process.send_signal(signal.SIGTERM)
                out, err = process.communicate(timeout=20)
                if attempt:
                    assert process.returncode != 0 and "worker 'one' exited" in err, err
                    assert 'fixture requested exit' in err, err
                else:
                    assert process.returncode == 0, err
                socket = re.search(r'tmux -L (buddies-[a-f0-9]+) attach', err).group(1)
                status = subprocess.run(['tmux', '-L', socket, 'list-sessions'], capture_output=True)
                assert status.returncode != 0, 'tmux group leaked'
                for pid in pids:
                    eventually(lambda: not Path(f'/proc/{pid}').exists())
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                    process.communicate(timeout=20)
        # A bad later entry must prevent *all* starts.
        config.write_text(config.read_text().replace('name = "two"', 'name = "one"'))
        invalid = subprocess.run(args, capture_output=True, text=True, timeout=15)
        assert invalid.returncode != 0 and 'duplicate worker' in invalid.stderr, invalid.stderr
        assert not (root / 'one' / 'started.json').exists()
    print('TOML validation, paths/quoting, parallel PTYs, exact resume, SIGTERM and failure cleanup: OK')


if __name__ == '__main__':
    main()
