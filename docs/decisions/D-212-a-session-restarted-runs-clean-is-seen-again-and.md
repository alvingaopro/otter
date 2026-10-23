# D-212 — A session restarted runs clean, is seen again and leaves starting (2026-10-23)
<!-- claim: 459291243e0b3d940963fb4cc8be9fd639b92e74 -->

On `code`, a Claude Code session restarted from the app showed a blank
terminal saying "… ended", and `otter ls` had it `starting` for good while
it sat at its prompt. Three things were wrong.

- **otterd passed on the Claude Code it was started from.** A deploy run by
  an agent restarted `otterd serve` inside Claude Code, with that
  conversation's `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_CHILD_SESSION=1` and
  the rest in its environment. The login shell it asks for the environment
  (design §17) inherits them, so every Claude Code it launched afterwards
  took the other conversation's id and, as a child, saved no transcript.
  The markers Claude Code sets for its children
  (`agents::claude::SESSION_MARKERS`) are now removed from the environment
  like the terminal's own (`env::STRIP`). A list, not a `CLAUDE_CODE_`
  prefix: `CLAUDE_CODE_OAUTH_TOKEN` and the like are the developer's
  settings. `CLAUDE_EFFORT` is kept for the same reason.
- **The app didn't attach to the new execution.** It attached again only
  when the session went from not running to running. A restart stops and
  starts it within milliseconds, so the session is never seen not running.
  The session's view now carries its current execution, and an attach that
  ended is made again once the execution is another, whichever is seen
  first.
- **A Claude Code session quiet at its prompt stayed `starting`.** With its
  hooks heard (`SessionStart`), the quiet-screen heuristic (`settle`) was
  skipped, and it was the only thing that moved a session out of
  `starting` without a turn in the transcript. It now applies to `starting`
  even with hooks; the hooks keep deciding everything else.

Tests: `a_daemon_started_inside_claude_code_does_not_pass_its_session_on`,
`a_resumed_session_quiet_at_its_prompt_is_not_starting` (watched failing
without the change). The app's re-attach has no test, and none of this was
checked in the macOS app. On `code`, otterd was restarted from a clean
environment and the session restarted: its Claude Code then had none of the
markers.

Revisit if Claude Code marks its children with other variables, or if the
hooks gain an event that says the prompt is up.
