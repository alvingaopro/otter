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

Releases: every merge to `main` builds an arm64 `.dmg` (Apple silicon only, D-143) and publishes it
with `otterd`/`otter` binaries (D-019). The version shown at the bottom of the
sidebar is the release version; a host running a different `otterd` shows its version in
the hosts list. An installed app updates itself (D-125): it checks the latest release
hourly and offers "Update to vX.Y.Z" beside its version (or "Check for Updates…" in the
app menu); a click downloads the signed update, swaps it in and restarts. Releases sign
it with the `TAURI_SIGNING_PRIVATE_KEY` secret; a local `tauri build` makes no update and
needs no key.

Remote hosts need a `otterd` that speaks the same protocol version (see
`docs/protocol.md`). Under `tauri dev`, macOS attributes notifications to the
terminal that launched the app; a built `.app` asks for permission itself.

Layout:

- `src-tauri/src/hosts.rs` — one task per host: snapshot, follow events, re-snapshot;
  forwards each event (`host-event`) and stream restarts (`host-resync`) to timelines.
- `src-tauri/src/view.rs` — the view model, with values derived by `otter-core`.
- `src-tauri/src/attach.rs` — terminal bridge (raw-byte channel out, commands in).
- `src-tauri/src/pins.rs` — pinned workspaces, kept in `pins.toml` (D-040).
- `src-tauri/src/updates.rs` — the app's own updates, from the latest release (D-125).
- `src-tauri/src/perf.rs` — the app process's memory and CPU, for the performance overlay (D-128).
- `src-tauri/src/tasks.rs` — task commands, passed through to each host's otterd (D-043).
- `src/` — React: `ActivityBar` + `nav.ts` (the Inbox / Tasks / Workspaces views, D-041, D-088, D-107; Otter's view on a host left the app, D-115),
  `TasksView` (a grouped list; a task's Overview and Activity, D-114) + `tasks.ts` (the contract) + `taskSource.ts` (where
  tasks come from: the hosts, or sample data in tests; D-042),
  `Sidebar` (with the filter, and updating every host behind the app at once, D-131), `QuickOpen` (⌘K), `WorkspacePane` (kept
  mounted while hidden, D-100; its sessions' terminals too, `keepSessions.ts`, D-152), `ChangesPanel` + `changes.ts` (D-102),
  `FindPanel` (⌘F), `SessionTerminal` (a session's terminal and its end
  banners; two side by side, D-110), `DiffView` + `diff.ts` + `highlight.ts`
  (diffs side by side, folded, highlighted, D-111), `Terminal` (xterm.js, which keeps the session's history, D-118; `imeCommit.ts` for Pinyin's punctuation, D-126; `pixelScroll.ts` for scrolling by the pixel, D-130; `links.ts` for ⌘-click,
  `terminalFont.ts` for ⌘=/⌘- and the font in Settings › Appearance, D-116; `SettingsDialog`, tabs by kind, D-127; `PerfOverlay` + `perf.ts`, Settings › Developer's readout, D-128), `newer.ts` (requests older hosts don't
  know), `panelResize` (the
  list's and the Files/Timeline panel's widths, D-097; `composerResize` is
  the message box's height), `model.ts`
  (grouping and wording only), `TimelinePanel` + `timeline.ts` (a workspace's
  events as one line each).
