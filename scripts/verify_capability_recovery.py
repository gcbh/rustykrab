"""Exercise the released capability recovery path in an isolated daemon.

The Ollama protocol is served by an inert local HTTP fixture, not a model.
No production data, credentials, gateway, account or model service is used.
"""
import argparse
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import threading
import time
import urllib.error
import urllib.request


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--fixture', required=True, type=Path)
    parser.add_argument('--expect', choices=['pass', 'preflight_denied'], required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    fixture = args.fixture.absolute()
    fixture.mkdir(mode=0o700, exist_ok=False)
    data = fixture / 'private-data'
    data.mkdir(mode=0o700)
    home = fixture / 'home'
    home.mkdir(mode=0o700)
    # Reproduce the live legacy skill/catalog collision without granting it.
    skill = data / 'skills/capability:tool:work_resources'
    skill.mkdir(parents=True)
    (skill / 'SKILL.md').write_text('---\nname = "capability:tool:work_resources"\n'
        'description = "Legacy instruction name, not functional tool proof"\n---\n'
        'Read the real work_resources inventory; this text cannot implement it.\n')
    wire = []
    errors = []
    lock = threading.Lock()
    trigger = 'ISOLATED_CAPABILITY_RECOVERY_735'
    inventory_result_seen = []

    class OllamaFixture(BaseHTTPRequestHandler):
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
                return self.reply(200, {'model_info': {'fixture.context_length': 131072},
                                        'capabilities': ['completion', 'tools']})
            if self.path != '/api/chat':
                return self.reply(404, {'error': 'unsupported fixture endpoint'})
            with lock:
                n = len(wire)
                wire.append(body)
                save(fixture / 'wire.json', wire)
            try:
                names = [t['function']['name'] for t in body.get('tools', [])]
                assert 'result_report' in names, 'not the bounded controller worker'
                if n == 0:
                    assert trigger in json.dumps(body['messages'])
                    name, arguments = 'result_report', {
                        'summary': 'Need the inventory before allocating fixture resources',
                        'blocked': {'reason': 'needs_tool', 'detail': 'inventory unavailable',
                                    'needs': ['capability:tool:work_resources']}}
                elif n == 1:
                    name, arguments = 'result_report', {'summary': 'Inventory is available'}
                elif n == 2:
                    assert 'work_resources' in names, 'inventory missing from actual run schema'
                    name, arguments = 'work_resources', {}
                elif n == 3:
                    results = [m for m in body['messages'] if m['role'] == 'tool']
                    found = []
                    for result in results:
                        try:
                            value = json.loads(result.get('content', ''))
                            if isinstance(value, dict) and isinstance(value.get('workers'), list) and isinstance(value.get('projects'), list):
                                found.append(value)
                        except (TypeError, ValueError):
                            pass
                    assert found, 'resumed original did not receive real inventory output'
                    inventory_result_seen.extend(found)
                    name, arguments = 'result_report', {'summary': 'Original fixture allocation completed'}
                else:
                    raise AssertionError('unexpected extra model call')
                self.reply(200, {'message': {'role': 'assistant', 'content': '',
                    'tool_calls': [{'function': {'name': name, 'arguments': arguments}}]},
                    'done': True, 'done_reason': 'stop', 'prompt_eval_count': 100, 'eval_count': 10})
            except Exception as error:
                errors.append(str(error))
                self.reply(500, {'error': str(error)})

    server = ThreadingHTTPServer(('127.0.0.1', 0), OllamaFixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    base = 'http://127.0.0.1:' + str(port)
    token = 'isolated-capability-fixture-not-a-live-credential'
    env = {
        'PATH': os.environ.get('PATH', '/usr/bin:/bin:/usr/sbin:/sbin'),
        'HOME': str(home), 'LANG': 'en_US.UTF-8',
        'RUSTYKRAB_DATA_DIR': str(data), 'RUSTYKRAB_PORT': str(port),
        'RUSTYKRAB_MASTER_KEY': '17' * 32, 'RUSTYKRAB_AUTH_TOKEN': token,
        'RUSTYKRAB_DISABLE_KEYCHAIN': '1', 'RUSTYKRAB_CREDENTIAL_BACKEND': 'memory',
        'RUSTYKRAB_PROVIDER': 'ollama', 'OLLAMA_MODEL': 'fixture:735',
        'OLLAMA_BASE_URL': 'http://127.0.0.1:' + str(server.server_port),
        'RUSTYKRAB_LOCAL_WORKER': 'off', 'RUSTYKRAB_PLANNER_WORKER': 'on',
        'RUSTYKRAB_WORK_MANAGER': '1', 'RUSTYKRAB_CONTROL_TICK_SECS': '1',
        'RUSTYKRAB_DISTILL': 'off', 'RUSTYKRAB_ALLOWED_ORIGINS': base,
        'RUSTYKRAB_HARNESS_ROUTER': 'off', 'RUSTYKRAB_LOG_STDOUT': '1',
        'RUST_LOG': 'info', 'RUSTYKRAB_DRAIN_SECS': '1', 'RUSTYKRAB_RATE_LIMIT_MAX': '100000',
    }

    def api(path, body=None, timeout=5):
        request = urllib.request.Request(base + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={'Origin': base, 'Authorization': 'Bearer ' + token,
                     'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.load(response)

    def snapshot():
        db = sqlite3.connect('file:' + str(data / 'db/store.db') + '?mode=ro', uri=True)
        db.row_factory = sqlite3.Row
        db.execute('PRAGMA query_only=ON')
        db.execute('BEGIN')
        rows = [dict(r) for r in db.execute('SELECT id,title,kind,status,status_reason,writable_resources FROM work_items')]
        evidence = [dict(r) for r in db.execute('SELECT item,kind,ref AS reference,verified_by FROM work_item_evidence')]
        events = [dict(r) for r in db.execute('SELECT item,kind,actor,reason FROM work_item_events')]
        db.close()
        return {'items': rows, 'evidence': evidence, 'events': events}

    receipt = {'binary': str(binary), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
               'expect': args.expect, 'fixture': str(fixture), 'live_gateway_contacted': False,
               'real_account_accessed': False, 'model_service_used': False,
               'inert_protocol_server': 'Ollama wire fixture on an isolated loopback port'}
    child = None
    try:
        with (fixture / 'daemon.log').open('w') as log:
            child = subprocess.Popen([str(binary)], cwd=fixture, env=env,
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            deadline = time.monotonic() + 45
            while True:
                if child.poll() is not None:
                    raise RuntimeError('Fixture exited during startup')
                try:
                    receipt['version'] = api('/api/version')
                    break
                except (OSError, urllib.error.URLError):
                    if time.monotonic() > deadline:
                        raise RuntimeError('Fixture startup timed out')
                    time.sleep(0.25)
            accepted = api('/api/work', {'title': trigger, 'objective': 'Inspect the fixture fleet inventory',
                'done_when': 'Original fixture allocation completed'})
            save(fixture / 'filing.json', accepted)
            root = accepted['root']
            receipt['original_item'] = root
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                snap = snapshot()
                originals = [i for i in snap['items'] if i['id'] == root]
                attempts = [json.loads(e['reference']) for e in snap['evidence'] if e['item'] == root and e['kind'] == 'attempt_state']
                legacy_required = any('capability:tool:work_resources' in a.get('required_tools', []) for a in attempts)
                terminal_hold = bool(originals and originals[0]['status'] == 'blocked' and originals[0]['status_reason'] != 'needs_tool')
                if originals and (originals[0]['status'] == 'done' or terminal_hold or args.expect == 'preflight_denied' and legacy_required):
                    break
                time.sleep(0.3)
            receipt['snapshot'] = snap
            receipt['model_calls'] = len(wire)
            receipt['wire_errors'] = errors
            receipt['original_inventory_result_seen'] = bool(inventory_result_seen)
            receipt['legacy_preflight_required_tool_seen'] = legacy_required
            caps = [i for i in snap['items'] if i['kind'] == 'capability']
            proofs = [json.loads(e['reference']) for e in snap['evidence'] if e['kind'] == 'operation_probe' and e['verified_by'] == 'executor:planner']
            leases = [e for e in snap['events'] if e['item'] == root and e['kind'] == 'lease']
            receipt['original_state'] = originals[0] if originals else None
            receipt['capability_states'] = caps
            receipt['proofs'] = proofs
            receipt['original_leases'] = len(leases)
            receipt['expectation_met'] = (
                originals[0]['status'] == 'done' and len(caps) == 1 and caps[0]['title'] == 'Acquire tool: work_resources'
                and caps[0]['status'] == 'done' and len(proofs) == 1 and proofs[0]['operation'] == 'tool.callable/work_resources'
                and proofs[0]['outcome'] == 'ok' and inventory_result_seen and len(wire) == 4 and len(leases) == 2
                and not legacy_required and not errors
                if args.expect == 'pass' else legacy_required and not inventory_result_seen)
            receipt['expectation_met'] = bool(receipt['expectation_met'])
            assert all(json.loads(i['writable_resources']) == [] for i in snap['items']), 'fixture acquired writable scope'
    except Exception as error:
        receipt['error'] = str(error)
        receipt['expectation_met'] = False
    finally:
        if child is not None and child.poll() is None:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)
        receipt['fixture_daemon_stopped'] = child is not None and child.poll() is not None
        receipt['fixture_protocol_server_stopped'] = not thread.is_alive()
        receipt['finished_at'] = datetime.now(timezone.utc).isoformat()
        save(fixture / 'receipt.json', receipt)
    print(json.dumps({k:v for k,v in receipt.items() if k not in ('snapshot','proofs')}, indent=2))
    return 0 if receipt['expectation_met'] and receipt['fixture_daemon_stopped'] and receipt['fixture_protocol_server_stopped'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
