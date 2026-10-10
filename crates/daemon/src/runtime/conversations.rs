//! Runtime conversations on this host (D-055): the registry the actors
//! change and clients read.
//!
//! ```text
//! state/conversations/<id>/snapshot.json   the conversation as it stands
//! ```
//!
//! Not yet a journal: a snapshot is rewritten (atomically) after each
//! change, receipts say `durable: false`, and a daemon restart ends every
//! live run — its conversation records the run as lost (outcome unknown),
//! never as finished. The journal, replay and receipts that survive a
//! restart come with milestone 3.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use otter_core::conversation::{CONVERSATION_SCHEMA, Conversation, TurnOutcome};
use otter_core::{ConversationId, FeatureId};

/// Told about every saved change: (conversation, revision, feature).
pub type Notify = Box<dyn Fn(&ConversationId, u64, Option<&FeatureId>) + Send + Sync>;

pub struct Conversations {
    dir: PathBuf,
    map: Mutex<HashMap<ConversationId, Conversation>>,
    notify: std::sync::OnceLock<Notify>,
}

impl Conversations {
    /// Load every conversation under `dir`. One that can't be read (a newer
    /// schema, a broken file) is left on disk untouched and skipped. A run
    /// still marked live belonged to the daemon before this one: it is gone.
    pub fn load(dir: &Path) -> Result<Conversations> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let mut map = HashMap::new();
        let now = Utc::now();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path().join("snapshot.json");
            if !path.exists() {
                continue;
            }
            let read = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|b| Ok(serde_json::from_slice::<Conversation>(&b)?));
            match read {
                Ok(c) if c.schema > CONVERSATION_SCHEMA => {
                    tracing::warn!(conversation = %c.id, schema = c.schema, "skipping a conversation from a newer otterd");
                }
                Ok(mut c) => {
                    if c.active_run.is_some() {
                        c.run_ended(c.generation, TurnOutcome::OutcomeUnknown, now);
                        c.cancel_queued("not delivered: otterd restarted", now);
                    }
                    map.insert(c.id.clone(), c);
                }
                Err(e) => tracing::warn!("skipping {}: {e:#}", path.display()),
            }
        }
        let this = Conversations {
            dir: dir.to_path_buf(),
            map: Mutex::new(map),
            notify: std::sync::OnceLock::new(),
        };
        for c in this.map.lock().unwrap().values() {
            this.save(c);
        }
        Ok(this)
    }

    /// None loaded (the directory couldn't be read): new ones are still kept.
    pub fn empty(dir: &Path) -> Conversations {
        Conversations {
            dir: dir.to_path_buf(),
            map: Mutex::new(HashMap::new()),
            notify: std::sync::OnceLock::new(),
        }
    }

    /// Who to tell about changes (set once, by the daemon).
    pub fn on_change(&self, notify: Notify) {
        let _ = self.notify.set(notify);
    }

    fn changed(&self, id: &ConversationId, revision: u64, feature: Option<&FeatureId>) {
        if let Some(n) = self.notify.get() {
            n(id, revision, feature);
        }
    }

    pub fn insert(&self, c: Conversation) {
        self.save(&c);
        let (id, revision, feature) = (c.id.clone(), c.revision, c.feature_id.clone());
        self.map.lock().unwrap().insert(c.id.clone(), c);
        self.changed(&id, revision, feature.as_ref());
    }

    pub fn get(&self, id: &str) -> Option<Conversation> {
        self.map
            .lock()
            .unwrap()
            .get(&ConversationId::from(id))
            .cloned()
    }

    /// Newest activity first; only `feature`'s, if given.
    pub fn list(&self, feature: Option<&str>) -> Vec<Conversation> {
        let mut out: Vec<Conversation> = self
            .map
            .lock()
            .unwrap()
            .values()
            .filter(|c| {
                feature.is_none_or(|f| c.feature_id.as_ref().is_some_and(|x| x.as_str() == f))
            })
            .cloned()
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        out
    }

    /// Change a conversation; saved when its revision moved. `None` if
    /// there's no such conversation.
    pub fn update<R>(
        &self,
        id: &ConversationId,
        f: impl FnOnce(&mut Conversation) -> R,
    ) -> Option<R> {
        let mut map = self.map.lock().unwrap();
        let c = map.get_mut(id)?;
        let before = c.revision;
        let out = f(c);
        let changed = (c.revision != before).then(|| {
            self.save(c);
            (c.revision, c.feature_id.clone())
        });
        drop(map);
        if let Some((revision, feature)) = changed {
            self.changed(id, revision, feature.as_ref());
        }
        Some(out)
    }

    /// Forget a feature's conversations (when the feature is deleted).
    /// Only Otter's records go: the provider's own transcripts stay.
    pub fn remove_for_feature(&self, feature: &FeatureId) {
        let mut map = self.map.lock().unwrap();
        let ids: Vec<ConversationId> = map
            .values()
            .filter(|c| c.feature_id.as_ref() == Some(feature) && c.active_run.is_none())
            .map(|c| c.id.clone())
            .collect();
        for id in ids {
            map.remove(&id);
            let dir = self.dir.join(id.as_str());
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!("removing {}: {e}", dir.display());
            }
        }
    }

    /// Write the snapshot (atomically, this user only). Not durable yet: a
    /// failure is logged and the conversation goes on in memory.
    fn save(&self, c: &Conversation) {
        let write = || -> Result<()> {
            let dir = self.dir.join(c.id.as_str());
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
            let tmp = dir.join("snapshot.json.tmp");
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec(c)?)?;
            std::fs::rename(&tmp, dir.join("snapshot.json"))?;
            Ok(())
        };
        if let Err(e) = write() {
            tracing::warn!(conversation = %c.id, "saving the conversation: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::RunId;
    use otter_core::conversation::{Command, Delivery, Initiator, InputBlock, TurnState};

    #[test]
    fn conversations_are_kept_and_a_restart_ends_their_runs_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let mut c = Conversation::new("claude", "legacy_cli", "ws_1".into(), Utc::now());
        c.feature_id = Some("ft_1".into());
        let id = c.id.clone();
        convs.insert(c);
        let send = |cmd: &str| Command::SendTurn {
            initiator: Initiator::Controller,
            input: vec![InputBlock::Text { text: cmd.into() }],
        };
        let (sent, queued) = convs
            .update(&id, |c| {
                let a = c.accept("cmd_1", "a", send("a"), Utc::now()).unwrap();
                let b = c.accept("cmd_2", "b", send("b"), Utc::now()).unwrap();
                let g = c.claim(RunId::from("run_1"), Utc::now()).unwrap();
                let t = a.receipt().turn_id.clone().unwrap();
                c.sending(&t, g, Utc::now()).unwrap();
                (t, b.receipt().turn_id.clone().unwrap())
            })
            .unwrap();
        assert_eq!(convs.list(Some("ft_1")).len(), 1);
        assert!(convs.list(Some("ft_2")).is_empty());

        // The daemon goes away mid-turn.
        drop(convs);
        let again = Conversations::load(dir.path()).unwrap();
        let c = again.get(id.as_str()).unwrap();
        assert!(c.active_run.is_none());
        let t = c.turn(&sent).unwrap();
        assert_eq!(t.delivery, Delivery::Unknown, "never resent on its own");
        assert_eq!(t.outcome, Some(TurnOutcome::OutcomeUnknown));
        let q = c.turn(&queued).unwrap();
        assert_eq!(q.state, TurnState::Finished);
        assert_eq!(q.outcome, Some(TurnOutcome::Cancelled));

        again.remove_for_feature(&"ft_1".into());
        assert!(again.get(id.as_str()).is_none());
        assert!(!dir.path().join(id.as_str()).exists());
    }
}
