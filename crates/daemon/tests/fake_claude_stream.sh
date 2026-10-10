#!/bin/sh
# A fake `claude -p` speaking stream-json the way Claude Code 2.1.295 does
# (see crates/daemon/src/agents/claude_stream.rs). Words in a prompt choose
# what happens: ASK_RM (a command only the developer may allow), ASK_INSTALL
# (one the Control Agent may decide), ASK_EDIT (a routine edit), ASK_QUESTION,
# FAIL, HANG (works until interrupted); anything else finishes the turn.
[ -n "$FAKE_CODEX_LOG" ] && echo "claude-stream $*" >> "$FAKE_CODEX_LOG"
sid="fake-session-$$"
resumed=
while [ $# -gt 0 ]; do
  [ "$1" = --resume ] && sid="$2" && resumed=1
  shift
done
# A conversation remembers how it started (where the log lives, if any).
memory=
[ -n "$FAKE_CODEX_LOG" ] && memory="$(dirname "$FAKE_CODEX_LOG")/conversation-$sid"
out() { printf '%s\n' "$1"; }
result() { out '{"type":"result","subtype":"'"$1"'","is_error":'"$2"',"session_id":"'"$sid"'","total_cost_usd":0.01,"result":"'"$3"'"}'; }
text() { out '{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"text","text":"'"$1"'"}]}}'; }
ask() { out '{"type":"control_request","request_id":"'"$1"'","request":{"subtype":"can_use_tool","tool_name":"'"$2"'","input":'"$3"',"tool_use_id":"toolu_1"}}'; }

read -r line # initialize
out '{"type":"control_response","response":{"subtype":"success","request_id":"otter_1","response":{}}}'
out '{"type":"system","subtype":"init","session_id":"'"$sid"'","capabilities":["interrupt_receipt_v1"]}'
while read -r line; do
  case "$line" in
    *'"subtype":"interrupt"'*) result error_during_execution true "" ;;
    *'"behavior":"allow"'*'"answers"'*) text "Going with that."; result success false "answered" ;;
    *'"behavior":"allow"'*) text "Ran it."; result success false "allowed and done" ;;
    *'"behavior":"deny"'*) result success false "denied, so I stopped" ;;
    *'"type":"user"'*)
      if [ -n "$memory" ]; then
        if [ -n "$resumed" ] && [ -f "$memory" ]; then
          line="$(cat "$memory") $line"
        else
          printf '%s' "$line" > "$memory"
        fi
      fi
      case "$line" in
        *ASK_RM*) ask r1 Bash '{"command":"rm -rf build"}' ;;
        *ASK_INSTALL*) ask r2 Bash '{"command":"npm install left-pad"}' ;;
        *ASK_EDIT*) ask r4 Write '{"file_path":"src/a.rs","content":"x"}' ;;
        *ASK_QUESTION*) ask r3 AskUserQuestion '{"questions":[{"question":"Pick one","options":[{"label":"red"},{"label":"blue"}]}]}' ;;
        *FAIL*) result error_during_execution true "it broke" ;;
        *HANG*) text "working on it" ;;
        # "BG <pidfile>": something left running in the background.
        *'BG '*)
          f=$(printf '%s' "$line" | sed 's/.*BG \([^"]*\)".*/\1/')
          sleep 300 &
          echo $! > "$f"
          text "started it"
          ;;
        *)
          out '{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg_tool","role":"assistant","content":[{"type":"tool_use","id":"toolu_mt","name":"Bash","input":{"command":"make test"}}]}}'
          out '{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_mt","is_error":false,"content":"ok"}]}}'
          # The text as it is written (--include-partial-messages), then whole.
          out '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"id":"msg_fake"}}}'
          out '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Did "}}}'
          sleep 0.2
          out '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"the work."}}}'
          out '{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg_fake","role":"assistant","content":[{"type":"text","text":"Did the work."}]}}'
          result success false "done in $sid"
          ;;
      esac
      ;;
  esac
done
