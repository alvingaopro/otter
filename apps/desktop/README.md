# Otter desktop

A thin desktop client (Tauri 2 + React/TypeScript) over the same Rust client
library `otter` uses. Design and rationale: `docs/decisions.md` D-018.

It shows the hosts in `$OTTER_CONFIG_DIR/hosts.toml` (default
`~/.config/otter`, shared with `otter`; hosts can be added and removed in
the app or with `otter host add/rm`), every workspace on them grouped by
what needs you, and an embedded terminal attached to the selected session.
It can install `otterd` on a host and the command-line tools on this Mac from
the matching release (D-020).

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
- `src/` — React: `Sidebar`, `WorkspacePane`, `Terminal` (xterm.js), `model.ts`
  (grouping and wording only), `TimelinePanel` + `timeline.ts` (a workspace's
  events as one line each).
