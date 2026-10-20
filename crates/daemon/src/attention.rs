//! Human attention (design §21): which work needs the developer right now.
//!
//! Attention items are raised from state transitions — an agent finishing a
//! turn or waiting on an approval or a question, a command or service failing, preparation
//! failing — and from a blocked agent moving on to another prompt. They are
//! resolved when the situation changes (the agent starts working again), when
//! the developer answers (sends to the session, restarts, stops it), or
//! explicitly. Looking at a session (attaching) resolves only what looking
//! settles — a finished turn, a completed command (D-099). So does a terminal
//! coming on screen, and while one is, nothing of that kind is raised for its
//! session (D-137). Each session has at most one open item; a newer one
//! replaces it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use otter_core::{
    AgentState, Attention, AttentionId, AttentionKind, ExecutionState, Session, SessionId,
    SessionKind, Workspace, WorkspaceState,
};
use otter_protocol::{AttentionResolve, Event, RpcError};

use crate::agents::Blocker;
use crate::daemon::{Daemon, RpcResult, save, workspace_not_found};

/// Raise an item, replacing the session's (or workspace's) open one. Returns
/// the events to emit.
pub fn raise(
    ws: &mut Workspace,
    session_id: Option<&SessionId>,
    kind: AttentionKind,
    summary: String,
) -> Vec<Event> {
    raise_item(ws, session_id, kind, summary, false)
}

/// [`raise`], saying whether it is a guess (D-109).
pub fn raise_item(
    ws: &mut Workspace,
    session_id: Option<&SessionId>,
    kind: AttentionKind,
    summary: String,
    inferred: bool,
) -> Vec<Event> {
    // An archived workspace asks for nothing (D-036).
    if ws.state == WorkspaceState::Archived {
        return Vec::new();
    }
    let mut events = resolve(ws, |a| a.session_id.as_ref() == session_id);
    let item = Attention {
        id: AttentionId::generate(),
        workspace_id: ws.id.clone(),
        session_id: session_id.cloned(),
        kind,
        summary,
        created_at: Utc::now(),
        inferred,
    };
    events.push(Event::AttentionCreated {
        workspace_id: ws.id.clone(),
        session_id: item.session_id.clone(),
        attention_id: item.id.clone(),
        kind,
    });
    ws.attention.push(item);
    events
}

/// Resolve every open item matching `pred`.
pub fn resolve(ws: &mut Workspace, pred: impl Fn(&Attention) -> bool) -> Vec<Event> {
    let mut events = Vec::new();
    ws.attention.retain(|a| {
        if pred(a) {
            events.push(Event::AttentionResolved {
                workspace_id: a.workspace_id.clone(),
                attention_id: a.id.clone(),
            });
            false
        } else {
            true
        }
    });
    events
}

pub fn resolve_session(ws: &mut Workspace, session_id: &SessionId) -> Vec<Event> {
    resolve(ws, |a| a.session_id.as_ref() == Some(session_id))
}

/// Attention for a session whose current execution just ended; `on_screen`:
/// the developer is looking at it (D-137).
pub fn on_execution_ended(ws: &mut Workspace, session: &Session, on_screen: bool) -> Vec<Event> {
    let Some(exec) = session.current_execution() else {
        return Vec::new();
    };
    let name = &session.name;
    let (kind, summary) = match (session.kind, exec.state, exec.exit_code) {
        // Leaving a shell or quitting an agent cleanly is the user's own doing.
        (SessionKind::Terminal, _, _) | (SessionKind::Agent, ExecutionState::Exited, Some(0)) => {
            return resolve_session(ws, &session.id);
        }
        (SessionKind::Command, ExecutionState::Exited, Some(0)) => {
            (AttentionKind::Completion, format!("{name} completed"))
        }
        (_, ExecutionState::Lost, _) => (
            AttentionKind::Failure,
            format!("{name} disappeared unexpectedly"),
        ),
        (SessionKind::Service, _, code) => (
            AttentionKind::Failure,
            match code {
                Some(c) => format!("{name} stopped (status {c})"),
                None => format!("{name} stopped"),
            },
        ),
        (_, _, Some(c)) => (
            AttentionKind::Failure,
            format!("{name} failed (status {c})"),
        ),
        (_, _, None) => (AttentionKind::Failure, format!("{name} exited")),
    };
    // Why, without its terminal: the last line it printed (D-076).
    let summary = match (kind, exec.last_line()) {
        (AttentionKind::Failure, Some(line)) => {
            format!("{summary}: {}", crate::agents::excerpt(line.trim(), 120))
        }
        _ => summary,
    };
    raise_unless_seen(ws, &session.id, kind, summary, on_screen)
}

