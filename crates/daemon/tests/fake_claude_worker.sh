#!/bin/sh
# A fake Claude worker (packages/claude-runtime) speaking its JSONL protocol,
# for otterd's tests: no Node, no SDK, no Claude. FAKE_WORKER_MODE picks the
# behaviour: ok (a turn with a policy check, a tool result and a two-question
# form; onequestion: one question with no choices instead), silent (never ready), fatal (fails to start), noisy (floods stderr
# and sends an oversized frame), old (an older protocol), coding-only (no
# Otter profile), refuses (refuses an Otter turn, as over its picture
# limits). Every command is appended to FAKE_WORKER_LOG.
#
# Initialized with the Otter profile (D-089), a turn looks the task up
# (get_task); when the prompt says FORWARD, it passes on the developer's
# last message by the id the prompt gives it (forward_to_agent); then it
# answers with what it was told.
#
# Whether it started signed in with a token from Settings (D-142) goes to
# FAKE_WORKER_LOG as `oauth token: set` or `unset` — never the token.
#
# A coding turn that is a question ("a question." in it, D-089 M2.5) tries
# to write a file and to run a command, then answers, saying what Otter's
# policy said to each. Modes for the work turn: slow (finishes after 2 s),
# hang (never finishes). A planning turn ("plan the work") tries to write a
# file, then answers in prose (NOPLAN) or proposes a two-step plan as JSON,
# saying what Otter's policy said to the write. Told its plan was turned
# down on an answer-only task ("turned down changing the code", D-158), it
# tries the write again, then answers in prose — or proposes the plan again
# (mode replans; replans-bare: the plan alone), or never finishes
# (answerhangs: carried on after that, it plans).
mode="${FAKE_WORKER_MODE:-ok}"
out() { printf '%s\n' "$1"; }
profiles='"profiles":["coding","otter"],'
[ "$mode" = coding-only ] && profiles=
if [ "${1:-}" = --check ]; then
  out '{"protocol_version":1,"worker_version":"0.1.0","sdk_version":"fake","node_version":"0","node_supported":true,'"$profiles"'"claude_code":true}'
  exit 0
fi
if [ -n "$FAKE_WORKER_LOG" ]; then
  if [ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]; then
    printf 'oauth token: set\n' >> "$FAKE_WORKER_LOG"
  else
    printf 'oauth token: unset\n' >> "$FAKE_WORKER_LOG"
  fi
