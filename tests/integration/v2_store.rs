use crate::repos::test_repo::TestRepo;
use git_ai::commit_links::store::{LinkStore, OperationContext, SessionIdentity};
use git_ai::error::GitAiError;
use std::collections::BTreeSet;

fn session(id: &str) -> SessionIdentity {
    SessionIdentity::new("codex", id).unwrap()
}

fn context(repo: &TestRepo, id: &str) -> OperationContext {
    OperationContext {
        id: id.into(),
        family: repo.path().join(".git").to_string_lossy().into_owned(),
        worktree: repo.path().to_string_lossy().into_owned(),
        git_dir: repo.path().join(".git"),
        repo_url: None,
        time_ns: 1,
    }
}

fn commit(repo: &TestRepo, value: &str) -> String {
    std::fs::write(repo.path().join("file.txt"), value).unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git(&["commit", "-m", value]).unwrap();
    repo.git(&["rev-parse", "HEAD"]).unwrap().trim().into()
}

#[test]
fn v2_store_consumes_once_rejects_stale_attachment_and_survives_restart() {
    let repo = TestRepo::new();
    let path = repo.path().join("links.sqlite");
    let mut store = LinkStore::open_at(&path).unwrap();
    let ctx = context(&repo, "first");
    let agent = session("raw-id");
    let state = store
        .pending_state(&ctx.family, &ctx.worktree, &agent)
        .unwrap();
    assert!(!state.attached);
    assert!(
        store
            .attach_session(&ctx.family, &ctx.worktree, state.generation, &agent)
            .unwrap()
    );
    let sha = commit(&repo, "one");
    assert!(
        store
            .apply_operation(&ctx, |tx| {
                tx.link_commit(&sha, &tx.pending_sessions()?)?;
                tx.clear_pending()
            })
            .unwrap()
    );
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &sha).unwrap(),
        BTreeSet::from([agent.clone()])
    );
    assert!(
        !store
            .attach_session(&ctx.family, &ctx.worktree, state.generation, &agent)
            .unwrap()
    );
    let next = store
        .pending_state(&ctx.family, &ctx.worktree, &agent)
        .unwrap();
    assert_eq!(next.generation, state.generation + 1);
    assert!(
        store
            .attach_session(&ctx.family, &ctx.worktree, next.generation, &agent)
            .unwrap()
    );
    assert!(
        !store
            .apply_operation(&ctx, |tx| tx.clear_pending())
            .unwrap()
    );
    drop(store);
    let store = LinkStore::open_at(&path).unwrap();
    assert!(
        store
            .pending_state(&ctx.family, &ctx.worktree, &agent)
            .unwrap()
            .attached
    );
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &sha).unwrap(),
        BTreeSet::from([agent])
    );
}

#[test]
fn v2_store_rollback_keeps_pending_and_worktrees_are_isolated() {
    let repo = TestRepo::new();
    let mut store = LinkStore::open_at(&repo.path().join("links.sqlite")).unwrap();
    let ctx = context(&repo, "failed");
    let agent = session("session");
    store
        .attach_session(&ctx.family, &ctx.worktree, 0, &agent)
        .unwrap();
    let sha = commit(&repo, "one");
    let result = store.apply_operation(&ctx, |tx| {
        tx.link_commit(&sha, &tx.pending_sessions()?)?;
        tx.clear_pending()?;
        Err(GitAiError::Generic("injected rollback".into()))
    });
    assert!(result.is_err());
    assert!(
        store
            .sessions_for_commit(&ctx.family, &sha)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .pending_state(&ctx.family, &ctx.worktree, &agent)
            .unwrap()
            .attached
    );
    assert!(
        !store
            .pending_state(&ctx.family, "other-worktree", &agent)
            .unwrap()
            .attached
    );
    assert!(
        !store
            .pending_state("other-family", &ctx.worktree, &agent)
            .unwrap()
            .attached
    );
    assert!(
        store
            .apply_operation(&ctx, |tx| {
                tx.link_commit(&sha, &tx.pending_sessions()?)?;
                tx.clear_pending()
            })
            .unwrap()
    );
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &sha).unwrap(),
        BTreeSet::from([agent])
    );
}

