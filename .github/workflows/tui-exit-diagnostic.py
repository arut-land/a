"""Temporary macOS exit investigation; never calls a provider."""
from pathlib import Path
import json
import hashlib
import os
import re
import subprocess
import sys
import time


def capture():
    destination = Path(sys.argv[2])
    roles = dict(zip(['tui', 'daemon', 'harness'], sys.argv[3:6]))
    (destination / 'capture-start.json').write_text(json.dumps({'time': time.time(), 'pids': roles, 'before_cleanup': True}, indent=2))
    pids = ','.join(pid for pid in roles.values() if pid != 'none')
    statuses = []
    commands = [('process-state', ['/bin/ps', '-ww', '-p', pids, '-o', 'pid,ppid,pgid,stat,wchan,etime,time,command'])]
    for role, pid in roles.items():
        if pid == 'none':
            continue
        commands.extend([
            (role + '-threads', ['/bin/ps', '-M', '-p', pid]),
            (role + '-fds', ['/usr/sbin/lsof', '-n', '-P', '-p', pid]),
            (role + '-sample-command', ['/usr/bin/sample', pid, '1', '1', '-file', str(destination / (role + '-sample.txt'))]),
        ])
    for name, command in commands:
        result = subprocess.run(command, capture_output=True)
        (destination / (name + '.txt')).write_bytes(result.stdout + result.stderr)
        statuses.append({'name': name, 'exit': result.returncode})
    (destination / 'capture-complete.json').write_text(json.dumps({'time': time.time(), 'before_cleanup': True, 'commands': statuses}, indent=2))


def run():
    root, target, evidence, label = map(Path, sys.argv[2:6])
    label = str(label)
    iterations = int(sys.argv[6])
    threads = int(sys.argv[7])
    evidence.mkdir(parents=True, exist_ok=True)
    source = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    changed = subprocess.check_output(['git', 'diff', '--name-only'], cwd=root, text=True).splitlines()
    assert changed == ['apps/cli/tests/blackbox/common/pty.rs']
    observer = hashlib.sha256((root / changed[0]).read_bytes()).hexdigest()
    manifest = {'label': label, 'source_sha': source, 'observer_sha256': observer, 'changed_files': changed, 'iterations_requested': iterations, 'parallel_test_threads': threads, 'started': time.time()}
    (evidence / 'manifest.json').write_text(json.dumps(manifest, indent=2))
    assert os.environ['OPENAI_API_KEY'] == 'loopback-dummy'
    assert os.environ['OPENAI_BASE_URL'] == 'http://127.0.0.1:1/v1'
    assert not any('shims' in part or 'wrappers' in part for part in os.environ['PATH'].split(':'))
    print('child_environment_checks_passed=true', flush=True)
    os.chdir(root)
    selection = 'test(/^tui::.*title/) | test(=acceptance::a_history_larger_than_the_pending_event_budget_opens_without_loss) | test(=reactor::unread_clients_do_not_prevent_a_fresh_clients_turn) | test(=query::a_burst_larger_than_a_connection_holds_arrives_whole) | test(=pipe::piped_stdin_is_one_prompt_and_only_the_reply_reaches_stdout)'
    options = ['-p', 'a', '--features', 'fixtures,lua', '--test', 'blackbox', '-E', selection]
    listed = subprocess.run(['cargo', 'nextest', 'list', *options, '--message-format', 'json'], capture_output=True)
    (evidence / 'test-list.json').write_bytes(listed.stdout)
    (evidence / 'build.log').write_bytes(listed.stderr)
    if listed.returncode:
        print(listed.stderr.decode(errors='replace'), flush=True)
        raise SystemExit(listed.returncode)
    suites = json.loads(listed.stdout)['rust-suites']
    titles = sorted(name for suite in suites.values() for name, case in suite.get('testcases', {}).items() if name.startswith('tui::') and 'title' in name and case.get('filter-match', {}).get('status', 'matches') == 'matches')
    assert 'tui::a_quote_only_title_reports_failure_and_preserves_the_previous_title' in titles
    assert len(titles) >= 5
    counts = {name: {'executed': 0, 'failed': 0} for name in titles}
    results = []
    pattern = re.compile(r'\b(PASS|FAIL|TIMEOUT|LEAK|ABORT)\s+\[[^\]]+\].*a::blackbox\s+(tui::[a-zA-Z0-9_:]+)')
    for iteration in range(1, iterations + 1):
        case_dir = evidence / ('iteration-' + str(iteration).zfill(3))
        case_dir.mkdir()
        env = dict(os.environ, A_PTY_EVIDENCE=str(case_dir), A_PTY_TIMEOUT_CAPTURE=str(Path(__file__).resolve()))
        command = ['cargo', 'nextest', 'run', *options, '--no-fail-fast', '--no-tests=fail', '--test-threads', str(threads)]
        result = subprocess.run(command, env=env, capture_output=True)
        output = result.stdout + result.stderr
        (case_dir / 'nextest.log').write_bytes(output)
        seen = {}
        for status, name in pattern.findall(output.decode(errors='replace')):
            if name in counts:
                seen[name] = seen.get(name, False) or status != 'PASS'
        for name, failed in seen.items():
            counts[name]['executed'] += 1
            counts[name]['failed'] += int(failed)
        results.append({'iteration': iteration, 'exit': result.returncode, 'titles_seen': len(seen)})
        (evidence / 'progress.json').write_text(json.dumps({'manifest': manifest, 'counts': counts, 'iterations': results}, indent=2))
        print(label + ': iteration ' + str(iteration) + '/' + str(iterations) + ', exit=' + str(result.returncode) + ', titles=' + str(len(seen)), flush=True)
    complete = all(value['executed'] >= iterations for value in counts.values())
    summary = {'manifest': manifest, 'counts': counts, 'iterations': results, 'complete': complete, 'ended': time.time()}
    (evidence / 'summary.json').write_text(json.dumps(summary, indent=2))
    raise SystemExit(0 if complete and all(result['exit'] == 0 for result in results) else 1)


if sys.argv[1] == 'capture':
    capture()
else:
    run()
