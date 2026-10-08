# Workd desktop

A thin desktop client (Tauri 2 + React/TypeScript) over the same Rust client
library `workctl` uses. Design and rationale: `docs/decisions.md` D-018.

It shows the hosts registered with `workctl host add` (from
`$WORKCTL_CONFIG_DIR/hosts.toml`, default `~/.config/workd`), every workspace
on them grouped by what needs you, and an embedded terminal attached to the
selected session.

```sh
cd apps/desktop
npm install
npm run tauri dev                       # development, hot reload
WORKCTL_CONFIG_DIR=$(git rev-parse --show-toplevel)/.dev/ctl npm run tauri dev   # a dev host list (absolute path)
npm run tauri build                     # .app / .dmg in src-tauri/target/release/bundle
```

Releases: every merge to `main` builds the universal `.dmg` and publishes it
with `workd`/`workctl` binaries (D-019). The version shown in the title bar is
the release version; a host running a different `workd` shows its version in
the hosts list.

Remote hosts need a `workd` that speaks the same protocol version (see
`docs/protocol.md`). Under `tauri dev`, macOS attributes notifications to the
terminal that launched the app; a built `.app` asks for permission itself.

Layout:

- `src-tauri/src/hosts.rs` — one task per host: snapshot, follow events, re-snapshot.
- `src-tauri/src/view.rs` — the view model, with values derived by `workd-core`.
- `src-tauri/src/attach.rs` — terminal bridge (raw-byte channel out, commands in).
- `src/` — React: `Sidebar`, `WorkspacePane`, `Terminal` (xterm.js), `model.ts`
  (grouping and wording only).
