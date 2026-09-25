#!/usr/bin/env python3
"""A stand-in for the Telegram Bot API, reached via TELEGRAM_API_BASE.

The first getUpdates returns one message from the allowed chat; later polls
return nothing. Every sendMessage is appended to $FAKE_TG_LOG with the chat
id and the text, where any credential link token (`/c/<token>`) is replaced
by `/c/<redacted>` so no live token is written to disk.
"""
import json
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

CHAT = int(os.environ.get("FAKE_TG_CHAT", "-100123"))
TRIGGER = os.environ["FAKE_TG_TRIGGER"]
LOG = os.environ.get("FAKE_TG_LOG", "/dev/null")
state = {"served": False, "sent": 0}


def redact(text):
    return re.sub(r"/c/[A-Za-z0-9_\-]+", "/c/<redacted>", text)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _ok(self, result):
        body = json.dumps({"ok": True, "result": result}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _body(self):
        n = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(n) if n else b""
        ctype = self.headers.get("Content-Type", "")
        if "json" in ctype and raw:
            return json.loads(raw)
        return {k: v[0] for k, v in parse_qs(raw.decode()).items()}

    def handle_method(self, method, params):
        if method == "getUpdates":
            if not state["served"]:
                state["served"] = True
                return self._ok([
                    {
                        "update_id": 1001,
                        "message": {
                            "message_id": 1,
                            "date": int(time.time()),
                            "chat": {"id": CHAT, "type": "supergroup", "title": "verify"},
                            "from": {"id": 42, "is_bot": False, "first_name": "Verify"},
                            "text": TRIGGER,
                        },
                    }
                ])
            time.sleep(1)
            return self._ok([])
        if method == "sendMessage":
            state["sent"] += 1
            text = str(params.get("text", ""))
            with open(LOG, "a") as f:
                f.write(json.dumps({
                    "n": state["sent"],
                    "chat_id": params.get("chat_id"),
                    "is_credential_link": "/c/" in text,
                    "text": redact(text)[:300],
                }) + "\n")
            return self._ok({"message_id": 100 + state["sent"], "date": int(time.time()),
                             "chat": {"id": CHAT, "type": "supergroup"}})
        if method == "getMe":
            return self._ok({"id": 1, "is_bot": True, "first_name": "verify", "username": "verify_bot"})
        return self._ok(True)

    def do_GET(self):
        u = urlparse(self.path)
        self.handle_method(u.path.rsplit("/", 1)[-1], {k: v[0] for k, v in parse_qs(u.query).items()})

    def do_POST(self):
        u = urlparse(self.path)
        params = {k: v[0] for k, v in parse_qs(u.query).items()}
        params.update(self._body())
        self.handle_method(u.path.rsplit("/", 1)[-1], params)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
