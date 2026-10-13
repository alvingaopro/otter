#!/usr/bin/env bash
# The three-mode autonomy benchmark on one small task, each run in its own
# disposable git worktree. See 06-benchmark-results.md.
#
#   bench.sh setup <task> <dir>        worktree at BASE with the task injected
#   bench.sh direct <task> <dir>       Mode A: the Claude Agent SDK, directly
#   bench.sh otter <task> <dir> rules  Mode B: Otter `sdk` runtime, no model controller
#   bench.sh otter <task> <dir> model  Mode C: Otter `sdk` runtime + the Control Agent
#   bench.sh verify <task> <dir>       independent acceptance check
#
# Env: BENCH_OTTER_BIN (dir with a built `otter` + `otterd`), BENCH_WORKER
# (the installed claude-runtime dir; its node_modules gives Mode A the same SDK
# version), BENCH_OUT (logs), BENCH_BUDGET_S (wall-clock cap per run, 1500).
# Never pushes; never touches ~/.otter or ~/.config/otter. Mode C copies the
# Control Agent's settings + key from ~/.otter/state into a scratch OTTER_HOME
# (read, not modified), so the controller is the one the developer uses.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$HERE" rev-parse --show-toplevel)
BASE=${BENCH_BASE:-5ebcf4f}
OUT=${BENCH_OUT:-/tmp/otter-bench}
BUDGET=${BENCH_BUDGET_S:-1500}
WORKER=${BENCH_WORKER:-$HOME/.local/lib/otter/claude-runtime/0.6.19}
CARGO_BIN="$HOME/.cargo/bin"
mkdir -p "$OUT"

# Browser mock (all modes). The repo's own e2e tests open sign-in pages
# (`xdg-open https://github.com/login/device`); inside an Otter terminal the
# fallback `xdg-open` on PATH is the *real* daemon's stand-in, which sends the
# page to the developer's Otter app. So: drop every Otter stand-in dir from
# PATH, and put stubs first that only log what would have opened.
STUB="$OUT/stub-bin"
mkdir -p "$STUB"
for n in xdg-open www-browser otter-open open; do
  printf '#!/bin/sh\necho "$(date +%%s) %s $*" >> "%s/browser-mock.log"\nexit 0\n' "$n" "$OUT" > "$STUB/$n"
  chmod +x "$STUB/$n"
done
PATH="$STUB:$(printf '%s' "$PATH" | tr ':' '\n' | grep -v '/\.otter/run/bin$' | grep -v '/run/bin$' | paste -sd: -)"
# No BROWSER export: a value here leaks into nested test daemons and breaks
# the repo's browser-login e2e test (it expects otterd's own default).
export PATH

prompt() {
  case "$1" in
    t1) cat <<'EOF'
The unit test `normalized_brief_drops_blank_text` in the otter-core crate (crates/core) fails. Fix the code so the test passes, without changing the test. Check with `cargo test -p otter-core`. Do not commit.
EOF
    ;;
    t2) cat <<'EOF'
Add a method `pub fn item_count(&self) -> usize` to `Brief` in crates/core/src/model.rs. It returns how many things the brief holds: 1 for each of `title`, `goal` and `description` that is `Some`, plus 1 for each entry in `constraints`, `references` and `decisions`. Add a unit test for it next to the existing tests in that file. Check with `cargo test -p otter-core`. Do not commit.
EOF
    ;;
    *) echo "unknown task $1" >&2; exit 2 ;;
  esac
}

setup() {
  local task=$1 dir=$2
  git -C "$REPO" worktree add --detach "$dir" "$BASE" >/dev/null
  # Fairness control: both modes get the same toolchain on PATH. The repo's
  # `.envrc` is `use flake`, which needs `nix`, absent from a login shell's
  # PATH on this machine (see 04-recovery-analysis.md, finding R-3).
  printf 'PATH_add %s\n' "$CARGO_BIN" > "$dir/.envrc"
  (cd "$dir" && direnv allow . >/dev/null 2>&1 || true)
  case "$task" in
    t1) # Inject a deterministic bug: list items are no longer trimmed.
      python3 - "$dir/crates/core/src/model.rs" <<'EOF'
import sys
p = sys.argv[1]; s = open(p).read()
old = """            v.into_iter()
                .map(|s| s.trim().to_owned())"""
new = """            v.into_iter()
                .map(|s| s.to_owned())"""
assert old in s; open(p, "w").write(s.replace(old, new, 1))
EOF
      ;;
    t2) ;;
  esac
  git -C "$dir" add -A && git -C "$dir" -c user.name=bench -c user.email=bench@localhost \
    commit -qm "bench: task $task baseline"
}

verify() {
  local task=$1 dir=$2
  case "$task" in
    t1)
      # The test must be untouched, and pass.
      git -C "$dir" diff --quiet HEAD -- ':(glob)crates/core/src/model.rs' && { echo "FAIL: no change"; return 1; }
      git -C "$dir" diff HEAD -- crates/core/src/model.rs | grep -E '^[-+]' | grep -q 'normalized_brief_drops_blank_text' && { echo "FAIL: test changed"; return 1; }
      (cd "$dir" && PATH="$CARGO_BIN:$PATH" cargo test -q -p otter-core normalized_brief_drops_blank_text 2>&1 | tail -3)
      ;;
    t2)
      # Hidden acceptance test, appended only for the check, then removed.
      local f="$dir/crates/core/src/model.rs"
      cp "$f" "$f.bench-orig"
      cat >> "$f" <<'EOF'

