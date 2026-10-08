//! Human attention (design §21): which work needs the developer right now.
//!
//! Attention items are raised from state transitions — an agent finishing a
//! turn or going quiet mid-turn, a task or service failing, preparation
//! failing — and resolved when the developer engages with the session (attach,
//! send, restart, stop), when the situation changes (the agent starts working
//! again), or explicitly. Each session has at most one open item; a newer one
//! replaces it.

use std::sync::Arc;

use chrono::Utc;
use otter_core::{
    AgentState, Attention, AttentionId, AttentionKind, ExecutionState, Session, SessionId,
    SessionKind, Workspace, WorkspaceState,
};
use otter_protocol::{AttentionResolve, Event, RpcError};

use crate::daemon::{Daemon, RpcResult, save, workspace_not_found};

/// Raise an item, replacing the session's (or workspace's) open one. Returns
/// the events to emit.
pub fn raise(
    ws: &mut Workspace,
    session_id: Option<&SessionId>,
    kind: AttentionKind,
    summary: String,
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

/// Attention for a session whose current execution just ended.
pub fn on_execution_ended(ws: &mut Workspace, session: &Session) -> Vec<Event> {
    let Some(exec) = session.current_execution() else {
        return Vec::new();
    };
    let name = &session.name;
    let (kind, summary) = match (session.kind, exec.state, exec.exit_code) {
        // Leaving a shell or quitting an agent cleanly is the user's own doing.
        (SessionKind::Terminal, _, _) | (SessionKind::Agent, ExecutionState::Exited, Some(0)) => {
            return resolve_session(ws, &session.id);
        }
        (SessionKind::Task, ExecutionState::Exited, Some(0)) => {
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
    raise(ws, Some(&session.id), kind, summary)
}

/// Attention for an agent state transition.
pub fn on_agent_state(
    ws: &mut Workspace,
    session_id: &SessionId,
    name: &str,
    state: AgentState,
    last_message: Option<&str>,
) -> Vec<Event> {
    match state {
        AgentState::WaitingForInput => {
            let summary = match last_message {
                Some(m) => format!("{name} finished: {m}"),
                None => format!("{name} finished its turn"),
            };
            raise(ws, Some(session_id), AttentionKind::Review, summary)
        }
        AgentState::Blocked => raise(
            ws,
            Some(session_id),
            AttentionKind::Approval,
            format!("{name} seems to be waiting for you (approval or question?)"),
        ),
        AgentState::Working => resolve_session(ws, session_id),
        AgentState::Starting | AgentState::Idle | AgentState::Exited => Vec::new(),
    }
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

    /// The developer engaged with a session (attached, typed into it…):
    /// whatever it was asking for has been seen.
    pub(crate) async fn engage(
        self: &Arc<Self>,
        ws_id: &otter_core::WorkspaceId,
        session_id: &SessionId,
    ) {
        let mut store = self.store.lock().await;
        let Some(ws) = store.workspace_mut(ws_id.as_str()) else {
            return;
        };
        let events = resolve_session(ws, session_id);
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
}
