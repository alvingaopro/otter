#!/usr/bin/env python3
"""Summarise one Otter feature's history and its runs' conversation journals.

Read-only. Usage:
    python3 -I feature_metrics.py ~/.otter/state <feature_id>

Prints per-run metrics (duration, tool calls, interactions, cost, repeated
commands, outcome) and feature-level counts (decisions by kind/risk/resolver,
time blocked on the developer, controller messages, evidence).
"""
import collections
import datetime as dt
import json
import os
import sys


def ts(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00")[:26] + "+00:00") if s else None


def main(state, fid):
    fdir = os.path.join(state, "features", fid)
    feature = json.load(open(os.path.join(fdir, "feature.json")))
    hist = [json.loads(l) for l in open(os.path.join(fdir, "history.jsonl"))]

    print(f"# feature {fid} title={feature['title']!r} request={feature['request']!r} status={feature['status']}")
    print(f"created {hist[0]['ts']}  last event {hist[-1]['ts']}  history events {len(hist)}")
    types = collections.Counter(h["type"] for h in hist)
    print("history event types:", dict(types))

    # Decisions: requested (kind, risk) and resolved (status, by).
    req = {h["decision_id"]: h for h in hist if h["type"] == "DecisionRequested"}
    res = {h["decision_id"]: h for h in hist if h["type"] == "DecisionResolved"}
    by = collections.Counter()
    for did, r in req.items():
        s = res.get(did)
        by[(r["kind"], r["risk"], s["status"] if s else "open", s["by"] if s else "-")] += 1
    print("\n## decisions (kind, risk, status, resolver) -> count")
    for k, v in sorted(by.items(), key=lambda kv: -kv[1]):
        print(f"  {k}: {v}")
    # Policy auto-approval latency and user wait time.
    pol, user = [], []
    for did, r in req.items():
        s = res.get(did)
        if not s:
            continue
        d = (ts(s["ts"]) - ts(r["ts"])).total_seconds()
        (pol if s["by"] == "policy" else user if s["by"] == "user" else []).append(d)
    if pol:
        print(f"policy-resolved: n={len(pol)} max latency {max(pol):.3f}s")
    if user:
        print(f"user-resolved: n={len(user)} total wait {sum(user)/60:.1f} min, median {sorted(user)[len(user)//2]:.0f}s, max {max(user)/60:.1f} min")

    # Feature status time accounting.
    print("\n## time per feature status")
    acc = collections.Counter()
    sc = [h for h in hist if h["type"] == "StatusChanged"]
    for a, b in zip(sc, sc[1:]):
        acc[a["to"]] += (ts(b["ts"]) - ts(a["ts"])).total_seconds()
    for k, v in acc.most_common():
        print(f"  {k}: {v/60:.1f} min")
    blocked_reasons = collections.Counter(
        (h.get("reason") or "").split(":")[0] for h in sc if h["to"] == "blocked")
    print("  blocked transitions by reason prefix:", dict(blocked_reasons))
    print("  failures:", [h.get("reason") for h in sc if h["to"] == "failed"])

    msgs = collections.Counter(m.get("role") for m in feature.get("messages", []))
    print("\n## messages by role:", dict(msgs))
    print("## evidence:", [(e["title"], e["ok"], (e.get("detail") or "")[:60]) for e in feature.get("evidence", [])])

    # Runs.
    print("\n## runs")
    tot_cost = 0.0
    for run in feature["runs"]:
        conv = os.path.join(state, "conversations", run.get("conversation_id") or "", "journal.jsonl")
        tools, inter, cmds, cost, outcome, sess = [], collections.Counter(), collections.Counter(), None, None, None
        if os.path.exists(conv):
            for l in open(conv):
                e = json.loads(l)
                if e.get("kind") != "op":
                    continue
                o = e["op"]
                k = o.get("op")
                if k == "tool_started":
                    r = o["record"]
                    tools.append(r["name"])
                    if r["name"] == "Bash":
                        cmds[r.get("summary", "")] += 1
                elif k == "interaction_requested":
                    inter[o["interaction"].get("type")] += 1
                elif k == "finished":
                    u = o.get("usage") or {}
                    cost = u.get("cost_usd", cost)
                    outcome = o.get("outcome")
                elif k == "bind":
                    sess = o["binding"].get("session_id")
        start, end = ts(run.get("started_at")), ts(run.get("ended_at"))
        dur = (end - start).total_seconds() / 60 if start and end else None
        rep = {c: n for c, n in cmds.items() if n > 1}
        tot_cost += cost or 0
        print(f"- {run['id']} task={run['task_id']} state={run['state']} dur={dur and round(dur,1)}min "
              f"session={sess and sess[:8]} tools={len(tools)} {dict(collections.Counter(tools))} "
              f"interactions={dict(inter)} cost=${cost} turn_outcome={outcome}")
        if rep:
            print(f"    repeated commands: {rep}")
        print(f"    summary: {(run.get('summary') or '')[:160]!r}")
    print(f"\nrun cost reported by the agent (session-cumulative per run): ${tot_cost:.2f}")


if __name__ == "__main__":
    main(os.path.expanduser(sys.argv[1]), sys.argv[2])
