#!/bin/sh
# A fake `gh` for delivery tests (D-047): pull requests and CI live in
# $FAKE_GH_DIR, which the test reads and writes. Every call is logged.
dir="$FAKE_GH_DIR"
echo "gh $*" >> "$dir/calls.log"
case "$1 $2" in
  "pr view")
    if [ -f "$dir/pr-view-fails" ]; then echo "error connecting to api.github.com" >&2; exit 1; fi
    if [ -f "$dir/pr" ]; then
      # What GitHub says of it (D-184): set by a test in pr-state,
      # mergeable, and (D-189) pr-head and merged-at.
      state=$(cat "$dir/pr-state" 2>/dev/null || echo OPEN)
      mergeable=$(cat "$dir/mergeable" 2>/dev/null || echo MERGEABLE)
      head=$(cat "$dir/pr-head" 2>/dev/null || echo 0123456789abcdef)
      merged=null
      if [ -f "$dir/merged-at" ]; then merged="\"$(cat "$dir/merged-at")\""; fi
      printf '{"number":7,"url":"https://github.com/o/r/pull/7","state":"%s","mergeable":"%s","headRefOid":"%s","baseRefName":"main","mergedAt":%s}\n' "$state" "$mergeable" "$head" "$merged"
    else
      echo "no pull requests found for branch" >&2; exit 1
    fi ;;
  "pr create")
    # no-commits: GitHub has every commit of the branch on its base already.
    if [ -f "$dir/no-commits" ]; then
      cat > /dev/null
      echo "pull request create failed: GraphQL: No commits between main and $4 (createPullRequest)" >&2; exit 1
    fi
    cat > "$dir/body"; touch "$dir/pr"; echo "https://github.com/o/r/pull/7" ;;
  "pr edit") cat > "$dir/body" ;;
  "pr checks")
    if [ -f "$dir/checks.json" ]; then cat "$dir/checks.json"; exit 8; fi
    echo "no checks reported on the branch" >&2; exit 1 ;;
  "pr comment") cat > "$dir/comment" ;;
  "run view")
    # A run's state (D-192): run-<id>.json, as a test writes it.
    case " $* " in
      *" --json "*)
        if [ -f "$dir/run-$3.json" ]; then cat "$dir/run-$3.json"; exit 0; fi
        echo "could not find any workflow run with ID $3" >&2; exit 1 ;;
    esac
    # As GitHub prints it, each view later than the last: @TIME@ and
    # @SECS@ in the log change from one view to the next.
    n=$(( $(cat "$dir/views" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$dir/views"
    sed -e "s/@TIME@/2026-10-20T18:0$((n % 10)):00.${n}4598260Z/g" -e "s/@SECS@/0.0${n}s/g" "$dir/log" 2>/dev/null ;;
  "run rerun")
    echo "$3" >> "$dir/reruns"
    if [ -f "$dir/after-rerun.json" ]; then mv "$dir/after-rerun.json" "$dir/checks.json"; fi ;;
  # Otter's own sign-in (D-133): only in its config directory, never with
  # a token from the environment (gh would use it, or refuse).
  "auth login")
    if [ -n "$GH_TOKEN$GITHUB_TOKEN" ]; then
      echo "The value of the GH_TOKEN environment variable is being used for authentication." >&2; exit 1
    fi
    [ -n "$GH_CONFIG_DIR" ] || { echo "fake gh: no GH_CONFIG_DIR" >&2; exit 1; }
    case " $* " in
      *" --with-token "*)
        read -r token
        if [ "$token" = bad ]; then echo "error validating token: HTTP 401: Bad credentials" >&2; exit 1; fi ;;
      *" --web "*)
        echo >&2
        echo "! First copy your one-time code: EFB1-6B49" >&2
        echo "Open this URL to continue in your web browser: https://github.com/login/device" >&2
        while [ ! -f "$dir/approve" ]; do sleep 1; done ;;
    esac
    printf 'github.com:\n    user: alvin\n' > "$GH_CONFIG_DIR/hosts.yml" ;;
  "api user")
    if [ -f "$dir/api-fails" ]; then echo "error connecting to api.github.com" >&2; exit 1; fi
    if [ -n "$GH_CONFIG_DIR" ] && [ -f "$GH_CONFIG_DIR/hosts.yml" ]; then echo alvin
    else echo "HTTP 401: Requires authentication" >&2; exit 1; fi ;;
  "ssh-key add")
    if [ -f "$dir/no-key-scope" ]; then
      echo "HTTP 403: Resource not accessible by integration (needs the admin:public_key scope)" >&2; exit 1
    fi
    cp "$3" "$dir/added-key" ;;
  *) echo "fake gh: unexpected: gh $*" >&2; exit 1 ;;
esac
