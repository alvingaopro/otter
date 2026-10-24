//! Watches (D-183): something Otter looks at again later, for the
//! developer, in the conversation where they asked — until what they wait
//! for happens, or its deadline comes. A watch fires once.
//!
//! A watch is kept on its task (`Task::watches`). otterd probes it without
//! a model; only a change of what it saw (a reading), its firing and its
//! end are written. When to probe next lives in otterd's memory, so a
//! restart probes once at start.

use serde::{Deserialize, Serialize};

use crate::ids::{MessageId, WatchId};
use crate::model::Timestamp;

/// Waiting watches a task may have at once.
pub const MAX_WAITING: usize = 3;
/// Waiting watches Otter's conversation on the host may have at once
/// (D-213).
pub const MAX_HOST_WAITING: usize = 5;
/// Ended watches kept on a task; older ones go.
pub const KEPT_ENDED: usize = 20;
/// Readings kept on a watch; older ones go.
pub const MAX_READINGS: usize = 50;
/// The furthest a watch reaches, in minutes (7 days).
pub const MAX_MINUTES: u32 = 7 * 24 * 60;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatchStatus {
    /// Probed now and then; nothing yet.
    Waiting,
    /// Found what it waited for (or its deadline came): Otter says so next.
    Firing,
    /// Otter said what it found. Final.
    Reported,
    /// Stopped before it fired. Final.
    Cancelled,
    /// A status this build doesn't know: ended, as far as it can tell.
    #[serde(other)]
    Unknown,
}

impl WatchStatus {
    /// Waiting or firing: not ended.
    pub fn is_open(self) -> bool {
        matches!(self, WatchStatus::Waiting | WatchStatus::Firing)
    }
}

/// What a watch's firing found.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// What it waited for went well: CI passed.
    Passed,
    /// It went badly: a check failed.
    Failed,
    /// It can't happen as things are: the pull request conflicts, no CI
    /// started.
    Stuck,
    /// What it watched went away: the pull request was merged or closed.
    Gone,
    /// The time came (a `time` watch).
    Due,
    /// Its deadline passed with nothing.
    Expired,
    /// What it watches couldn't be read, three times running.
    Unreadable,
    /// An outcome this build doesn't know.
    #[serde(other)]
    Unknown,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::Stuck => "stuck",
            Outcome::Gone => "gone",
            Outcome::Due => "due",
            Outcome::Expired => "expired",
            Outcome::Unreadable => "unreadable",
            Outcome::Unknown => "unknown",
        }
    }
}

/// What a probe saw, kept when it differs from the last.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Reading {
    pub at: Timestamp,
    /// A short key, to tell a change.
    pub key: String,
    pub text: String,
}

/// What made a watch fire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    /// When it was read.
    pub at: Timestamp,
    pub outcome: Outcome,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Watch {
    pub id: WatchId,
    /// What kind of thing it watches (`time`, `ci`).
    pub kind: String,
    /// The kind's own: for `ci`, the pull request and its repository.
    #[serde(default)]
    pub target: serde_json::Value,
    /// What it watches, to show: “CI on #171”.
    pub label: String,
    /// What the developer asked for, as Otter put it.
    pub what: String,
    /// The developer's message that asked; absent when accepting a task
    /// before its CI finished set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_in: Option<MessageId>,
    pub created_at: Timestamp,
    /// When it gives up; for `time`, when it's due.
    pub deadline: Timestamp,
    pub status: WatchStatus,
    /// Each change of what it saw, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readings: Vec<Reading>,
    /// What its firing found, from the moment it fires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<Finding>,
    /// How many times it has fired (the `n` of its key).
    #[serde(default)]
    pub findings: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    /// Otter's message that reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<MessageId>,
    /// A look again every so many minutes (D-213): once reported, it waits
    /// again, until `until`. Absent: it fires once (D-183).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_minutes: Option<u32>,
    /// When a recurring look ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
}

impl Watch {
    /// What its turn and its report are tied to: `watch:<id>:<n>` — never
    /// a developer's message id, which would count as answering them.
    pub fn key(&self) -> String {
        format!("watch:{}:{}", self.id, self.findings)
    }

    /// The last thing a probe saw.
    pub fn last_reading(&self) -> Option<&Reading> {
        self.readings.last()
    }

