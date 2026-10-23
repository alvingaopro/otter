# D-210 — The coding agent loads the developer's own Claude settings (2026-10-23)
<!-- claim: 5f9dfdaf9a30cef490d8b4539189673914c0c36c -->

The coding agent (the Claude Agent SDK worker, D-057) loaded the project's
settings only, `setting_sources: ["project"]`. It read the repository's
CLAUDE.md, but none of the developer's own `~/.claude`. On 2026-10-23 the
developer asked two tasks to "use /pr-review", their own skill in
`~/.claude/skills/pr-review`. Both tasks failed to find it and reviewed by
hand. D-207 left this open as a decision of its own.

The developer decided: load the user settings ("就加载用户skill吧"). They
were told the cost first: their Notification, Stop and UserPromptSubmit hooks
would run in every task too. They answered that those hooks can go ("我的本地通知hook可以去掉了").

What changes:

- The worker starts with `setting_sources: ["user", "project"]`. What the
  developer keeps in `~/.claude` now comes with every task's coding agent:
  - **Skills.** Checked against the SDK the worker ships (0.3.285) on the dev
    VM. Its first message lists `pr-review` with `["user", "project"]` and
    doesn't with `["project"]`.
  - **Hooks.**
  - **Plugins.** The dev VM's gopls and rust-analyzer LSP plugins load.
  - **Model settings.** The effort set for a model applies when the agent runs
    that model. When the host sets no coding model (Settings › coding), the
    developer's own `model` picks it, where before it was Claude Code's
    default. A coding model the host sets is passed explicitly and still wins.
  - **Permission rules.**
- Otter's policy still decides every tool call. Its `PreToolUse` hook runs
  before the developer's rules and answers each call. The developer's own
  hooks run as well, and a deny from one of them stops a call Otter allowed.
  That is the developer's choice to make in their own settings.
- The instructions Otter appends stay as they are. Otter's own turn (the
  `otter` profile, D-089) still loads no settings at all, so it has only
  Otter's tools.
- `legacy_cli` (`claude -p --setting-sources ""`) is unchanged. It is only
  still selectable, and its isolation is part of how it is checked.

This deliberately deviates from the isolation D-057 chose and the autonomy
audit (09, §4) recorded: "loads the repo's CLAUDE.md but not the developer's
user settings, skills or MCP servers — deliberate isolation". Otter's hosts
are the developer's own machines, and the coding agent works as the
developer would. Their own setup (skills above all) is what they expect it to
have.

**Revisit** if a setting of the developer's makes tasks misbehave: a hook that
blocks or prompts, a plugin too heavy for many workspaces at once, or an effort
setting that makes tasks too slow or costly. The narrower choice is then
skills alone: the SDK's `plugins: [{type: "local"}]` can bring
`~/.claude/skills` without the rest. Revisit too if Otter runs on a host shared
with someone else's `~/.claude`.

Tests: the unit test `agents::claude_sdk::tests::a_turn_goes_through_the_worker_with_a_policy_check_and_a_form`
checks the worker is started with `["user", "project"]`. It was watched failing
with `["project"]`. The worker's own tests (`worker.test.ts`) already accept
any of `user`, `project` and `local`, and still check that Otter's profile
takes none.
