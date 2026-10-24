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
//! - **A task's, or the host's** (D-213). A watch lives in the conversation
//!   that asked ([`Home`]): a task's, or Otter's on the host
//!   (`OtterChat::watches`), whose turn runs where the host's follow-ups do.
//!   A look on the host may come back every so many minutes, until its end.

mod ci;
mod run;
mod time;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use otter_core::task::{Message, MessageRole, Task, TaskEvent};
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

/// Where a watch lives: the conversation that asked for it (D-213).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Home {
    Task(TaskId),
    Host,
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
    /// Resolve what the model asked against the task (none on the host),
    /// or say why not.
    fn target(&self, f: Option<&Task>, args: &Value, what: &str) -> Result<Target, String>;
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
static RUN: run::Run = run::Run;

/// The kinds this build knows.
pub(crate) fn kinds() -> [&'static dyn WatchKind; 3] {
    [&TIME, &CI, &RUN]
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

/// How many watches may wait at once where they live, and how that is said.
fn cap(home: &Home) -> (usize, &'static str) {
    match home {
        Home::Task(_) => (w::MAX_WAITING, "this task already has"),
        Home::Host => (w::MAX_HOST_WAITING, "Otter on this host already has"),
    }
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
    let home = Home::Task(f.id.clone());
    set_in(
        &mut f.watches,
        &home,
        kind,
        target,
        args,
        what,
        asked_in,
        first,
        now,
    )
}

/// [`set`], in the watches of `home`. A `time` look with `every_minutes`
/// comes back that often until `for_minutes` (60 unless said) have gone
/// (D-213).
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_in(
    watches: &mut Vec<Watch>,
    home: &Home,
    kind: &dyn WatchKind,
    target: Target,
    args: &Value,
    what: &str,
    asked_in: Option<MessageId>,
    first: Option<Reading>,
    now: Timestamp,
) -> Result<(Watch, Vec<Change>), String> {
    if let Some(w) = same_in(watches, kind.name(), &target.value) {
        return Ok((w.clone(), vec![]));
    }
    let minutes = check_in(watches, home, kind, args, what)?;
    let (every_minutes, until) = recurring(kind, args, now)?;
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
        every_minutes,
        until,
    };
    let change = Change::new(TaskEvent::WatchSet {
        watch_id: watch.id.clone(),
        label: watch.label.clone(),
    });
    watches.push(watch.clone());
    Ok((watch, vec![change]))
}

/// A look that comes back (D-213): every how many minutes, and until when.
/// Only `time` comes back; the rest fire once.
fn recurring(
    kind: &dyn WatchKind,
    args: &Value,
    now: Timestamp,
) -> Result<(Option<u32>, Option<Timestamp>), String> {
    let Some(every) = minutes_arg(args, "every_minutes")? else {
        return Ok((None, None));
    };
    if kind.name() != "time" {
        return Err("only a time look comes back every so many minutes".into());
    }
    let lasts = minutes_arg(args, "for_minutes")?.unwrap_or(60).max(every);
    Ok((Some(every), Some(now + minute() * lasts as i32)))
}

/// Whether a new watch of `kind` may be set on `f` for `what`: how many
/// minutes it reaches, or why not (for the developer).
pub(crate) fn check(
    f: &Task,
    kind: &dyn WatchKind,
    args: &Value,
    what: &str,
) -> Result<u32, String> {
    check_in(&f.watches, &Home::Task(f.id.clone()), kind, args, what)
}

/// [`check`], in the watches of `home`.
pub(crate) fn check_in(
    watches: &[Watch],
    home: &Home,
    kind: &dyn WatchKind,
    args: &Value,
    what: &str,
) -> Result<u32, String> {
    if what.trim().is_empty() {
        return Err("say what to watch for (what)".into());
    }
    let open: Vec<String> = watches
        .iter()
        .filter(|w| w.status.is_open())
        .map(|w| w.label.clone())
        .collect();
    let (most, who) = cap(home);
    if open.len() >= most {
        return Err(format!(
            "{who} {} watches open ({}): stop one first",
            open.len(),
            open.join("; ")
        ));
    }
    kind.minutes(args)
}

