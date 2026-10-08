//! Comprehensive tests for src/git/repository.rs
//!
//! This test suite covers the core git operations layer including:
//! - Repository initialization and discovery
//! - Git command execution and error handling
//! - HEAD operations and branch management
//! - Commit operations and traversal
//! - Config get/set operations
//! - Pathspec validation and filtering
//! - Rewrite log operations
//! - Error handling and edge cases
//! - Working directory operations
//! - Bare repository support

use crate::repos::test_repo::TestRepo;
use git_ai::git::repository::{find_repository, find_repository_in_path};
use std::fs;
use std::path::Path;

// ============================================================================
// Repository Discovery and Initialization Tests
// ============================================================================

#[test]
fn test_find_repository_in_valid_repo() {
    let repo = TestRepo::new();

    // Create a commit to ensure it's a valid repo
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Should successfully find repository
    let found_repo =
        find_repository(&["-C".to_string(), repo.path().to_str().unwrap().to_string()]);

    assert!(found_repo.is_ok(), "Should find valid repository");
}

#[test]
fn test_find_repository_in_subdirectory() {
    let repo = TestRepo::new();

    // Create subdirectory
    let subdir = repo.path().join("subdir");
    fs::create_dir(&subdir).unwrap();

    // Should find repository from subdirectory
    let found_repo = find_repository(&["-C".to_string(), subdir.to_str().unwrap().to_string()]);

    assert!(
        found_repo.is_ok(),
        "Should find repository from subdirectory"
    );
}

#[test]
fn test_find_repository_in_nested_subdirectory() {
    let repo = TestRepo::new();

    // Create nested subdirectories
    let nested = repo.path().join("a").join("b").join("c");
    fs::create_dir_all(&nested).unwrap();

    // Should find repository from deeply nested subdirectory
    let found_repo = find_repository(&["-C".to_string(), nested.to_str().unwrap().to_string()]);

    assert!(
        found_repo.is_ok(),
        "Should find repository from nested subdirectory"
    );
}

#[test]
fn test_find_repository_for_bare_repo() {
    let bare_repo = TestRepo::new_bare();

    let found_repo = find_repository(&[
        "-C".to_string(),
        bare_repo.path().to_str().unwrap().to_string(),
    ]);

    assert!(found_repo.is_ok(), "Should find bare repository");

    let repo = found_repo.unwrap();
    assert!(
        repo.is_bare_repository().unwrap(),
        "Should detect bare repository"
    );
}

#[test]
fn test_repository_path_methods() {
    let test_repo = TestRepo::new();
    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    // path() should always point at a valid git directory.
    let git_path = repo.path();
    assert!(git_path.is_dir(), "path() should return a git directory");
    if git_path == repo.common_dir() {
        assert!(
            git_path.ends_with(".git"),
            "non-worktree path() should return .git directory"
        );
    } else {
        assert!(
            git_path.to_string_lossy().contains("/worktrees/")
                || git_path
                    .components()
                    .any(|c| c.as_os_str() == std::ffi::OsStr::new("worktrees")),
            "worktree path() should resolve to a linked worktree git dir"
        );
    }

    // Test workdir() returns repository root (use canonical paths for macOS /var vs /private/var)
    let workdir = repo.workdir().unwrap();
    let canonical_workdir = workdir.canonicalize().unwrap();
    let canonical_test_path = test_repo.path().canonicalize().unwrap();
    assert_eq!(
        canonical_workdir, canonical_test_path,
        "workdir() should return repository root"
    );
}