#[cfg(test)]
mod bench_acceptance {
    use super::*;
    #[test]
    fn bench_item_count() {
        assert_eq!(Brief::default().item_count(), 0);
        let b = Brief {
            title: Some("t".into()),
            description: Some("d".into()),
            constraints: vec!["a".into(), "b".into()],
            decisions: vec!["c".into()],
            ..Default::default()
        };
        assert_eq!(b.item_count(), 5);
    }
}
EOF
      (cd "$dir" && PATH="$CARGO_BIN:$PATH" cargo test -p otter-core --lib 2>&1 | grep -E "bench_item_count|^test result|error(\[|:)" | head -5) || true
      mv "$f.bench-orig" "$f"
      ;;
  esac
}

direct() {
  local task=$1 dir=$2
  local t0; t0=$(date +%s)
  (cd "$dir" && PATH="$CARGO_BIN:$PATH" perl -e 'alarm shift; exec @ARGV' "$BUDGET" \
     node "$HERE/direct_sdk.mjs" "$WORKER/node_modules" "$dir" "$(prompt "$task")" \
     "$OUT/$task-A.json") > "$OUT/$task-A.out" 2> "$OUT/$task-A.err" || true
  echo "A $task wall_s=$(( $(date +%s) - t0 ))" | tee "$OUT/$task-A.wall"
}

otter_mode() {
  local task=$1 dir=$2 ctl=$3 bin=${BENCH_OTTER_BIN:?set BENCH_OTTER_BIN}
  local M; [[ $ctl == rules ]] && M=B || M=C
  # OTTER_HOME must be short (Unix socket paths are length-limited).
  local H; H=$(mktemp -d "${TMPDIR:-/tmp}/ob$M.XXXX"); local C="$OUT/$task-$M-ctl"
  echo "$H" > "$OUT/$task-$M.home"
  mkdir -p "$H/state" "$C"
  # Configured like a host is, in settings.json (not env vars, which would
  # leak into the agent's environment and from there into nested tests).
  if [[ $ctl == model ]]; then
    # The developer's own Control Agent: provider, model and key.
    python3 - "$HOME/.otter/state/settings.json" "$H/state/settings.json" <<'PY'
import json, sys
s = json.load(open(sys.argv[1])); s["coding_backend"] = "sdk"
json.dump(s, open(sys.argv[2], "w"))
PY
    cp "$HOME/.otter/state/secrets.json" "$H/state/"
    chmod 600 "$H/state/secrets.json"
  else
    echo '{"controller":"rules","coding_backend":"sdk"}' > "$H/state/settings.json"
  fi
  # otter + otterd laid out like an install, so otterd finds the 0.6.19
  # worker (and its SDK) at ../lib/otter/claude-runtime with no env var.
  local root="$OUT/root"
  mkdir -p "$root/bin" "$root/lib/otter"
  cp "$bin/otter" "$bin/otterd" "$root/bin/"
  ln -sfn "$(dirname "$WORKER")" "$root/lib/otter/claude-runtime"
  bin="$root/bin"
  export OTTER_CONFIG_DIR="$C" PATH="$bin:$PATH"
  otter host add bench --local --home "$H" >/dev/null
  otter workspace create "bench-$task" --dir "$dir" --no-shell --no-agent >/dev/null
  local t0; t0=$(date +%s)
  otter feature new "bench $task" --request "$(prompt "$task")" --workspace "bench-$task" --json \
    > "$OUT/$task-$M-feature.json"
  local fid; fid=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$OUT/$task-$M-feature.json")
  otter feature start "$fid" >/dev/null
  # The developer, played by this loop: answers every pending decision as
  # soon as it appears (approve, except clearly destructive commands; answer
  # questions with "your call"), and logs each one.
  while :; do
    local now; now=$(date +%s)
    otter feature show "$fid" --json > "$OUT/$task-$M-state.json" || true
    local st; st=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "$OUT/$task-$M-state.json")
    case "$st" in review|done|failed|cancelled|paused) break ;; esac
    (( now - t0 > BUDGET )) && { otter feature pause "$fid" >/dev/null || true; break; }
    python3 - "$OUT/$task-$M-state.json" <<'EOF' | while IFS=$'\t' read -r did kind summary; do
import json, sys
f = json.load(open(sys.argv[1]))
for d in f.get("decisions", []):
    if d["status"] == "pending":
        print(f'{d["id"]}\t{d["kind"]}\t{d["summary"][:200]}'.replace("\n", " "))
EOF
      echo "$(date +%s) $did $kind $summary" >> "$OUT/$task-$M-decisions.log"
      if [[ $kind == question ]]; then
        otter feature decide "$fid" "$did" --answer "Your call: pick what fits the task and keep going." >/dev/null || true
      elif [[ $summary =~ (rm\ -rf|git\ push|sudo) ]]; then
        otter feature decide "$fid" "$did" --deny >/dev/null || true
      else
        otter feature decide "$fid" "$did" >/dev/null || true
      fi
    done
    sleep 2
  done
  echo "$M $task wall_s=$(( $(date +%s) - t0 )) status=$st" | tee "$OUT/$task-$M.wall"
  cp "$H/state/features"/*/history.jsonl "$OUT/$task-$M-history.jsonl" 2>/dev/null || true
  otter host shutdown bench >/dev/null 2>&1 || true
}

case "${1:-}" in
  setup) setup "$2" "$3" ;;
  direct) direct "$2" "$3" ;;
  otter) otter_mode "$2" "$3" "$4" ;;
  verify) verify "$2" "$3" ;;
  *) sed -n 2,14p "$0"; exit 2 ;;
esac
