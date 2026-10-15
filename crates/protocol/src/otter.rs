//! Otter's conversation on a host (D-093): `otter.*` requests.

use serde::{Deserialize, Serialize};

/// The developer's message to Otter on this host.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OtterSend {
    /// Chosen by the client: the same command again adds nothing.
    pub command_id: String,
    pub text: String,
}

/// The conversation's history (Otter's tool calls) after a cursor.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OtterEvents {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}
