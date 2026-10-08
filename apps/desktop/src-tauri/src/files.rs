//! Files and images between this Mac and a host's workspace: browsing,
//! downloading and uploading (chunked through otterd, with progress events),
//! and pasting a clipboard image into a session for Claude Code.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use otter_client::Connection;
use otter_protocol::fs::{DirListing, FsPath, FsWrite, MAX_CHUNK};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::hosts::Hosts;

async fn conn(app: &AppHandle, host: &str) -> Result<Connection, String> {
    let transport = app.state::<Hosts>().transport(host)?;
    Connection::connect(&transport)
        .await
        .map_err(|e| e.to_string())
}

/// Progress of one transfer, for the files panel.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    id: String,
    name: String,
    done: u64,
    total: u64,
}

#[tauri::command]
pub async fn fs_list(
    app: AppHandle,
    host: String,
    workspace: String,
    path: String,
) -> Result<DirListing, String> {
    conn(&app, &host)
        .await?
        .fs_list(&workspace, &path)
        .await
        .map_err(|e| e.to_string())
}

/// Download a file from the workspace to `dest` on this Mac.
#[tauri::command]
pub async fn file_download(
    app: AppHandle,
    host: String,
    workspace: String,
    path: String,
    dest: String,
    id: String,
) -> Result<(), String> {
    let mut c = conn(&app, &host).await?;
    let tmp = PathBuf::from(format!("{dest}.otter-download"));
    let mut out = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let name = Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    let mut done = 0u64;
    let result = async {
        loop {
            let chunk = c
                .fs_read(&workspace, &path, done, MAX_CHUNK as u64)
                .await
                .map_err(|e| e.to_string())?;
            let data = B64.decode(&chunk.data).map_err(|e| e.to_string())?;
            out.write_all(&data).map_err(|e| e.to_string())?;
            done += data.len() as u64;
            let _ = app.emit(
                "transfer",
                Progress {
                    id: id.clone(),
                    name: name.clone(),
                    done,
                    total: chunk.size,
                },
            );
            if chunk.eof || data.is_empty() {
                return Ok::<(), String>(());
            }
        }
    }
    .await;
    drop(out);
    match result {
        Ok(()) => std::fs::rename(&tmp, &dest).map_err(|e| format!("{dest}: {e}")),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Upload files from this Mac into `dir` of the workspace. Directories are
/// skipped; existing files are replaced only with `overwrite`.
#[tauri::command]
pub async fn file_upload(
    app: AppHandle,
    host: String,
    workspace: String,
    dir: String,
    sources: Vec<String>,
    overwrite: bool,
    id: String,
) -> Result<Vec<String>, String> {
    let mut c = conn(&app, &host).await?;
    let mut uploaded = Vec::new();
    for src in sources {
        let src = PathBuf::from(src);
        if !src.is_file() {
            continue;
        }
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or("bad file name")?;
        let total = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
        let target = if dir.is_empty() {
            name.clone()
        } else {
            format!("{}/{name}", dir.trim_end_matches('/'))
        };
        let mut file = std::fs::File::open(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        let mut buf = vec![0u8; MAX_CHUNK];
        let mut offset = 0u64;
        loop {
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 && offset > 0 {
                break;
            }
            c.fs_write(FsWrite {
                at: FsPath {
                    workspace: workspace.clone(),
                    path: target.clone(),
                },
                offset,
                data: B64.encode(&buf[..n]),
                create: offset == 0,
                overwrite,
            })
            .await
            .map_err(|e| format!("{name}: {e}"))?;
            offset += n as u64;
            let _ = app.emit(
                "transfer",
                Progress {
                    id: id.clone(),
                    name: name.clone(),
                    done: offset,
                    total,
                },
            );
            if n == 0 {
                break;
            }
        }
        uploaded.push(name);
    }
    Ok(uploaded)
}

/// If this Mac's clipboard holds an image, hand it to the session (as PNG)
/// and return true: the caller then sends Ctrl+V, and Claude Code reads it
/// through otterd's wl-paste stand-in. False when there is no image.
#[tauri::command]
pub async fn paste_image(
    app: AppHandle,
    host: String,
    workspace: String,
    session: String,
) -> Result<bool, String> {
    let png = {
        let Ok(image) = app.clipboard().read_image() else {
            return Ok(false);
        };
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, image.width(), image.height());
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().map_err(|e| e.to_string())?;
            w.write_image_data(image.rgba())
                .map_err(|e| e.to_string())?;
        }
        out
    };
    conn(&app, &host)
        .await?
        .paste_image(&workspace, &session, B64.encode(&png))
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}
