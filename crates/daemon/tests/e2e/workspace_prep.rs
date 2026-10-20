//! A Git workspace's preparation and deletion when Git is cut short or slow
//! (D-135): a checkout killed mid-way (otterd stopped for an update, a
//! delete during preparation) is made again, not marked ready; a locked or
//! vanished worktree is removed or made again; `workspace.create` doesn't
//! wait for Git; and a delete doesn't hold up the rest of the host.

use super::*;
use std::time::Instant;

/// What a `git worktree add` killed mid-checkout leaves (reproduced with
/// plain git): its registration locked "initializing" with the index still
/// locked, files missing from the tree, and the workspace's record never
/// told the branch.
fn cut_short(host: &TestHost, base: &Path, ws: &Workspace, missing: &str) {
    let registration = registration(base, &ws.root);
    std::fs::write(registration.join("locked"), "initializing").unwrap();
    std::fs::write(registration.join("index.lock"), "").unwrap();
    std::fs::remove_file(Path::new(&ws.root).join(missing)).unwrap();
    let file = host.home().join("state/state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let record = state["workspaces"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|w| w["name"] == ws.name.as_str())
        .unwrap();
    record["state"] = "preparing".into();
    let source = record["source"].as_object_mut().unwrap();
    source.insert("branch".into(), "".into());
    source.remove("base");
    source.remove("base_commit");
    std::fs::write(&file, serde_json::to_string_pretty(&state).unwrap()).unwrap();
}

/// The copy's registration of the worktree at `root`.
fn registration(base: &Path, root: &str) -> std::path::PathBuf {
    std::fs::read_dir(base.join("worktrees"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|d| {
            std::fs::read_to_string(d.join("gitdir"))
                .is_ok_and(|g| g.trim() == format!("{root}/.git"))
        })
        .unwrap_or_else(|| panic!("no registration of {root}"))
}

/// Whether the copy has any worktree locked.
fn locked(base: &Path) -> bool {
    git(base, &["worktree", "list", "--porcelain"])
        .lines()
        .any(|l| l.starts_with("locked"))
}

/// otterd stopped while `git worktree add` checked out (each update
/// restarts it, D-125): the next one makes the worktree again, on the
/// branch it was going to have, instead of calling a half-made tree ready.
#[tokio::test]
async fn a_checkout_cut_short_is_made_again_after_a_restart() {
    let host = TestHost::new();
    let src = make_repo(&host, "src", &[("README.md", "hello\n"), ("b.txt", "b\n")]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("cut", &src, None, vec![SessionSpec::shell()]))
        .await
        .unwrap();
    let (ws, base) = ready_git_ws(&host, "cut").await;
    host.stop_daemon().await;
    cut_short(&host, &base, &ws, "b.txt");

    // The next connection starts otterd, which resumes preparation.
    let ws = wait_prepared(&host, "cut").await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    let root = Path::new(&ws.root);
    assert!(root.join("b.txt").is_file(), "the tree is whole again");
    assert_eq!(git_source(&ws).branch, "otterd/cut");
    assert_eq!(local_branches(&base).trim(), "otterd/cut");
    assert!(!locked(&base));
    wait_status(&host, "cut", "shell", SessionStatus::Running).await;
    // Its branch is known, so deleting it deletes the branch (D-108).
    let mut conn = host.conn().await;
    conn.workspace_delete("cut", false).await.unwrap();
    assert!(!root.exists() && !base.exists());
}

/// A locked worktree (a checkout cut short) is still Otter's to remove:
/// deleting its workspace works, and so does deleting one whose checkout
/// was cut short, at whatever point its preparation is when the delete
/// comes.
#[tokio::test]
async fn a_locked_or_half_made_worktree_is_deleted() {
    let host = TestHost::new();
    let src = make_repo(&host, "src", &[("README.md", "hello\n"), ("b.txt", "b\n")]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("stuck", &src, None, vec![]))
        .await
        .unwrap();
    let (ws, base) = ready_git_ws(&host, "stuck").await;
    std::fs::write(registration(&base, &ws.root).join("locked"), "initializing").unwrap();
    conn.workspace_delete("stuck", false).await.unwrap();
    assert!(!Path::new(&ws.root).exists());
    assert!(!base.exists(), "the last workspace on the copy");

    conn.workspace_create(git_ws("again", &src, None, vec![]))
        .await
        .unwrap();
    let (ws, base) = ready_git_ws(&host, "again").await;
    host.stop_daemon().await;
    cut_short(&host, &base, &ws, "b.txt");
    // Never finished, so nothing in it is anyone's work: no force needed.
    let mut conn = host.conn().await;
    conn.workspace_delete("again", false).await.unwrap();
    assert!(!host.home().join("workspaces").join(ws.id.as_str()).exists());
    assert!(!base.exists());
}

/// A worktree that went missing (deleted by hand), its registration left
/// locked: preparing the workspace again checks out the branch it had,
/// with its commits, rather than failing or starting `otterd/<name>-2`.
#[tokio::test]
async fn a_worktree_that_went_missing_is_made_again_on_its_branch() {
    let host = TestHost::new();
    let src = make_repo(&host, "src", &[("README.md", "hello\n")]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("gone", &src, None, vec![]))
        .await
        .unwrap();
    let (ws, base) = ready_git_ws(&host, "gone").await;
    let root = Path::new(&ws.root);
    commit_file(root, "work.txt");
    std::fs::write(
        registration(&base, &ws.root).join("locked"),
        "on a USB disk",
    )
    .unwrap();
    std::fs::rename(root, host.home().join("moved")).unwrap();

    conn.workspace_prepare("gone").await.unwrap();
    let ws = wait_prepared(&host, "gone").await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    assert_eq!(git_source(&ws).branch, "otterd/gone");
    assert!(root.join("work.txt").is_file(), "its commit is there");
    assert_eq!(local_branches(&base).trim(), "otterd/gone");
    assert!(!locked(&base));
}

/// A `git` that sleeps before the subcommands named in `$SLOW_GIT` (noting
/// each in `$GIT_LOG`), having started a child that touches `$GIT_MARK` two
/// seconds later — as `git worktree add`'s checkout child goes on after
/// `git` itself is killed.
fn slow_git(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let real = std::env::split_paths(&path)
        .map(|d| d.join("git"))
        .find(|g| g.is_file())
        .expect("git on PATH");
    let bin = dir.join("slowbin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
for a in "$@"; do
  case " $SLOW_GIT " in *" $a "*)
    [ -n "$GIT_LOG" ] && echo "$*" >> "$GIT_LOG"
    [ -n "$GIT_MARK" ] && (sleep 2; touch "$GIT_MARK") &
    sleep 3; break;;
  esac
done
exec {} "$@"
"#,
        real.display()
    );
    std::fs::write(bin.join("git"), script).unwrap();
    std::fs::set_permissions(bin.join("git"), std::fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}:/usr/local/bin:/usr/bin:/bin",
        bin.display(),
        tmux_dir()
    )
}

