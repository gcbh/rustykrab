"""Usage-only recovery regression; real Git/LocalWorker, inert Ollama wire.

Only the temporary fixture database is seeded with historical verified work.
New recovery allocation, tool execution, Git verification and original resumption
are performed by the actual daemon. No live credentials, model or data are used.
"""
import argparse
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import signal
import socket
import sqlite3
import subprocess
import threading
import time
import urllib.request
import uuid


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--fixture', required=True, type=Path)
    parser.add_argument('--expect', choices=['self_correct', 'stalled'], default='self_correct')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    fixture = args.fixture.absolute()
    fixture.mkdir(mode=0o700, exist_ok=False)
    data = fixture / 'private-data'
    repo = data / 'repos/source'
    repo.mkdir(parents=True)

    def git(*words, cwd=repo):
        return subprocess.check_output(['git', '-c', 'core.hooksPath=/dev/null',
            '-c', 'user.name=Recovery fixture', '-c', 'user.email=fixture@example.invalid',
            '-C', str(cwd), *words], text=True, stderr=subprocess.STDOUT).strip()

    git('init', '-b', 'main')
    (repo / 'policy.txt').write_text('base\n')
    git('add', 'policy.txt')
    git('commit', '-m', 'base')
    ancestor = git('rev-parse', 'HEAD')
    git('checkout', '-b', 'old-change')
    (repo / 'policy.txt').write_text('old policy\n')
    git('commit', '-am', 'old branch')
    old = git('rev-parse', 'HEAD')
    git('checkout', 'main')
    (repo / 'policy.txt').write_text('new policy\n')
    git('commit', '-am', 'current branch')
    current = git('rev-parse', 'HEAD')
    marker = 'OWNER_DIRECTION_USAGE_ONLY'
    wire, errors, commits = [], [], []
    repair_calls = 0
    original_calls = 0
    warnings_seen = []
    lock = threading.Lock()
    repair_command = '\n'.join([
        'set -eu', 'pwd -P',
        'git -c user.name=Fixture -c user.email=fixture@example.invalid merge --no-commit --no-ff ' + old + ' || test -f "$(git rev-parse --git-path MERGE_HEAD)"',
        "printf 'new policy\\n' > policy.txt", 'git add policy.txt',
        'git -c user.name=Fixture -c user.email=fixture@example.invalid commit --no-gpg-sign -m resolve',
        'git merge-base --is-ancestor ' + old + ' HEAD',
        'git merge-base --is-ancestor ' + current + ' HEAD',
        'echo COMMIT:$(git rev-parse HEAD)',
    ])
    original_command = '\n'.join([
        'set -eu', 'pwd -P', 'git merge-base --is-ancestor ' + old + ' HEAD',
        'git merge-base --is-ancestor ' + current + ' HEAD',
        "printf 'original operation completed\\n' > original-result.txt", 'git add original-result.txt',
        'git -c user.name=Fixture -c user.email=fixture@example.invalid commit --no-gpg-sign -m original',
        'echo COMMIT:$(git rev-parse HEAD)',
    ])

    class Protocol(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def reply(self, code, body):
            payload = json.dumps(body).encode() + b'\n'
            self.send_response(code)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
            if self.path == '/api/show':
                return self.reply(200, {'model_info': {'fixture.context_length': 131072}, 'capabilities': ['completion', 'tools']})
            if self.path != '/api/chat':
                return self.reply(404, {'error': 'fixture endpoint unsupported'})
            with lock:
                n = len(wire)
                wire.append(body)
                save(fixture / 'wire.json', wire)
            try:
                nonlocal repair_calls, original_calls
                assert n < 60, 'unexpected additional execution'
                text = json.dumps(body['messages'])
                assert marker in text, 'original owner direction absent'
                assert 'Execution usage is recorded; there is no per-task token, iteration or runtime allowance.' in text, 'usage-only execution prompt absent'
                assert 'budget: ' not in text, 'retired step allowance in prompt'
                assert 'Own this objective from planning through execution and verification' in text, 'worker autonomy brief absent'
                repair = 'title: Resolve shared Git conflict' in text
                k = repair_calls if repair else original_calls
                if repair:
                    repair_calls += 1
                    assert 'Plan, implement and verify this repair yourself' in text, 'independent repair contract absent'
                else:
                    original_calls += 1
                if 'Continuing without new evidence will end this attempt' in text:
                    warnings_seen.append(n)
                if n == 0:
                    assert all('FIXTURE_OWNER_ANSWER_' + str(i) + '_END' in text for i in range(18)), 'saved owner answers absent'
                if args.expect == 'stalled':
                    assert repair, 'stalled fixture must not execute original work'
                    offset = k % 15
                    if offset == 0:
                        name, arguments = 'tools_load', {'names': ['exec']}
                    elif offset == 1:
                        name, arguments = 'exec', {'command': "printf 'unverified partial repair\\n' > partial.txt", 'timeout_secs': 20}
                    else:
                        name, arguments = 'exec', {'command': 'git diff -- policy.txt', 'timeout_secs': 20}
                elif (repair and k == 0) or (not repair and k == 0):
                    name, arguments = 'tools_load', {'names': ['exec']}
                elif repair and 1 <= k <= 7:
                    name, arguments = 'exec', {'command': 'git diff -- policy.txt', 'timeout_secs': 20}
                elif (repair and k == 8) or (not repair and k == 1):
                    if repair:
                        assert warnings_seen, 'self correction did not receive progress warning'
                    name, arguments = 'exec', {'command': repair_command if repair else original_command, 'timeout_secs': 20}
                else:
                    assert k == (9 if repair else 2), 'unexpected report position'
                    tool_text = '\n'.join(m.get('content', '') for m in body['messages'] if m['role'] == 'tool')
                    tips = re.findall(r'COMMIT:([0-9a-f]{40})', tool_text)
                    assert tips, 'real worker Git command did not produce a commit'
                    tip = tips[-1]
                    commits.append(tip)
                    name, arguments = 'result_report', {'summary': 'Verified fixture repair' if repair else 'Original operation completed',
                        'commit': tip, 'changed_paths': [] if repair else ['original-result.txt']}
                return self.reply(200, {'message': {'role': 'assistant', 'content': '',
                    'tool_calls': [{'function': {'name': name, 'arguments': arguments}}]},
                    'done': True, 'done_reason': 'stop', 'prompt_eval_count': 100, 'eval_count': 10})
            except Exception as error:
                errors.append(str(error))
                return self.reply(500, {'error': str(error)})

    server = ThreadingHTTPServer(('127.0.0.1', 0), Protocol)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    base = 'http://127.0.0.1:' + str(port)
    token = 'isolated-git-recovery-fixture-not-a-live-credential'
    env = {'PATH': os.environ.get('PATH', '/usr/bin:/bin:/usr/sbin:/sbin'), 'HOME': str(fixture), 'LANG': 'en_US.UTF-8',
        'RUSTYKRAB_DATA_DIR': str(data), 'RUSTYKRAB_PORT': str(port), 'RUSTYKRAB_MASTER_KEY': '13' * 32,
        'RUSTYKRAB_AUTH_TOKEN': token, 'RUSTYKRAB_DISABLE_KEYCHAIN': '1', 'RUSTYKRAB_CREDENTIAL_BACKEND': 'memory',
        'RUSTYKRAB_PROVIDER': 'ollama', 'OLLAMA_MODEL': 'fixture:usage-only', 'OLLAMA_BASE_URL': 'http://127.0.0.1:' + str(server.server_port),
        'RUSTYKRAB_LOCAL_WORKER': 'on', 'RUSTYKRAB_PLANNER_WORKER': 'off', 'RUSTYKRAB_CONTROL_TICK_SECS': '1',
        'RUSTYKRAB_ALLOWED_ORIGINS': base, 'RUSTYKRAB_HARNESS_ROUTER': 'off', 'RUSTYKRAB_LOG_STDOUT': '1',
        'RUST_LOG': 'info', 'RUSTYKRAB_DRAIN_SECS': '1', 'RUSTYKRAB_RATE_LIMIT_MAX': '100000'}

    def api(path, body=None):
        request = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(),
            headers={'Origin': base, 'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=8) as response:
            return json.load(response)

    child, log = None, None

    def stop():
        if child is not None and child.poll() is None:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)

    def start():
        nonlocal child, log
        log = (fixture / 'daemon.log').open('a')
        child = subprocess.Popen([str(binary)], cwd=fixture, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            if child.poll() is not None:
                raise RuntimeError('fixture daemon exited during startup')
            try:
                return api('/api/version')
            except OSError:
                time.sleep(.25)
        raise RuntimeError('fixture startup timeout')

    receipt = {'binary': str(binary), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'fixture': str(fixture), 'model_service_used': False, 'live_gateway_contacted': False,
        'expect': args.expect, 'parents': [current, old], 'expectation_met': False}
    try:
        receipt['version'] = start()
        project = str(uuid.uuid4())
        save(fixture / 'project.json', api('/api/projects', {'project_id': project, 'title': 'Isolated Git recovery',
            'repository_path': str(repo), 'intent': marker + ': preserve current policy and both histories'}))
        draft = {'title': 'Historical fixture work', 'objective': marker, 'done_when': 'fixture historical branch',
            'artifact_refs': [{'kind': 'project', 'value': project}], 'writable_resources': ['repo:' + str(repo)],
            'trigger': {'kind': 'at', 'value': '2099-01-01T00:00:00Z'}}
        historical = api('/api/work', draft)['root']
        draft.update(title='Original operation fixture', objective=marker + ': complete original operation',
            done_when='original-result.txt committed after Git recovery',
            decisions_made=[marker + ': preserve current policy and both histories'])
        original = api('/api/work', draft)['root']
        receipt['original_item'] = original
        draft.update(title='Independent consent fixture', objective='An independently held operation', done_when='Owner approval remains required')
        independent = api('/api/work', draft)['root']
        receipt['independent_item'] = independent
        stop()
        log.close()
        # Only this new fixture DB is seeded, while its daemon is stopped.
        db = sqlite3.connect(data / 'db/store.db')
        now = datetime.now(timezone.utc).isoformat()
        db.execute("UPDATE work_items SET status='done',status_reason=NULL,closed_at=?,updated_at=? WHERE id=?", (now, now, historical))
        ws = {'repo': str(repo), 'base': ancestor, 'branch': 'old-change', 'path': str(fixture / 'historical-worktree')}
        for kind, reference, digest, verifier in [('commit', old, None, 'git'), ('workspace', json.dumps(ws), ancestor, None)]:
            db.execute('INSERT INTO work_item_evidence(item,kind,ref,hash,verified_by,at) VALUES(?,?,?,?,?,?)', (historical, kind, reference, digest, verifier, now))
        db.execute("UPDATE work_items SET trigger=?,trigger_at=NULL,status='blocked',status_reason='budget_exhausted',budget=? WHERE id=?", (json.dumps({'kind': 'now'}), json.dumps({'iterations':100,'tokens':600000,'wall_seconds':3600,'repairs':2}), original))
        db.execute("INSERT INTO work_spend(item,run,worker,tokens,wall_ms,iterations,at) VALUES(?,?,?,?,?,?,?)", (original,'historical-budget-run','fixture-history',1603490,1802805,25,now))
        db.execute('INSERT INTO questions(id,item,root,kind,class,text,options,rule,asked_by,status,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)',
            ('fixture-budget-question',original,original,'decision','blocking_now','Its budget is spent (budget/tokens): 1603490 of 600000 tokens used','[]','surfaced','controller','open',now))
        db.execute("UPDATE work_items SET trigger=?,trigger_at=NULL,status='blocked',status_reason='needs_consent',held_by=? WHERE id=?", (json.dumps({'kind':'now'}),'fixture-consent-question',independent))
        db.execute('INSERT INTO questions(id,item,root,kind,class,text,options,rule,asked_by,status,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)',
            ('fixture-consent-question',independent,independent,'approval','blocking_now','Independent owner scope decision','[]','approval','controller','open',now))
        for i in range(119):
            obsolete = i < 101
            text = ('Retired fixture technical diagnostic. ' * 50) if obsolete else ('Owner fixture direction ' + str(i - 101))
            answer = None if obsolete else 'FIXTURE_OWNER_ANSWER_' + str(i - 101) + '_END'
            db.execute('INSERT INTO questions(id,item,root,kind,class,text,options,rule,asked_by,status,answer,answered_by,answered_at,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)',
                (str(uuid.uuid4()),original,original,'decision','blocking_now',text,'[]','project_context','controller',
                 'obsolete' if obsolete else 'answered',answer,None if obsolete else 'user:fixture',None if obsolete else now,now))
        db.commit()
        db.close()
        receipt['obsolete_questions'] = 101
        receipt['recorded_owner_answers'] = 18
        receipt['version_after_restart'] = start()
        deadline = time.monotonic() + 100
        while time.monotonic() < deadline:
            db = sqlite3.connect('file:' + str(data / 'db/store.db') + '?mode=ro', uri=True)
            db.row_factory = sqlite3.Row
            db.execute('PRAGMA query_only=ON')
            db.execute('BEGIN')
            items = [dict(r) for r in db.execute('SELECT id,title,status,status_reason FROM work_items')]
            evidence = [dict(r) for r in db.execute('SELECT item,kind,ref AS reference,verified_by FROM work_item_evidence')]
            events = [dict(r) for r in db.execute('SELECT item,kind,actor,reason FROM work_item_events')]
            questions = [dict(row) for row in db.execute("SELECT id,item,status,answer,answered_by FROM questions WHERE id IN ('fixture-budget-question','fixture-consent-question')")]
            usage = [dict(row) for row in db.execute('SELECT item,run,tokens,wall_ms,iterations FROM work_spend')]
            db.close()
            state = next(i for i in items if i['id'] == original)
            if state['status'] == 'done' or args.expect == 'stalled' and any(i['title'].startswith('Resolve shared Git conflict') and i['status'] == 'blocked' for i in items):
                break
            time.sleep(.3)
        receipt.update(items=items, questions=questions, usage=usage, evidence=evidence, events=events, wire_errors=errors, model_calls=len(wire), commits=commits, repair_calls=repair_calls, original_calls=original_calls, warnings_seen=warnings_seen)
        repairs = {json.loads(e['reference'])['repair'] for e in evidence if e['kind'] == 'merge_recovery' and e['verified_by'] == 'controller'}
        receipt['repair_ids'] = sorted(repairs)
        receipt['repository_unchanged'] = git('rev-parse', 'HEAD') == current and git('status', '--porcelain') == '' and not (repo / 'original-result.txt').exists()
        verified = [e for e in evidence if e['kind'] == 'commit' and e['verified_by'] == 'git' and e['item'] != historical]
        if args.expect == 'self_correct':
            assert state['status'] == 'done' and len(repairs) == 1 and len(commits) == 2 and len(wire) == 13 and not errors, 'recovery/original did not complete'
            assert len(verified) == 2, 'missing real controller Git evidence'
            repair_contexts = [json.loads(e['reference']) for e in evidence if e['item'] in repairs and e['kind'] == 'project_context']
            assert len(repair_contexts) == 1 and len(repair_contexts[0]['questions']) == 121, 'full receipt history lost'
            assert len(json.dumps(repair_contexts[0])) > 128 * 1024, 'fixture did not exceed old prompt limit'
            receipt['complete_question_history_retained'] = True
            for tip in commits:
                git('merge-base', '--is-ancestor', old, tip)
                git('merge-base', '--is-ancestor', current, tip)
            assert git('show', commits[-1] + ':original-result.txt') == 'original operation completed'
            assert any(i['id'] in repairs and i['status'] == 'done' for i in items)
        else:
            assert state['status'] == 'queued' and len(repairs) == 1 and wire and not errors, 'stalled fixture did not finish at bounded hold'
            partials = sorted(data.glob('worktrees/*/partial.txt'))
            assert partials and all(f.read_text() == 'unverified partial repair\n' for f in partials), 'failed workspace partial changes lost'
            receipt['retained_partial_paths'] = [str(f) for f in partials]
            check = sqlite3.connect('file:' + str(data / 'db/store.db') + '?mode=ro', uri=True)
            check.row_factory = sqlite3.Row
            check.execute('PRAGMA query_only=ON')
            repair_id = next(iter(repairs))
            runs = [dict(r) for r in check.execute('SELECT * FROM work_spend WHERE item=?', (repair_id,))]
            receipt['spend'] = runs
            check.close()
            assert runs and all(r['iterations'] < 100 for r in runs), 'persistent loop exceeded the no-progress recovery window'
            assert any('model/loop' in str(e) or '12 steps without new evidence' in str(e) for e in events), 'typed loop evidence absent'
            assert len(wire) % 15 == 0 and len(wire) <= 45, 'loop exceeded bounded attempt windows'
            assert len(partials) == len(wire) // 15, 'failed partial workspace missing'
            receipt['loop_recovery_without_execution_caps'] = True
            assert not commits and not original_calls

        by_id = {q['id']:q for q in questions}
        assert by_id['fixture-budget-question']['status'] == 'obsolete', 'legacy budget question not retired'
        assert by_id['fixture-budget-question']['answer'] is None and by_id['fixture-budget-question']['answered_by'] is None, 'fabricated owner answer'
        assert by_id['fixture-consent-question']['status'] == 'open', 'independent consent question changed'
        independent_state = next(i for i in items if i['id'] == independent)
        assert independent_state['status'] == 'blocked' and independent_state['status_reason'] == 'needs_consent', 'independent hold cleared'
        assert not any(e['item'] == independent and e['kind'] == 'lease' for e in events), 'held operation executed'
        original_usage = [u for u in usage if u['item'] == original]
        assert sum(u['tokens'] for u in original_usage) >= 1603490, 'observed prior usage lost'
        if args.expect == 'self_correct':
            assert sum(u['tokens'] for u in original_usage) > 1603490, 'new execution usage not accumulated'
        assert not any(e['kind'] == 'budget_grant' for e in evidence), 'new execution grant created'
        receipt['legacy_usage_retained'] = True
        receipt['independent_consent_preserved'] = True
        assert receipt['repository_unchanged'], 'fixture repository checkout was modified'
        receipt['expectation_met'] = True
    except Exception as error:
        receipt['error'] = str(error)
    finally:
        stop()
        if log:
            log.close()
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)
        receipt['fixture_daemon_stopped'] = child is not None and child.poll() is not None
        receipt['fixture_protocol_server_stopped'] = not thread.is_alive()
        receipt['finished_at'] = datetime.now(timezone.utc).isoformat()
        save(fixture / 'receipt.json', receipt)
    print(json.dumps({k: v for k, v in receipt.items() if k not in ('items', 'evidence', 'events')}, indent=2))
    return 0 if receipt['expectation_met'] and receipt['fixture_daemon_stopped'] and receipt['fixture_protocol_server_stopped'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
