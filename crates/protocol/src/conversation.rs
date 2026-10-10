//! Runtime conversations and capabilities (D-055): reading what a coding
//! agent did, turn by turn, and what this host's runtime can do.
//!
//! Read-only for now. Coding work is sent by Otter (the controller), not by
//! clients; commands on conversations come with durable receipts later.

use otter_core::conversation::Conversation;
use serde::{Deserialize, Serialize};

/// `runtime.capabilities`: what the managed coding runtime on this host is
/// and can do. Says so plainly when something is missing or unproven.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RuntimeCapabilities {
    /// The agent, e.g. `claude`.
    pub provider: String,
    /// How it is driven: `legacy_cli` (Claude Code's stream-json, an
    /// observed, undocumented protocol) or `sdk`.
    pub backend: String,
    /// Whether a run can start here now (the agent is installed).
    pub available: bool,
    /// Why not, or what to know, in a sentence each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The version the backend's protocol was observed or tested on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tested_version: Option<String>,
    /// Whether this is the structured runtime ready for general use. Not
    /// yet: the SDK worker and a durable journal come first.
    pub structured_ready: bool,
    pub features: RuntimeFeatures,
}

/// What a runtime supports. A missing feature is reported, never simulated
/// (e.g. pausing by killing the process).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeFeatures {
    pub send_turn: bool,
    pub resume: bool,
    pub interrupt_turn: bool,
    pub permission_requests: bool,
    pub questions: bool,
    /// Tool results are reported (else a tool's outcome is unknown).
    pub tool_results: bool,
    pub streaming: bool,
    pub attachments: bool,
    pub usage: bool,
}

/// `conversation.get`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRef {
    pub conversation: String,
}

/// `conversation.list`: every conversation, or a feature's.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
}

/// A conversation as a client sees it: the provider's own session id stays
/// on the host; `resumable` says whether there is one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConversationView {
    #[serde(flatten)]
    pub conversation: Conversation,
    pub resumable: bool,
}

impl ConversationView {
    pub fn of(c: &Conversation) -> ConversationView {
        let mut conversation = c.clone();
        let resumable = conversation.binding.take().is_some();
        ConversationView {
            conversation,
            resumable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::conversation::NativeBinding;

    #[test]
    fn the_provider_session_stays_on_the_host() {
        let mut c = Conversation::new("claude", "legacy_cli", "ws_1".into(), chrono::Utc::now());
        c.binding = Some(NativeBinding {
            session_id: "native-secret-session".into(),
        });
        let v = ConversationView::of(&c);
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.contains("native-secret-session"));
        assert!(json.contains("\"resumable\":true"));
        let back: ConversationView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.conversation.id, c.id);
    }
}
