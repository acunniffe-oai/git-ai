use crate::repos::test_repo::TestRepo;
use std::fs;

fn runtime_build_count(path: &std::path::Path) -> usize {
    fs::read_to_string(path).unwrap_or_default().lines().count()
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn daemon_limits_glibc_allocator_arenas() {
    let temp = tempfile::tempdir().unwrap();
    let allocator_policy_log = temp.path().join("allocator-policy.log");
    let _repo = TestRepo::new_with_daemon_env(&[
        (
            "GIT_AI_TEST_ALLOCATOR_POLICY_LOG",
            allocator_policy_log.to_str().unwrap(),
        ),
        ("MALLOC_ARENA_MAX", "2"),
    ]);

    assert_eq!(
        fs::read_to_string(&allocator_policy_log).unwrap_or_default(),
        "arena_max=2\n",
        "the long-lived daemon must constrain glibc arena retention before starting worker threads"
    );
}

#[test]
fn repeated_agent_commits_reuse_the_daemon_helper_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let runtime_build_log = temp.path().join("runtime-builds.log");
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_TOKIO_RUNTIME_BUILD_LOG",
        runtime_build_log.to_str().unwrap(),
    )]);
    let path = repo.path().join("crew-state.txt");

    fs::write(&path, "base\n").unwrap();
    let initial = repo.stage_all_and_commit("base").unwrap();
    repo.assert_commit_links(&initial.commit_sha, &[]);
    let runtime_builds_before_agent_commits = runtime_build_count(&runtime_build_log);
    for index in 0..3 {
        fs::write(&path, format!("agent state {index}\n")).unwrap();
        crate::test_utils::codex_checkpoint(
            &repo,
            &path,
            "runtime-session",
            "PostToolUse",
            &format!("write-{index}"),
        );
        let commit = repo
            .stage_all_and_commit(&format!("agent state {index}"))
            .unwrap();
        repo.assert_commit_links(&commit.commit_sha, &[("codex", "runtime-session")]);
    }

    let helper_runtimes_built =
        runtime_build_count(&runtime_build_log) - runtime_builds_before_agent_commits;
    assert!(
        helper_runtimes_built <= 1,
        "the daemon must reuse one bounded helper runtime across commits; built {helper_runtimes_built}"
    );
}