#[test]
fn test_path_is_in_workdir() {
    let test_repo = TestRepo::new();
    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    // Path inside workdir - create the file so it can be canonicalized
    let inside = test_repo.path().join("file.txt");
    fs::write(&inside, "test content").unwrap();
    assert!(
        repo.path_is_in_workdir(&inside),
        "File in workdir should return true"
    );

    // Path outside workdir
    let outside = Path::new("/tmp/outside.txt");
    assert!(
        !repo.path_is_in_workdir(outside),
        "File outside workdir should return false"
    );

    // Path inside a nested subrepo (has its own .git/ directory) should return false
    let nested_repo_dir = test_repo.path().join("nested-repo");
    fs::create_dir_all(nested_repo_dir.join("src")).unwrap();
    // Initialize a real git repo in the nested directory
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&nested_repo_dir)
        .output()
        .expect("failed to git init nested repo");
    let nested_file = nested_repo_dir.join("src").join("nested.txt");
    fs::write(&nested_file, "nested content").unwrap();
    assert!(
        !repo.path_is_in_workdir(&nested_file),
        "File inside a nested subrepo (with its own .git/ dir) should return false"
    );

    // Path directly in the nested repo root should also return false
    let nested_root_file = nested_repo_dir.join("root.txt");
    fs::write(&nested_root_file, "root content").unwrap();
    assert!(
        !repo.path_is_in_workdir(&nested_root_file),
        "File at root of nested subrepo should return false"
    );

    // Path in a subdirectory (no nested .git/) should still return true
    let subdir = test_repo.path().join("regular-subdir");
    fs::create_dir_all(&subdir).unwrap();
    let subdir_file = subdir.join("file.txt");
    fs::write(&subdir_file, "subdir content").unwrap();
    assert!(
        repo.path_is_in_workdir(&subdir_file),
        "File in a regular subdirectory (no .git/) should return true"
    );

    // Path inside a submodule (.git file, not directory) should return true
    // Submodules are transparent to the parent repo
    let submodule_dir = test_repo.path().join("my-submodule");
    fs::create_dir_all(submodule_dir.join("src")).unwrap();
    // Simulate a submodule by creating a .git *file* (not directory)
    fs::write(
        submodule_dir.join(".git"),
        "gitdir: ../.git/modules/my-submodule\n",
    )
    .unwrap();
    let submodule_file = submodule_dir.join("src").join("lib.rs");
    fs::write(&submodule_file, "submodule content").unwrap();
    assert!(
        repo.path_is_in_workdir(&submodule_file),
        "File inside a submodule (.git file, not directory) should return true"
    );

    // Non-existent file path inside a nested subrepo should return false
    // (exercises the normalized fallback path since canonicalize() will fail)
    let nonexistent = nested_repo_dir.join("does-not-exist").join("phantom.txt");
    assert!(
        !repo.path_is_in_workdir(&nonexistent),
        "Non-existent file inside a nested subrepo should return false (fallback path)"
    );

    // Non-existent file path in the repo (no nested .git) should return true
    let nonexistent_in_repo = test_repo.path().join("not-yet-created.txt");
    assert!(
        repo.path_is_in_workdir(&nonexistent_in_repo),
        "Non-existent file in the repo (no nested .git/) should return true (fallback path)"
    );
}

// ============================================================================
// HEAD and Reference Tests
// ============================================================================

#[test]
fn test_head_on_main_branch() {
    let test_repo = TestRepo::new();

    // Create initial commit
    let mut file = test_repo.filename("test.txt");
    file.set_contents(crate::lines!["content"]);
    test_repo.stage_all_and_commit("Initial commit").unwrap();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let head = repo.head().unwrap();
    let name = head.name().unwrap();

    // Should be on main or master
    assert!(
        name.contains("main") || name.contains("master"),
        "HEAD should be on main/master branch, got: {}",
        name
    );
}

#[test]
fn test_head_on_feature_branch() {
    let test_repo = TestRepo::new();

    // Create initial commit
    let mut file = test_repo.filename("test.txt");
    file.set_contents(crate::lines!["content"]);
    test_repo.stage_all_and_commit("Initial commit").unwrap();

    // Create and checkout feature branch
    test_repo.git(&["checkout", "-b", "feature"]).unwrap();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let head = repo.head().unwrap();
    let shorthand = head.shorthand().unwrap();

    assert_eq!(shorthand, "feature", "HEAD should be on feature branch");
}

#[test]
fn test_head_target() {
    let test_repo = TestRepo::new();

    // Create commit
    let mut file = test_repo.filename("test.txt");
    file.set_contents(crate::lines!["content"]);
    let commit = test_repo.stage_all_and_commit("Initial commit").unwrap();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let head = repo.head().unwrap();
    let target = head.target().unwrap();

    assert_eq!(
        target, commit.commit_sha,
        "HEAD target should match commit SHA"
    );
}

// ============================================================================
// Commit Operations and Traversal Tests
// ============================================================================

// ============================================================================
// Tree and Blob Tests
// ============================================================================

// ============================================================================
// Config Operations Tests
// ============================================================================

#[test]
fn test_config_get_str() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    // Get user.name which is set in test repo
    let name = repo.config_get_str("user.name");
    assert!(name.is_ok(), "Should get config value");

    let name = name.unwrap();
    assert!(name.is_some(), "user.name should be set");
    assert_eq!(
        name.unwrap(),
        "Test User",
        "user.name should be 'Test User'"
    );
}

#[test]
fn test_config_get_str_nonexistent() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    // Get nonexistent config
    let result = repo.config_get_str("nonexistent.config.key");
    assert!(result.is_ok(), "Should not error on nonexistent key");

    let value = result.unwrap();
    assert!(value.is_none(), "Nonexistent key should return None");
}

#[test]
fn test_config_get_regexp() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    // Get all user.* configs
    let configs = repo.config_get_regexp("user\\..*");
    assert!(configs.is_ok(), "Should get matching configs");

    let configs = configs.unwrap();
    assert!(
        !configs.is_empty(),
        "Should have at least one user.* config"
    );
    assert!(
        configs.contains_key("user.name"),
        "Should contain user.name"
    );
}

#[test]
fn test_git_version() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let version = repo.git_version();
    assert!(version.is_some(), "Should get git version");

    let (major, _minor, _patch) = version.unwrap();
    assert!(major >= 2, "Git major version should be at least 2");
}

