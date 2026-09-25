#!/usr/bin/env python3
"""Drive one Telegram message through the real daemon and record what it sends.

  run_telegram.py <daemon-binary> <label> <trigger> <out-dir>

Same daemon environment as run_scenario.py, plus a Telegram channel whose Bot
API is fake_telegram.py (TELEGRAM_API_BASE). The fake hands the daemon one
message from the allowed chat and logs every sendMessage. After the reply,
waits a few seconds for any credential link the turn parked, then reads the
store read-only. Link tokens are redacted by the fake before logging.
"""
import json
import os
import sqlite3
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from run_scenario import HERE, MASTER_KEY, ORIGIN, TOKEN, free_port  # noqa: E402

CHAT = "-100123"


def main():
    binary, label, trigger, out = sys.argv[1:5]
    # Optional: wait until a sent message contains this text (a scheduled
    # job's result arrives a poll interval after it falls due).
    wait_text = sys.argv[5] if len(sys.argv) > 5 else None
    os.makedirs(out, exist_ok=True)
    data_dir = os.path.join(out, "data")
    os.makedirs(data_dir, exist_ok=True)
    oport, dport, tport = free_port(), free_port(), free_port()
    tg_log = os.path.join(out, "fake_telegram.jsonl")
    open(tg_log, "w").close()

    fake_ollama = subprocess.Popen(
        [sys.executable, os.path.join(HERE, "fake_ollama.py"), str(oport)],
        env={**os.environ, "FAKE_OLLAMA_LOG": os.path.join(out, "fake_ollama.jsonl")},
    )
    fake_tg = subprocess.Popen(
        [sys.executable, os.path.join(HERE, "fake_telegram.py"), str(tport)],
        env={**os.environ, "FAKE_TG_LOG": tg_log, "FAKE_TG_TRIGGER": trigger, "FAKE_TG_CHAT": CHAT},
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
        "RUSTYKRAB_ALLOWED_ORIGINS": ORIGIN,
        "RUSTYKRAB_PUBLIC_URL": "https://verify.invalid",
        "RUSTYKRAB_BROWSER_ISOLATED_ROOT": os.path.join(data_dir, "browser"),
        "RUSTYKRAB_PROVIDER": "ollama",
        "RUSTYKRAB_HARNESS_ROUTER": "off",
        "OLLAMA_MODEL": "verify-model",
        "OLLAMA_BASE_URL": f"http://127.0.0.1:{oport}",
        "OLLAMA_TIMEOUT_SECS": "60",
        "TELEGRAM_BOT_TOKEN": "verify-bot",
        "TELEGRAM_ALLOWED_CHATS": CHAT,
        "TELEGRAM_API_BASE": f"http://127.0.0.1:{tport}",
    }
    log = open(os.path.join(out, "daemon.log"), "w")
    daemon = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    result = {"label": label, "trigger": trigger, "binary": binary, "channel": "telegram (fake Bot API)"}
    try:
        deadline = time.time() + 150
        sends = []
        while time.time() < deadline:
            sends = [json.loads(l) for l in open(tg_log) if l.strip()]
            done = [s for s in sends if not s["is_credential_link"]]
            if done and (wait_text is None or any(wait_text in s["text"] for s in done)):
                break
            if daemon.poll() is not None:
                raise RuntimeError(f"daemon exited early: {daemon.returncode}")
            time.sleep(0.5)
        time.sleep(4)  # a parked link is sent right after the reply
        sends = [json.loads(l) for l in open(tg_log) if l.strip()]
        result["telegram_sends"] = sends
        result["reply_sent"] = any(not s["is_credential_link"] for s in sends)
        result["credential_link_sent"] = any(s["is_credential_link"] for s in sends)
        result["daemon_alive_after"] = daemon.poll() is None

        db = sqlite3.connect(f"file:{os.path.join(data_dir, 'db', 'store.db')}?mode=ro", uri=True)
        cols = [c[1] for c in db.execute("PRAGMA table_info(credential_requests)")]
        convs = {r[0] for r in db.execute("SELECT id FROM conversations")}
        creq = []
        for row in db.execute("SELECT * FROM credential_requests"):
            r = dict(zip(cols, row))
            creq.append({
                "id": r["id"][:8],
                "name": r["name"],
                "status": r["status"],
                "conversation_id_is_null": r.get("conversation_id") is None,
                "conversation_id_is_a_real_conversation": r.get("conversation_id") in convs,
                "link_token_hash_present": bool(r.get("link_token_hash")),
            })
        result["credential_requests"] = creq
    except Exception as e:
        result["harness_error"] = repr(e)
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        fake_ollama.terminate()
        fake_tg.terminate()
        log.close()
    text = open(os.path.join(out, "daemon.log")).read()
    result["log_marker_counts"] = {
        m: text.count(m)
        for m in ["invoked outside of an agent session context", "failed to send credential link",
                  "could not mint a credential link", "Telegram"]
    }
    with open(os.path.join(out, "result.json"), "w") as f:
        json.dump(result, f, indent=2)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