    /// Keep `r` if it differs from the last reading. Whether it was kept.
    pub fn read(&mut self, r: Reading) -> bool {
        if self.last_reading().is_some_and(|l| l.key == r.key) {
            return false;
        }
        self.readings.push(r);
        let extra = self.readings.len().saturating_sub(MAX_READINGS);
        self.readings.drain(..extra);
        true
    }

    /// Start firing on `finding`. Only a waiting watch fires.
    pub fn fire(&mut self, finding: Finding) -> bool {
        if self.status != WatchStatus::Waiting {
            return false;
        }
        self.findings += 1;
        self.finding = Some(finding);
        self.status = WatchStatus::Firing;
        true
    }

    /// Stop it. Only an open watch stops.
    pub fn cancel(&mut self, now: Timestamp) -> bool {
        if !self.status.is_open() {
            return false;
        }
        self.status = WatchStatus::Cancelled;
        self.ended_at = Some(now);
        true
    }

    /// Reported by `message`. Only a firing watch is reported.
    pub fn reported(&mut self, message: MessageId, now: Timestamp) -> bool {
        if self.status != WatchStatus::Firing {
            return false;
        }
        self.status = WatchStatus::Reported;
        self.report = Some(message);
        self.ended_at = Some(now);
        true
    }

    /// A recurring look, just reported, waits again for `next` (D-213) —
    /// while that is before its end. Whether it does.
    pub fn again(&mut self, next: Timestamp) -> bool {
        let (Some(_), Some(until)) = (self.every_minutes, self.until) else {
            return false;
        };
        if self.status != WatchStatus::Reported || next > until {
            return false;
        }
        self.status = WatchStatus::Waiting;
        self.deadline = next;
        self.ended_at = None;
        true
    }
}

impl crate::task::OtterChat {
    pub fn watch(&self, id: &WatchId) -> Option<&Watch> {
        self.watches.iter().find(|w| &w.id == id)
    }

    /// Its watches still waiting or firing (D-213).
    pub fn open_watches(&self) -> impl Iterator<Item = &Watch> {
        self.watches.iter().filter(|w| w.status.is_open())
    }

    /// The watch Otter should report on next, if one fired.
    pub fn firing_watch(&self) -> Option<&Watch> {
        self.watches
            .iter()
            .find(|w| w.status == WatchStatus::Firing)
    }
}

impl crate::task::Task {
    pub fn watch(&self, id: &WatchId) -> Option<&Watch> {
        self.watches.iter().find(|w| &w.id == id)
    }

    pub fn watch_mut(&mut self, id: &WatchId) -> Option<&mut Watch> {
        self.watches.iter_mut().find(|w| &w.id == id)
    }

    /// Its watches still waiting or firing.
    pub fn open_watches(&self) -> impl Iterator<Item = &Watch> {
        self.watches.iter().filter(|w| w.status.is_open())
    }

    /// The watch Otter should report on next, if one fired.
    pub fn firing_watch(&self) -> Option<&Watch> {
        self.watches
            .iter()
            .find(|w| w.status == WatchStatus::Firing)
    }
}

/// Drop the oldest ended watches beyond [`KEPT_ENDED`].
pub fn prune(watches: &mut Vec<Watch>) {
    let ended = watches.iter().filter(|w| !w.status.is_open()).count();
    let mut extra = ended.saturating_sub(KEPT_ENDED);
    if extra == 0 {
        return;
    }
    let mut order: Vec<(Timestamp, WatchId)> = watches
        .iter()
        .filter(|w| !w.status.is_open())
        .map(|w| (w.ended_at.unwrap_or(w.created_at), w.id.clone()))
        .collect();
    order.sort();
    let gone: Vec<WatchId> = order.into_iter().take(extra).map(|(_, id)| id).collect();
    watches.retain(|w| {
        if extra > 0 && gone.contains(&w.id) {
            extra -= 1;
            false
        } else {
            true
        }
    });
}

