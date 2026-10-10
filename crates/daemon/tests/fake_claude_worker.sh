#!/bin/sh
# A fake Claude worker (packages/claude-runtime) speaking its JSONL protocol,
# for otterd's tests: no Node, no SDK, no Claude. FAKE_WORKER_MODE picks the
# behaviour: ok (a turn with a policy check, a tool result and a two-question
# form), silent (never ready), fatal (fails to start), noisy (floods stderr
# and sends an oversized frame), old (an older protocol). Every command is
# appended to FAKE_WORKER_LOG.
mode="${FAKE_WORKER_MODE:-ok}"
out() { printf '%s\n' "$1"; }
if [ "${1:-}" = --check ]; then
  out '{"protocol_version":1,"worker_version":"0.1.0","sdk_version":"fake","node_version":"0","node_supported":true,"claude_code":true}'
  exit 0
fi
turn=
while IFS= read -r line; do
  [ -n "$FAKE_WORKER_LOG" ] && printf '%s\n' "$line" >> "$FAKE_WORKER_LOG"
  case "$line" in
    *'"type":"initialize"'*)
      case "$mode" in
        silent) continue ;;
        fatal)
          echo "diagnostic line: looking for credentials" >&2
          out '{"type":"fatal","message":"no credentials","category":"auth_required"}'
          exit 1
          ;;
        old) out '{"type":"ready","protocol_version":0,"worker_version":"0.0.1","sdk_version":"0","node_version":"0"}' ;;
        *) out '{"type":"ready","protocol_version":1,"worker_version":"0.1.0","sdk_version":"fake","node_version":"0"}' ;;
      esac
      ;;
    *'"type":"send_turn"'*)
      turn=$(printf '%s' "$line" | sed 's/.*"turn_id":"\([^"]*\)".*/\1/')
      out '{"type":"session","session_id":"s-1","model":"fake","claude_code_version":"0","auth_source":"none"}'
      out '{"type":"delivered","turn_id":"'"$turn"'"}'
      if [ "$mode" = noisy ]; then
        head -c 3000000 /dev/zero | tr '\0' x >&2
        printf '{"type":"text","message":"big","block":0,"text":"'
        head -c 2000000 /dev/zero | tr '\0' y
        printf '"}\n'
        out '{"type":"text","message":"m1","block":0,"text":"small"}'
        out '{"type":"turn_finished","turn_id":"'"$turn"'","outcome":"completed","summary":"small","error":null,"session_cost_usd":0.01}'
        continue
      fi
      out '{"type":"policy_check","check_id":"chk-1","tool":"Bash","input":{"command":"make test"},"tool_use_id":"tu_1"}'
      ;;
    *'"type":"policy_reply"'*'"decision":"deny"'*)
      out '{"type":"tool_started","call":"tu_1","parent":null,"tool":"Bash","input":{"command":"make test"}}'
      out '{"type":"tool_finished","call":"tu_1","ok":false,"output":"denied by Otter"}'
      out '{"type":"turn_finished","turn_id":"'"$turn"'","outcome":"completed","summary":"I was not allowed to.","error":null,"session_cost_usd":0.01}'
      ;;
    *'"type":"policy_reply"'*)
      out '{"type":"tool_started","call":"tu_1","parent":null,"tool":"Bash","input":{"command":"make test"}}'
      out '{"type":"tool_finished","call":"tu_1","ok":true,"output":"all good"}'
      out '{"type":"permission_request","request_id":"perm-1","tool":"AskUserQuestion","input":{"questions":[{"question":"Which format?","header":"Format","options":[{"label":"CSV","description":"comma separated"},{"label":"JSON","description":"one object per line"}],"multiSelect":false},{"question":"Which columns?","header":"Columns","options":[{"label":"name","description":""},{"label":"time","description":""}],"multiSelect":true}]},"tool_use_id":"tu_2"}'
      ;;
    *'"type":"resolve_interaction"'*)
      out '{"type":"ack","request_id":"x"}'
      out '{"type":"text_delta","message":"m1","block":0,"text":"Do"}'
      out '{"type":"text","message":"m1","block":0,"text":"Done."}'
      out '{"type":"turn_finished","turn_id":"'"$turn"'","outcome":"completed","summary":"Done.","error":null,"session_cost_usd":0.02}'
      ;;
    *'"type":"interrupt"'*)
      out '{"type":"turn_finished","turn_id":"'"$turn"'","outcome":"interrupted","summary":null,"error":null,"session_cost_usd":0.02}'
      ;;
    *'"type":"shutdown"'*)
      exit 0
      ;;
  esac
done
