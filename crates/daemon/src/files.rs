//! Files for clients: browsing, downloading and uploading in a workspace
//! (`fs.*`), and pasting an image into a session (`session.paste_image`).
//!
//! **Image paste.** Claude Code on Linux reads a clipboard image by running
//! `xclip …`, then `wl-paste -l` / `wl-paste --type image/png`. Every session
//! gets `run/bin` first on its PATH, holding a stand-in `wl-paste`: when the
//! Otter app sees Ctrl+V with an image on the Mac's clipboard, it sends the
//! PNG here first (stored for that session), then the keystroke. The stand-in
//! serves an image pasted in the last two minutes and otherwise defers to a
//! real `wl-paste`. Nothing on the host can read the Mac's clipboard.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use otter_protocol::fs::{
    DirEntry, DirListing, EntryKind, FileChunk, FsPath, FsRead, FsWrite, MAX_CHUNK, PasteImage,
};
use otter_protocol::{RpcError, SessionRef};

use crate::daemon::{Daemon, RpcResult};
use crate::paths::Paths;
use crate::sessions::find_session;

/// Largest image accepted for a paste.
const MAX_IMAGE: usize = 20 * 1024 * 1024;

/// Directory put first on every session's PATH.
pub fn shim_dir(paths: &Paths) -> PathBuf {
    paths.run_dir.join("bin")
}

fn clipboard_dir(paths: &Paths) -> PathBuf {
    paths.run_dir.join("clipboard")
}

/// Write the stand-in `wl-paste` (on every daemon start).
pub fn install_shim(paths: &Paths) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = shim_dir(paths);
    std::fs::create_dir_all(&dir)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(clipboard_dir(paths))?;
    let q = |p: &Path| otter_client_quote(&p.to_string_lossy());
    let script = format!(
        r#"#!/bin/sh
# otterd's stand-in for wl-paste (generated; see files.rs). Serves an image
# pasted from the Otter app (Ctrl+V) to Claude Code and the like.
img={clip}/"$OTTER_SESSION_ID".png
fresh() {{ [ -n "$OTTER_SESSION_ID" ] && [ -f "$img" ] && [ -n "$(find "$img" -mmin -2 2>/dev/null)" ]; }}
case " $* " in
  *" -l "* | *" --list-types "*) if fresh; then echo image/png; exit 0; fi ;;
  *image/png*) if fresh; then cat "$img"; exit 0; fi ;;
esac
# Anything else: a real wl-paste, if there is one.
self={dir}
IFS=:
for d in $PATH; do
  [ "$d" = "$self" ] && continue
  [ -x "$d/wl-paste" ] && exec "$d/wl-paste" "$@"
done
exit 1
"#,
        clip = q(&clipboard_dir(paths)),
        dir = q(&dir),
    );
    let path = dir.join("wl-paste");
    std::fs::write(&path, script)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;

    // Browser login (login.rs): links a tool opens go to the Otter app's
    // browser on the Mac. One script under the names tools use.
    let otterd = std::env::current_exe()?;
    let open = format!(
        r#"#!/bin/sh
# otterd's browser stand-in (generated; see login.rs). Opens sign-in pages
# from this host in the Otter app's browser on the Mac.
name=$(basename "$0")
self={dir}
real() {{
  [ "$name" = otter-open ] && exit 1
  IFS=:
  for d in $PATH; do
    [ "$d" = "$self" ] && continue
    [ -x "$d/$name" ] && exec "$d/$name" "$@"
  done
  exit 1
}}
case $1 in
  http://* | https://*) ;;
  *) real "$@" ;;
esac
# A desktop session on this host opens its own browser.
if [ -n "$DISPLAY$WAYLAND_DISPLAY" ] && [ "$name" != otter-open ]; then real "$@"; fi
{otterd} --home {home} open-url "$1" || real "$@"
"#,
        dir = q(&dir),
        otterd = q(&otterd),
        home = q(&paths.home),
    );
    for name in ["xdg-open", "www-browser", "otter-open"] {
        let path = dir.join(name);
        std::fs::write(&path, &open)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// POSIX shell quoting.
fn otter_client_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn kind(meta: &std::fs::Metadata) -> EntryKind {
    let t = meta.file_type();
    if t.is_symlink() {
        EntryKind::Symlink
    } else if t.is_dir() {
        EntryKind::Dir
    } else if t.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn io_err(path: &Path, e: std::io::Error) -> RpcError {
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            RpcError::not_found(format!("{}: no such file", path.display()))
        }
        std::io::ErrorKind::AlreadyExists => RpcError::new(
            otter_protocol::ErrorCode::AlreadyExists,
            format!("{} already exists", path.display()),
        ),
        _ => RpcError::internal(format!("{}: {e}", path.display())),
    }
}

impl Daemon {
    /// A workspace path: relative to its root, or absolute.
    async fn resolve(&self, at: &FsPath) -> RpcResult<(PathBuf, PathBuf)> {
        let ws = self.workspace_get(&at.workspace).await?;
        let root = PathBuf::from(&ws.root);
        let path = if at.path.is_empty() {
            root.clone()
        } else if Path::new(&at.path).is_absolute() {
            PathBuf::from(&at.path)
        } else {
            root.join(&at.path)
        };
        Ok((root, path))
    }

