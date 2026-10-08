//! Otter domain model.
//!
//! These types are shared verbatim (via serde) by the host daemon (`otterd`),
//! the control-plane CLI (`otter`) and the future desktop app.

pub mod ids;
pub mod model;

pub use ids::{AttentionId, ExecutionId, SessionId, WorkspaceId};
pub use model::*;

/// Validate a user-chosen workspace or session name.
///
/// Names are free-form (spaces allowed) but may not contain `/` — the CLI uses
/// `workspace/session` addressing — or control characters.
pub fn validate_name(what: &str, name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err(format!("{what} name must not be empty"));
    }
    if name.len() > 128 {
        return Err(format!("{what} name is too long (max 128 bytes)"));
    }
    if name.contains('/') || name.contains(':') {
        return Err(format!("{what} name must not contain `/` or `:`"));
    }
    if name.chars().any(char::is_control) {
        return Err(format!("{what} name must not contain control characters"));
    }
    Ok(())
}
