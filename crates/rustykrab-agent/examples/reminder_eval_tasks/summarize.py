"""Summarize a reminder eval's runs.jsonl as markdown.

    python3 summarize.py eval-results/reminder-2026-10-14 [--ollama-log ~/.ollama/logs/server.log]

Prints the per-run table, per-arm totals, the harness signals worth reading
a transcript for (rejected reports with their reasons, refused execs,
unparseable replies, stall notices, compactions), and a contention check:
the /api/chat requests Ollama logged during each run against the model
calls the run itself made. More requests than the run made means another
client (the live daemon) was using the model at the same time.
Python 3.9, standard library only.
"""

import argparse
import collections
import datetime
import json
import os
import re


def load(out):
    runs = []
    with open(os.path.join(out, "runs.jsonl")) as f:
        for line in f:
            if line.strip():
                runs.append(json.loads(line))
    return runs


def messages(out, run):
    path = os.path.join(out, run["transcript_file"])
    if not os.path.isfile(path):
        return []
    with open(path) as f:
        return [json.loads(l) for l in f.read().splitlines()[1:] if l.strip()]


def rejected_reasons(msgs):
    names = {}
    reasons = []
    for m in msgs:
        c = m["content"]
        if c.get("type") == "tool_call":
            names[c["data"]["id"]] = c["data"]["name"]
        elif c.get("type") == "multi_tool_call":
            for call in c["data"]:
                names[call["id"]] = call["name"]
        elif c.get("type") == "tool_result":
            d = c["data"]
            if names.get(d["call_id"]) == "result_report" and d.get("is_error"):
                out = d["output"]
                text = out.get("error") if isinstance(out, dict) else str(out)
                reasons.append(re.sub(r"^tool execution error: tool 'result_report': ", "", text or "")[:160])
    return reasons


LOG_SIGNALS = [
    ("exec refused", "command rejected"),
    ("report rejected", "result_report not accepted"),
    ("report type error", "tool 'result_report': field"),
    ("reminder", "EndTurn without the completion signal"),
    ("compaction", "compacting conversation history"),
    ("summary failed -> Extractive", "compaction summary failed"),
    ("unparseable reply", "could not be parsed"),
    ("reflection prompt", "injecting reflection prompt"),
    ("empty reply", "EndTurn with empty assistant text"),
]


def log_windows(out):
    """The harness log's lines per run label: the session log keeps only
    what survived compaction, the harness log keeps every call."""
    path = os.path.join(out, "harness.log")
    windows = collections.defaultdict(list)
    current = None
    if not os.path.isfile(path):
        return windows
    with open(path, errors="replace") as f:
        for line in f:
            m = re.search(r"eval run (start|end) label=(\S+)", line)
            if m:
                current = m.group(2) if m.group(1) == "start" else None
                continue
            if current:
                windows[current].append(line)
    return windows


def log_signals(lines):
    counts = collections.Counter()
    for line in lines:
        for label, needle in LOG_SIGNALS:
            if needle in line:
                counts[label] += 1
    # A compaction that fell back logs "compacting" twice: once for the
    # failed summary, once for the Extractive pass.
    if counts["compaction"]:
        counts["compaction"] -= counts["summary failed -> Extractive"]
    return {k: v for k, v in counts.items() if v}


def chat_requests(log, start, end):
    """/api/chat lines Ollama logged in [start, end] (local time)."""
    if not log or not os.path.isfile(log):
        return None
    pat = re.compile(r"^\[GIN\] (\d{4}/\d\d/\d\d - \d\d:\d\d:\d\d) \| (\d+) \|\s*(\S+) .*\"/api/chat\"")
    s = datetime.datetime.fromisoformat(start).replace(tzinfo=None)
    e = datetime.datetime.fromisoformat(end).replace(tzinfo=None)
    n = 0
    with open(log, errors="replace") as f:
        for line in f:
            m = pat.match(line)
            if not m:
                continue
            at = datetime.datetime.strptime(m.group(1), "%Y/%m/%d - %H:%M:%S")
            if s <= at <= e + datetime.timedelta(seconds=1):
                n += 1
    return n


def main():
    p = argparse.ArgumentParser()
    p.add_argument("out")
    p.add_argument("--ollama-log", default=os.path.expanduser("~/.ollama/logs/server.log"))
    a = p.parse_args()
    runs = load(a.out)

    print("| task | arm | rep | attempt | pass (tree) | pass (commit) | committed | ending | reminders | iterations | tokens | wall s | Ollama chats / own calls |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for r in runs:
        t = r["transcript"]
        own = (r.get("iterations") or 0)
        chats = chat_requests(a.ollama_log, r["started_at"], r["ended_at"])
        print("| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s / %s |" % (
            r["task"], r["arm"], r["rep"], r["attempt"],
            "yes" if r["pass"] else "no", "yes" if r["tip_pass"] else "no",
            "yes" if r["committed"] else "no",
            r["ending"] + (" (%s)" % r["error_class"] if r.get("error_class") else ""),
            r.get("reminders"), r.get("iterations"), r.get("tokens"), r.get("wall_s"),
            chats, own,
        ))

    print("\n### Per arm (final attempt of each task and rep)\n")
    final = {}
    for r in runs:
        final[(r["task"], r["arm"], r["rep"])] = r
    by_arm = collections.defaultdict(list)
    for r in final.values():
        by_arm[r["arm"]].append(r)
    for arm, rs in sorted(by_arm.items()):
        endings = collections.Counter(r["ending"] for r in rs)
        n = len(rs)
        print("- **%s**: %d runs; pass (tree) %d, pass (commit) %d; endings %s; mean tokens %d, mean wall %.0f s, mean iterations %.1f; reminders sent %d" % (
            arm, n, sum(bool(r["pass"]) for r in rs), sum(bool(r["tip_pass"]) for r in rs),
            dict(endings), sum(r.get("tokens") or 0 for r in rs) / max(n, 1),
            sum(r.get("wall_s") or 0 for r in rs) / max(n, 1),
            sum(r.get("iterations") or 0 for r in rs) / max(n, 1),
            sum(r.get("reminders") or 0 for r in rs),
        ))

    print("\n### Harness signals per run (harness log, every call)\n")
    windows = log_windows(a.out)
    for r in runs:
        sig = log_signals(windows.get(r["label"], []))
        print("- `%s`: %s" % (r["label"], ", ".join("%s %d" % kv for kv in sig.items()) or "none"))

    print("\n### Session-log detail per run (what survived compaction)\n")
    for r in runs:
        t = r["transcript"]
        msgs = messages(a.out, r)
        notes = []
        reasons = rejected_reasons(msgs)
        if reasons:
            notes.append("rejected result_report x%d: %s" % (len(reasons), "; ".join(reasons)))
        if t.get("after_reminder"):
            notes.append("after reminders: %s (work calls %d, repeats %d)" % (
                ", ".join(t["after_reminder"]), t["post_reminder_work_calls"], t["post_reminder_repeat_calls"]))
        for key, label in [("exec_refusals", "exec refused"), ]:
            if t.get(key):
                notes.append("%s x%d: %s" % (label, len(t[key]), " | ".join(t[key])))
        for key in ["malformed_notices", "stall_notices", "compactions", "extractive_compactions", "empty_notices", "not_callable"]:
            if t.get(key):
                notes.append("%s %d" % (key, t[key]))
        if r.get("error"):
            notes.append("error: %s" % r["error"][:300])
        print("- `%s`: %s" % (r["label"], "; ".join(notes) if notes else "nothing notable"))


if __name__ == "__main__":
    main()
