import errno
import json
import os
from pathlib import Path
import pty
import re
import selectors
import shutil
import signal
import subprocess
import time


out = Path(os.environ['A_DIAGNOSTIC']) / os.environ['A_DIAGNOSTIC_ITERATION']
out.mkdir(parents=True)
home = Path(os.environ['A_HOME'])
assert home != Path.home() / '.a'
(out / 'home-path').write_text(str(home))


def capture(name, command, timeout=40):
    with (out / name).open('wb') as f:
        try:
            subprocess.run(command, stdout=f, stderr=subprocess.STDOUT, timeout=timeout)
        except subprocess.TimeoutExpired:
            f.write(b'\nDIAGNOSTIC COMMAND TIMED OUT\n')


def processes():
    return subprocess.check_output(['ps', '-axo', 'pid=,ppid=,state=,etime=,command='], text=True)


def snapshot(pid):
    listing = processes()
    (out / 'processes.txt').write_text(listing)
    rows = [line.split(None, 4) for line in listing.splitlines()]
    owned = {pid}
    while True:
        children = {int(r[0]) for r in rows if int(r[1]) in owned}
        if children <= owned:
            break
        owned |= children
    # Staged daemons escape the Swift process tree; their executable lives in A_HOME.
    owned |= {int(r[0]) for r in rows if str(home) in r[4]}
    for target in sorted(owned):
        capture(f'{target}-sample.txt', ['sample', str(target), '3', '1'])
        capture(f'{target}-files.txt', ['lsof', '-nP', '-p', str(target)])
        capture(f'{target}-sockets.txt', ['lsof', '-nP', '-a', '-p', str(target), '-i'])
        capture(f'{target}-threads.txt', ['lldb', '-b', '-p', str(target), '-o', 'thread backtrace all', '-o', 'detach', '-o', 'quit'])
    capture('home-listing.txt', ['find', str(home), '-ls'])
    shutil.copytree(home, out / 'home', symlinks=True, ignore=shutil.ignore_patterns('*.sock'), dirs_exist_ok=True)
    return owned


master, slave = pty.openpty()
env = dict(os.environ, NSUnbufferedIO='YES')
started = time.monotonic()
process = subprocess.Popen(['swift', 'test', '--verbose', '--package-path', 'apps/apple/AKit'],
                           stdin=subprocess.DEVNULL, stdout=slave, stderr=slave, env=env,
                           start_new_session=True)
os.close(slave)
selector = selectors.DefaultSelector()
selector.register(master, selectors.EVENT_READ)
hung = False
with (out / 'swift.log').open('wb', buffering=0) as log:
    while selector.get_map():
        if not hung and process.poll() is None and time.monotonic() - started >= 600:
            hung = True
            (out / 'HANG').write_text('swift test exceeded 600 seconds; capturing before termination\n')
            owned = snapshot(process.pid)
            for target in sorted(owned, reverse=True):
                try:
                    os.kill(target, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        for key, _ in selector.select(timeout=1):
            try:
                data = os.read(key.fd, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                data = b''
            if not data:
                selector.unregister(key.fd)
                break
            log.write(data)
            os.write(1, data)
code = process.wait()
completed = re.search(r'Test run with \d+ tests?[^\r\n]*passed', (out / 'swift.log').read_text()) is not None
if code == 0 and not completed:
    print('Swift exited without a completed test-run summary', flush=True)
    code = 125
(out / 'result.json').write_text(json.dumps(dict(code=code, hang=hung, completed=completed, seconds=time.monotonic() - started)))
raise SystemExit(124 if hung else code)