fi
otter=
forward=
msg=
status=
turn=
question=
q1=
planning=
noplan=
bare=
while IFS= read -r line; do
  [ -n "$FAKE_WORKER_LOG" ] && printf '%s\n' "$line" >> "$FAKE_WORKER_LOG"
  if [ -n "$planning" ]; then
    case "$line" in
      *'"type":"policy_reply"'*)
        d=$(printf '%s' "$line" | grep -o '"decision":"[a-z]*"' | sed 's/.*:"//; s/"//')
        if [ -n "$noplan" ]; then
          out '{"type":"turn_finished","turn_id":"'"$turn"'","outcome":"completed","summary":"The export lives in export.sh:3. (Write: '"$d"'.)","error":null,"session_cost_usd":0.01}'
        else
          said='(Write: '"$d"'.)\n'
          [ -n "$bare" ] && said=
          out '{"type":"turn_finished","turn_id":"'"$turn"'","outcome":"completed","summary":"'"$said"'```json\n{\"summary\": \"Add an exporter, then cover it\", \"steps\": [{\"title\": \"Write the exporter\", \"detail\": \"in export.sh\"}, {\"title\": \"Cover it with a test\", \"detail\": \"in the Makefile\"}], \"criteria\": [{\"text\": \"Rows export as CSV with a header\", \"how\": \"diff\"}, {\"text\": \"make test passes\", \"how\": \"check\"}], \"check\": \"make test\"}\n```","error":null,"session_cost_usd":0.01}'
        fi
        planning=
        noplan=
        bare=
        continue
        ;;
    esac
  fi
  if [ -n "$question" ]; then
    case "$line" in
      *'"type":"policy_reply"'*)
        d=$(printf '%s' "$line" | grep -o '"decision":"[a-z]*"' | sed 's/.*:"//; s/"//')
        if [ -z "$q1" ]; then
          q1=$d
          out '{"type":"policy_check","check_id":"chk-q2","tool":"Bash","input":{"command":"make test"},"tool_use_id":"tu_q2"}'
          continue
        fi
        out '{"type":"text","message":"mq","block":0,"text":"It is in src/export.rs:12."}'
        out '{"type":"turn_finished","turn_id":"'"$turn"'","outcome":"completed","summary":"It is in src/export.rs:12. (Write: '"$q1"'; Bash: '"$d"'.)","error":null,"session_cost_usd":0.01}'
        question=
        q1=
        continue
        ;;
    esac
  fi
  case "$line" in
    *'"type":"initialize"'*)
      case "$mode" in
        silent) continue ;;
        fatal)
          # Node's warning, as the real worker prints it on every start.
          echo "(node:4242) [CLAUDE_SDK_CAN_USE_TOOL_SHADOWED] Warning: canUseTool will not be invoked for: Glob, Grep." >&2
          echo '(Use `node --trace-warnings ...` to show where the warning was created)' >&2
          echo "diagnostic line: looking for credentials" >&2
          echo "tried ANTHROPIC_API_KEY=sk-ant-api03-WORKERLEAKWORKERLEAK00" >&2
          out '{"type":"fatal","message":"no credentials","category":"auth_required"}'
          exit 1
          ;;
        old) out '{"type":"ready","protocol_version":0,"worker_version":"0.0.1","sdk_version":"0","node_version":"0"}' ;;
        *) out '{"type":"ready",'"$profiles"'"protocol_version":1,"worker_version":"0.1.0","sdk_version":"fake","node_version":"0"}' ;;
      esac
      case "$line" in *'"profile":"otter"'*) otter=1 ;; esac
      ;;
    *'"type":"send_turn"'*)
      if [ -n "$otter" ]; then
        if [ "$mode" = refuses ]; then
          out '{"type":"nack","request_id":"r2","error":"at most 4 images","category":"invalid_input"}'
          continue
        fi
        turn=t1
        case "$line" in *FORWARD*) forward=1 ;; esac
        msg=$(printf '%s' "$line" | grep -o '\[msg_[A-Za-z0-9]*\]' | tail -1 | tr -d '[]')
        out '{"type":"text_delta","message":"m0","block":0,"text":"Let me "}'
        out '{"type":"text_delta","message":"m0","block":0,"text":"look."}'
        out '{"type":"tool_call","call_id":"call-1","tool":"get_task","input":{}}'
        continue
      fi
      turn=$(printf '%s' "$line" | sed 's/.*"turn_id":"\([^"]*\)".*/\1/')
      out '{"type":"session","session_id":"s-1","model":"fake","claude_code_version":"0","auth_source":"none"}'
      out '{"type":"delivered","turn_id":"'"$turn"'"}'
      case "$line" in
        *'plan the work'*)
          planning=1
          case "$line" in *NOPLAN*) noplan=1 ;; esac
          out '{"type":"policy_check","check_id":"chk-p1","tool":"Write","input":{"file_path":"export.sh","content":"x"},"tool_use_id":"tu_p1"}'
          continue
          ;;
        *'turned down changing the code'*)
          [ "$mode" = answerhangs ] && continue
          planning=1
          case "$mode" in
            replans) ;;
            replans-bare) bare=1 ;;
            *) noplan=1 ;;
          esac
          out '{"type":"policy_check","check_id":"chk-p1","tool":"Write","input":{"file_path":"export.sh","content":"x"},"tool_use_id":"tu_p1"}'
          continue
          ;;
        *'Continue the step where you left off'*)
          if [ "$mode" = answerhangs ]; then
            planning=1
            out '{"type":"policy_check","check_id":"chk-p1","tool":"Write","input":{"file_path":"export.sh","content":"x"},"tool_use_id":"tu_p1"}'
            continue
          fi
          ;;
        *'a question.'*)
          question=1
          out '{"type":"policy_check","check_id":"chk-q1","tool":"Write","input":{"file_path":"src/a.rs","content":"x"},"tool_use_id":"tu_q1"}'
          continue
          ;;
      esac
      case "$mode" in
        slow)
          sleep 2
          out '{"type":"text","message":"mw","block":0,"text":"Did the work."}'
          out '{"type":"turn_finished","turn_id":"'"$turn"'","outcome":"completed","summary":"Did the work.","error":null,"session_cost_usd":0.01}'
          continue
          ;;
        hang) continue ;;
      esac
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
      if [ "$mode" = onequestion ]; then
        out '{"type":"permission_request","request_id":"perm-1","tool":"AskUserQuestion","input":{"questions":[{"question":"Which repository is it in?","header":"Repository","options":[],"multiSelect":false}]},"tool_use_id":"tu_2"}'
        continue
      fi
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
    *'"type":"tool_result"'*)
      case "$line" in
        *'"call_id":"call-2"'*)
          done=$(printf '%s' "$line" | grep -o '\\"done\\":\\"[a-z ]*' | head -1 | sed 's/.*"//')
          out '{"type":"turn_finished","turn_id":"t1","outcome":"completed","summary":"The task is '"$status"'; your message: '"$done"'.","error":null,"session_cost_usd":0.01}'
          continue
          ;;
      esac
      status=$(printf '%s' "$line" | grep -o '\\"status\\":\\"[a-z_]*' | head -1 | sed 's/.*"//')
      if [ -n "$forward" ]; then
        out '{"type":"tool_call","call_id":"call-2","tool":"forward_to_agent","input":{"message_ids":["'"$msg"'"]}}'
        continue
      fi
      out '{"type":"text_delta","message":"m1","block":0,"text":"The task is "}'
      out '{"type":"text_delta","message":"m1","block":0,"text":"'"$status"'."}'
      out '{"type":"text","message":"m1","block":0,"text":"The task is '"$status"'."}'
      out '{"type":"turn_finished","turn_id":"t1","outcome":"completed","summary":"The task is '"$status"'.","error":null,"session_cost_usd":0.01}'
      ;;
    *'"type":"shutdown"'*)
      exit 0
      ;;
  esac
done