/// [`raise`], unless the developer is looking at the session and seeing it
/// is all the item asks (D-137): then it only replaces what was open.
fn raise_unless_seen(
    ws: &mut Workspace,
    session_id: &SessionId,
    kind: AttentionKind,
    summary: String,
    on_screen: bool,
) -> Vec<Event> {
    if on_screen && settled_by_looking(kind) {
        return resolve_session(ws, session_id);
    }
    raise(ws, Some(session_id), kind, summary)
}

/// The kind and summary of a blocked agent's item: what it said it waits for,
/// or a guess from a still turn (agents::settle).
pub fn blocked_item(name: &str, blocker: Option<&Blocker>) -> (AttentionKind, String) {
    match blocker {
        Some(b) => (
            b.kind,
            match (b.kind, &b.detail) {
                (AttentionKind::Question, Some(q)) => format!("{name} asks: {q}"),
                (AttentionKind::Question, None) => format!("{name} has a question"),
                (_, Some(what)) => format!("{name} needs your approval: {what}"),
                (_, None) => format!("{name} needs your approval"),
            },
        ),
        None => (
            AttentionKind::Approval,
            format!("{name} seems to be waiting for you (approval or question?)"),
        ),
    }
}

/// Attention for an agent state transition; `on_screen`: the developer is
/// looking at the session (D-137).
pub fn on_agent_state(
    ws: &mut Workspace,
    session_id: &SessionId,
    blocker: Option<&Blocker>,
    name: &str,
    state: AgentState,
    last_message: Option<&str>,
    on_screen: bool,
) -> Vec<Event> {
    match state {
        AgentState::WaitingForInput => {
            let summary = match last_message {
                Some(m) => format!("{name} finished: {m}"),
                None => format!("{name} finished its turn"),
            };
            raise_unless_seen(ws, session_id, AttentionKind::Review, summary, on_screen)
        }
        AgentState::Blocked => {
            let (kind, summary) = blocked_item(name, blocker);
            raise_item(ws, Some(session_id), kind, summary, blocker.is_none())
        }
        // Back at work, or the prompt was declined / the turn interrupted.
        AgentState::Working | AgentState::Idle => resolve_session(ws, session_id),
        AgentState::Starting | AgentState::Exited => Vec::new(),
    }
}

/// How the developer engaged with a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Engaged {
    /// Attached, or its terminal came on screen (D-137): they saw it. That
    /// settles what was only to be seen.
    Looked,
    /// Sent it text: whatever it asked, they answered.
    Answered,
}

/// Seeing it is all it asks (D-099): a finished turn, a completed command.
/// An approval, a question or a failure waits for an answer — the agent
/// moving on, the developer typing, restarting, or marking it handled.
pub fn settled_by_looking(kind: AttentionKind) -> bool {
    matches!(kind, AttentionKind::Review | AttentionKind::Completion)
}

impl Daemon {
    pub(crate) async fn attention_resolve(&self, p: &AttentionResolve) -> RpcResult<usize> {
        let mut store = self.store.lock().await;
        let ws = store
            .workspace_mut(&p.workspace)
            .ok_or_else(|| workspace_not_found(&p.workspace))?;
        let session_id = match &p.session {
            Some(s) => Some(
                ws.session(s)
                    .map(|s| s.id.clone())
                    .ok_or_else(|| RpcError::not_found(format!("no session `{s}`")))?,
            ),
            None => None,
        };
        let events = resolve(ws, |a| {
            session_id
                .as_ref()
                .is_none_or(|s| a.session_id.as_ref() == Some(s))
                && p.attention.as_ref().is_none_or(|id| a.id.as_str() == id)
        });
        let n = events.len();
        save(&store)?;
        drop(store);
        self.emit_all(events);
        Ok(n)
    }

