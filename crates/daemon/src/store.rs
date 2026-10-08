//! Durable state (design §30).
//!
//! V1 format: one JSON document (`state/state.json`) rewritten atomically on
//! every change. Small, transparent and easy to inspect; see docs/decisions.md.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use otter_core::Workspace;
use serde::{Deserialize, Serialize};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: STATE_VERSION,
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    pub state: State,
}

impl Store {
    pub fn load(path: &Path) -> Result<Store> {
        let state = match std::fs::read(path) {
            Ok(bytes) => {
                let state: State = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if state.version > STATE_VERSION {
                    bail!(
                        "{} was written by a newer otterd (state version {})",
                        path.display(),
                        state.version
                    );
                }
                state
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Store {
            path: path.to_path_buf(),
            state,
        })
    }

    /// Persist atomically: write a temp file, fsync, rename over the original.
    pub fn save(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        serde_json::to_writer_pretty(&mut f, &self.state)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    /// Find a workspace by id, falling back to name.
    pub fn workspace(&self, id_or_name: &str) -> Option<&Workspace> {
        let i = self.workspace_index(id_or_name)?;
        Some(&self.state.workspaces[i])
    }

    pub fn workspace_mut(&mut self, id_or_name: &str) -> Option<&mut Workspace> {
        let i = self.workspace_index(id_or_name)?;
        Some(&mut self.state.workspaces[i])
    }

    pub fn workspace_index(&self, id_or_name: &str) -> Option<usize> {
        let ws = &self.state.workspaces;
        ws.iter()
            .position(|w| w.id.as_str() == id_or_name)
            .or_else(|| ws.iter().position(|w| w.name == id_or_name))
    }
}
