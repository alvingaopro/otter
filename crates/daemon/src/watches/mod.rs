//! Watches (D-183): what Otter looks at again later, for the developer, in
//! the task's conversation where they asked.
//!
//! - **One engine, many kinds.** Everything here is the same for every
//!   kind of thing watched; a kind ([`WatchKind`], one file each: `time.rs`,
//!   `ci.rs`) only says what it watches and how to read its state —
//!   deterministic, read-only, no model, nothing asked of the developer.
//! - **Only changes are written.** A watch's document changes when it's
//!   set, when what its probe saw changed (a reading), when it fires and
//!   when it ends. When to probe next and failures in a row live in memory
//!   ([`Probing`]): a restart probes once at start. Every write moves the
//!   task in the app's list and wakes every client — never once a minute.
//! - **One look before it's set** (W-1). The `watch` tool reads what it
//!   would wait for once, at once: the turn says what is true now, not an
//!   older gate; what has already happened needs no watch, and is said.
//! - **Firing before the turn.** What the probe found is written with the
//!   status `firing` before any model runs; a restart says it then, on what
//!   was found, instead of probing again for another answer.
//! - **The turn is the controller's.** `run_watches` only probes. A firing
//!   watch's turn runs in the task's step (one turn per task at a time,
//!   after the developer's messages): no message started it, so it may
//!   look and pause, nothing on the developer's word — not `watch`, so a
//!   watch never sets another.
//! - **It never fails the task, nor stays firing.** Its turn runs apart: no
//!   answer, an error or a panic is said as a plain line, with the watch
//!   marked reported in the same write; cancelled meanwhile, nothing is said.

mod ci;
mod time;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use otter_core::task::{MessageRole, Task, TaskEvent};
use otter_core::watch::{self as w, Finding, Outcome, Reading, Watch, WatchStatus};
use otter_core::{MessageId, TaskId, Timestamp, WatchId};
use otter_protocol::RpcError;
use serde_json::Value;

use crate::daemon::{Daemon, RpcResult};
use crate::env::EnvMap;
use crate::tasks::{Change, message};

/// What the engine keeps in memory, in the controller's state.
#[derive(Default)]
pub(crate) struct Probing {
    /// When each waiting watch is probed next (absent: now).
    next: HashMap<WatchId, Instant>,
    /// Probes in a row that couldn't read the state.
    failures: HashMap<WatchId, u32>,
    /// Probes in flight.
    busy: HashSet<WatchId>,
    /// When a firing watch's turn last ran: one that couldn't be recorded
    /// is tried again a minute later, not on every tick.
    turned: HashMap<WatchId, Instant>,
}

/// What a watch is on, as its kind resolved it.
#[derive(Clone)]
pub(crate) struct Target {
    /// The kind's own, kept on the watch.
    pub value: Value,
    /// What it watches, to show: “CI on #171”.
    pub label: String,
    /// What tells it apart from another of its kind on the task, for its
    /// command id (`pr7`).
    pub key: String,
}

/// What one probe saw.
pub(crate) enum Probe {
    /// Not yet: what it saw (a short key, to tell a change; and to say).
    NotYet { key: String, text: String },
    /// What it waited for: it fires.
    Found { outcome: Outcome, text: String },
    /// The state couldn't be read: why.
    Unreadable(String),
}

/// What a probe may use.
pub(crate) struct ProbeContext<'a> {
    /// Where commands run: Otter's own directory, not the workspace (put
    /// away once the task is done).
    pub dir: &'a Path,
    pub env: &'a EnvMap,
}