#[test]
fn v2_store_late_shell_link_reaches_rewritten_commit_without_next_window_leak() {
    let repo = TestRepo::new();
    let mut store = LinkStore::open_at(&repo.path().join("links.sqlite")).unwrap();
    let ctx = context(&repo, "first");
    let agent = session("shell-session");
    store
        .begin_invocation(&ctx.family, &ctx.worktree, "shell", &agent)
        .unwrap();
    let first = commit(&repo, "one");
    store
        .apply_operation(&ctx, |tx| tx.record_created_commit(&first, 1, 2))
        .unwrap();
    assert!(
        store
            .sessions_for_commit(&ctx.family, &first)
            .unwrap()
            .is_empty()
    );
    let second = commit(&repo, "two");
    store
        .apply_operation(&context(&repo, "rewrite"), |tx| {
            tx.record_rewrite(&first, &second)
        })
        .unwrap();
    assert!(
        store
            .sessions_for_commit(&ctx.family, &second)
            .unwrap()
            .is_empty()
    );
    store
        .finish_invocation(&context(&repo, "shell-post"), "shell", true)
        .unwrap();
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &first).unwrap(),
        BTreeSet::from([agent.clone()])
    );
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &second).unwrap(),
        BTreeSet::from([agent.clone()])
    );
    assert!(
        !store
            .pending_state(&ctx.family, &ctx.worktree, &agent)
            .unwrap()
            .attached
    );
    assert!(
        !store
            .finish_invocation(&context(&repo, "shell-post"), "shell", true)
            .unwrap()
    );
}

#[test]
fn v2_store_abort_removes_deferred_sources_but_keeps_checkpoint_sessions() {
    let repo = TestRepo::new();
    let mut store = LinkStore::open_at(&repo.path().join("links.sqlite")).unwrap();
    let ctx = context(&repo, "defer");
    let regular = session("regular");
    let deferred = session("deferred");
    store
        .attach_session(&ctx.family, &ctx.worktree, 0, &regular)
        .unwrap();
    store
        .apply_operation(&ctx, |tx| {
            tx.add_deferred("cherry-pick", &BTreeSet::from([deferred.clone()]))
        })
        .unwrap();
    store
        .apply_operation(&context(&repo, "abort"), |tx| {
            tx.clear_deferred("cherry-pick")
        })
        .unwrap();
    let sha = commit(&repo, "one");
    store
        .apply_operation(&context(&repo, "commit"), |tx| {
            tx.link_commit(&sha, &tx.pending_sessions()?)?;
            tx.clear_pending()
        })
        .unwrap();
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &sha).unwrap(),
        BTreeSet::from([regular])
    );
    assert!(
        !store
            .pending_state(&ctx.family, &ctx.worktree, &deferred)
            .unwrap()
            .attached
    );
}

#[test]
fn v2_store_historical_links_do_not_join_active_shell_and_restart_abandons_it() {
    let repo = TestRepo::new();
    let path = repo.path().join("links.sqlite");
    let mut store = LinkStore::open_at(&path).unwrap();
    let ctx = context(&repo, "history");
    let historical = commit(&repo, "old");
    assert!(
        store
            .sessions_for_commit(&ctx.family, &historical)
            .unwrap()
            .is_empty()
    );
    let old = session("old-session");
    let shell = session("shell-session");
    store
        .begin_invocation(&ctx.family, &ctx.worktree, "shell", &shell)
        .unwrap();
    store
        .apply_operation(&ctx, |tx| {
            tx.link_commit(&historical, &BTreeSet::from([old.clone()]))
        })
        .unwrap();
    store
        .finish_invocation(&context(&repo, "post"), "shell", true)
        .unwrap();
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &historical).unwrap(),
        BTreeSet::from([old])
    );
    assert!(
        store
            .pending_state(&ctx.family, &ctx.worktree, &shell)
            .unwrap()
            .attached
    );

    let lost = session("lost-session");
    store
        .begin_invocation(&ctx.family, &ctx.worktree, "lost", &lost)
        .unwrap();
    drop(store);
    let mut store = LinkStore::open_at(&path).unwrap();
    store.abandon_invocations().unwrap();
    let next = commit(&repo, "next");
    store
        .apply_operation(&context(&repo, "next"), |tx| {
            tx.record_created_commit(&next, 2, 3)
        })
        .unwrap();
    assert!(
        store
            .sessions_for_commit(&ctx.family, &next)
            .unwrap()
            .is_empty()
    );
    store
        .finish_invocation(&context(&repo, "missing-post"), "lost", true)
        .unwrap();
    assert!(
        store
            .sessions_for_commit(&ctx.family, &next)
            .unwrap()
            .is_empty()
    );
    assert!(
        !store
            .pending_state(&ctx.family, &ctx.worktree, &lost)
            .unwrap()
            .attached
    );
}

