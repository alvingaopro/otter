#!/bin/sh
# A fake `gh` for delivery tests (D-047): pull requests and CI live in
# $FAKE_GH_DIR, which the test reads and writes. Every call is logged.
dir="$FAKE_GH_DIR"
echo "gh $*" >> "$dir/calls.log"
case "$1 $2" in
  "pr view")
    if [ -f "$dir/pr" ]; then
      printf '{"number":7,"url":"https://github.com/o/r/pull/7","state":"OPEN"}\n'
    else
      echo "no pull requests found for branch" >&2; exit 1
    fi ;;
  "pr create") cat > "$dir/body"; touch "$dir/pr"; echo "https://github.com/o/r/pull/7" ;;
  "pr edit") cat > "$dir/body" ;;
  "pr checks")
    if [ -f "$dir/checks.json" ]; then cat "$dir/checks.json"; exit 8; fi
    echo "no checks reported on the branch" >&2; exit 1 ;;
  "pr comment") cat > "$dir/comment" ;;
  "run view") cat "$dir/log" 2>/dev/null ;;
  "run rerun")
    echo "$3" >> "$dir/reruns"
    if [ -f "$dir/after-rerun.json" ]; then mv "$dir/after-rerun.json" "$dir/checks.json"; fi ;;
  *) echo "fake gh: unexpected: gh $*" >&2; exit 1 ;;
esac