/// A kind of thing watched.
#[async_trait]
pub(crate) trait WatchKind: Send + Sync {
    fn name(&self) -> &'static str;
    /// Resolve what the model asked against the task, or say why not.
    fn target(&self, f: &Task, args: &Value, what: &str) -> Result<Target, String>;
    /// How many minutes from now its deadline is (for `time`: when it's due).
    fn minutes(&self, args: &Value) -> Result<u32, String>;
    /// How often it's probed; `None`: never — its deadline is the event.
    fn every(&self) -> Option<Duration>;
    /// What its deadline means.
    fn at_deadline(&self, w: &Watch) -> (Outcome, String);
    /// Read the state once.
    async fn probe(&self, w: &Watch, cx: &ProbeContext<'_>) -> Probe;
    /// What's said without Otter's tools.
    fn line(&self, w: &Watch, finding: &Finding) -> String;
}

static TIME: time::Time = time::Time;
static CI: ci::Ci = ci::Ci;

/// The kinds this build knows.
pub(crate) fn kinds() -> [&'static dyn WatchKind; 2] {
    [&TIME, &CI]
}

pub(crate) fn kind(name: &str) -> Option<&'static dyn WatchKind> {
    kinds().into_iter().find(|k| k.name() == name)
}

/// A whole number of minutes in `args[key]`, from 1 to 7 days.
pub(crate) fn minutes_arg(args: &Value, key: &str) -> Result<Option<u32>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=u64::from(w::MAX_MINUTES)).contains(n))
            .map(|n| Some(n as u32))
            .ok_or_else(|| format!("{key} is a number of minutes from 1 to {}", w::MAX_MINUTES)),
    }
}

pub(crate) fn env_ms(var: &str, default: u64) -> Duration {
    Duration::from_millis(
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

/// A minute, as watches count them (`OTTER_WATCH_MINUTE_MS`: tests only).
fn minute() -> chrono::Duration {
    chrono::Duration::from_std(env_ms("OTTER_WATCH_MINUTE_MS", 60_000))
        .unwrap_or(chrono::Duration::minutes(1))
}

/// How often due watches are looked for (`OTTER_WATCH_TICK_MS`, 15 s).
fn tick() -> Duration {
    env_ms("OTTER_WATCH_TICK_MS", 15_000)
}

/// How long a probe may take before it counts as one that failed.
const PROBE_LIMIT: Duration = Duration::from_secs(150);
/// How long a watch's first look may take: it's inside Otter's turn.
const FIRST_LOOK_LIMIT: Duration = Duration::from_secs(20);

/// `d`, give or take a tenth, so watches set together don't probe together.
pub(crate) fn jitter(d: Duration) -> Duration {
    use rand::Rng;
    let tenth = d.as_millis() as u64 / 10;
    if tenth == 0 {
        return d;
    }
    let off = rand::rng().random_range(0..=2 * tenth);
    Duration::from_millis(d.as_millis() as u64 - tenth + off)
}

/// Set a watch on `f` (inside a store step): `kind` on what `args` name,
/// for `what` the developer asked, with what a first look read, if one did.
/// The same thing watched already is that watch, with no changes. `Err`:
/// why not, for the developer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn set(
    f: &mut Task,
    kind: &dyn WatchKind,
    target: Target,
    args: &Value,
    what: &str,
    asked_in: Option<MessageId>,
    first: Option<Reading>,
    now: Timestamp,
) -> Result<(Watch, Vec<Change>), String> {
    if let Some(w) = same(f, kind.name(), &target.value) {
        return Ok((w.clone(), vec![]));
    }
    let minutes = check(f, kind, args, what)?;
    let what = what.trim();
    let watch = Watch {
        id: WatchId::generate(),
        kind: kind.name().into(),
        target: target.value,
        label: target.label,
        what: crate::agents::excerpt(what, 500),
        asked_in,
        created_at: now,
        deadline: now + minute() * minutes as i32,
        status: WatchStatus::Waiting,
        readings: first.into_iter().collect(),
        finding: None,
        findings: 0,
        ended_at: None,
        report: None,
    };
    let change = Change::new(TaskEvent::WatchSet {
        watch_id: watch.id.clone(),
        label: watch.label.clone(),
    });
    f.watches.push(watch.clone());
    Ok((watch, vec![change]))
}

/// Whether a new watch of `kind` may be set on `f` for `what`: how many
/// minutes it reaches, or why not (for the developer).
pub(crate) fn check(
    f: &Task,
    kind: &dyn WatchKind,
    args: &Value,
    what: &str,
) -> Result<u32, String> {
    if what.trim().is_empty() {
        return Err("say what to watch for (what)".into());
    }
    let open: Vec<String> = f.open_watches().map(|w| w.label.clone()).collect();
    if open.len() >= w::MAX_WAITING {
        return Err(format!(
            "this task already has {} watches open ({}): stop one first",
            open.len(),
            open.join("; ")
        ));
    }
    kind.minutes(args)
}

/// The open watch of `kind` on the same thing, if there is one.
pub(crate) fn same<'a>(f: &'a Task, kind: &str, target: &Value) -> Option<&'a Watch> {
    f.open_watches()
        .find(|w| w.kind == kind && &w.target == target)
}

