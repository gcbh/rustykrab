#!/usr/bin/env python3
"""A scripted stand-in for Ollama's HTTP API, for driving the real daemon.

The daemon's Ollama provider talks to this over a real socket exactly as it
would to Ollama: /api/show at startup, NDJSON-streamed /api/chat per turn.
Replies are chosen from the conversation the provider sends, so every
turn is reproducible:

  "verify: batch tools_list"    -> one response carrying TWO tool calls
                                   (tools_list, tools_list), then task_complete
  "verify: batch credential"    -> credential_request + tools_list in one
                                   response, then task_complete
  "verify: answer then silence" -> tools_list, then a text answer; once the
                                   runner's task_complete reminder appears,
                                   a zero-token reply (what qwen3.8 sent on
                                   the M4 on 2026-09-25)

Anything else (distiller, titles) gets a one-word text reply. Every request
is appended to $FAKE_OLLAMA_LOG as one JSON line.
"""
import json
import os
import sys
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "verify-model"
REMINDER = "did not call `task_complete`"
LOG = os.environ.get("FAKE_OLLAMA_LOG", "/dev/null")


def calls(*pairs):
    return [{"function": {"name": n, "arguments": a}} for n, a in pairs]


def text_of(m):
    c = m.get("content")
    return c if isinstance(c, str) else json.dumps(c)


def decide(messages):
    """Return (kind, payload): kind is tools | text | empty."""
    joined = "\n".join(text_of(m) for m in messages)
    last = messages[-1] if messages else {}
    role = last.get("role")
    if "verify: schedule credential job" in joined:
        # Schedule a one-shot job 20 s out whose task is the batched
        # credential request, delivered to the Telegram chat. This is the
        # path the M4's Gmail request took (cron job -> batch -> link).
        if role == "tool":
            return "tools", calls(("task_complete", {"summary": "scheduled"}))
        due = (datetime.now(timezone.utc) + timedelta(seconds=20)).strftime("%Y-%m-%dT%H:%M:%SZ")
        return "tools", calls(
            (
                "cron",
                {
                    "action": "create",
                    "schedule": due,
                    "task": "verify: batch credential",
                    "channel": "telegram",
                    "chat_id": os.environ.get("FAKE_TG_CHAT", "-100123"),
                },
            )
        )
    if "verify: silent first" in joined:
        return "empty", None
    if "verify: batch todo" in joined:
        if role == "tool":
            return "tools", calls(("task_complete", {"summary": "batch todo done"}))
        return "tools", calls(
            ("todo_write", {"todos": [{"content": "check the batch", "status": "pending"}]}),
            ("tools_load", {"names": ["web_fetch"]}),
        )
    if "verify: answer then silence" in joined:
        if REMINDER in joined:
            return "empty", None
        if role == "tool":
            return "text", "The answer is forty-two."
        return "tools", calls(("tools_list", {}))
    if "verify: batch tools_list" in joined:
        if role == "tool":
            return "tools", calls(("task_complete", {"summary": "batch tools_list done"}))
        return "tools", calls(("tools_list", {}), ("tools_list", {"category": "memory"}))
    if "verify: batch credential" in joined:
        if role == "tool":
            return "tools", calls(("task_complete", {"summary": "batch credential done"}))
        return "tools", calls(
            (
                "credential_request",
                {
                    "name": "verify_widget_token",
                    "service": "Verify Widget",
                    "reason": "verification run",
                    "fields": [{"key": "verify_widget_token", "label": "Token", "secret": True}],
                },
            ),
            ("tools_list", {}),
        )
    return "text", "ok"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # quiet
        pass

    def _json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/api/version"):
            return self._json({"version": "0.0.0-verify"})
        if self.path.startswith("/api/tags"):
            return self._json({"models": [{"name": MODEL, "model": MODEL}]})
        if self.path.startswith("/api/ps"):
            return self._json({"models": []})
        return self._json({"error": "not found"}, 404)

    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        req = json.loads(self.rfile.read(n) or b"{}")
        if self.path.startswith("/api/show"):
            return self._json(
                {
                    "model_info": {"general.architecture": "verify", "verify.context_length": 65536},
                    "capabilities": ["completion", "tools"],
                }
            )
        if not self.path.startswith("/api/chat"):
            return self._json({"error": "not found"}, 404)

        messages = req.get("messages", [])
        kind, payload = decide(messages)
        with open(LOG, "a") as f:
            f.write(
                json.dumps(
                    {
                        "stream": req.get("stream"),
                        "n_messages": len(messages),
                        "last_role": messages[-1].get("role") if messages else None,
                        "last_content_head": text_of(messages[-1])[:80] if messages else None,
                        "n_tools": len(req.get("tools") or []),
                        "reply": kind,
                        "reply_calls": [c["function"]["name"] for c in payload] if kind == "tools" else None,
                    }
                )
                + "\n"
            )

        msg = {"role": "assistant", "content": ""}
        eval_count, prompt_count = 0, 31390
        if kind == "tools":
            msg["tool_calls"] = payload
            eval_count = 20
        elif kind == "text":
            msg["content"] = payload
            eval_count = max(1, len(payload.split()))
        final = {
            "model": MODEL,
            "done": True,
            "done_reason": "stop",
            "prompt_eval_count": prompt_count,
            "eval_count": eval_count,
        }

        if not req.get("stream"):
            return self._json({**final, "message": msg})

        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.end_headers()
        chunks = []
        if kind != "empty":
            chunks.append({"model": MODEL, "message": msg, "done": False})
        chunks.append({**final, "message": {"role": "assistant", "content": ""}})
        for c in chunks:
            self.wfile.write((json.dumps(c) + "\n").encode())
            self.wfile.flush()


if __name__ == "__main__":
    port = int(sys.argv[1])
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