#[test]
fn v2_diagnostics_work_without_retired_session_delivery_controls() {
    let repo = TestRepo::new();
    for request in [
        serde_json::json!({"method":"cas.submit","params":{"records":[]}}),
        serde_json::json!({"method":"notes.flush"}),
        serde_json::json!({"method":"metrics.reingest","params":{"from_ts":null,"to_ts":null}}),
        serde_json::json!({"method":"telemetry.submit","params":{"envelopes":[{"type":"metrics","events":[]}]}}),
    ] {
        assert!(serde_json::from_value::<git_ai::daemon::ControlRequest>(request).is_err());
    }
    let request = serde_json::from_value(serde_json::json!({
        "method":"telemetry.submit","params":{"envelopes":[{
            "type":"message","timestamp":"2026-10-08T00:00:00Z",
            "message":"test diagnostic","level":"info","context":null
        }]}
    }))
    .unwrap();
    let response =
        git_ai::daemon::send_control_request(&repo.daemon_control_socket_path(), &request).unwrap();
    assert!(response.ok, "{response:?}");
    repo.git_ai(&["await"]).unwrap();
}

#[test]
fn v2_store_reused_shell_id_does_not_claim_an_abandoned_calls_commits() {
    let repo = TestRepo::new();
    let mut store = LinkStore::open_at(&repo.path().join("links.sqlite")).unwrap();
    let ctx = context(&repo, "first");
    let session = session("shell-session");
    store
        .begin_invocation(&ctx.family, &ctx.worktree, "bash", &session)
        .unwrap();
    let first = commit(&repo, "abandoned");
    store
        .apply_operation(&ctx, |tx| tx.record_created_commit(&first, 1, 2))
        .unwrap();
    assert!(
        store
            .sessions_for_commit(&ctx.family, &first)
            .unwrap()
            .is_empty()
    );

    store
        .begin_invocation(&ctx.family, &ctx.worktree, "bash", &session)
        .unwrap();
    let second = commit(&repo, "new invocation");
    store
        .apply_operation(&context(&repo, "second"), |tx| {
            tx.record_created_commit(&second, 3, 4)
        })
        .unwrap();
    assert!(
        store
            .sessions_for_commit(&ctx.family, &second)
            .unwrap()
            .is_empty()
    );
    store
        .finish_invocation(&context(&repo, "post"), "bash", true)
        .unwrap();
    assert!(
        store
            .sessions_for_commit(&ctx.family, &first)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &second).unwrap(),
        BTreeSet::from([session])
    );
}

#[test]
fn v2_store_targeted_invocation_eviction_preserves_other_active_calls() {
    let repo = TestRepo::new();
    let mut store = LinkStore::open_at(&repo.path().join("links.sqlite")).unwrap();
    let ctx = context(&repo, "commit");
    let expired = session("expired-session");
    let active = session("active-session");
    store
        .begin_invocation(&ctx.family, &ctx.worktree, "expired", &expired)
        .unwrap();
    store
        .begin_invocation(&ctx.family, &ctx.worktree, "active", &active)
        .unwrap();
    store
        .abandon_invocation(&ctx.family, &ctx.worktree, "expired")
        .unwrap();
    let sha = commit(&repo, "one");
    store
        .apply_operation(&ctx, |tx| tx.record_created_commit(&sha, 1, 2))
        .unwrap();
    assert!(
        store
            .sessions_for_commit(&ctx.family, &sha)
            .unwrap()
            .is_empty()
    );
    store
        .finish_invocation(&context(&repo, "expired-post"), "expired", true)
        .unwrap();
    store
        .finish_invocation(&context(&repo, "active-post"), "active", true)
        .unwrap();
    assert_eq!(
        store.sessions_for_commit(&ctx.family, &sha).unwrap(),
        BTreeSet::from([active])
    );
    assert!(
        !store
            .pending_state(&ctx.family, &ctx.worktree, &expired)
            .unwrap()
            .attached
    );
}