// ============================================================================
// Remote Operations Tests
// ============================================================================

#[test]
fn test_remotes_empty() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let remotes = repo.remotes().unwrap();
    assert!(
        remotes.is_empty() || remotes == vec!["".to_string()],
        "New repo should have no remotes"
    );
}

#[test]
fn test_remotes_with_origin() {
    let (mirror, _upstream) = TestRepo::new_with_remote();

    let repo = find_repository(&[
        "-C".to_string(),
        mirror.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let remotes = repo.remotes().unwrap();
    assert!(
        remotes.contains(&"origin".to_string()),
        "Cloned repo should have origin remote"
    );
}

#[test]
fn test_remotes_with_urls() {
    let (mirror, _upstream) = TestRepo::new_with_remote();

    let repo = find_repository(&[
        "-C".to_string(),
        mirror.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let remotes_with_urls = repo.remotes_with_urls().unwrap();
    assert!(
        !remotes_with_urls.is_empty(),
        "Should have remotes with URLs"
    );

    let has_origin = remotes_with_urls
        .iter()
        .any(|(name, _url)| name == "origin");
    assert!(has_origin, "Should have origin remote with URL");
}

#[test]
fn test_get_default_remote() {
    let (mirror, _upstream) = TestRepo::new_with_remote();

    let repo = find_repository(&[
        "-C".to_string(),
        mirror.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let default_remote = repo.get_default_remote().unwrap();
    assert!(default_remote.is_some(), "Should have default remote");
    assert_eq!(
        default_remote.unwrap(),
        "origin",
        "Default remote should be origin"
    );
}

#[test]
fn test_get_default_remote_no_remotes() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let default_remote = repo.get_default_remote().unwrap();
    // New repos might have an empty string as a remote or None
    assert!(
        default_remote.is_none() || default_remote == Some("".to_string()),
        "Repo without remotes should have no default or empty default"
    );
}

// ============================================================================
// Commit Range Tests
// ============================================================================

// ============================================================================
// Merge Base Tests
// ============================================================================

// ============================================================================
// File Content Tests
// ============================================================================

// ============================================================================
// Error Handling Tests
// ============================================================================

// ============================================================================
// Bare Repository Tests
// ============================================================================

#[test]
fn test_is_bare_repository() {
    let bare_repo = TestRepo::new_bare();

    let repo = find_repository(&[
        "-C".to_string(),
        bare_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let is_bare = repo.is_bare_repository();
    assert!(is_bare.is_ok(), "Should check if bare");
    assert!(is_bare.unwrap(), "Should be bare repository");
}

#[test]
fn test_is_not_bare_repository() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let is_bare = repo.is_bare_repository();
    assert!(is_bare.is_ok(), "Should check if bare");
    assert!(!is_bare.unwrap(), "Should not be bare repository");
}

// ============================================================================
// Working Directory Operations Tests
// ============================================================================

#[test]
fn test_find_repository_in_path() {
    let test_repo = TestRepo::new();

    // Create a commit
    let mut file = test_repo.filename("test.txt");
    file.set_contents(crate::lines!["content"]);
    test_repo.stage_all_and_commit("Test").unwrap();

    let result = find_repository_in_path(test_repo.path().to_str().unwrap());
    assert!(result.is_ok(), "Should find repository in path");
}

#[test]
fn test_global_args_for_exec() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    let args = repo.global_args_for_exec();

    // Should include --no-pager
    assert!(
        args.contains(&"--no-pager".to_string()),
        "Global args should include --no-pager"
    );
}

// ============================================================================
// Edge Cases and Special Scenarios
// ============================================================================

#[test]
fn test_empty_repository() {
    let test_repo = TestRepo::new();

    let repo = find_repository(&[
        "-C".to_string(),
        test_repo.path().to_str().unwrap().to_string(),
    ])
    .unwrap();

    // HEAD should exist even in empty repo
    let head = repo.head();
    assert!(head.is_ok(), "Should get HEAD in empty repository");
}

crate::reuse_tests_in_worktree!(
    test_find_repository_in_valid_repo,
    test_find_repository_in_subdirectory,
    test_find_repository_in_nested_subdirectory,
    test_find_repository_for_bare_repo,
    test_repository_path_methods,
    test_path_is_in_workdir,
    test_head_on_main_branch,
    test_head_on_feature_branch,
    test_head_target,
    test_config_get_str,
    test_config_get_str_nonexistent,
    test_config_get_regexp,
    test_git_version,
    test_remotes_empty,
    test_remotes_with_origin,
    test_remotes_with_urls,
    test_get_default_remote,
    test_get_default_remote_no_remotes,
    test_is_bare_repository,
    test_is_not_bare_repository,
    test_find_repository_in_path,
    test_global_args_for_exec,
    test_empty_repository,
);
