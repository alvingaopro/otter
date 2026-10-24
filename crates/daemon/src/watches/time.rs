//! `time`: look again at the task later (D-183). Nothing is probed; the
//! time coming is the event, and Otter's turn looks at the task then.

use std::time::Duration;

use async_trait::async_trait;
use otter_core::task::Task;
use otter_core::watch::{Finding, Outcome, Watch};
use serde_json::{Value, json};

use super::{Probe, ProbeContext, Target, WatchKind, minutes_arg};

pub(crate) struct Time;

#[async_trait]
impl WatchKind for Time {
    fn name(&self) -> &'static str {
        "time"
    }

    fn target(&self, _f: Option<&Task>, args: &Value, what: &str) -> Result<Target, String> {
        let minutes = self.minutes(args)?;
        let what = what.trim();
        // What it's for tells two looks in as many minutes apart.
        let hash = what.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        Ok(Target {
            value: json!({ "in_minutes": minutes, "for": crate::agents::excerpt(what, 200) }),
            label: crate::agents::excerpt(what, 60),
            key: format!("in{minutes}-{:08x}", hash as u32),
        })
    }

    fn minutes(&self, args: &Value) -> Result<u32, String> {
        // A look that comes back first comes after its own interval
        // (D-213), unless said otherwise.
        match minutes_arg(args, "in_minutes")? {
            Some(n) => Ok(n),
            None => minutes_arg(args, "every_minutes")?
                .ok_or_else(|| "say when to look again: in_minutes".to_owned()),
        }
    }

    fn every(&self) -> Option<Duration> {
        None
    }

    fn at_deadline(&self, w: &Watch) -> (Outcome, String) {
        let late = (chrono::Utc::now() - w.deadline).num_minutes();
        let text = if late >= 2 {
            format!("the time came {late} min ago (otterd wasn't running then)")
        } else {
            "the time came".to_owned()
        };
        (Outcome::Due, text)
    }

    async fn probe(&self, _w: &Watch, _cx: &ProbeContext<'_>) -> Probe {
        Probe::NotYet {
            key: "waiting".into(),
            text: "waiting for the time".into(),
        }
    }

    fn line(&self, w: &Watch, finding: &Finding) -> String {
        format!("Time to look again: {} ({}).", w.what, finding.text)
    }
}