/// Stop watch `id` (the developer, Otter, or the task cancelled).
pub(crate) fn cancel(
    f: &mut Task,
    id: &WatchId,
    by: &str,
    now: Timestamp,
) -> RpcResult<Vec<Change>> {
    let w = f
        .watch_mut(id)
        .ok_or_else(|| RpcError::not_found(format!("no watch `{id}` on this task")))?;
    if !w.cancel(now) {
        return Err(RpcError::conflict("that watch has already ended"));
    }
    let change = Change::new(TaskEvent::WatchCancelled {
        watch_id: id.clone(),
        label: w.label.clone(),
    })
    .because(by);
    w::prune(&mut f.watches);
    Ok(vec![change])
}

/// Remove watch `id`; a waiting one is stopped first.
pub(crate) fn delete(
    f: &mut Task,
    id: &WatchId,
    by: &str,
    now: Timestamp,
) -> RpcResult<Vec<Change>> {
    let w = f
        .watch(id)
        .ok_or_else(|| RpcError::not_found(format!("no watch `{id}` on this task")))?;
    let label = w.label.clone();
    let mut changes = Vec::new();
    if w.status.is_open() {
        changes.extend(cancel(f, id, by, now)?);
    }
    f.watches.retain(|w| &w.id != id);
    changes.push(
        Change::new(TaskEvent::WatchDeleted {
            watch_id: id.clone(),
            label,
        })
        .because(by),
    );
    Ok(changes)
}

/// Stop every open watch of a task that was cancelled.
pub(crate) fn cancel_all(f: &mut Task, by: &str, now: Timestamp) -> Vec<Change> {
    let open: Vec<WatchId> = f.open_watches().map(|w| w.id.clone()).collect();
    open.iter()
        .filter_map(|id| cancel(f, id, by, now).ok())
        .flatten()
        .collect()
}