/// `owner/name` of a GitHub pull request's URL
/// (`https://github.com/owner/name/pull/7`).
pub fn repo_of(pr_url: &str) -> Option<String> {
    let rest = pr_url.split("://").nth(1).unwrap_or(pr_url);
    let mut parts = rest.split('/');
    let _host = parts.next()?;
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next() == Some("pull") && !owner.is_empty() && !name.is_empty())
        .then(|| format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn watch(status: WatchStatus) -> Watch {
        let now = Utc::now();
        Watch {
            id: WatchId::generate(),
            kind: "ci".into(),
            target: serde_json::json!({"pr": 7}),
            label: "CI on #7".into(),
            what: "tell me when CI is done".into(),
            asked_in: None,
            created_at: now,
            deadline: now + Duration::hours(2),
            status,
            readings: vec![],
            finding: None,
            findings: 0,
            ended_at: None,
            report: None,
            every_minutes: None,
            until: None,
        }
    }

    /// A recurring look waits again after each report, until its end; one
    /// that fires once, or one cancelled, doesn't (D-213).
    #[test]
    fn a_recurring_look_waits_again_until_its_end() {
        let now = Utc::now();
        let found = || Finding {
            at: Utc::now(),
            outcome: Outcome::Due,
            text: "the time came".into(),
        };
        let mut w = watch(WatchStatus::Waiting);
        w.every_minutes = Some(1);
        w.until = Some(now + Duration::minutes(2));
        assert!(w.fire(found()));
        assert!(w.reported(MessageId::generate(), now));
        assert!(w.again(now + Duration::minutes(1)));
        assert_eq!(w.status, WatchStatus::Waiting);
        assert_eq!(w.ended_at, None);
        assert!(w.fire(found()));
        assert_eq!(w.key(), format!("watch:{}:2", w.id));
        assert!(w.reported(MessageId::generate(), now));
        assert!(!w.again(now + Duration::minutes(3)), "past its end");
        assert_eq!(w.status, WatchStatus::Reported);
        let mut once = watch(WatchStatus::Firing);
        assert!(once.reported(MessageId::generate(), now));
        assert!(!once.again(now + Duration::minutes(1)));
    }

    #[test]
    fn a_reading_is_kept_only_when_it_changed() {
        let mut w = watch(WatchStatus::Waiting);
        let r = |key: &str| Reading {
            at: Utc::now(),
            key: key.into(),
            text: key.into(),
        };
        assert!(w.read(r("pending:3")));
        assert!(!w.read(r("pending:3")));
        assert!(w.read(r("pending:1")));
        assert_eq!(w.readings.len(), 2);
        for i in 0..100 {
            w.read(r(&format!("k{i}")));
        }
        assert_eq!(w.readings.len(), MAX_READINGS);
        assert_eq!(w.readings.last().unwrap().key, "k99");
    }

    #[test]
    fn a_watch_fires_once_and_a_cancel_after_it_ends_does_nothing() {
        let mut w = watch(WatchStatus::Waiting);
        let found = Finding {
            at: Utc::now(),
            outcome: Outcome::Passed,
            text: "3 checks passed".into(),
        };
        assert!(w.fire(found.clone()));
        assert_eq!(w.key(), format!("watch:{}:1", w.id));
        assert!(!w.fire(found));
        assert!(w.reported(MessageId::generate(), Utc::now()));
        assert!(!w.cancel(Utc::now()));
        assert_eq!(w.status, WatchStatus::Reported);
        // Cancelled while firing: never reported.
        let mut w = watch(WatchStatus::Firing);
        assert!(w.cancel(Utc::now()));
        assert!(!w.reported(MessageId::generate(), Utc::now()));
    }

    #[test]
    fn only_the_oldest_ended_watches_go() {
        let mut all: Vec<Watch> = (0..25)
            .map(|i| {
                let mut w = watch(WatchStatus::Reported);
                w.ended_at = Some(Utc::now() + Duration::minutes(i));
                w.label = format!("w{i}");
                w
            })
            .collect();
        all.push(watch(WatchStatus::Waiting));
        prune(&mut all);
        assert_eq!(all.len(), KEPT_ENDED + 1);
        assert!(all.iter().any(|w| w.status == WatchStatus::Waiting));
        assert!(!all.iter().any(|w| w.label == "w4"));
        assert!(all.iter().any(|w| w.label == "w5"));
    }

    #[test]
    fn a_pull_requests_repository_comes_from_its_url() {
        assert_eq!(
            repo_of("https://github.com/alvingaopro/otter/pull/171").as_deref(),
            Some("alvingaopro/otter")
        );
        assert_eq!(repo_of("https://github.com/o/r/issues/3"), None);
        assert_eq!(repo_of("nonsense"), None);
    }

    #[test]
    fn an_outcome_from_a_newer_build_reads_as_unknown() {
        let o: Outcome = serde_json::from_str("\"merged_by_bot\"").unwrap();
        assert_eq!(o, Outcome::Unknown);
    }
}
