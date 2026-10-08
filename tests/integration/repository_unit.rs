use git_ai::git::repository::{find_repository_in_path, resolve_command_base_dir};
use std::fs;
use std::path::Path;
use std::process::Command;

use std::sync::OnceLock;

fn init_test_git_config() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let path = std::env::temp_dir().join("git-ai-test-global-gitconfig");
        let _ = fs::write(
            &path,
            "[user]\n\tname = Test User\n\temail = test@example.com\n",
        );
        #[cfg(not(windows))]
        let canonical = path.canonicalize().unwrap_or(path);
        #[cfg(windows)]
        let canonical = path;
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &canonical);
            #[cfg(not(windows))]
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }
    });
}

fn run_git(cwd: &Path, args: &[&str]) {
    init_test_git_config();
    let output = Command::new(git_ai::config::Config::get().git_cmd())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn resolve_command_base_dir_applies_chained_c_arguments() {
    let temp = tempfile::tempdir().expect("tempdir");
    let base = temp.path().join("root");
    let args = vec![
        "-C".to_string(),
        base.to_string_lossy().to_string(),
        "-C".to_string(),
        "nested".to_string(),
        "-C".to_string(),
        "..".to_string(),
        "-C".to_string(),
        "repo".to_string(),
        "status".to_string(),
    ];

    let resolved = resolve_command_base_dir(&args).expect("resolve base dir");
    assert_eq!(resolved, base.join("nested").join("..").join("repo"));
}

#[test]
fn find_repository_in_path_supports_bare_repositories() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    let bare = temp.path().join("repo.git");
    fs::create_dir_all(&source).expect("create source");

    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.name", "Test User"]);
    run_git(&source, &["config", "user.email", "test@example.com"]);
    fs::write(source.join("README.md"), "# repo\n").expect("write readme");
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "initial"]);
    run_git(
        temp.path(),
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );

    let repo = find_repository_in_path(bare.to_str().unwrap()).expect("find bare repo");
    assert!(repo.is_bare_repository().expect("bare check"));
    assert_eq!(
        repo.path().canonicalize().expect("canonical bare"),
        bare.canonicalize().expect("canonical path")
    );

    let discovered = git_ai::git::repository::discover_repository_in_path_no_git_exec(&bare)
        .expect("discover bare repo");
    assert_eq!(
        discovered.path().canonicalize().expect("canonical bare"),
        bare.canonicalize().expect("canonical path")
    );
}

#[test]
fn path_is_in_workdir_returns_false_for_linked_worktree_file() {
    // Sibling worktree: the worktree lives OUTSIDE the main repo's working tree.
    // path_is_in_workdir returns false purely because the path doesn't
    // start_with(workdir) — no .git file inspection is needed.  This test
    // passes even without the is_linked_worktree_git_file fix.
    let temp = tempfile::tempdir().expect("tempdir");
    let main_repo = temp.path().join("main");
    let worktree = temp.path().join("linked");

    fs::create_dir_all(&main_repo).expect("create main repo dir");
    run_git(&main_repo, &["init"]);
    run_git(&main_repo, &["config", "user.name", "Test User"]);
    run_git(&main_repo, &["config", "user.email", "test@example.com"]);
    // Write a file so the sanity-check path exists on disk — path_is_in_workdir
    // calls path.canonicalize() which only resolves symlinks for existing paths
    // (on macOS /var/... is a symlink to /private/var/...; on Windows temp paths
    // may use short names that differ from the canonical workdir stored by git).
    fs::write(main_repo.join("README.md"), "# main\n").expect("write README");
    run_git(&main_repo, &["worktree", "add", worktree.to_str().unwrap()]);

    let dot_git = worktree.join(".git");
    assert!(
        dot_git.is_file(),
        ".git should be a file in a linked worktree"
    );

    let main = find_repository_in_path(main_repo.to_str().unwrap()).expect("find main repo");

    let wt_file = worktree.join("somefile.rs");
    assert!(
        !main.path_is_in_workdir(&wt_file),
        "sibling linked worktree file should not be in main repo workdir"
    );

    // Use an existing file so path.canonicalize() resolves symlinks correctly.
    let main_file = main_repo.join("README.md");
    assert!(
        main.path_is_in_workdir(&main_file),
        "main repo file should be in main repo workdir"
    );
}

#[test]
fn path_is_in_workdir_returns_false_for_nested_linked_worktree_file() {
    // Nested worktree: the worktree lives INSIDE the main repo's working tree
    // (e.g. main_repo/.worktrees/feature).  This is the exact Bug-A / Bug-B
    // scenario: path starts_with(workdir) so the starts_with check passes,
    // and only is_linked_worktree_git_file makes path_is_in_workdir return
    // false.  This test FAILS without the fix.
    let temp = tempfile::tempdir().expect("tempdir");
    let main_repo = temp.path().join("main");
    let worktree = main_repo.join(".worktrees").join("feature");

    fs::create_dir_all(&main_repo).expect("create main repo dir");
    run_git(&main_repo, &["init"]);
    run_git(&main_repo, &["config", "user.name", "Test User"]);
    run_git(&main_repo, &["config", "user.email", "test@example.com"]);
    // git worktree add requires at least one commit
    fs::write(main_repo.join("README.md"), "# test\n").expect("write README");
    run_git(&main_repo, &["add", "."]);
    run_git(&main_repo, &["commit", "-m", "initial"]);
    run_git(
        &main_repo,
        &["worktree", "add", "--detach", worktree.to_str().unwrap()],
    );

    let dot_git = worktree.join(".git");
    assert!(
        dot_git.is_file(),
        ".git should be a file in a nested worktree"
    );
    let gitfile_content = fs::read_to_string(&dot_git).expect("read .git file");
    assert!(
        gitfile_content.contains("/worktrees/"),
        ".git file should reference /worktrees/: {}",
        gitfile_content.trim()
    );

    let main = find_repository_in_path(main_repo.to_str().unwrap()).expect("find main repo");

    // The nested worktree file is physically under main_repo/ but must NOT
    // be reported as part of the main repo's working tree.
    let wt_file = worktree.join("somefile.rs");
    assert!(
        !main.path_is_in_workdir(&wt_file),
        "nested linked worktree file should not be in main repo workdir \
         (path starts_with workdir, but .git file marks a repo boundary)"
    );

    // Sanity: file is in the worktree's own workdir.
    let wt_repo =
        find_repository_in_path(worktree.to_str().unwrap()).expect("find nested worktree");
    assert!(
        wt_repo.path_is_in_workdir(&wt_file),
        "nested worktree file should be in the worktree's own workdir"
    );

    // Sanity: a normal file in the main repo is still in the main workdir.
    // Use README.md which already exists so path.canonicalize() resolves
    // symlinks correctly (macOS /var/... → /private/var/...; Windows short names).
    let main_file = main_repo.join("README.md");
    assert!(
        main.path_is_in_workdir(&main_file),
        "main repo file should be in main repo workdir"
    );
}
