//! Feature requests (D-043). Results are [`otter_core::feature::Feature`]s
//! and their history ([`otter_core::feature::FeatureEventRecord`]).
//!
//! Every request that changes something carries a `command_id` chosen by
//! the client (unique per intent, e.g. a random string). The daemon applies
//! a command once: a repeat — a retry after a dropped connection — gets the
//! feature as it is, without applying anything again.

use otter_core::feature::FeatureAction;
use serde::{Deserialize, Serialize};

/// Identifies a feature by id.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureRef {
    pub feature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureCreate {
    pub command_id: String,
    pub title: String,
    /// What should be built, as the developer puts it. Becomes the first
    /// message of the conversation.
    #[serde(default)]
    pub request: String,
    /// A workspace (id or name) on this host to do the work in. Without
    /// one, the feature blocks when it starts until one is chosen
    /// (`feature.act` `set_workspace`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// A message from the developer to the feature's Control Agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureSend {
    pub command_id: String,
    pub feature: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FeatureAct {
    pub command_id: String,
    pub feature: String,
    #[serde(flatten)]
    pub action: FeatureAction,
}

/// A file the feature's checks produced (a screenshot), by name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureArtifact {
    pub feature: String,
    /// The file's name in the feature's artifacts (no directories).
    pub name: String,
}

/// A feature's history after a cursor.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureEvents {
    pub feature: String,
    /// Return entries with `seq > after` (all when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}
