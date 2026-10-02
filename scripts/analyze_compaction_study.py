#!/usr/bin/env python3
"""Read-only evidence audit; emits JSON, never changes raw grades or trials.

Usage: python3 scripts/analyze_compaction_study.py PATH [PATH ...]
This is deliberately independent of the Rust grader. Literal fact presence is
diagnostic, not semantic adjudication (especially for natural-language dates).
"""

import hashlib
import json
import statistics
import sys
from pathlib import Path


def literal_match(text, criterion):
    return any(part in text.lower() for part in criterion.lower().split("|"))


def analyze(root):
    report = json.loads((root / "report.json").read_text())
    manifest_bytes = (root / "manifest.json").read_bytes()
    trials = []
    for row in report["trials"]:
        path = root / f'{row["case"]}-{row["strategy"]}-{row["repetition"]}.json'
        if not path.exists():
            trials.append({**row, "evidence_missing": True})
            continue
        raw = path.read_bytes()
        evidence = json.loads(raw)
        wire = evidence["wire_exchanges"]
        integrity = []
        for exchange in wire:
            body = exchange["wire_body_utf8"].encode()
            integrity.append(
                len(body) == exchange["wire_bytes"]
                and hashlib.sha256(body).hexdigest() == exchange["wire_sha256"]
                and json.loads(body) == exchange["wire_request"]
            )
        probe_index = evidence.get("probe_sequence")
        probe = wire[probe_index] if isinstance(probe_index, int) and probe_index < len(wire) else {}
        terminal = probe.get("response", {}).get("terminal", {})
        checkpoint = evidence.get("checkpoint")
        context = json.dumps(evidence["final_context"]["messages"], ensure_ascii=False)
        facts = []
        for check in row.get("score", {}).get("fact_checks", []):
            criterion = check["criterion"]
            facts.append({
                **check,
                "literal_in_final_context": literal_match(context, criterion),
                "literal_in_each_compacted_round": [
                    literal_match(json.dumps(r["messages"], ensure_ascii=False), criterion)
                    for r in evidence["compacted_rounds"]
                ],
                "note": "literal match only; not a semantic loss verdict",
            })
        schema_valid = checkpoint is not None and row["score"].get("schema_valid") is True
        # Explicit, narrow rubric repair, reported beside the unchanged grade.
        # "explanation" does not contain the old literal stem "explain".
        equivalent_direction = bool(
            schema_valid and row["case"] == "explicit-task-switch"
            and row["score"].get("direction_preserved") is False
            and "explanation" in json.dumps({k: checkpoint.get(k) for k in
                                             ("current_direction", "next_action")}).lower()
        )
        repaired_score = dict(row["score"])
        if equivalent_direction:
            repaired_score["direction_preserved"] = True
        repaired_pass = all(repaired_score.get(k) is True for k in (
            "schema_valid", "intent_preserved", "direction_preserved", "relevant_facts_preserved",
            "current_constraints_preserved", "safe_next_action", "no_false_completion"))
        trials.append({
            **row,
            "evidence_path": str(path),
            "evidence_sha256": hashlib.sha256(raw).hexdigest(),
            "wire_integrity_passed": bool(wire) and all(integrity),
            "wire_count": len(wire),
            "checkpoint_present": checkpoint is not None,
            "schema_valid_readout": schema_valid,
            "semantic_dimensions_scorable": schema_valid,
            "direction_morphology_false_negative": equivalent_direction,
            "morphology_only_all_passed": repaired_pass,
            "probe_done_reason": terminal.get("done_reason"),
            "probe_generated_tokens": terminal.get("eval_count"),
            "summary_done_reasons": [
                w["response"].get("terminal", {}).get("done_reason")
                for i, w in enumerate(wire) if i != probe_index
            ],
            "fact_diagnostics": facts,
        })
    summaries = []
    for method in sorted({r["strategy"] for r in trials}):
        rows = [r for r in trials if r["strategy"] == method]
        scorable = [r for r in rows if r.get("semantic_dimensions_scorable")]
        counts = {}
        for dimension in ("intent_preserved", "direction_preserved", "relevant_facts_preserved",
                          "current_constraints_preserved", "safe_next_action", "no_false_completion"):
            counts[dimension] = {"passed": sum(r["score"].get(dimension) is True for r in scorable),
                                 "scorable": len(scorable)}
        tokens = [r["probe_prompt_tokens"] for r in rows if isinstance(r.get("probe_prompt_tokens"), int)]
        summaries.append({
            "strategy": method, "trials": len(rows),
            "raw_all_passed": sum(r["score"].get("all_passed") is True for r in rows),
            "morphology_only_all_passed": sum(r.get("morphology_only_all_passed") is True for r in rows),
            "schema_valid_readouts": len(scorable), "raw_dimensions_on_valid_readouts": counts,
            "median_actual_probe_prompt_tokens": statistics.median(tokens) if tokens else None,
            "wire_integrity_failures": sum(r.get("wire_integrity_passed") is not True for r in rows),
        })
    return {"root": str(root), "complete": report["complete"],
            "manifest_sha256": hashlib.sha256(manifest_bytes).hexdigest(),
            "source_revision": report.get("source_revision"), "summaries": summaries, "trials": trials}


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    print(json.dumps({"runs": [analyze(Path(p)) for p in sys.argv[1:]],
                      "analyzer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                      "limits": ["Raw keyword grades are unchanged; inspect false negatives separately.",
                                 "Invalid readouts are probe failures, not automatic semantic amnesia.",
                                 "Literal fact matching cannot resolve paraphrases, negation or natural dates.",
                                 "Not domain-action E2E or a production reliability estimate."]}, indent=2))
