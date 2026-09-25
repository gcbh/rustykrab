#!/usr/bin/env python3
"""Summarize the matrix into matrix-summary.json and a markdown table.

Reads matrix/*/result.json and each run's fake_telegram.jsonl, and derives the
per-scenario observations the packets cite. No secret material is involved:
the daemon ran with dummy credentials and the fake Telegram redacts link tokens.
"""
import collections
import glob
import json
import os
import re

V = os.path.dirname(os.path.abspath(__file__))


def observe(name, r):
    o = {"run": name}
    if "-tg-" in name:
        sends = r.get("telegram_sends", [])
        o["telegram_messages"] = ["<credential link /c/<redacted>>" if s["is_credential_link"] else s["text"][:80] for s in sends]
        o["credential_link_delivered"] = r.get("credential_link_sent")
        o["reply_delivered"] = r.get("reply_sent")
        creq = r.get("credential_requests", [])
        o["credential_request_conversation_id_null"] = [c["conversation_id_is_null"] for c in creq]
        o["credential_request_link_minted"] = [c["link_token_hash_present"] for c in creq]
    else:
        tool = [m for m in r.get("stored_messages", []) if m["role"] == "tool"]
        o["http_status"] = r.get("send_status")
        o["reply"] = r.get("reply_text")
        o["tool_results"] = len(tool)
        o["tool_results_outside_context_error"] = sum(m["has_outside_context_error"] for m in tool)
        o["credential_request_conversation_id_matches"] = [c["conversation_id_matches"] for c in r.get("credential_requests", [])]
        o["health_after"] = r.get("health_after")
    o["daemon_alive_after"] = r.get("daemon_alive_after")
    o["log_markers"] = {k: v for k, v in r.get("log_marker_counts", {}).items() if v and k not in ("Telegram", "rustykrab 5.")}
    if r.get("harness_error"):
        o["harness_error"] = r["harness_error"]
    return o


rows = []
for d in sorted(glob.glob(f"{V}/matrix/*")):
    rows.append(observe(os.path.basename(d), json.load(open(f"{d}/result.json"))))
json.dump(rows, open(f"{V}/matrix-summary.json", "w"), indent=2)

# Secret scan of everything that will be committed as evidence.
pat = re.compile(r"(/c/[A-Za-z0-9_\-]{16,}|[0-9]{8,10}:[A-Za-z0-9_\-]{30,}|sk-ant-|ntn_[A-Za-z0-9]{20,}|BEGIN [A-Z ]*PRIVATE KEY)")
hits = []
for path in glob.glob(f"{V}/matrix/**/*", recursive=True) + [f"{V}/matrix-summary.json"]:
    if os.path.isfile(path) and not path.endswith((".db", ".db-wal", ".db-shm")):
        for i, line in enumerate(open(path, errors="replace")):
            if pat.search(line):
                hits.append(f"{os.path.relpath(path, V)}:{i+1}")
print("secret-scan hits:", len(hits), hits[:10])

by = collections.defaultdict(list)
for r in rows:
    key = re.sub(r"-r\d+$", "", r["run"])
    by[key].append(r)
for key in sorted(by):
    rs = by[key]
    first = {k: v for k, v in rs[0].items() if k != "run"}
    same = all({k: v for k, v in x.items() if k != "run"} == first for x in rs)
    print(f"{key:26} n={len(rs)} identical_across_reps={same}  {json.dumps(first)[:260]}")