    /// The developer engaged with a session: looked at it (attached) or
    /// answered it (sent to it). See [`Engaged`].
    pub(crate) async fn engage(
        self: &Arc<Self>,
        ws_id: &otter_core::WorkspaceId,
        session_id: &SessionId,
        how: Engaged,
    ) {
        let mut store = self.store.lock().await;
        let Some(ws) = store.workspace_mut(ws_id.as_str()) else {
            return;
        };
        let events = match how {
            Engaged::Answered => resolve_session(ws, session_id),
            Engaged::Looked => resolve(ws, |a| {
                a.session_id.as_ref() == Some(session_id) && settled_by_looking(a.kind)
            }),
        };
        if events.is_empty() {
            return;
        }
        let _ = save(&store);
        drop(store);
        self.emit_all(events);
    }

    pub(crate) fn emit_all(&self, events: Vec<Event>) {
        for e in events {
            self.events.emit(e);
        }
    }

    /// The sessions a client shows on screen now (D-137).
    pub(crate) fn on_screen(&self) -> HashSet<SessionId> {
        self.on_screen.lock().unwrap().sessions(Instant::now())
    }
}

/// How long a terminal counts as on screen after its client last said so
/// (D-137). The app says it again every 30 s; a client gone without a word
/// (a Mac asleep, its connection not yet failed) stops counting after this.
const ON_SCREEN_FOR: Duration = Duration::from_secs(90);

/// The terminals on screen (D-137): each one's session, and when its client
/// last said it's on screen.
#[derive(Default)]
pub(crate) struct Screens(HashMap<u64, (SessionId, Instant)>);

impl Screens {
    /// Terminal `id` shows `session` on screen (`Some`), or is off screen or
    /// gone (`None`). True if it just came on screen; said again, it only
    /// counts for longer.
    fn set(&mut self, id: u64, session: Option<&SessionId>, now: Instant) -> bool {
        match session {
            Some(s) => self.0.insert(id, (s.clone(), now)).is_none(),
            None => {
                self.0.remove(&id);
                false
            }
        }
    }

    /// The sessions on screen as of `now`.
    fn sessions(&self, now: Instant) -> HashSet<SessionId> {
        self.0
            .values()
            .filter(|(_, said)| now.saturating_duration_since(*said) < ON_SCREEN_FOR)
            .map(|(s, _)| s.clone())
            .collect()
    }
}

/// A terminal showing a session, from a client that says whether it is on
/// screen (D-119). While it is, the developer is looking at the session
/// (D-137). It starts on screen, as a stream does; dropped, it's gone.
pub(crate) struct OnScreen {
    daemon: Arc<Daemon>,
    id: u64,
    session_id: SessionId,
}

impl OnScreen {
    pub(crate) fn new(daemon: Arc<Daemon>, session_id: SessionId) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let on = OnScreen {
            daemon,
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            session_id,
        };
        on.show(true);
        on
    }

    /// The client says whether it's on screen; true if it just came on. Said
    /// again, it only counts for longer: a mark that lapsed comes back, but
    /// settles nothing, as a timer isn't the developer looking.
    pub(crate) fn show(&self, shown: bool) -> bool {
        let session = shown.then_some(&self.session_id);
        let mut screens = self.daemon.on_screen.lock().unwrap();
        screens.set(self.id, session, Instant::now())
    }
}

impl Drop for OnScreen {
    fn drop(&mut self) {
        self.show(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_counts_as_on_screen_while_its_client_says_so() {
        let mut screens = Screens::default();
        let (a, b) = (SessionId::generate(), SessionId::generate());
        let t = Instant::now();
        let secs = |n| t + Duration::from_secs(n);
        assert!(screens.set(1, Some(&a), t), "came on screen");
        assert!(screens.set(2, Some(&a), t), "a second terminal on it");
        assert!(screens.set(3, Some(&b), t));
        // Off screen or gone, one terminal; the other still shows `a`.
        assert!(!screens.set(1, None, t));
        assert!(!screens.set(3, None, t));
        assert_eq!(screens.sessions(secs(1)), HashSet::from([a.clone()]));
        // Not said again: it lapses.
        assert_eq!(screens.sessions(secs(89)), HashSet::from([a.clone()]));
        assert!(screens.sessions(secs(90)).is_empty());
        // Said again, it counts for longer, and isn't coming on screen.
        assert!(!screens.set(2, Some(&a), secs(60)));
        assert_eq!(screens.sessions(secs(149)), HashSet::from([a.clone()]));
        assert!(screens.sessions(secs(150)).is_empty());
        // A mark that lapsed comes back, still not as coming on screen.
        assert!(!screens.set(2, Some(&a), secs(200)));
        assert_eq!(screens.sessions(secs(201)), HashSet::from([a]));
    }
}
