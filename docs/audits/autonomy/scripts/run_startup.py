#!/usr/bin/env python3
"""Per-run start-up and turn timing from conversation journals (read-only).

    python3 -I run_startup.py <OTTER_HOME>/state

claim -> delivered: worker spawn + SDK init + turn hand-over.
claim -> first tool: until Claude's first tool call.
turn: delivered -> finished.  run total: claim -> run_ended.
"""
import datetime as dt, glob, json, os, sys

def t(s): return dt.datetime.fromisoformat(s.replace("Z", "+00:00")[:26] + "+00:00")

rows = []
for j in sorted(glob.glob(os.path.join(sys.argv[1], "conversations", "*", "journal.jsonl"))):
    ev = {}
    backend = None
    for l in open(j):
        e = json.loads(l)
        if e.get("kind") == "create":
            backend = e["conversation"].get("backend")
            continue
        o = e.get("op", {})
        k = o.get("op")
        at = o.get("at") or e.get("at")
        if k == "tool_started":
            at = o["record"]["started_at"]
        if k in ("claim", "process", "delivered", "tool_started", "finished", "run_ended") and k not in ev:
            ev[k] = t(at)
    if "claim" not in ev:
        continue
    d = lambda a, b: round((ev[b] - ev[a]).total_seconds(), 2) if a in ev and b in ev else None
    rows.append((ev["claim"], backend, d("claim", "delivered"), d("claim", "tool_started"), d("delivered", "finished"), d("claim", "run_ended")))
print("backend   claim->delivered  claim->first_tool  turn_s  run_total_s")
for r in sorted(rows):
    print(f"{r[1]:<10}{r[2]!s:>16}{r[3]!s:>19}{r[4]!s:>8}{r[5]!s:>13}")
