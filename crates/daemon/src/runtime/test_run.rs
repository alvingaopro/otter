//! `runtime.test` (D-124): the coding agent started for one short reply, to
//! know before any work needs it that it's installed, signed in and
//! answering. It runs in a scratch directory under the host's state, every
//! tool call is refused, and it's stopped once it answers: two minutes at
//! most. Nothing is recorded: no conversation, no task.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use otter_core::conversation::{ErrorCategory, TurnOutcome};
use otter_core::{ConversationId, RunId, TurnId};
use otter_protocol::conversation::{RuntimeTest, StepState, TestStep};
use tokio::time::timeout_at;

use super::{
    AgentRuntime, CheckDecision, DecisionReply, RunHandle, RunLimits, RunSpec, RuntimeEvent,
    TurnSpec,
};
use crate::env::EnvMap;

/// How long the reply may take, starting included.
const LIMIT: Duration = Duration::from_secs(120);
/// The model when none is set: the reply proves the sign-in, and a small
/// one costs least.
pub const SMALL_MODEL: &str = "haiku";
const PROMPT: &str = "Otter is checking that you can run on this host. Reply with one word: ready";
const INSTRUCTIONS: &str = "This is a setup check by Otter: answer in one word, use no tools.";
const REFUSED: &str = "Otter's setup check uses no tools.";

pub const INSTALLED: &str = "Installed";
pub const ANSWERS: &str = "Signed in and answering";

/// One test at a time on a host.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Test `rt` (as `backend`) with `env`, in `scratch` (made, then removed).
/// `Err` only when a test is already running.
pub async fn run(
    rt: &'static dyn AgentRuntime,
    backend: &str,
    env: EnvMap,
    scratch: &Path,
    model: Option<String>,
) -> Result<RuntimeTest, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("a test is already running on this host; it ends within two minutes".into());
    }
    struct Running;
    impl Drop for Running {
        fn drop(&mut self) {
            RUNNING.store(false, Ordering::SeqCst);
        }
    }
    let _running = Running;

    // It may run the worker's `--check`, which waits.
    let probe = env.clone();
    let (caps, version) =
        tokio::task::spawn_blocking(move || (rt.capabilities(&probe), rt.version(&probe)))
            .await
            .map_err(|e| e.to_string())?;
    let mut steps = vec![];
    if !caps.available {
        steps.push(step(
            INSTALLED,
            StepState::Failed,
            caps.notes
                .first()
                .cloned()
                .unwrap_or_else(|| "It can't run on this host.".into()),
        ));
        steps.push(step(ANSWERS, StepState::Skipped, "Not tried.".into()));
        return Ok(RuntimeTest {
            backend: backend.into(),
            ready: false,
            steps,
        });
    }
    steps.push(step(
        INSTALLED,
        StepState::Passed,
        version.unwrap_or_else(|| "Found on this host.".into()),
    ));

    let model = model.unwrap_or_else(|| SMALL_MODEL.into());
    let _ = std::fs::remove_dir_all(scratch);
    let answered = match std::fs::create_dir_all(scratch) {
        Ok(()) => answer(rt, env, scratch, &model).await,
        Err(e) => Err(format!("Otter couldn't make a scratch directory: {e}.")),
    };
    let _ = std::fs::remove_dir_all(scratch);
    let ready = answered.is_ok();
    steps.push(match answered {
        Ok(detail) => step(ANSWERS, StepState::Passed, detail),
        Err(detail) => step(ANSWERS, StepState::Failed, detail),
    });
    Ok(RuntimeTest {
        backend: backend.into(),
        ready,
        steps,
    })
}

fn step(name: &str, state: StepState, detail: String) -> TestStep {
    TestStep {
        name: name.into(),
        state,
        detail,
    }
}

/// Start it, ask, wait for the reply; it's stopped whatever happens.
async fn answer(
    rt: &dyn AgentRuntime,
    env: EnvMap,
    scratch: &Path,
    model: &str,
) -> Result<String, String> {
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + LIMIT;
    let spec = RunSpec {
        conversation_id: ConversationId::generate(),
        run_id: RunId::generate(),
        generation: 1,
        cwd: scratch.to_owned(),
        env,
        resume: None,
        instructions: Some(INSTRUCTIONS.into()),
        model: Some(model.into()),
        // A second turn, should it try a tool and be refused.
        limits: RunLimits {
            max_turns: Some(2),
            max_budget_usd: None,
        },
    };
    // Not under the deadline: a start bounds its own wait, and a start cut
    // short would leave what it began running.
    let mut handle = match rt.start(spec).await {
        Err(e) => return Err(format!("It didn't start: {}.", e.root_cause())),
        Ok(h) => h,
    };
    let replied = reply(handle.as_mut(), deadline).await;
    let _ = handle.stop().await;
    replied?;
    Ok(format!(
        "Answered in {:.1} s, with {model}.",
        started.elapsed().as_secs_f64()
    ))
}

async fn reply(h: &mut dyn RunHandle, deadline: tokio::time::Instant) -> Result<(), String> {
    let turn = TurnSpec {
        turn_id: TurnId::generate(),
        text: PROMPT.into(),
    };
    h.send_turn(&turn)
        .await
        .map_err(|e| format!("It didn't take the question: {}.", e.root_cause()))?;
    loop {
        let event = match timeout_at(deadline, h.next_event()).await {
            Err(_) => return Err(too_slow()),
            Ok(None) => return Err("It stopped before answering.".into()),
            Ok(Some(e)) => e,
        };
        match event {
            RuntimeEvent::ToolCheck { check_id, .. } => {
                let deny = CheckDecision::Deny {
                    reason: REFUSED.into(),
                };
                let _ = h.check(&check_id, deny).await;
            }
            RuntimeEvent::DecisionNeeded(ask) => {
                let deny = DecisionReply::Deny {
                    message: REFUSED.into(),
                };
                let _ = h.decide(&ask.request_id, deny).await;
            }
            RuntimeEvent::TurnFinished { outcome, error, .. } => {
                return match (error, outcome) {
                    (Some(e), _) => Err(failed(e)),
                    // An answer either way: it's signed in and replies.
                    (None, TurnOutcome::Completed | TurnOutcome::LimitReached) => Ok(()),
                    (None, o) => Err(format!("The turn ended without an answer ({o:?}).")),
                };
            }
            RuntimeEvent::Exited { error, .. } => {
                return Err(match error {
                    Some(e) => format!(
                        "It stopped before answering: {}.",
                        crate::agents::excerpt(&e, 300)
                    ),
                    None => "It stopped before answering.".into(),
                });
            }
            _ => {}
        }
    }
}

fn too_slow() -> String {
    format!("No answer within {} s.", LIMIT.as_secs())
}

/// What a failed reply means here, and what to do.
fn failed(e: ErrorCategory) -> String {
    match e {
        ErrorCategory::AuthRequired => "It isn't signed in. Sign Claude Code in on the host: run `claude` there and log in, or set the API key it uses.".into(),
        ErrorCategory::InvalidInput => "Claude refused the request: check the coding model.".into(),
        ErrorCategory::RateLimited => "Claude's rate limit was reached. Try again once it resets.".into(),
        ErrorCategory::ProviderUnavailable => "Claude's service didn't answer. Try again in a few minutes.".into(),
        e => {
            let x = e.explain();
            format!("{} {}", x.what, x.next)
        }
    }
}