    pub(crate) async fn fs_list(&self, at: &FsPath) -> RpcResult<DirListing> {
        let (root, path) = self.resolve(at).await?;
        let path = std::fs::canonicalize(&path).map_err(|e| io_err(&path, e))?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)
            .map_err(|e| io_err(&path, e))?
            .flatten()
        {
            // DirEntry::metadata does not follow symlinks: say what the entry is.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let target = if meta.file_type().is_symlink() {
                std::fs::metadata(entry.path()).ok()
            } else {
                None
            };
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                // A link to a directory browses like one.
                kind: match &target {
                    Some(t) if t.is_dir() => EntryKind::Dir,
                    _ => kind(&meta),
                },
                size: target.as_ref().unwrap_or(&meta).len(),
                modified: meta.modified().ok().map(chrono::DateTime::from),
            });
        }
        entries.sort_by(|a, b| {
            (a.kind != EntryKind::Dir)
                .cmp(&(b.kind != EntryKind::Dir))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(DirListing {
            parent: path.parent().map(|p| p.to_string_lossy().into_owned()),
            path: path.to_string_lossy().into_owned(),
            root: std::fs::canonicalize(&root)
                .unwrap_or(root)
                .to_string_lossy()
                .into_owned(),
            entries,
        })
    }

    pub(crate) async fn fs_read(&self, r: &FsRead) -> RpcResult<FileChunk> {
        use std::io::{Read, Seek, SeekFrom};
        let (_, path) = self.resolve(&r.at).await?;
        let mut file = std::fs::File::open(&path).map_err(|e| io_err(&path, e))?;
        let size = file.metadata().map_err(|e| io_err(&path, e))?.len();
        if file.metadata().is_ok_and(|m| m.is_dir()) {
            return Err(RpcError::invalid(format!(
                "{} is a directory",
                path.display()
            )));
        }
        let len = (r.len as usize).min(MAX_CHUNK);
        file.seek(SeekFrom::Start(r.offset))
            .map_err(|e| io_err(&path, e))?;
        let mut buf = Vec::with_capacity(len);
        file.take(len as u64)
            .read_to_end(&mut buf)
            .map_err(|e| io_err(&path, e))?;
        Ok(FileChunk {
            eof: r.offset + buf.len() as u64 >= size,
            data: B64.encode(&buf),
            size,
        })
    }

    pub(crate) async fn fs_write(&self, w: &FsWrite) -> RpcResult<()> {
        use std::io::{Seek, SeekFrom, Write};
        let (_, path) = self.resolve(&w.at).await?;
        let data = B64
            .decode(&w.data)
            .map_err(|e| RpcError::invalid(format!("data is not base64: {e}")))?;
        if data.len() > MAX_CHUNK {
            return Err(RpcError::invalid("chunk too large"));
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true);
        if w.create {
            if w.overwrite {
                opts.create(true).truncate(true);
            } else {
                opts.create_new(true);
            }
        }
        let mut file = opts.open(&path).map_err(|e| io_err(&path, e))?;
        file.seek(SeekFrom::Start(w.offset))
            .map_err(|e| io_err(&path, e))?;
        file.write_all(&data).map_err(|e| io_err(&path, e))?;
        Ok(())
    }

    pub(crate) async fn paste_image(self: &Arc<Self>, p: &PasteImage) -> RpcResult<()> {
        let session_id = {
            let store = self.store.lock().await;
            find_session(
                &store,
                &SessionRef {
                    workspace: p.workspace.clone(),
                    session: p.session.clone(),
                },
            )?
            .1
            .id
        };
        let png = B64
            .decode(&p.png)
            .map_err(|e| RpcError::invalid(format!("png is not base64: {e}")))?;
        if png.len() > MAX_IMAGE {
            return Err(RpcError::invalid("image is larger than 20 MB"));
        }
        if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(RpcError::invalid("not a PNG"));
        }
        use std::os::unix::fs::OpenOptionsExt;
        let path = clipboard_dir(&self.paths).join(format!("{session_id}.png"));
        let tmp = path.with_extension("png.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| io_err(&tmp, e))?;
            f.write_all(&png).map_err(|e| io_err(&tmp, e))?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shim_serves_a_fresh_paste_and_defers_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        install_shim(&paths).unwrap();
        let shim = shim_dir(&paths).join("wl-paste");
        let run = |args: &[&str], session: &str| {
            std::process::Command::new(&shim)
                .args(args)
                .env("OTTER_SESSION_ID", session)
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", shim_dir(&paths).display()),
                )
                .output()
                .unwrap()
        };
        // Nothing pasted: no image, and no real wl-paste to defer to.
        assert!(!run(&["-l"], "ses_a").status.success());
        std::fs::write(
            clipboard_dir(&paths).join("ses_a.png"),
            b"\x89PNG\r\n\x1a\nfake",
        )
        .unwrap();
        let list = run(&["-l"], "ses_a");
        assert!(list.status.success());
        assert_eq!(String::from_utf8_lossy(&list.stdout).trim(), "image/png");
        let got = run(&["--type", "image/png"], "ses_a");
        assert_eq!(got.stdout, b"\x89PNG\r\n\x1a\nfake");
        // Another session's paste isn't served.
        assert!(!run(&["-l"], "ses_b").status.success());
    }
}