/// The open watch of `kind` on the same thing, if there is one.
pub(crate) fn same<'a>(f: &'a Task, kind: &str, target: &Value) -> Option<&'a Watch> {
    same_in(&f.watches, kind, target)
}

/// [`same`], among `watches`.
pub(crate) fn same_in<'a>(watches: &'a [Watch], kind: &str, target: &Value) -> Option<&'a Watch> {
    watches
        .iter()
        .find(|w| w.status.is_open() && w.kind == kind && &w.target == target)
}

/// Stop watch `id` (the developer, Otter, or the task cancelled).
pub(crate) fn cancel(
    f: &mut Task,
    id: &WatchId,
    by: &str,
    now: Timestamp,
) -> RpcResult<Vec<Change>> {
    cancel_in(&mut f.watches, "on this task", id, by, now)
}

/// [`cancel`], among `watches` (`where_` says where, for the developer).
pub(crate) fn cancel_in(
    watches: &mut Vec<Watch>,
    where_: &str,
    id: &WatchId,
    by: &str,
    now: Timestamp,
) -> RpcResult<Vec<Change>> {
    let w = watches
        .iter_mut()
        .find(|w| &w.id == id)
        .ok_or_else(|| RpcError::not_found(format!("no watch `{id}` {where_}")))?;
    if !w.cancel(now) {
        return Err(RpcError::conflict("that watch has already ended"));
    }
    let change = Change::new(TaskEvent::WatchCancelled {
        watch_id: id.clone(),
        label: w.label.clone(),
    })
    .because(by);
    w::prune(watches);
    Ok(vec![change])
}

/// Remove watch `id`; a waiting one is stopped first.
pub(crate) fn delete(
    f: &mut Task,
    id: &WatchId,
    by: &str,
    now: Timestamp,
) -> RpcResult<Vec<Change>> {
    delete_in(&mut f.watches, "on this task", id, by, now)
}