/// Accepted before CI finished (on the developer's *Accept anyway*): CI is
/// watched by itself, and Otter says so (D-183). Nothing when CI isn't
/// waited on, there's no pull request, or it's watched already.
pub(crate) fn on_accept(f: &mut Task, now: Timestamp) -> Vec<Change> {
    let ci_waits = f
        .gates
        .iter()
        .any(|g| g.name == "CI" && g.status == otter_core::task::GateStatus::Pending);
    if !ci_waits {
        return vec![];
    }
    let what = "say what CI comes to: it hadn't finished when the task was accepted";
    let Ok(target) = CI.target(f, &Value::Null, what) else {
        return vec![];
    };
    let label = target.label.clone();
    let Ok((watch, mut changes)) = set(f, &CI, target, &Value::Null, what, None, None, now) else {
        return vec![];
    };
    // Watched already: nothing more to say.
    if changes.is_empty() {
        return vec![];
    }
    let hours = (watch.deadline - now).num_hours().max(1);
    let (m, c) = message(
        MessageRole::Controller,
        format!(
            "{} hadn't finished when you accepted. I'll keep an eye on it for up to {hours} h and say here what it comes to — tell me to stop if you don't need that.",
            capital(&label)
        ),
        Some(watch.key()),
    );
    f.messages.push(m);
    changes.push(c);
    changes
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// What a watch's turn is told happened.
fn event_text(w: &Watch, finding: &Finding) -> String {
    let asked = if w.asked_in.is_some() {
        "The developer asked you"
    } else {
        "Otter set itself, when the developer accepted the task before CI finished,"
    };
    format!(
        "{asked} to watch {} (“{}”). It came to: {} — {}, read at {} UTC",
        w.label,
        w.what,
        finding.outcome.as_str(),
        finding.text,
        finding.at.format("%H:%M")
    )
}

impl Daemon {
    /// Probe due watches until the daemon stops.
    pub(crate) async fn run_watches(self: Arc<Self>) {
        let mut shutdown = self.shutdown_signal();
        loop {
            self.watch_tick().await;
            tokio::select! {
                _ = tokio::time::sleep(tick()) => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    async fn watch_tick(self: &Arc<Self>) {
        let now = Utc::now();
        let open = self.tasks.lock().await.open_watches();
        // What memory keeps is only for watches still open.
        {
            let ids: HashSet<&WatchId> = open.iter().map(|(_, w)| &w.id).collect();
            let mut c = self.controller.lock().unwrap();
            let p = &mut c.watches;
            p.next.retain(|id, _| ids.contains(id));
            p.failures.retain(|id, _| ids.contains(id));
            p.turned.retain(|id, _| ids.contains(id));
        }
        // With the controller off no turn says what a watch found: a plain
        // line does, so a firing watch still ends (D-183).
        if self.brain_choice().controller.as_deref() == Some("off") {
            for (task, w) in open.iter().filter(|(_, w)| w.status == WatchStatus::Firing) {
                let line = match (kind(&w.kind), &w.finding) {
                    (Some(k), Some(f)) => k.line(w, f),
                    _ => format!("{}: it came to something.", capital(&w.label)),
                };
                self.record_watch_report(task, &w.id, line).await;
            }
        }
        let waiting: Vec<(TaskId, Watch)> = open
            .into_iter()
            .filter(|(_, w)| w.status == WatchStatus::Waiting)
            .collect();
        for (task, w) in waiting {
            // A probe in flight answers first; its deadline waits a tick.
            if self.controller.lock().unwrap().watches.busy.contains(&w.id) {
                continue;
            }
            let Some(kind) = kind(&w.kind) else {
                // A kind from a newer build: it ends at its deadline.
                if now >= w.deadline {
                    let finding = Finding {
                        at: now,
                        outcome: Outcome::Expired,
                        text: "its deadline passed".into(),
                    };
                    self.fire_watch(&task, &w.id, finding).await;
                }
                continue;
            };
            if now >= w.deadline {
                let (outcome, text) = kind.at_deadline(&w);
                let finding = Finding {
                    at: now,
                    outcome,
                    text,
                };
                self.fire_watch(&task, &w.id, finding).await;
                continue;
            }
            let Some(every) = kind.every() else {
                continue;
            };
            {
                let mut c = self.controller.lock().unwrap();
                let p = &mut c.watches;
                if p.busy.contains(&w.id) || p.next.get(&w.id).is_some_and(|t| *t > Instant::now())
                {
                    continue;
                }
                p.busy.insert(w.id.clone());
            }
            let daemon = self.clone();
            tokio::spawn(async move {
                let probe = daemon.probe(kind, &task, &w, PROBE_LIMIT).await;
                daemon.after_probe(&task, &w, probe).await;
                let mut c = daemon.controller.lock().unwrap();
                c.watches.busy.remove(&w.id);
                c.watches
                    .next
                    .insert(w.id.clone(), Instant::now() + jitter(every));
            });
        }
    }

    /// One probe of `w`, on its task as it is now, for at most `limit`.
    async fn probe(
        &self,
        kind: &'static dyn WatchKind,
        task: &TaskId,
        w: &Watch,
        limit: Duration,
    ) -> Probe {
        if self.task_get(task.as_str()).await.is_err() {
            return Probe::Unreadable("the task is gone".into());
        }
        let env = self.git_account.gh_env(self.environments.base());
        let dir = self.otter_dir();
        let cx = ProbeContext {
            dir: &dir,
            env: &env,
        };
        // A probe that hangs is a probe that failed (`gh` has its own limit).
        match tokio::time::timeout(limit, kind.probe(w, &cx)).await {
            Ok(p) => p,
            Err(_) => Probe::Unreadable("reading it took too long".into()),
        }
    }

    async fn after_probe(&self, task: &TaskId, seen: &Watch, probe: Probe) {
        let now = Utc::now();
        match probe {
            Probe::NotYet { key, text } => {
                self.controller
                    .lock()
                    .unwrap()
                    .watches
                    .failures
                    .remove(&seen.id);
                if seen.last_reading().is_some_and(|r| r.key == key) {
                    return;
                }
                let id = seen.id.clone();
                let applied = self.tasks.lock().await.apply(task, None, |f, _| {
                    if let Some(w) = f.watch_mut(&id)
                        && w.status == WatchStatus::Waiting
                    {
                        w.read(Reading { at: now, key, text });
                    }
                    Ok(vec![])
                });
                match applied {
                    Ok(a) => self.task_changed(&a),
                    Err(e) => {
                        tracing::warn!(task = %task, "keeping a watch's reading: {}", e.message)
                    }
                }
            }
            Probe::Found { outcome, text } => {
                self.controller
                    .lock()
                    .unwrap()
                    .watches
                    .failures
                    .remove(&seen.id);
                let finding = Finding {
                    at: now,
                    outcome,
                    text,
                };
                self.fire_watch(task, &seen.id, finding).await;
            }
            Probe::Unreadable(why) => {
                let n = {
                    let mut c = self.controller.lock().unwrap();
                    let n = c.watches.failures.entry(seen.id.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                tracing::info!(task = %task, watch = %seen.id, "a watch's probe: {why} ({n} in a row)");
                if n >= 3 {
                    let finding = Finding {
                        at: now,
                        outcome: Outcome::Unreadable,
                        text: format!("couldn't read it three times running: {why}"),
                    };
                    self.fire_watch(task, &seen.id, finding).await;
                }
            }
        }
    }

    /// One look at what `kind` would watch on `f`, before the watch is set
    /// (W-1): what the turn says is what was read now. `None`: the kind
    /// isn't probed (`time`).
    pub(crate) async fn first_look(
        &self,
        kind: &'static dyn WatchKind,
        f: &Task,
        target: &Target,
    ) -> Option<Probe> {
        kind.every()?;
        let now = Utc::now();
        let draft = Watch {
            id: WatchId::generate(),
            kind: kind.name().into(),
            target: target.value.clone(),
            label: target.label.clone(),
            what: String::new(),
            asked_in: None,
            created_at: now,
            deadline: now,
            status: WatchStatus::Waiting,
            readings: vec![],
            finding: None,
            findings: 0,
            ended_at: None,
            report: None,
        };
        Some(self.probe(kind, &f.id, &draft, FIRST_LOOK_LIMIT).await)
    }

    /// `id` was just looked at (its first look): probed next a while from now.
    pub(crate) fn looked(&self, id: &WatchId, kind: &dyn WatchKind) {
        if let Some(every) = kind.every() {
            self.controller
                .lock()
                .unwrap()
                .watches
                .next
                .insert(id.clone(), Instant::now() + jitter(every));
        }
    }

    /// `id` found `finding`: written as firing, before any turn says so.
    async fn fire_watch(&self, task: &TaskId, id: &WatchId, finding: Finding) {
        let applied = self.tasks.lock().await.apply(task, None, |f, _| {
            let Some(w) = f.watch_mut(id).filter(|w| w.status == WatchStatus::Waiting) else {
                return Ok(vec![]);
            };
            // Its last reading is what it found.
            w.read(Reading {
                at: finding.at,
                key: format!("{}:{}", finding.outcome.as_str(), finding.text),
                text: finding.text.clone(),
            });
            let outcome = finding.outcome;
            if !w.fire(finding) {
                return Ok(vec![]);
            }
            Ok(vec![
                Change::new(TaskEvent::WatchFiring {
                    watch_id: id.clone(),
                    label: w.label.clone(),
                    outcome,
                })
                .because(w.key()),
            ])
        });
        match applied {
            Ok(a) => self.task_changed(&a),
            Err(e) => tracing::warn!(task = %task, "a watch's firing: {}", e.message),
        }
        let mut c = self.controller.lock().unwrap();
        c.watches.next.remove(id);
        c.watches.failures.remove(id);
        drop(c);
        crate::gateway::crash_point("watch", "firing");
    }

    /// A firing watch's turn, from the task's step: Otter says what it
    /// found, once. Never an error: whatever happens, the watch ends.
    pub(crate) async fn watch_turn(self: &Arc<Self>, f: &Task, w: &Watch) {
        {
            let mut c = self.controller.lock().unwrap();
            if c.watches
                .turned
                .get(&w.id)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
            {
                return;
            }
            c.watches.turned.insert(w.id.clone(), Instant::now());
        }
        let finding = w.finding.clone().unwrap_or(Finding {
            at: Utc::now(),
            outcome: Outcome::Unknown,
            text: "it fired".into(),
        });
        let line = match kind(&w.kind) {
            Some(k) => k.line(w, &finding),
            None => format!("{}: {}.", capital(&w.label), finding.text),
        };
        let reply = match self.turn_backend() {
            None => line,
            Some(backend) => {
                let (daemon, f, key, what) =
                    (self.clone(), f.clone(), w.key(), event_text(w, &finding));
                // Apart: a panic in the turn is said, not the task's failure.
                let turn =
                    tokio::spawn(async move { daemon.event_turn(&*backend, &f, key, what).await });
                match turn.await {
                    Ok(Some(text)) => text,
                    Ok(None) => line,
                    Err(_) => format!("{line} (I couldn't look further: my turn stopped.)"),
                }
            }
        };
        // A turn that took long and whose report can't be written is tried
        // again a minute after it ended, not at once.
        self.controller
            .lock()
            .unwrap()
            .watches
            .turned
            .insert(w.id.clone(), Instant::now());
        crate::gateway::crash_point("watch", "report");
        self.record_watch_report(&f.id, &w.id, reply).await;
    }

    /// What `id` came to, said in its task's conversation — with the watch
    /// reported in the same write, and only while it still fires.
    async fn record_watch_report(&self, task: &TaskId, id: &WatchId, reply: String) {
        let applied = self.tasks.lock().await.apply(task, None, |f, now| {
            let Some(w) = f.watch_mut(id).filter(|w| w.status == WatchStatus::Firing) else {
                return Ok(vec![]);
            };
            let key = w.key();
            let (m, c) = message(MessageRole::Controller, reply, Some(key.clone()));
            w.reported(m.id.clone(), now);
            let label = w.label.clone();
            f.messages.push(m);
            w::prune(&mut f.watches);
            Ok(vec![
                c,
                Change::new(TaskEvent::WatchReported {
                    watch_id: id.clone(),
                    label,
                })
                .because(key),
            ])
        });
        match applied {
            Ok(a) => {
                self.task_changed(&a);
                self.controller.lock().unwrap().watches.turned.remove(id);
            }
            Err(e) => tracing::warn!(task = %task, "saying what a watch found: {}", e.message),
        }
    }
}
