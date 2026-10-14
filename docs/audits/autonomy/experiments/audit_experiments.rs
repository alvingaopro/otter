// Autonomy-audit experiments (docs/audits/autonomy/06-benchmark-results.md).
//
// NOT part of the test suite. To reproduce, append this file to
// crates/daemon/tests/e2e.rs in a disposable worktree and run:
//
//   cargo test -p otterd --test e2e audit_ -- --nocapture --test-threads=1
//
// Each experiment drives a real otterd + tmux with the fake `claude -p`
// (tests/fake_claude_stream.sh) and the deterministic `rules` brain, and
// prints what happened rather than asserting the desired behaviour.


mod autonomy_audit {
    use super::*;
    use otter_core::feature::{FeatureAction, FeatureStatus};

    async fn history(host: &TestHost, id: &str) -> Vec<String> {
        host.conn()
            .await
            .feature_events(id, None)
            .await
            .unwrap()
            .iter()
            .map(|r| r.text.clone())
            .collect()
    }

    /// E1: is time spent waiting on the developer counted against the
    /// 30-minute run timeout? Timeout shortened to 3 s; the agent asks for a
    /// developer-only approval; the developer answers after 5 s.
    #[tokio::test]
    async fn audit_e1_run_timeout_counts_time_blocked_on_the_developer() {
        let fake = fake_agent_host_with("ok", "rules", |_| {
            vec![("OTTER_RUN_TIMEOUT_MS", "3000".into()), ("FAKE_ALLOW_DELAY", "2".into())]
        })
        .await;
        let id = started_feature(&fake.host, "Clean up ASK_RM").await;
        let f = wait_feature(&fake.host, &id, "blocked on the developer", |f| {
            f.status == FeatureStatus::Blocked
        })
        .await;
        let d = f.pending_decisions().next().expect("pending").clone();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let before = fake.host.conn().await.feature_get(&id).await.unwrap();
        println!(
            "E1 after 5 s blocked: status={:?} run={:?}",
            before.status,
            before.runs.iter().map(|r| (&r.state, &r.summary)).collect::<Vec<_>>()
        );
        fake.host
            .conn()
            .await
            .feature_act(
                "approve",
                &id,
                FeatureAction::Decide {
                    decision_id: d.id.clone(),
                    approve: true,
                    answer: None,
                    answers: None,
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let after = fake.host.conn().await.feature_get(&id).await.unwrap();
        println!(
            "E1 after approval: status={:?} runs={:?}",
            after.status,
            after
                .runs
                .iter()
                .map(|r| (r.id.to_string(), r.state, r.summary.clone()))
                .collect::<Vec<_>>()
        );
        for line in history(&fake.host, &id).await {
            println!("E1 history: {line}");
        }
    }

    /// E2: a check command that cannot run in the workspace environment
    /// (exit 127, like `cargo` missing from PATH) while the agent reports
    /// success every time. How many runs before the feature stops, and why?
    #[tokio::test]
    async fn audit_e2_an_unrunnable_check_burns_the_attempt_budget() {
        let fake = fake_agent_host("ok", "rules").await;
        let host = &fake.host;
        let dir = host.home().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Makefile"), "test:\n\t@not-on-this-path test\n").unwrap();
        let mut conn = host.conn().await;
        conn.workspace_create(WorkspaceCreate {
            name: "proj".into(),
            source: SourceSpec::Directory {
                path: dir.to_string_lossy().into_owned(),
            },
            sessions: Some(vec![]),
            ..Default::default()
        })
        .await
        .unwrap();
        let f = conn
            .feature_create(otter_protocol::feature::FeatureCreate {
                command_id: "create-proj".into(),
                title: "CSV export".into(),
                request: "Add CSV export".into(),
                workspace: Some("proj".into()),
            })
            .await
            .unwrap();
        conn.feature_act("start", f.id.as_str(), FeatureAction::Start)
            .await
            .unwrap();
        let id = f.id.to_string();
        let f = wait_feature(host, &id, "the feature stops", |f| {
            matches!(f.status, FeatureStatus::Failed | FeatureStatus::Done)
        })
        .await;
        println!(
            "E2 final: status={:?} reason={:?} runs={} evidence={:?} budget={:?}",
            f.status,
            f.status_reason,
            f.runs.len(),
            f.evidence.iter().map(|e| (&e.title, e.ok)).collect::<Vec<_>>(),
            f.budget
        );
        for line in history(host, &id).await {
            println!("E2 history: {line}");
        }
    }

    /// E3: does a routine, policy-approved edit become a Decision record and
    /// history entries the developer sees?
    #[tokio::test]
    async fn audit_e3_routine_edits_are_recorded_as_decisions() {
        let fake = fake_agent_host("ok", "rules").await;
        let id = started_feature(&fake.host, "Tidy ASK_EDIT").await;
        let f = wait_feature(&fake.host, &id, "the run ends", |f| {
            f.runs.first().is_some_and(|r| r.ended_at.is_some())
        })
        .await;
        println!(
            "E3 decisions: {:?}",
            f.decisions
                .iter()
                .map(|d| (d.kind, d.risk, d.status, d.decided_by, d.summary.clone()))
                .collect::<Vec<_>>()
        );
        for line in history(&fake.host, &id).await {
            println!("E3 history: {line}");
        }
    }
}