/// [`delete`], among `watches`.
pub(crate) fn delete_in(
    watches: &mut Vec<Watch>,
    where_: &str,
    id: &WatchId,
    by: &str,
    now: Timestamp,
) -> RpcResult<Vec<Change>> {
    let w = watches
        .iter()
        .find(|w| &w.id == id)
        .ok_or_else(|| RpcError::not_found(format!("no watch `{id}` {where_}")))?;
    let label = w.label.clone();
    let mut changes = Vec::new();
    if w.status.is_open() {
        changes.extend(cancel_in(watches, where_, id, by, now)?);
    }
    watches.retain(|w| &w.id != id);
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
    let Ok(target) = CI.target(Some(f), &Value::Null, what) else {
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
    } else if w.what.ends_with("when the task was accepted") {
        "Otter set itself, when the developer accepted the task before CI finished,"
    } else {
        // Set on Otter's own (D-193).
        "You chose"
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

    /// Every open watch, the tasks' and the host's (D-213), with its home.
    async fn all_open_watches(&self) -> Vec<(Home, Watch)> {
        let mut open: Vec<(Home, Watch)> = self
            .tasks
            .lock()
            .await
            .open_watches()
            .into_iter()
            .map(|(t, w)| (Home::Task(t), w))
            .collect();
        open.extend(
            self.chat
                .lock()
                .await
                .get()
                .open_watches()
                .map(|w| (Home::Host, w.clone())),
        );
        open
    }

    async fn watch_tick(self: &Arc<Self>) {
        let now = Utc::now();
        let open = self.all_open_watches().await;
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
            for (home, w) in open.iter().filter(|(_, w)| w.status == WatchStatus::Firing) {
                let line = match (kind(&w.kind), &w.finding) {
                    (Some(k), Some(f)) => k.line(w, f),
                    _ => format!("{}: it came to something.", capital(&w.label)),
                };
                self.record_watch_report(home, &w.id, line).await;
            }
        }
        let waiting: Vec<(Home, Watch)> = open
            .into_iter()
            .filter(|(_, w)| w.status == WatchStatus::Waiting)
            .collect();
        for (home, w) in waiting {
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
                    self.fire_watch(&home, &w.id, finding).await;
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
                self.fire_watch(&home, &w.id, finding).await;
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
                let probe = daemon.probe(kind, &home, &w, PROBE_LIMIT).await;
                daemon.after_probe(&home, &w, probe).await;
                let mut c = daemon.controller.lock().unwrap();
                c.watches.busy.remove(&w.id);
                c.watches
                    .next
                    .insert(w.id.clone(), Instant::now() + jitter(every));
            });
        }
    }

    /// One probe of `w`, where it lives as it is now, for at most `limit`.
    async fn probe(
        &self,
        kind: &'static dyn WatchKind,
        home: &Home,
        w: &Watch,
        limit: Duration,
    ) -> Probe {
        if let Home::Task(task) = home
            && self.task_get(task.as_str()).await.is_err()
        {
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

    /// Change the watches where `home` keeps them, and its messages, in one
    /// write: a task's document, or Otter's conversation on the host.
    async fn on_watches(
        &self,
        home: &Home,
        step: impl FnOnce(&mut Vec<Watch>, &mut Vec<Message>, Timestamp) -> RpcResult<Vec<Change>>,
    ) -> RpcResult<()> {
        match home {
            Home::Task(task) => {
                let applied = self.tasks.lock().await.apply(task, None, |f, now| {
                    step(&mut f.watches, &mut f.messages, now)
                })?;
                self.task_changed(&applied);
            }
            Home::Host => {
                let applied = self.chat.lock().await.apply(None, |chat, now| {
                    step(&mut chat.watches, &mut chat.messages, now)
                })?;
                self.chat_changed(&applied);
            }
        }
        Ok(())
    }

    async fn after_probe(&self, home: &Home, seen: &Watch, probe: Probe) {
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
                let kept = self
                    .on_watches(home, |watches, _, _| {
                        if let Some(w) = watches.iter_mut().find(|w| w.id == id)
                            && w.status == WatchStatus::Waiting
                        {
                            w.read(Reading { at: now, key, text });
                        }
                        Ok(vec![])
                    })
                    .await;
                if let Err(e) = kept {
                    tracing::warn!(home = ?home, "keeping a watch's reading: {}", e.message)
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
                self.fire_watch(home, &seen.id, finding).await;
            }
            Probe::Unreadable(why) => {
                let n = {
                    let mut c = self.controller.lock().unwrap();
                    let n = c.watches.failures.entry(seen.id.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                tracing::info!(home = ?home, watch = %seen.id, "a watch's probe: {why} ({n} in a row)");
                if n >= 3 {
                    let finding = Finding {
                        at: now,
                        outcome: Outcome::Unreadable,
                        text: format!("couldn't read it three times running: {why}"),
                    };
                    self.fire_watch(home, &seen.id, finding).await;
                }
            }
        }
    }

    /// One look at what `kind` would watch for `home`, before the watch is
    /// set (W-1): what the turn says is what was read now. `None`: the kind
    /// isn't probed (`time`).
    pub(crate) async fn first_look(
        &self,
        kind: &'static dyn WatchKind,
        home: &Home,
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
            every_minutes: None,
            until: None,
        };
        Some(self.probe(kind, home, &draft, FIRST_LOOK_LIMIT).await)
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
    async fn fire_watch(&self, home: &Home, id: &WatchId, finding: Finding) {
        let fired = self
            .on_watches(home, |watches, _, _| {
                let Some(w) = watches
                    .iter_mut()
                    .find(|w| &w.id == id && w.status == WatchStatus::Waiting)
                else {
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
            })
            .await;
        if let Err(e) = fired {
            tracing::warn!(home = ?home, "a watch's firing: {}", e.message);
        }
        let mut c = self.controller.lock().unwrap();
        c.watches.next.remove(id);
        c.watches.failures.remove(id);
        drop(c);
        crate::gateway::crash_point("watch", "firing");
    }

    /// Whether `w`'s turn ran less than a minute ago: one that couldn't be
    /// recorded is tried again a minute later, not on every tick. Marks it
    /// as running now otherwise.
    fn turned_lately(&self, w: &Watch) -> bool {
        let mut c = self.controller.lock().unwrap();
        if c.watches
            .turned
            .get(&w.id)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
        {
            return true;
        }
        c.watches.turned.insert(w.id.clone(), Instant::now());
        false
    }

    /// A firing watch's turn, from the task's step: Otter says what it
    /// found, once. Never an error: whatever happens, the watch ends.
    pub(crate) async fn watch_turn(self: &Arc<Self>, f: &Task, w: &Watch) {
        if self.turned_lately(w) {
            return;
        }
        let (finding, line) = said_plainly(w);
        // Otter at it while it looks (D-196); the report is said whole, so the
        // sign goes once it is written.
        let _writing = self.writing(&f.id, &format!("watch-{}", w.key()));
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
        self.reported_after_turn(&Home::Task(f.id.clone()), w, reply)
            .await;
    }

    /// A firing watch's turn in Otter's conversation on the host (D-213),
    /// where its follow-ups run: one at a time, after the developer's
    /// messages. As a task's, it never fails and never stays firing.
    pub(crate) async fn host_watch_turn(self: &Arc<Self>, w: &Watch) {
        if self.turned_lately(w) {
            return;
        }
        let (finding, line) = said_plainly(w);
        let reply = match self.turn_backend() {
            None => line,
            Some(backend) => {
                let (daemon, key, what) = (self.clone(), w.key(), event_text(w, &finding));
                let turn =
                    tokio::spawn(async move { daemon.host_event_turn(&*backend, key, what).await });
                match turn.await {
                    Ok(Some(text)) => text,
                    Ok(None) => line,
                    Err(_) => format!("{line} (I couldn't look further: my turn stopped.)"),
                }
            }
        };
        self.reported_after_turn(&Home::Host, w, reply).await;
    }

    async fn reported_after_turn(&self, home: &Home, w: &Watch, reply: String) {
        // A turn that took long and whose report can't be written is tried
        // again a minute after it ended, not at once.
        self.controller
            .lock()
            .unwrap()
            .watches
            .turned
            .insert(w.id.clone(), Instant::now());
        crate::gateway::crash_point("watch", "report");
        self.record_watch_report(home, &w.id, reply).await;
    }

    /// What `id` came to, said where it lives — with the watch reported in
    /// the same write, and only while it still fires. A recurring look
    /// then waits again for its next time, until its end (D-213).
    async fn record_watch_report(&self, home: &Home, id: &WatchId, reply: String) {
        let every = minute();
        let recorded = self
            .on_watches(home, |watches, messages, now| {
                let Some(w) = watches
                    .iter_mut()
                    .find(|w| &w.id == id && w.status == WatchStatus::Firing)
                else {
                    return Ok(vec![]);
                };
                let key = w.key();
                let (m, c) = message(MessageRole::Controller, reply, Some(key.clone()));
                w.reported(m.id.clone(), now);
                if let Some(n) = w.every_minutes {
                    w.again(now + every * n as i32);
                }
                let label = w.label.clone();
                messages.push(m);
                w::prune(watches);
                Ok(vec![
                    c,
                    Change::new(TaskEvent::WatchReported {
                        watch_id: id.clone(),
                        label,
                    })
                    .because(key),
                ])
            })
            .await;
        match recorded {
            Ok(()) => {
                self.controller.lock().unwrap().watches.turned.remove(id);
            }
            Err(e) => tracing::warn!(home = ?home, "saying what a watch found: {}", e.message),
        }
    }
}

/// What a firing watch found, and how it is said without a model.
fn said_plainly(w: &Watch) -> (Finding, String) {
    let finding = w.finding.clone().unwrap_or(Finding {
        at: Utc::now(),
        outcome: Outcome::Unknown,
        text: "it fired".into(),
    });
    let line = match kind(&w.kind) {
        Some(k) => k.line(w, &finding),
        None => format!("{}: {}.", capital(&w.label), finding.text),
    };
    (finding, line)
}