async fn slow_git_host(scratch: &Path, slow: &str, mark: Option<&Path>) -> TestHost {
    let mut vars = vec![
        ("SHELL", isolated_shell(scratch)),
        ("PATH", slow_git(scratch)),
        ("SLOW_GIT", slow.to_owned()),
        (
            "GIT_LOG",
            scratch.join("slow.log").to_string_lossy().into_owned(),
        ),
    ];
    if let Some(m) = mark {
        vars.push(("GIT_MARK", m.to_string_lossy().into_owned()));
    }
    TestHost::with_env(&vars).await
}

/// `workspace.create` comes back at once for a Git workspace, which opens
/// while it prepares; an empty one still comes back ready.
#[tokio::test]
async fn a_git_workspace_comes_back_at_once() {
    let scratch = tempfile::Builder::new().prefix("wdg").tempdir().unwrap();
    let host = slow_git_host(scratch.path(), "fetch", None).await;
    let src = make_repo(&host, "src", &[("README.md", "hello\n")]);
    let mut conn = host.conn().await;
    let started = Instant::now();
    let ws = conn
        .workspace_create(git_ws("slow", &src, None, vec![]))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(ws.state, WorkspaceState::Preparing);
    let other = create(&host, "other", Some(vec![])).await;
    assert_eq!(other.state, WorkspaceState::Ready);
    ready_git_ws(&host, "slow").await;
}

/// A delete holds the host's lock only to stop the sessions and to drop
/// the record: removing the worktree, however long it takes, doesn't hold
/// up the rest. A second delete of it meanwhile is told so.
#[tokio::test]
async fn a_delete_doesnt_hold_up_the_host() {
    let scratch = tempfile::Builder::new().prefix("wdg").tempdir().unwrap();
    let host = slow_git_host(scratch.path(), "remove", None).await;
    let src = make_repo(&host, "src", &[("README.md", "hello\n")]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("slow", &src, None, vec![]))
        .await
        .unwrap();
    let (ws, _) = ready_git_ws(&host, "slow").await;
    create(&host, "other", Some(vec![])).await;

    let deleting = {
        let mut conn = host.conn().await;
        tokio::spawn(async move { conn.workspace_delete("slow", false).await })
    };
    let log = scratch.path().join("slow.log");
    eventually("the worktree is being removed", || {
        let removing = std::fs::read_to_string(&log).is_ok_and(|l| l.contains("worktree remove"));
        async move { removing.then_some(()) }
    })
    .await;
    let asked = Instant::now();
    conn.workspace_get("other").await.unwrap();
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked.elapsed()
    );
    let err = conn.workspace_delete("slow", false).await.unwrap_err();
    assert!(err.to_string().contains("being deleted"), "{err}");
    deleting.await.unwrap().unwrap();
    assert!(!Path::new(&ws.root).exists());
    assert!(conn.workspace_get("slow").await.is_err());
}

/// Deleting a workspace while it prepares stops its Git first, and all of
/// it: nothing it started goes on writing after the delete returns.
#[tokio::test]
async fn deleting_while_preparing_leaves_nothing_running() {
    let scratch = tempfile::Builder::new().prefix("wdg").tempdir().unwrap();
    let mark = scratch.path().join("late");
    let host = slow_git_host(scratch.path(), "fetch", Some(&mark)).await;
    let src = make_repo(&host, "src", &[("README.md", "hello\n")]);
    let creating = {
        let mut conn = host.conn().await;
        let create = git_ws("early", &src, None, vec![]);
        tokio::spawn(async move { conn.workspace_create(create).await })
    };
    let log = scratch.path().join("slow.log");
    eventually("fetching", || {
        let fetching = std::fs::read_to_string(&log).is_ok_and(|l| l.contains("fetch"));
        async move { fetching.then_some(()) }
    })
    .await;
    // Its child started too.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut conn = host.conn().await;
    let ws = conn.workspace_get("early").await.unwrap();
    conn.workspace_delete("early", false).await.unwrap();
    let _ = creating.await;
    assert!(!host.home().join("workspaces").join(ws.id.as_str()).exists());
    let repos = host.home().join("repos");
    let left: Vec<_> = std::fs::read_dir(&repos)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!mark.exists(), "a process of the preparation outlived it");
}
