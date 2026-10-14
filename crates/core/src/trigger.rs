//! Triggers (D-083): what starts work without a person at the keyboard, and
//! the policy that work runs under — decided by the developer in advance,
//! on the trigger, never by a model in the moment (D-044, D-066).

use chrono::{Duration, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{FeatureId, TriggerId, WorkspaceId};
use crate::model::Timestamp;

/// How a triggered feature that waited too long for the developer says so,
/// paused (D-083).
pub const WAITED: &str = "Waited ";

/// When a trigger fires.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Every so many minutes (at least one), from when it was made.
    Every { minutes: u32 },
    /// Every day at `at` (`HH:MM`, UTC).
    Daily { at: String },
    /// Only when fired by hand (`otter trigger run`).
    Manual,
}

impl Schedule {
    /// The first occurrence strictly after `after` (`None`: by hand only).
    pub fn next_after(&self, after: Timestamp, since: Timestamp) -> Option<Timestamp> {
        match self {
            Schedule::Every { minutes } => {
                let step = Duration::minutes(i64::from((*minutes).max(1)));
                let n = (after - since).num_seconds().div_euclid(step.num_seconds()) + 1;
                Some(since + step * n.max(1) as i32)
            }
            Schedule::Daily { at } => {
                let t = NaiveTime::parse_from_str(at, "%H:%M").ok()?;
                let today = Utc.from_utc_datetime(&after.date_naive().and_time(t));
                Some(if today > after {
                    today
                } else {
                    today + Duration::days(1)
                })
            }
            Schedule::Manual => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Schedule::Every { minutes } if minutes % 60 == 0 => format!("every {}h", minutes / 60),
            Schedule::Every { minutes } => format!("every {minutes}m"),
            Schedule::Daily { at } => format!("daily at {at} UTC"),
            Schedule::Manual => "by hand".into(),
        }
    }
}

/// What unattended work may do, decided in advance (D-083).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Policy {
    /// Publishing — pushing the workspace's branch and opening its pull
    /// request — is allowed without asking. Nothing else is granted.
    #[serde(default)]
    pub publish: bool,
    /// Attempts and minutes of agent work per feature (`None`: the defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_minutes: Option<u32>,
    /// How long a decision only the developer can make may wait before the
    /// feature pauses, its agent stopped (`None`: as long as it takes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_limit_secs: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Trigger {
    pub id: TriggerId,
    pub name: String,
    pub workspace_id: WorkspaceId,
    /// The feature each occurrence starts.
    pub title: String,
    pub request: String,
    pub schedule: Schedule,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub created_at: Timestamp,
    /// The next due occurrence (`None`: by hand only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_at: Option<Timestamp>,
    /// What its occurrences did, newest last (the last few).
    #[serde(default)]
    pub history: Vec<Firing>,
}

fn yes() -> bool {
    true
}

/// One occurrence of a trigger, and what came of it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Firing {
    /// Which occurrence: its scheduled time, or when it was fired by hand.
    pub occurrence: Timestamp,
    pub at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<FeatureId>,
    /// Why nothing started, when nothing did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

impl Trigger {
    /// The command id an occurrence's feature is created with: one feature
    /// per occurrence, however often it's fired (D-083).
    pub fn command_id(&self, occurrence: Timestamp) -> String {
        format!("trigger:{}:{}", self.id, occurrence.timestamp_millis())
    }
}

/// What started a feature, when a trigger did.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Started {
    pub trigger_id: TriggerId,
    pub trigger: String,
    pub occurrence: Timestamp,
    /// The trigger's policy, as it was then.
    #[serde(default)]
    pub policy: Policy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_say_when_next() {
        let since = Utc.with_ymd_and_hms(2026, 10, 14, 0, 0, 0).unwrap();
        let every = Schedule::Every { minutes: 90 };
        let at = |h, m| Utc.with_ymd_and_hms(2026, 10, 14, h, m, 0).unwrap();
        assert_eq!(every.next_after(since, since), Some(at(1, 30)));
        assert_eq!(every.next_after(at(1, 30), since), Some(at(3, 0)));
        assert_eq!(every.next_after(at(2, 59), since), Some(at(3, 0)));
        let daily = Schedule::Daily { at: "02:00".into() };
        assert_eq!(daily.next_after(at(1, 0), since), Some(at(2, 0)));
        assert_eq!(
            daily.next_after(at(2, 0), since),
            Some(Utc.with_ymd_and_hms(2026, 10, 15, 2, 0, 0).unwrap())
        );
        assert_eq!(
            Schedule::Daily { at: "25:00".into() }.next_after(at(1, 0), since),
            None
        );
        assert_eq!(Schedule::Manual.next_after(at(1, 0), since), None);
        assert_eq!(every.describe(), "every 90m");
        assert_eq!(Schedule::Every { minutes: 360 }.describe(), "every 6h");
    }
}
