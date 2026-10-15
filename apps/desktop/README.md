# Otter desktop

A thin desktop client (Tauri 2 + React/TypeScript) over the same Rust client
library `otter` uses. Design and rationale: `docs/decisions.md` D-018.

It shows the hosts in `$OTTER_CONFIG_DIR/hosts.toml` (default
`~/.config/otter`, shared with `otter`; hosts can be added and removed in
the app or with `otter host add/rm`), every workspace on them grouped by
what needs you (pinned ones first, in your order), and an embedded terminal attached to the selected session.
It can install `otterd` on a host from the matching release (D-020). It
carries `otter` and `otterd` of its own version and links them into
`~/.local/bin` as the command-line tools on this Mac (D-085):
`scripts/sidecars.mjs` builds them into `src-tauri/binaries/` before
`tauri dev` / `tauri build` (`npm run sidecars` alone, before a plain
`cargo` build of `src-tauri`).

```sh
cd apps/desktop
npm install
npm run tauri dev                       # development, hot reload
OTTER_CONFIG_DIR=$(git rev-parse --show-toplevel)/.dev/ctl npm run tauri dev   # a dev host list (absolute path)
npm run tauri build                     # .app / .dmg in src-tauri/target/release/bundle
```

Releases: every merge to `main` builds an arm64 and an x86_64 `.dmg` and publishes them
with `otterd`/`otter` binaries (D-019). The version shown at the bottom of the
sidebar is the release version; a host running a different `otterd` shows its version in
the hosts list.

Remote hosts need a `otterd` that speaks the same protocol version (see
`docs/protocol.md`). Under `tauri dev`, macOS attributes notifications to the
terminal that launched the app; a built `.app` asks for permission itself.

Layout:

- `src-tauri/src/hosts.rs` — one task per host: snapshot, follow events, re-snapshot;
  forwards each event (`host-event`) and stream restarts (`host-resync`) to timelines.
- `src-tauri/src/view.rs` — the view model, with values derived by `otter-core`.
- `src-tauri/src/attach.rs` — terminal bridge (raw-byte channel out, commands in).
- `src-tauri/src/pins.rs` — pinned workspaces, kept in `pins.toml` (D-040).
- `src-tauri/src/tasks.rs` — task commands, passed through to each host's otterd (D-043).
- `src/` — React: `ActivityBar` + `nav.ts` (the Tasks / Workspaces / Otter views, D-041, D-088, D-107; `OtterView` + `otterChat.ts` are Otter's conversation on a host, D-093),
  `TasksView` + `tasks.ts` (the contract) + `taskSource.ts` (where
  tasks come from: the hosts, or sample data in tests; D-042),
  `Sidebar` (with the filter), `QuickOpen` (⌘K), `WorkspacePane` (kept
  mounted while hidden, D-100), `ChangesPanel` + `changes.ts` (D-102),
  `FindPanel` (⌘F), `SessionTerminal` (a session's terminal and its end
  banners; two side by side, D-110), `DiffView` + `diff.ts` + `highlight.ts`
  (diffs side by side, folded, highlighted, D-111), `Terminal` (xterm.js; `links.ts` for ⌘-click,
  `terminalFont.ts` for ⌘=/⌘-), `newer.ts` (requests older hosts don't
  know), `panelResize` (the
  list's and the Files/Timeline panel's widths, D-097; `composerResize` is
  the message box's height), `model.ts`
  (grouping and wording only), `TimelinePanel` + `timeline.ts` (a workspace's
  events as one line each).
