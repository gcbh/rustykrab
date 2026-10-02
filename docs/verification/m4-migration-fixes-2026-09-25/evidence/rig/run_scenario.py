#!/usr/bin/env python3
"""Drive one scenario through the real daemon over real sockets.

  run_scenario.py <daemon-binary> <label> <trigger> <out-dir>

Boots the daemon as its own OS process on an ephemeral port with a throwaway
data dir, pointed at fake_ollama.py (also its own process) via
RUSTYKRAB_PROVIDER=ollama. Environment mirrors scripts/e2e.sh's model mode
(env cleared, keychain disabled, memory credential backend, test master key)
plus RUSTYKRAB_PUBLIC_URL so credential links are minted. Sends the trigger
through POST /api/conversations/{id}/messages, then reads the SQLite store
directly (read-only) and the daemon log. Writes result.json in the out dir.
No secret values are recorded: link tokens appear only as present/absent.
"""
import json
import os
import socket
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
TOKEN = "verify-master-token"
ORIGIN = "https://verify.example.ts.net"
MASTER_KEY = "5e" * 32


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def http(method, url, body=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Authorization", f"Bearer {TOKEN}")
    req.add_header("Origin", ORIGIN)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def main():
    binary, label, trigger, out = sys.argv[1:5]
    os.makedirs(out, exist_ok=True)
    data_dir = os.path.join(out, "data")
    os.makedirs(data_dir, exist_ok=True)
    oport, dport = free_port(), free_port()

    fake = subprocess.Popen(
        [sys.executable, os.path.join(HERE, "fake_ollama.py"), str(oport)],
        env={**os.environ, "FAKE_OLLAMA_LOG": os.path.join(out, "fake_ollama.jsonl")},
    )
    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin",
        "HOME": os.environ["HOME"],
        "RUSTYKRAB_DATA_DIR": data_dir,
        "RUSTYKRAB_PORT": str(dport),
        "RUSTYKRAB_MASTER_KEY": MASTER_KEY,
        "RUSTYKRAB_AUTH_TOKEN": TOKEN,
        "RUSTYKRAB_DISABLE_KEYCHAIN": "1",
        "RUSTYKRAB_CREDENTIAL_BACKEND": "memory",
        "NOTION_API_TOKEN": "verify-dummy-notion",
        "OBSIDIAN_API_KEY": "verify-dummy-obsidian",
        "RUSTYKRAB_LOG_STDOUT": "1",
        "RUST_LOG": "info",
        "RUSTYKRAB_RATE_LIMIT_MAX": "100000",
        "RUSTYKRAB_RATE_LIMIT_LOCKOUT_SECS": "1",
        "RUSTYKRAB_ALLOWED_ORIGINS": ORIGIN,
        "RUSTYKRAB_PUBLIC_URL": "https://verify.invalid",
        "RUSTYKRAB_BROWSER_ISOLATED_ROOT": os.path.join(data_dir, "browser"),
        "RUSTYKRAB_PROVIDER": "ollama",
        "RUSTYKRAB_HARNESS_ROUTER": "off",
        "OLLAMA_MODEL": "verify-model",
        "OLLAMA_BASE_URL": f"http://127.0.0.1:{oport}",
        "OLLAMA_TIMEOUT_SECS": "60",
    }
    log = open(os.path.join(out, "daemon.log"), "w")
    daemon = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    result = {"label": label, "trigger": trigger, "binary": binary, "daemon_pid": daemon.pid}
    try:
        base = f"http://127.0.0.1:{dport}"
        for _ in range(240):
            try:
                if http("GET", f"{base}/api/health", timeout=2)[0] == 200:
                    break
            except Exception:
                pass
            if daemon.poll() is not None:
                raise RuntimeError(f"daemon exited early: {daemon.returncode}")
            time.sleep(0.5)
        else:
            raise RuntimeError("daemon never became healthy")

        status, body = http("POST", f"{base}/api/conversations", {})
        conv_id = json.loads(body)["id"]
        result["conversation_id"] = conv_id
        t0 = time.time()
        status, body = http("POST", f"{base}/api/conversations/{conv_id}/messages", {"content": trigger}, timeout=180)
        result["send_status"] = status
        result["send_seconds"] = round(time.time() - t0, 2)
        try:
            parsed = json.loads(body)
            result["reply_text"] = parsed.get("content") if isinstance(parsed, dict) else None
            result["reply_raw_head"] = body[:400]
        except Exception:
            result["reply_raw_head"] = body[:400]
        time.sleep(1.0)
        result["daemon_alive_after"] = daemon.poll() is None
        result["health_after"] = http("GET", f"{base}/api/health")[0]

        db = sqlite3.connect(f"file:{os.path.join(data_dir, 'db', 'store.db')}?mode=ro", uri=True)
        rows = db.execute(
            "SELECT idx, data FROM messages WHERE conversation_id = ? ORDER BY idx", (conv_id,)
        ).fetchall()
        msgs = []
        for idx, data in rows:
            m = json.loads(data)
            content = m.get("content")
            summary = {"idx": idx, "role": m.get("role")}
            if isinstance(content, dict):
                kind = content.get("type") or next(iter(content), None)
                summary["kind"] = kind
                blob = json.dumps(content)
                summary["has_outside_context_error"] = "outside of an agent session context" in blob
                summary["head"] = blob[:160]
            else:
                summary["head"] = str(content)[:160]
            msgs.append(summary)
        result["stored_messages"] = msgs
        cols = [c[1] for c in db.execute("PRAGMA table_info(credential_requests)")]
        creq = []
        for row in db.execute("SELECT * FROM credential_requests"):
            r = dict(zip(cols, row))
            creq.append(
                {
                    "id": r["id"][:8],
                    "name": r["name"],
                    "status": r["status"],
                    "conversation_id_matches": r.get("conversation_id") == conv_id,
                    "conversation_id_is_null": r.get("conversation_id") is None,
                    "link_token_hash_present": bool(r.get("link_token_hash")) if "link_token_hash" in r else "column absent",
                }
            )
        result["credential_requests"] = creq
    except Exception as e:
        result["harness_error"] = repr(e)
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        fake.terminate()
        log.close()

    text = open(os.path.join(out, "daemon.log")).read()
    markers = [
        "invoked outside of an agent session context",
        "EndTurn without task_complete after tool use",
        "empty response to the task_complete reminder",
        "Ollama returned an empty response",
        "could not mint a credential link",
        "rustykrab 5.",
    ]
    result["log_marker_counts"] = {m: text.count(m) for m in markers}
    with open(os.path.join(out, "result.json"), "w") as f:
        json.dump(result, f, indent=2)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
