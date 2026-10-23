# Decisions from D-198 on

Each decision is a file of its own here: `D-NNN-<slug>.md`, its first line
`# D-NNN — Title (YYYY-MM-DD)`, then what was decided, why, and what would
make us revisit it — as in [`../decisions.md`](../decisions.md), which keeps
D-001–D-197.

Make one with

```sh
python3 scripts/decisions.py new "What was decided"
```

when the work starts, before its number is written into code, tests or
docs. It claims the next number on `origin` (the tag `decision/D-NNN`, which
only one agent can push) and writes the file, its second line naming the
claim (`<!-- claim: … -->`; keep it). CI runs `scripts/decisions.py check`
(D-198).
