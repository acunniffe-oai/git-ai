#[macro_use]
#[path = "integration/repos/mod.rs"]
mod repos;

use git_ai::commit_links::checkpoint::{CheckpointSignal, LinkCheckpoint};
use git_ai::commit_links::store::{LinkStore, SessionIdentity};
#[cfg(not(windows))]
use git_ai::daemon::ControlResponse;
use git_ai::daemon::send_checkpoint_request_with_timeout;
use git_ai::daemon::send_control_request_with_timeout;
use git_ai::daemon::{
    ControlRequest, DaemonConfig, DaemonLock, local_socket_connects_with_timeout,
    open_local_socket_stream_with_timeout, read_daemon_pid, send_control_request,
};
#[cfg(not(windows))]
use repos::test_repo::live_pid_trace_sid;
use repos::test_repo::{
    DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS, DaemonTestCompletionLogEntry, DaemonTestScope, TestRepo,
    get_binary_path, is_windows_loader_init_failure, real_git_executable, trace_atexit_frame,
    write_trace_frames,
};
use serde_json::Value;
use serde_json::json;
use serial_test::serial;
use std::fs;
#[cfg(not(windows))]
use std::io::{BufRead, BufReader};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

macro_rules! native_checkpoint_args {
    ($path:expr) => { ["checkpoint", "agent-v1", "--hook-input", &serde_json::json!({
        "type":"ai_agent", "repo_working_dir":".", "agent_name":"codex",
        "conversation_id":"daemon-test-session", "edited_filepaths":[$path]
    }).to_string()] };
}

fn checkpoint_request(repo: &TestRepo, request_id: String, session_id: String) -> LinkCheckpoint {
    LinkCheckpoint {
        request_id,
        repo_working_dir: repo.canonical_path(),
        session: SessionIdentity::new("codex", &session_id).unwrap(),
        generation: 0,
        signal: CheckpointSignal::PostWrite,
    }
}

fn persisted_checkpoint_count(database_repo: &TestRepo, scope: &TestRepo) -> usize {
    let family = git_ai::git::repo_state::common_dir_for_worktree(scope.path())
        .unwrap()
        .canonicalize()
        .unwrap();
    let connection =
        git_ai::sqlite::open_with_memory_limits(database_repo.link_store_path_for_test()).unwrap();
    connection.query_row("SELECT count(*) FROM link_operations WHERE family=?1 AND operation_id LIKE 'checkpoint:%'", [family.to_string_lossy().as_ref()], |row| row.get(0)).unwrap_or(0)
}

fn has_commit_links(repo: &TestRepo, sha: &str) -> bool {
    let family = git_ai::git::repo_state::common_dir_for_worktree(repo.path())
        .unwrap()
        .canonicalize()
        .unwrap();
    let store = LinkStore::open_at(&repo.link_store_path_for_test()).unwrap();
    !store
        .sessions_for_commit(&family.to_string_lossy(), sha)
        .unwrap()
        .is_empty()
}

const DAEMON_TEST_PROBE_TIMEOUT: Duration = Duration::from_millis(100);

/// Outcome of a failed `DaemonGuard` readiness wait: a transient Windows loader
/// hiccup (respawn) versus a genuine failure (fail loudly).
enum DaemonReadyOutcome {
    LoaderInitFailure(String),
    Fatal(String),
}

fn daemon_control_socket_path(repo: &TestRepo) -> PathBuf {
    repo.daemon_control_socket_path()
}

fn daemon_trace_socket_path(repo: &TestRepo) -> PathBuf {
    repo.daemon_trace_socket_path()
}

fn daemon_lock_path(repo: &TestRepo) -> PathBuf {
    DaemonConfig::from_home(&repo.daemon_home_path()).lock_path
}

#[cfg(not(windows))]
fn wait_for_daemon_log(repo: &TestRepo, needle: &str) -> String {
    let started = std::time::Instant::now();
    loop {
        let logs = repo.daemon_stderr_contents();
        if logs.contains(needle) || started.elapsed() >= Duration::from_secs(2) {
            return logs;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[allow(clippy::zombie_processes)]
fn start_daemon_for_repo(repo: &TestRepo) {
    let daemon_home = repo.daemon_home_path();
    let control_socket_path = daemon_control_socket_path(repo);
    let trace_socket_path = daemon_trace_socket_path(repo);
    let mut command = Command::new(get_binary_path());
    command
        .arg("bg")
        .arg("run")
        .current_dir(repo.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_test_home_env(&mut command, repo.test_home_path());
    configure_test_daemon_env(
        &mut command,
        &daemon_home,
        &control_socket_path,
        &trace_socket_path,
    );
    command.spawn().expect("failed to spawn daemon for repo");

    let repo_workdir = repo_workdir_string(repo);
    for _ in 0..200 {
        if send_control_request(
            &control_socket_path,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir.clone(),
            },
        )
        .is_ok()
            && local_socket_connects_with_timeout(&trace_socket_path, DAEMON_TEST_PROBE_TIMEOUT)
                .is_ok()
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "daemon did not become ready at {}",
        control_socket_path.display()
    );
}

fn get_rss_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb_str = rest.trim().trim_end_matches(" kB").trim();
            return kb_str.parse().ok();
        }
    }
    None
}

fn send_trace_frames(trace_socket_path: &Path, payloads: &[Value]) {
    let mut stream =
        open_local_socket_stream_with_timeout(trace_socket_path, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to connect to trace socket");
    write_trace_frames(&mut stream, payloads);
}

/// Waits until the repo's dedicated daemon logs that a delayed side-effect
/// pass (GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND) has started executing,
/// so grind tests gate on the actual daemon-side precondition instead of
/// fixed sleeps that can pass vacuously or fail spuriously on loaded runners.
#[cfg(not(windows))]
fn wait_for_test_side_effect_delay_started(repo: &TestRepo) {
    let started = std::time::Instant::now();
    loop {
        if repo
            .daemon_stderr_contents()
            .contains("test side-effect delay started")
        {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "daemon never entered the delayed side-effect pass; logs:\n{}",
            repo.daemon_stderr_contents()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn repo_workdir_string(repo: &TestRepo) -> String {
    repo.path().to_string_lossy().to_string()
}

struct MockApiServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<Value>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockApiServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock API server");
        listener
            .set_nonblocking(true)
            .expect("failed to set nonblocking listener");
        let addr = listener.local_addr().expect("failed to read listener addr");
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        let thread = thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_http_connection(stream, &tx);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock API accept failed: {}", error),
                }
            }
        });

        Self {
            base_url: format!("http://{}", addr),
            stop,
            rx,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Collect all requests captured by the mock so far.
    fn collect_requests(&mut self) -> Vec<Value> {
        let mut requests = Vec::new();
        while let Ok(request) = self.rx.try_recv() {
            requests.push(request);
        }
        requests
    }
}

impl Drop for MockApiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_http_connection(mut stream: TcpStream, tx: &mpsc::Sender<Value>) {
    let Some((path, body)) = read_http_request(&mut stream) else {
        return;
    };

    let request_json: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));

    let response_body = match path.as_str() {
        "/worker/cas/upload" => {
            let _ = tx.send(json!({ "path": path, "body": request_json }));
            let hashes = request_json["objects"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|object| object["hash"].as_str().map(|hash| hash.to_string()))
                .collect::<Vec<_>>();
            json!({
                "results": hashes.iter().map(|hash| {
                    json!({
                        "hash": hash,
                        "status": "ok"
                    })
                }).collect::<Vec<_>>(),
                "success_count": hashes.len(),
                "failure_count": 0
            })
            .to_string()
        }
        "/worker/metrics/upload" => {
            let _ = tx.send(json!({ "path": path, "body": request_json }));
            json!({ "errors": [] }).to_string()
        }
        "/worker/logs/upload" => {
            let accepted = request_json["events"].as_array().map_or(0, Vec::len);
            let _ = tx.send(json!({ "path": path, "body": request_json }));
            json!({
                "accepted": accepted,
                "dropped": 0,
                "enqueued": true,
                "errors": []
            })
            .to_string()
        }
        "/worker/notes/upload" => {
            let _ = tx.send(json!({ "path": path, "body": request_json }));
            let success_count = request_json["entries"]
                .as_array()
                .map(|entries| entries.len())
                .unwrap_or(0);
            json!({
                "success_count": success_count,
                "failure_count": 0
            })
            .to_string()
        }
        _ => "{}".to_string(),
    };

    write_http_response(&mut stream, response_body.as_bytes());
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("failed to set mock API read timeout");

    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&buffer) {
            break end;
        }
    };

    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let request_line = headers.lines().next()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
        })
        .unwrap_or(0);

    while buffer.len() - header_end < content_length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Some((
        path,
        buffer[header_end..header_end + content_length].to_vec(),
    ))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
}

fn write_http_response(stream: &mut TcpStream, body: &[u8]) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("failed to write mock API response headers");
    stream
        .write_all(body)
        .expect("failed to write mock API response body");
    stream.flush().expect("failed to flush mock API response");
}

fn configure_test_home_env(command: &mut Command, test_home: &Path) {
    command.env("HOME", test_home);
    command.env("GIT_CONFIG_GLOBAL", test_home.join(".gitconfig"));
    // Redirect XDG_CONFIG_HOME so git does not read the real user's
    // $XDG_CONFIG_HOME/git/config (which may contain filter drivers,
    // aliases, or other settings that break test isolation).
    command.env("XDG_CONFIG_HOME", test_home.join(".config"));
    // Suppress system-level git config (e.g., Xcode credential helpers)
    // that could interfere with test isolation.
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    // Sanitize PATH to remove directories containing the Nix git-ai
    // wrapper.  When the wrapper (a release build) runs with HOME
    // pointing to the test home it starts a background daemon at
    // the test socket path, poisoning the test environment.
    if let Ok(path) = std::env::var("PATH") {
        let sanitized: Vec<&str> = path
            .split(':')
            .filter(|dir| {
                // Keep only dirs that do NOT contain a git-ai wrapper
                // (heuristic: skip dirs where the `git` binary is a
                //  shell-script wrapper for git-ai, or a symlink to git-ai).
                let git_path = std::path::Path::new(dir).join("git");
                if git_path.is_file() || git_path.is_symlink() {
                    if let Ok(contents) = std::fs::read_to_string(&git_path)
                        && contents.contains("git-ai")
                    {
                        return false;
                    }
                    if let Ok(target) = std::fs::read_link(&git_path)
                        && target.to_string_lossy().contains("git-ai")
                    {
                        return false;
                    }
                    if let Ok(canonical) = git_path.canonicalize()
                        && canonical.to_string_lossy().contains("git-ai")
                    {
                        return false;
                    }
                }
                true
            })
            .collect();
        command.env("PATH", sanitized.join(":"));
    }
    #[cfg(windows)]
    {
        command.env("USERPROFILE", test_home);
        command.env("APPDATA", test_home.join("AppData").join("Roaming"));
        command.env("LOCALAPPDATA", test_home.join("AppData").join("Local"));
    }
}

fn configure_test_daemon_env(
    command: &mut Command,
    daemon_home: &Path,
    control_socket_path: &Path,
    trace_socket_path: &Path,
) {
    command.env("GIT_AI_DAEMON_HOME", daemon_home);
    command.env("GIT_AI_DAEMON_CONTROL_SOCKET", control_socket_path);
    command.env("GIT_AI_DAEMON_TRACE_SOCKET", trace_socket_path);
}

/// Cleanup for self-restarted replacement daemons that no `DaemonGuard`
/// owns: sends a best-effort Shutdown on drop so an assertion failure in the
/// test body cannot strand a daemon (max uptime is 24h) on the machine.
/// Gated like its users (the self-restart tests are unix-only): an unused
/// struct fails the Windows dead-code lint.
#[cfg(not(windows))]
struct StrayDaemonGuard {
    control_socket_path: PathBuf,
}

#[cfg(not(windows))]
impl StrayDaemonGuard {
    fn for_repo(repo: &TestRepo) -> Self {
        Self {
            control_socket_path: daemon_control_socket_path(repo),
        }
    }
}

#[cfg(not(windows))]
impl Drop for StrayDaemonGuard {
    fn drop(&mut self) {
        let _ = send_control_request(&self.control_socket_path, &ControlRequest::Shutdown);
    }
}

struct DaemonGuard {
    child: Child,
    control_socket_path: PathBuf,
    trace_socket_path: PathBuf,
    repo_working_dir: String,
    stderr_log_path: PathBuf,
}

impl DaemonGuard {
    fn start(repo: &TestRepo) -> Self {
        Self::start_with_env(repo, &[])
    }

    fn start_with_env(repo: &TestRepo, extra_env: &[(&str, &str)]) -> Self {
        let daemon_home = repo.daemon_home_path();
        let control_socket_path = daemon_control_socket_path(repo);
        let trace_socket_path = daemon_trace_socket_path(repo);
        let stderr_log_path = daemon_home.join("daemon-guard.stderr.log");
        fs::create_dir_all(&daemon_home).expect("failed to create daemon test home");
        let stderr_log = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&stderr_log_path)
            .expect("failed to create daemon stderr log");
        let mut command = Command::new(get_binary_path());
        command
            .arg("bg")
            .arg("run")
            .current_dir(repo.path())
            .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
            .env("GITAI_TEST_DB_PATH", repo.test_db_path())
            .stdout(Stdio::null())
            .stderr(
                stderr_log
                    .try_clone()
                    .expect("failed to clone daemon stderr log"),
            );
        for (key, value) in extra_env {
            command.env(key, value);
        }
        configure_test_home_env(&mut command, repo.test_home_path());
        configure_test_daemon_env(
            &mut command,
            &daemon_home,
            &control_socket_path,
            &trace_socket_path,
        );

        // Respawn loop: a Windows `STATUS_DLL_INIT_FAILED` exit means the OS
        // loader never started the daemon process (a hosted-Windows-runner
        // hiccup), so retry. Any other early exit / timeout panics immediately.
        let mut attempt = 0;
        loop {
            let child = command.spawn().expect("failed to spawn git-ai subprocess");
            let mut daemon = Self {
                child,
                control_socket_path: control_socket_path.clone(),
                trace_socket_path: trace_socket_path.clone(),
                repo_working_dir: repo_workdir_string(repo),
                stderr_log_path: stderr_log_path.clone(),
            };
            match daemon.wait_until_ready() {
                Ok(()) => return daemon,
                Err(DaemonReadyOutcome::LoaderInitFailure(message)) => {
                    let _ = daemon.child.kill();
                    let _ = daemon.child.wait();
                    attempt += 1;
                    if attempt < DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS {
                        eprintln!(
                            "[test-harness] daemon loader init failed (attempt {}/{}), respawning: {}",
                            attempt, DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS, message
                        );
                        continue;
                    }
                    panic!("{}", message);
                }
                Err(DaemonReadyOutcome::Fatal(message)) => {
                    let _ = daemon.child.kill();
                    let _ = daemon.child.wait();
                    panic!("{}", message);
                }
            }
        }
    }

    fn wait_until_ready(&mut self) -> Result<(), DaemonReadyOutcome> {
        for _ in 0..200 {
            if let Some(status) = self
                .child
                .try_wait()
                .expect("failed to poll daemon process status")
            {
                let message = format!("daemon exited before becoming ready: {}", status);
                if is_windows_loader_init_failure(&status) {
                    return Err(DaemonReadyOutcome::LoaderInitFailure(message));
                }
                return Err(DaemonReadyOutcome::Fatal(message));
            }
            let status = send_control_request(
                &self.control_socket_path,
                &ControlRequest::StatusFamily {
                    repo_working_dir: self.repo_working_dir.clone(),
                },
            );
            if status.is_ok()
                && local_socket_connects_with_timeout(
                    &self.trace_socket_path,
                    DAEMON_TEST_PROBE_TIMEOUT,
                )
                .is_ok()
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Err(DaemonReadyOutcome::Fatal(format!(
            "daemon did not become ready at {}",
            self.control_socket_path.display()
        )))
    }

    fn shutdown(&mut self) {
        if self
            .child
            .try_wait()
            .expect("failed polling daemon process")
            .is_some()
        {
            return;
        }

        let _ = send_control_request(&self.control_socket_path, &ControlRequest::Shutdown);

        for _ in 0..200 {
            if self
                .child
                .try_wait()
                .expect("failed polling daemon process")
                .is_some()
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn stderr_contents(&self) -> String {
        fs::read_to_string(&self.stderr_log_path).unwrap_or_default()
    }
}

fn git_trace_env(trace_socket_path: &Path) -> [(&'static str, String); 2] {
    [
        (
            "GIT_TRACE2_EVENT",
            DaemonConfig::trace2_event_target_for_path(trace_socket_path),
        ),
        ("GIT_TRACE2_EVENT_NESTING", "0".to_string()),
    ]
}

fn traced_git_with_env(
    repo: &TestRepo,
    args: &[&str],
    envs: &[(&str, &str)],
    expected_top_level_completions: &mut u64,
) -> Result<String, String> {
    *expected_top_level_completions += 1;
    repo.git_og_with_env(args, envs)
}

fn wait_for_expected_top_level_completions(
    repo: &TestRepo,
    baseline: u64,
    expected_top_level_completions: u64,
) {
    repo.wait_for_daemon_total_completion_count(
        baseline,
        baseline.saturating_add(expected_top_level_completions),
    );
}

fn completion_entries_for_command(
    repo: &TestRepo,
    command: &str,
) -> Vec<DaemonTestCompletionLogEntry> {
    repo.daemon_completion_entries()
        .into_iter()
        .filter(|entry| entry.primary_command.as_deref() == Some(command))
        .collect()
}

#[derive(Clone)]
struct WorkdirRaceHarness {
    test_home: PathBuf,
    test_db_path: PathBuf,
    daemon_home: PathBuf,
    control_socket_path: PathBuf,
    trace_socket_path: PathBuf,
}

impl WorkdirRaceHarness {
    fn new(repo: &TestRepo, trace_socket_path: PathBuf) -> Self {
        Self {
            test_home: repo.test_home_path().to_path_buf(),
            test_db_path: repo.test_db_path().to_path_buf(),
            daemon_home: repo.daemon_home_path(),
            control_socket_path: repo.daemon_control_socket_path(),
            trace_socket_path,
        }
    }

    fn run_delegated_checkpoint(&self, workdir: &Path, file_rel: &str) -> u64 {
        let mut command = Command::new(get_binary_path());
        command
            .args(native_checkpoint_args!(file_rel))
            .current_dir(workdir);
        configure_test_home_env(&mut command, &self.test_home);
        configure_test_daemon_env(
            &mut command,
            &self.daemon_home,
            &self.control_socket_path,
            &self.trace_socket_path,
        );
        let output = command
            .env("GIT_AI_TEST_DB_PATH", &self.test_db_path)
            .env("GITAI_TEST_DB_PATH", &self.test_db_path)
            .env("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")
            .output()
            .expect("failed to execute delegated checkpoint");
        assert!(
            output.status.success(),
            "delegated checkpoint failed in {} for {} \nstdout:{}\nstderr:{}",
            workdir.display(),
            file_rel,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        repos::test_repo::parse_checkpoint_request_count(&String::from_utf8_lossy(&output.stdout))
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[test]
#[serial]
fn daemon_start_spawns_detached_run_process() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = Command::new(get_binary_path());
    command
        .arg("bg")
        .arg("start")
        .current_dir(repo.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path());
    configure_test_home_env(&mut command, repo.test_home_path());
    configure_test_daemon_env(
        &mut command,
        &repo.daemon_home_path(),
        &daemon_control_socket_path(&repo),
        &daemon_trace_socket_path(&repo),
    );
    let output = command.output().expect("failed to invoke daemon start");
    assert!(
        output.status.success(),
        "daemon start should return success: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut status_ok = false;
    for _ in 0..80 {
        match send_control_request(
            &daemon_control_socket_path(&repo),
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        ) {
            Ok(response) if response.ok => {
                status_ok = true;
                break;
            }
            _ => {
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
    assert!(status_ok, "daemon should be reachable after `daemon start`");

    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[test]
#[serial]
fn daemon_refuses_to_start_in_sandbox() {
    for (env_var, sandbox) in [
        ("CURSOR_SANDBOX", "Cursor"),
        ("SANDBOX_RUNTIME", "Claude Code"),
        ("CODEX_SANDBOX", "Codex"),
        ("CODEX_SANDBOX_NETWORK_DISABLED", "Codex"),
    ] {
        let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

        for subcommand in ["start", "run"] {
            let output = bg_command_with_env(&repo, subcommand, &[], &[(env_var, "1")]);
            if output.status.success() {
                let _ = send_control_request(
                    &daemon_control_socket_path(&repo),
                    &ControlRequest::Shutdown,
                );
            }

            assert!(
                !output.status.success(),
                "daemon {subcommand} should fail in the {sandbox} sandbox"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(&format!("{sandbox} sandbox")) && stderr.contains(env_var),
                "daemon {subcommand} should explain the sandbox refusal: {stderr}"
            );
        }

        assert!(
            send_control_request_with_timeout(
                &daemon_control_socket_path(&repo),
                &ControlRequest::Ping,
                DAEMON_TEST_PROBE_TIMEOUT,
            )
            .is_err(),
            "daemon control socket should not be available"
        );
        assert!(
            local_socket_connects_with_timeout(
                &daemon_trace_socket_path(&repo),
                DAEMON_TEST_PROBE_TIMEOUT,
            )
            .is_err(),
            "daemon trace socket should not be available"
        );
    }
}

#[test]
#[should_panic(expected = "pending daemon sync work")]
fn dedicated_daemon_restart_rejects_pending_traced_command_for_test() {
    let mut repo = TestRepo::new_dedicated_daemon();

    repo.git(&["commit", "--allow-empty", "-m", "base"])
        .expect("base commit should succeed");
    repo.git(&["branch", "pending-before-restart"])
        .expect("branch creation should succeed");

    repo.restart_dedicated_daemon_for_test();
}

#[test]
#[serial]
fn checkpoint_delegate_autostarts_daemon_when_unavailable() {
    // Test builds disable daemon auto-spawning from ensure_daemon_running to
    // prevent process storms. We verify that checkpoint delegation works by
    // restarting the daemon manually before the checkpoint call.
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    fs::write(repo.path().join("delegate-fallback.txt"), "base\n").expect("failed to write base");
    repo.git(&["add", "delegate-fallback.txt"])
        .expect("add should succeed");
    repo.stage_all_and_commit("base commit")
        .expect("base commit should succeed");

    fs::write(
        repo.path().join("delegate-fallback.txt"),
        "base\nchanged without daemon\n",
    )
    .expect("failed to write updated file");

    // Shut down any stale daemon, then restart it manually.
    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Manually restart the daemon (production auto-start is disabled in test builds)
    start_daemon_for_repo(&repo);

    let completion_baseline = repo.daemon_total_completion_count();
    repo.git_ai_with_env(
        &native_checkpoint_args!("delegate-fallback.txt"),
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("checkpoint should delegate to daemon and succeed");

    // Wait for the fire-and-forget checkpoint to complete
    repo.wait_for_next_daemon_checkpoint_completion(completion_baseline);

    let status = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    )
    .expect("daemon status request should succeed");
    assert!(
        status.ok,
        "daemon should be running after delegated checkpoint; ok={}, error={:?}, data={:?}, socket={}, workdir={}",
        status.ok,
        status.error,
        status.data,
        daemon_control_socket_path(&repo).display(),
        repo_workdir_string(&repo)
    );
    assert!(
        persisted_checkpoint_count(&repo, &repo) > 0,
        "delegated write should persist through the daemon"
    );

    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[test]
#[serial]
fn checkpoint_fails_hard_when_daemon_startup_is_blocked() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    fs::write(repo.path().join("delegate-fallback-blocked.txt"), "base\n")
        .expect("failed to write base");
    repo.git(&["add", "delegate-fallback-blocked.txt"])
        .expect("add should succeed");
    repo.stage_all_and_commit("base commit")
        .expect("base commit should succeed");

    fs::write(
        repo.path().join("delegate-fallback-blocked.txt"),
        "base\nchanged while startup blocked\n",
    )
    .expect("failed to write updated file");

    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
    std::thread::sleep(std::time::Duration::from_millis(500));

    fs::create_dir_all(
        daemon_lock_path(&repo)
            .parent()
            .expect("daemon lock path should have a parent"),
    )
    .expect("failed to create daemon lock parent directory");
    let held_lock = DaemonLock::acquire(&daemon_lock_path(&repo))
        .expect("should acquire daemon lock before checkpoint invocation");

    let result = repo.git_ai(&native_checkpoint_args!("delegate-fallback-blocked.txt"));
    assert!(
        result.is_ok(),
        "checkpoint should exit(0) when daemon is unavailable (never block agents)"
    );

    drop(held_lock);
}

#[test]
#[cfg(windows)]
#[serial]
fn daemon_windows_stalled_checkpoint_clients_do_not_block_later_control_requests() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_WINDOWS_CONTROL_PIPE_WORKERS", "2"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let control_socket = daemon_control_socket_path(&repo);

    let mut stalled_clients = (0..2)
        .map(|_| {
            let mut command = Command::new(get_binary_path());
            command
                .args(["checkpoint", "codex", "--hook-input", "stdin"])
                .current_dir(repo.path())
                .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
                .env("GITAI_TEST_DB_PATH", repo.test_db_path())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            configure_test_home_env(&mut command, repo.test_home_path());
            configure_test_daemon_env(
                &mut command,
                &repo.daemon_home_path(),
                &control_socket,
                &daemon_trace_socket_path(&repo),
            );
            command.spawn().expect("failed to spawn stalled checkpoint")
        })
        .collect::<Vec<_>>();
    thread::sleep(Duration::from_millis(250));

    let (response_tx, response_rx) = mpsc::channel();
    let request_socket = control_socket.clone();
    let request_repo = repo_workdir_string(&repo);
    thread::spawn(move || {
        let _ = response_tx.send(send_control_request(
            &request_socket,
            &ControlRequest::StatusFamily {
                repo_working_dir: request_repo,
            },
        ));
    });
    let response = response_rx.recv_timeout(Duration::from_secs(2));

    for client in &mut stalled_clients {
        let _ = client.kill();
        let _ = client.wait();
    }
    let response = response
        .expect("control request timed out after every original pipe worker was stalled")
        .expect("control request failed after every original pipe worker was stalled");
    assert!(
        response.ok,
        "later control request should return an ok response: {:?}",
        response
    );
    daemon.shutdown();
}

#[test]
#[serial]
fn daemon_write_mode_applies_delegated_checkpoint_and_updates_state() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    let completion_baseline = repo.daemon_total_completion_count();

    fs::write(repo.path().join("delegate-write.txt"), "base\n").expect("failed to write base");
    repo.git(&["add", "delegate-write.txt"])
        .expect("add should succeed");
    repo.stage_all_and_commit("base commit")
        .expect("base commit should succeed");

    fs::write(
        repo.path().join("delegate-write.txt"),
        "base\nwritten by delegated checkpoint\n",
    )
    .expect("failed to write updated file");

    repo.git_ai_with_env(
        &native_checkpoint_args!("delegate-write.txt"),
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("delegated checkpoint should succeed");

    wait_for_expected_top_level_completions(&repo, completion_baseline, 1);

    assert!(
        persisted_checkpoint_count(&repo, &repo) > 0,
        "delegated write should persist through the daemon"
    );
}

#[test]
#[serial]
fn daemon_test_mode_git_ai_checkpoint_runs_via_daemon() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    fs::write(repo.path().join("daemon-mode-checkpoint.txt"), "base\n")
        .expect("failed to write base");
    repo.git(&["add", "daemon-mode-checkpoint.txt"])
        .expect("add should succeed");
    repo.stage_all_and_commit("base commit")
        .expect("base commit should succeed");

    fs::write(
        repo.path().join("daemon-mode-checkpoint.txt"),
        "base\nchanged through daemon mode\n",
    )
    .expect("failed to write updated file");
    let completion_baseline = repo.daemon_total_completion_count();

    repo.git_ai(&native_checkpoint_args!("daemon-mode-checkpoint.txt"))
        .expect("daemon-mode checkpoint should succeed");

    repo.wait_for_next_daemon_checkpoint_completion(completion_baseline);

    assert!(
        persisted_checkpoint_count(&repo, &repo) > 0,
        "delegated write should persist through the daemon"
    );
}

#[test]
#[cfg(unix)]
#[serial]
fn daemon_symlink_repo_path_trace_and_status_use_same_family() {
    let unique = format!(
        "git-ai-symlink-family-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let real_path = std::env::temp_dir().join(format!("{unique}-real"));
    let alias_path = std::env::temp_dir().join(format!("{unique}-alias"));
    fs::create_dir_all(&real_path).expect("failed to create real test repo path");
    std::os::unix::fs::symlink(&real_path, &alias_path).expect("failed to create repo symlink");

    let repo = TestRepo::new_at_path_with_daemon_scope(&alias_path, DaemonTestScope::Dedicated);
    assert_ne!(
        repo.path(),
        &repo.canonical_path(),
        "test must exercise an alias path distinct from its canonical path"
    );

    let completion_baseline = repo.daemon_total_completion_count();
    fs::write(repo.path().join("alias.txt"), "alias\n").expect("failed writing aliased file");
    repo.git(&["add", "alias.txt"])
        .expect("aliased path git add should succeed");
    repo.wait_for_daemon_total_completion_count(
        completion_baseline,
        completion_baseline.saturating_add(1),
    );

    let status = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    )
    .expect("daemon status request should succeed for aliased path");
    assert!(status.ok, "aliased path daemon status should be ok");

    let checkpoint_baseline = repo.daemon_total_completion_count();
    fs::write(repo.path().join("alias.txt"), "alias\nhuman\n")
        .expect("failed writing human aliased file");
    repo.git_ai(&native_checkpoint_args!("alias.txt"))
        .expect("aliased path human checkpoint should succeed");
    repo.wait_for_next_daemon_checkpoint_completion(checkpoint_baseline);

    let attachment_for = |path: &Path| {
        let response = send_control_request(
            &daemon_control_socket_path(&repo),
            &ControlRequest::CheckpointAttachment {
                session: SessionIdentity::new("codex", "daemon-test-session").unwrap(),
                repo_working_dir: path.to_string_lossy().to_string(),
            },
        )
        .expect("daemon attachment request should succeed");
        assert!(
            response.ok,
            "daemon attachment response should be ok for {}: {:?}",
            path.display(),
            response.error
        );
        response
            .data
            .as_ref()
            .and_then(|data| data.get("attached"))
            .and_then(serde_json::Value::as_bool)
    };

    assert!(
        attachment_for(repo.path()) == Some(true),
        "aliased worktree path should see attached session"
    );
    assert!(
        attachment_for(&repo.canonical_path()) == Some(true),
        "canonical worktree path should see same attached session"
    );

    let _ = fs::remove_file(&alias_path);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_commit_after_ai_checkpoint_preserves_ai_replacement_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let file_path = repo.path().join("daemon-ai-replace.txt");
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(&file_path, "old line\n").expect("failed to write base contents");
    traced_git_with_env(
        &repo,
        &["add", "daemon-ai-replace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    fs::write(&file_path, "new line from ai\n").expect("failed to write ai contents");
    expected_top_level_completions += 1;
    repo.git_ai_with_env(
        &native_checkpoint_args!("daemon-ai-replace.txt"),
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("ai checkpoint should succeed");
    traced_git_with_env(
        &repo,
        &["add", "daemon-ai-replace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit ai replacement"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("commit should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    let mut file = repo.filename("daemon-ai-replace.txt");
    file.assert_committed_lines(lines!["new line from ai"]);
}

#[test]
fn daemon_trace_current_dir_commands_reserve_order_from_def_repo() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let untraced_git = |args: &[&str]| repo.git_og_with_env(args, &[("GIT_TRACE2_EVENT", "0")]);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    fs::write(repo.path().join("base.txt"), "base\n").expect("failed to write base");
    untraced_git(&["add", "base.txt"]).expect("base add should succeed");
    untraced_git(&["commit", "-m", "base"]).expect("base commit should succeed");

    fs::write(repo.path().join("a.txt"), "a ai\n").expect("failed to write a.txt");
    repo.git_ai(&native_checkpoint_args!("a.txt"))
        .expect("a checkpoint should succeed");
    untraced_git(&["add", "a.txt"]).expect("a add should succeed");
    untraced_git(&["commit", "-m", "commit A"]).expect("commit A should succeed");
    let commit_a = untraced_git(&["rev-parse", "HEAD"])
        .expect("rev-parse A should succeed")
        .trim()
        .to_string();

    fs::write(repo.path().join("b.txt"), "b ai\n").expect("failed to write b.txt");
    repo.git_ai(&native_checkpoint_args!("b.txt"))
        .expect("b checkpoint should succeed");
    untraced_git(&["add", "b.txt"]).expect("b add should succeed");
    untraced_git(&["commit", "-m", "commit B"]).expect("commit B should succeed");
    let commit_b = untraced_git(&["rev-parse", "HEAD"])
        .expect("rev-parse B should succeed")
        .trim()
        .to_string();

    let session_a = repos::test_repo::new_daemon_test_sync_session_id();
    let session_b = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg_a = format!("git-ai.testSyncSession={session_a}");
    let session_arg_b = format!("git-ai.testSyncSession={session_b}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "current-dir-a",
                "argv": ["git", "-c", session_arg_a, "commit", "-m", "commit A"],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "current-dir-a",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "start",
                "sid": "current-dir-b",
                "argv": ["git", "-c", session_arg_b, "commit", "-m", "commit B"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "current-dir-b",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "current-dir-b",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("current-dir-b", 0, 2_101u64),
            json!({
                "event": "exit",
                "sid": "current-dir-a",
                "code": 0,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("current-dir-a", 0, 1_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[session_a.clone(), session_b.clone()]);

    let commands = completion_entries_for_command(&repo, "commit");
    let a = commands
        .iter()
        .find(|entry| entry.test_sync_session.as_deref() == Some(session_a.as_str()))
        .expect("first command completion");
    let b = commands
        .iter()
        .find(|entry| entry.test_sync_session.as_deref() == Some(session_b.as_str()))
        .expect("second command completion");
    assert!(
        a.seq < b.seq,
        "def_repo admission must reserve order even when exits arrive in reverse"
    );
    // Both writes entered the same pending window before either commit was
    // sequenced. The cursor resolves A first even though B's exit arrived first,
    // so A consumes the one session and B cannot borrow it from the current HEAD.
    repo.assert_commit_links(&commit_a, &[("codex", "daemon-test-session")]);
    repo.assert_commit_links(&commit_b, &[]);
    let mut file_a = repo.filename("a.txt");
    file_a.assert_committed_lines(lines!["a ai"]);
    let mut file_b = repo.filename("b.txt");
    file_b.assert_committed_lines(lines!["b ai"]);
}

#[test]
#[cfg(not(windows))]
fn daemon_trace_listener_stalled_connection_does_not_block_later_trace_connections() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let _stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "stalled-listener-followup",
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
                "time_ns": 10_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "stalled-listener-followup",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 10_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "stalled-listener-followup",
                "code": 0,
                "time_ns": 10_100u64,
            }),
            trace_atexit_frame("stalled-listener-followup", 0, 10_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    panic!(
        "daemon did not process a later trace connection while an earlier trace socket was stalled"
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_stalled_unidentified_trace_connection_does_not_block_checkpoint_control_request() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let control_socket = daemon_control_socket_path(&repo);

    let _stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");
    thread::sleep(Duration::from_millis(150));

    let file_path = repo.path().join("checkpoint-after-stalled-trace.txt");
    fs::write(&file_path, "checkpoint content\n").unwrap();

    let request = checkpoint_request(
        &repo,
        "checkpoint-after-stalled-trace".to_string(),
        "daemon-test-session".to_string(),
    );

    let response =
        send_checkpoint_request_with_timeout(&control_socket, &request, Duration::from_millis(500))
            .expect("checkpoint control request should not block on unidentified trace sockets");

    assert!(
        response.ok,
        "checkpoint control request should succeed: {:?}",
        response
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_checkpoint_receipt_rejects_oversized_body_before_receiving_it() {
    let repo = TestRepo::new_dedicated_daemon();
    let response = send_control_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &ControlRequest::CheckpointRun {
            body_bytes: 64 * 1024 * 1024 + 1,
        },
        Duration::from_millis(500),
    )
    .expect("daemon should return a quota response without waiting for a body");

    assert!(!response.ok, "oversized checkpoint must be rejected");
    assert_eq!(
        response.error.as_deref(),
        Some("checkpoint ingress busy: byte_limit")
    );
    let daemon_logs = wait_for_daemon_log(&repo, "checkpoint ingress quota exhausted");
    assert!(
        daemon_logs.contains("reason=\"byte_limit\"")
            && daemon_logs.contains("requested_bytes=67108865"),
        "daemon logs did not contain checkpoint overflow context:\n{daemon_logs}"
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_checkpoint_receipt_logs_body_receive_errors() {
    let repo = TestRepo::new_dedicated_daemon();
    let control_socket = daemon_control_socket_path(&repo);
    let stream = open_local_socket_stream_with_timeout(&control_socket, Duration::from_millis(500))
        .expect("connect to daemon control socket");
    let mut reader = BufReader::new(stream);
    let mut header = serde_json::to_vec(&ControlRequest::CheckpointRun { body_bytes: 4 }).unwrap();
    header.push(b'\n');
    reader.get_mut().write_all(&header).unwrap();
    reader.get_mut().flush().unwrap();

    let mut ready_line = String::new();
    reader.read_line(&mut ready_line).unwrap();
    let ready: ControlResponse = serde_json::from_str(ready_line.trim()).unwrap();
    assert_eq!(
        ready.data.as_ref().and_then(|data| data.get("ready")),
        Some(&Value::Bool(true))
    );

    reader.get_mut().write_all(b"body!").unwrap();
    reader.get_mut().flush().unwrap();
    let mut final_line = String::new();
    assert_eq!(
        reader.read_line(&mut final_line).unwrap(),
        0,
        "invalid body delimiter must close the connection without an acknowledgement"
    );

    let daemon_logs = wait_for_daemon_log(&repo, "failed receiving checkpoint body");
    assert!(
        daemon_logs.contains("reason=\"body_receive_failed\"")
            && daemon_logs.contains("body_bytes=4"),
        "daemon logs did not contain checkpoint body receive context:\n{daemon_logs}"
    );
}

#[test]
fn daemon_checkpoint_ack_preserves_order_with_immediate_commit() {
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_CHECKPOINT_SIDE_EFFECT",
        "slow-checkpoint=2000",
    )]);
    let file_path = repo.path().join("slow-checkpoint.txt");
    fs::write(&file_path, "AI content\n").unwrap();
    let request = checkpoint_request(
        &repo,
        "slow-checkpoint".to_string(),
        "slow-checkpoint-session".to_string(),
    );

    let started = std::time::Instant::now();
    let response = send_checkpoint_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &request,
        Duration::from_millis(500),
    )
    .expect("checkpoint receipt acknowledgement must not wait for processing");
    assert!(response.ok, "checkpoint receipt failed: {response:?}");
    assert!(
        response.seq.is_some(),
        "receipt acknowledgement needs a sequence"
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "receipt acknowledgement waited for checkpoint processing"
    );

    repo.git_without_test_sync_for_test(&["add", "."], &[])
        .unwrap();
    repo.git_without_test_sync_for_test(
        &[
            "commit",
            "-m",
            "Commit immediately after checkpoint receipt",
        ],
        &[],
    )
    .unwrap();

    let mut file = repo.filename("slow-checkpoint.txt");
    file.assert_committed_lines(lines!["AI content"]);

    let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.assert_commit_links(head.trim(), &[("codex", "slow-checkpoint-session")]);
}

#[test]
#[cfg(not(windows))]
fn daemon_soft_shutdown_drains_acknowledged_checkpoints() {
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_CHECKPOINT_SIDE_EFFECT",
        "shutdown-first=1000",
    )]);
    let control_socket = daemon_control_socket_path(&repo);
    let request = |trace_id: &str, _path: &str, _content: &str| {
        checkpoint_request(
            &repo,
            trace_id.to_string(),
            "shutdown-drain-session".to_string(),
        )
    };

    fs::write(repo.path().join("first.txt"), "first\n").unwrap();
    let first = send_checkpoint_request_with_timeout(
        &control_socket,
        &request("shutdown-first", "first.txt", "first\n"),
        Duration::from_millis(500),
    )
    .expect("first checkpoint should be acknowledged");
    assert!(first.ok, "first checkpoint failed: {first:?}");

    let logs = wait_for_daemon_log(&repo, "checkpoint prepared for family admission");
    assert!(
        logs.contains("checkpoint prepared for family admission"),
        "first checkpoint never started processing:\n{logs}"
    );

    fs::write(repo.path().join("second.txt"), "second\n").unwrap();
    let second = send_checkpoint_request_with_timeout(
        &control_socket,
        &request("shutdown-second", "second.txt", "second\n"),
        Duration::from_millis(500),
    )
    .expect("second checkpoint should be acknowledged");
    assert!(second.ok, "second checkpoint failed: {second:?}");
    assert!(
        second.seq > first.seq,
        "checkpoint receipts must preserve acceptance order"
    );

    let shutdown_socket = control_socket.clone();
    let shutdown_thread = thread::spawn(move || {
        send_control_request_with_timeout(
            &shutdown_socket,
            &ControlRequest::Shutdown,
            Duration::from_secs(5),
        )
    });
    let logs = wait_for_daemon_log(&repo, "checkpoint acceptance closed for graceful shutdown");
    assert!(
        logs.contains("checkpoint acceptance closed for graceful shutdown"),
        "daemon never closed checkpoint acceptance:\n{logs}"
    );

    fs::write(repo.path().join("too-late.txt"), "too late\n").unwrap();
    let too_late = send_checkpoint_request_with_timeout(
        &control_socket,
        &request("shutdown-too-late", "too-late.txt", "too late\n"),
        Duration::from_millis(500),
    )
    .expect("checkpoint submitted during graceful shutdown should receive a response");
    assert!(
        !too_late.ok && too_late.error.as_deref() == Some("daemon is shutting down"),
        "graceful shutdown must stop accepting new checkpoints: {too_late:?}"
    );

    let shutdown = shutdown_thread
        .join()
        .expect("shutdown request thread panicked")
        .expect("soft shutdown should wait for acknowledged checkpoints");
    assert!(shutdown.ok, "soft shutdown failed: {shutdown:?}");

    assert_eq!(
        persisted_checkpoint_count(&repo, &repo),
        2,
        "soft shutdown must not discard successfully acknowledged checkpoints"
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_drains_independent_checkpoint_families_concurrently() {
    let barrier_dir = tempfile::tempdir().expect("checkpoint side-effect barrier directory");
    let barrier_path = barrier_dir.path().to_string_lossy().to_string();
    let first_repo = TestRepo::new_with_daemon_env(&[
        (
            "GIT_AI_TEST_DELAY_CHECKPOINT_ADMISSION",
            "concurrent-family-first=250",
        ),
        (
            "GIT_AI_TEST_CHECKPOINT_SIDE_EFFECT_BARRIER_DIR",
            barrier_path.as_str(),
        ),
    ]);
    let second_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket = daemon_control_socket_path(&first_repo);
    let request = |repo: &TestRepo, trace_id: &str, _path: &str| {
        checkpoint_request(repo, trace_id.to_string(), trace_id.to_string())
    };

    fs::write(
        first_repo.path().join("first.txt"),
        "concurrent-family-first\n",
    )
    .unwrap();
    fs::write(
        second_repo.path().join("second.txt"),
        "concurrent-family-second\n",
    )
    .unwrap();

    for checkpoint in [
        request(&first_repo, "concurrent-family-first", "first.txt"),
        request(&second_repo, "concurrent-family-second", "second.txt"),
    ] {
        let response = send_checkpoint_request_with_timeout(
            &control_socket,
            &checkpoint,
            Duration::from_millis(500),
        )
        .expect("checkpoint should be acknowledged before processing");
        assert!(response.ok, "checkpoint failed: {response:?}");
    }

    let started = std::time::Instant::now();
    loop {
        let checkpoint_count = |repo: &TestRepo| persisted_checkpoint_count(&first_repo, repo);
        if checkpoint_count(&first_repo) == 1 && checkpoint_count(&second_repo) == 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "independent checkpoint families did not finish processing"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Regression test for #2252: a long write-op side-effect pass in one family
/// (a large checkout/commit in a monorepo) must not stall the trace-ingest
/// watermark that checkpoint admission waits on. A checkpoint for an
/// independent repository family must be admitted and processed while the
/// other family's side effects are still grinding.
#[test]
#[cfg(not(windows))]
fn family_side_effect_grind_does_not_stall_independent_checkpoint_admission() {
    let grinding_repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        "commit=10000",
    )]);
    let independent_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket = daemon_control_socket_path(&grinding_repo);

    // A traced commit whose side-effect pass grinds for 10s in its family.
    fs::write(grinding_repo.path().join("grind.txt"), "grind\n").unwrap();
    grinding_repo
        .git_without_test_sync_for_test(&["add", "grind.txt"], &[])
        .unwrap();
    grinding_repo
        .git_without_test_sync_for_test(&["commit", "-m", "grind commit"], &[])
        .unwrap();

    wait_for_test_side_effect_delay_started(&grinding_repo);

    fs::write(
        independent_repo.path().join("independent.txt"),
        "independent-grind-checkpoint\n",
    )
    .unwrap();
    let checkpoint = checkpoint_request(
        &independent_repo,
        "independent-grind-checkpoint".to_string(),
        "independent-grind-checkpoint".to_string(),
    );
    let response =
        send_checkpoint_request_with_timeout(&control_socket, &checkpoint, Duration::from_secs(2))
            .expect("checkpoint should be acknowledged during an unrelated side-effect grind");
    assert!(response.ok, "checkpoint failed: {response:?}");

    // The independent family must finish processing well before the 10s grind
    // completes; admission queueing behind the grind is issue #2252.
    let started = std::time::Instant::now();
    loop {
        let checkpoint_count = persisted_checkpoint_count(&grinding_repo, &independent_repo);
        if checkpoint_count == 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "independent checkpoint was not admitted/processed while another \
             family's side effects were running (#2252)"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// Regression test for #2252's headline symptom: checkpoints "received into
/// bounded ingress" but never "prepared for family admission" while the same
/// family's write-op side effects grind. Admission is a global sequencing
/// stage and must not queue behind side-effect execution; only processing may
/// wait for the family's turn.
#[test]
#[cfg(not(windows))]
fn checkpoint_admission_proceeds_while_same_family_side_effects_grind() {
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        "commit=10000",
    )]);

    fs::write(repo.path().join("grind.txt"), "grind\n").unwrap();
    repo.git_without_test_sync_for_test(&["add", "grind.txt"], &[])
        .unwrap();
    repo.git_without_test_sync_for_test(&["commit", "-m", "grind commit"], &[])
        .unwrap();

    wait_for_test_side_effect_delay_started(&repo);

    fs::write(repo.path().join("same-family.txt"), "same-family\n").unwrap();
    let checkpoint = checkpoint_request(
        &repo,
        "same-family-grind-checkpoint".to_string(),
        "same-family-grind-checkpoint".to_string(),
    );
    let response = send_checkpoint_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &checkpoint,
        Duration::from_secs(2),
    )
    .expect("checkpoint should be acknowledged during a same-family side-effect grind");
    assert!(response.ok, "checkpoint failed: {response:?}");

    // Admission (not processing) must complete well before the 10s grind ends.
    let started = std::time::Instant::now();
    loop {
        let logs = repo.daemon_stderr_contents();
        if logs.lines().any(|line| {
            line.contains("checkpoint prepared for family admission")
                && line.contains("same-family-grind-checkpoint")
        }) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "checkpoint was received but not admitted while the same family's \
             side effects were running (#2252); daemon logs:\n{logs}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// #2252: side effects for commands that do not participate in the family
/// sequencer (e.g. `git am`) run on detached tasks. `sync.family` must still
/// cover them — it must not report a family as synced while such a pass is
/// in flight.
#[test]
#[cfg(not(windows))]
fn sync_family_waits_for_detached_non_sequencer_side_effects() {
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        "am=3000",
    )]);

    // Base commit so `git am` has a parent to apply onto.
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.git_without_test_sync_for_test(&["add", "base.txt"], &[])
        .unwrap();
    repo.git_without_test_sync_for_test(&["commit", "-m", "base"], &[])
        .unwrap();

    let patch = "\
From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001
From: Repro <repro@example.com>
Date: Mon, 1 Sep 2026 00:00:00 +0000
Subject: [PATCH] add am file

---
 am.txt | 1 +
 1 file changed, 1 insertion(+)
 create mode 100644 am.txt

diff --git a/am.txt b/am.txt
new file mode 100644
index 0000000..7898192
--- /dev/null
+++ b/am.txt
@@ -0,0 +1 @@
+a
--\x20
2.39.0
";
    fs::write(repo.path().join("am-patch.mbox"), patch).unwrap();
    repo.git_without_test_sync_for_test(&["am", "am-patch.mbox"], &[])
        .unwrap();
    wait_for_test_side_effect_delay_started(&repo);

    let sync = send_control_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &ControlRequest::SyncFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
        Duration::from_secs(30),
    )
    .expect("sync.family should succeed");
    assert!(sync.ok, "sync.family failed: {sync:?}");

    let am_entries = completion_entries_for_command(&repo, "am");
    assert!(
        !am_entries.is_empty(),
        "sync.family returned before the detached `git am` side-effect pass completed (#2252)"
    );
}

/// #2252: with side-effect passes running on detached tasks, graceful
/// shutdown must not complete while such a pass is still in flight — the
/// process exit right after the shutdown response would kill it mid-write.
#[test]
#[cfg(not(windows))]
fn graceful_shutdown_waits_for_detached_side_effect_passes() {
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        "commit=3000",
    )]);

    fs::write(repo.path().join("grind.txt"), "grind\n").unwrap();
    repo.git_without_test_sync_for_test(&["add", "grind.txt"], &[])
        .unwrap();
    repo.git_without_test_sync_for_test(&["commit", "-m", "grind commit"], &[])
        .unwrap();
    wait_for_test_side_effect_delay_started(&repo);

    let shutdown = send_control_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
        Duration::from_secs(30),
    )
    .expect("graceful shutdown request should succeed");
    assert!(shutdown.ok, "graceful shutdown failed: {shutdown:?}");

    let commit_entries = completion_entries_for_command(&repo, "commit");
    assert!(
        !commit_entries.is_empty(),
        "graceful shutdown completed while the commit side-effect pass was still running (#2252)"
    );
}

/// #2252 observability: when checkpoint admission is delayed behind the
/// trace-ingest watermark, the daemon must log the delay explicitly instead
/// of it having to be inferred from absent admission log lines.
#[test]
#[cfg(not(windows))]
fn delayed_checkpoint_admission_is_logged() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    // Delay the trace ingest worker's start so the synthetic frames below are
    // enqueued but the watermark checkpoint admission waits on cannot advance.
    // The delay-log interval is lowered so the warning fires well within the
    // stall window even when daemon startup eats seconds on a loaded runner.
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_INGEST_WORKER_START_DELAY_MS", "10000"),
            (
                "GIT_AI_TEST_CHECKPOINT_ADMISSION_DELAY_LOG_INTERVAL_MS",
                "500",
            ),
        ],
    );
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    send_trace_frames(
        &daemon_trace_socket_path(&repo),
        &[
            json!({
                "event": "start",
                "sid": "stalled-ingest-root",
                "argv": ["git", "commit", "-m", "synthetic"],
                "time_ns": 10_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "stalled-ingest-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 10_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "stalled-ingest-root",
                "code": 0,
                "time_ns": 10_100u64,
            }),
            trace_atexit_frame("stalled-ingest-root", 0, 10_101u64),
        ],
    );
    // Let the trace reader enqueue the frames (allocating ingest sequence
    // numbers) before the checkpoint samples its admission target.
    thread::sleep(Duration::from_millis(200));

    fs::write(repo.path().join("delayed.txt"), "delayed\n").unwrap();
    let checkpoint = checkpoint_request(
        &repo,
        "delayed-admission-checkpoint".to_string(),
        "delayed-admission-checkpoint".to_string(),
    );
    let response = send_checkpoint_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &checkpoint,
        Duration::from_secs(2),
    )
    .expect("checkpoint should be acknowledged while trace ingest is stalled");
    assert!(response.ok, "checkpoint failed: {response:?}");

    let started = std::time::Instant::now();
    loop {
        let logs = daemon.stderr_contents();
        if logs.contains("checkpoint admission delayed waiting for trace ingestion") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(9),
            "stalled checkpoint admission was not logged (#2252); daemon logs:\n{logs}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    daemon.shutdown();
}

#[test]
#[cfg(not(windows))]
fn daemon_checkpoint_processing_failure_is_logged_after_receipt_ack() {
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_FAIL_CHECKPOINT_SIDE_EFFECT",
        "failing-checkpoint",
    )]);
    let file_path = repo.path().join("failing-checkpoint.txt");
    fs::write(&file_path, "content\n").unwrap();
    let request = checkpoint_request(
        &repo,
        "failing-checkpoint".to_string(),
        "daemon-test-session".to_string(),
    );

    let receipt = send_checkpoint_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &request,
        Duration::from_millis(500),
    )
    .expect("processing failure must happen after receipt acknowledgement");
    assert!(receipt.ok, "checkpoint receipt failed: {receipt:?}");
    let receipt_seq = receipt.seq.expect("receipt sequence");

    let sync = send_control_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &ControlRequest::SyncFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
        Duration::from_secs(5),
    )
    .expect("sync.family should complete with family error state");
    assert!(sync.ok, "sync.family request failed: {sync:?}");
    let last_error = sync
        .data
        .as_ref()
        .and_then(|data| data.get("last_error"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        last_error.contains("synthetic checkpoint processing failure"),
        "sync.family did not surface checkpoint failure: {sync:?}"
    );

    let daemon_logs = repo.daemon_stderr_contents();
    assert!(
        daemon_logs.contains("side_effect_failed")
            && daemon_logs.contains("synthetic checkpoint processing failure")
            && daemon_logs.contains(&format!("receipt_seq={receipt_seq}")),
        "daemon logs did not contain structured checkpoint failure context:\n{daemon_logs}"
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_checkpoint_receipt_releases_quota_when_body_sender_stalls() {
    let repo = TestRepo::new_dedicated_daemon();
    let control_socket = daemon_control_socket_path(&repo);
    let stream = open_local_socket_stream_with_timeout(&control_socket, Duration::from_millis(500))
        .expect("connect to daemon control socket");
    let mut stalled_reader = BufReader::new(stream);
    let mut header = serde_json::to_vec(&ControlRequest::CheckpointRun {
        body_bytes: 64 * 1024 * 1024,
    })
    .unwrap();
    header.push(b'\n');
    stalled_reader.get_mut().write_all(&header).unwrap();
    stalled_reader.get_mut().flush().unwrap();

    let mut ready_line = String::new();
    stalled_reader.read_line(&mut ready_line).unwrap();
    let ready: git_ai::daemon::ControlResponse = serde_json::from_str(ready_line.trim()).unwrap();
    assert!(ready.ok, "daemon should reserve the declared body");

    let blocked = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::CheckpointRun { body_bytes: 1 },
        Duration::from_millis(500),
    )
    .expect("daemon should reject before receiving another body");
    assert_eq!(
        blocked.error.as_deref(),
        Some("checkpoint ingress busy: byte_limit")
    );

    thread::sleep(Duration::from_millis(2_500));
    let after_timeout = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::CheckpointRun { body_bytes: 1 },
        Duration::from_millis(500),
    )
    .expect("daemon should release stalled checkpoint quota");
    assert!(
        after_timeout.ok,
        "stalled checkpoint reservation should be released after receive timeout: {after_timeout:?}"
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_stalled_unidentified_trace_connection_does_not_block_sync_control_request() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let control_socket = daemon_control_socket_path(&repo);

    let _stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");
    thread::sleep(Duration::from_millis(150));

    let response = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::SyncFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
        Duration::from_millis(500),
    )
    .expect("sync control request should not block on unidentified trace sockets");

    assert!(
        response.ok,
        "sync control request should succeed: {:?}",
        response
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_sync_family_returns_promptly_with_open_unwritten_mutating_root() {
    let repo = TestRepo::new_dedicated_daemon();
    // An attributed `git commit` that is still running (its worktree HEAD
    // reflog has not grown) fences nothing: nobody waits for an editor.
    let sid = live_pid_trace_sid("sync");
    let mut open_trace = repo.open_mutating_commit_trace_root(&sid, true);
    thread::sleep(Duration::from_millis(150));

    let started = std::time::Instant::now();
    let response = send_control_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &ControlRequest::SyncFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
        Duration::from_secs(5),
    )
    .expect("sync.family must return while an interactive git command stays open");
    assert!(response.ok, "sync.family failed: {response:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "sync.family must not wait for a root that has written nothing"
    );

    write_trace_frames(&mut open_trace, &[trace_atexit_frame(&sid, 0, 1_002)]);
}

#[test]
#[cfg(not(windows))]
fn daemon_await_completes_with_idle_open_mutating_root() {
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_DAEMON_CAUSAL_GRACE_MS", "300")]);
    let sid = live_pid_trace_sid("await");
    let mut open_trace = repo.open_mutating_commit_trace_root(&sid, true);
    thread::sleep(Duration::from_millis(150));

    let started = std::time::Instant::now();
    let response = send_control_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Await { timeout_secs: 5 },
        Duration::from_secs(15),
    )
    .expect("await must answer while an interactive git command stays open");
    assert!(response.ok, "await failed: {response:?}");
    let data = response.data.expect("await response data");
    assert_eq!(
        data["timed_out"],
        json!(false),
        "await must not run to its deadline behind a human at an editor: {data}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "await should return well before its 5s deadline"
    );

    write_trace_frames(&mut open_trace, &[trace_atexit_frame(&sid, 0, 1_002)]);
}

#[cfg(not(windows))]
fn status_daemon(repo: &TestRepo) -> Value {
    let response = send_control_request(
        &daemon_control_socket_path(repo),
        &ControlRequest::StatusDaemon,
    )
    .expect("status.daemon request");
    assert!(response.ok, "status.daemon failed: {response:?}");
    response.data.expect("status.daemon data")
}

#[test]
#[cfg(not(windows))]
fn daemon_status_daemon_reports_open_root_and_fenced_checkpoint() {
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_DAEMON_CAUSAL_GRACE_MS", "10000")]);
    let idle = status_daemon(&repo);
    assert_eq!(idle["trace_roots_open_mutating"], json!(0), "{idle}");
    assert_eq!(idle["sequencer_entries_total"], json!(0), "{idle}");
    assert_eq!(idle["snapshot_partial"], json!(false), "{idle}");
    assert!(idle["uptime_seconds"].is_u64(), "{idle}");
    assert!(idle["checkpoints_dropped"].is_u64(), "{idle}");

    let sid = live_pid_trace_sid("status");
    let mut open_trace = repo.open_mutating_commit_trace_root(&sid, true);
    // The reader must have registered the root before anything waits on it.
    let started = std::time::Instant::now();
    while status_daemon(&repo)["trace_roots_open_mutating"] != json!(1) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "status.daemon never reported the open root"
        );
        thread::sleep(Duration::from_millis(25));
    }
    // The root writes refs (as a commit running its post-commit hook would):
    // from here on its family's work is fenced behind it.
    repo.git_og_with_env(
        &[
            "commit",
            "--allow-empty",
            "-m",
            "written under the open root",
        ],
        &[("GIT_TRACE2_EVENT", "0")],
    )
    .expect("untraced commit");
    fs::write(repo.path().join("fenced.txt"), "fenced ai\n").unwrap();
    let receipt = send_checkpoint_request_with_timeout(
        &daemon_control_socket_path(&repo),
        &checkpoint_request(&repo, "fenced-checkpoint".into(), "fenced-session".into()),
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(receipt.ok);

    let started = std::time::Instant::now();
    let fenced = loop {
        let snapshot = status_daemon(&repo);
        if snapshot["trace_roots_open_mutating"] == json!(1)
            && snapshot["families"][0]["front_kind"] == json!("checkpoint")
        {
            break snapshot;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "status.daemon never reported the fenced checkpoint: {snapshot}"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(
        fenced["sequencer_entries_checkpoints"],
        json!(1),
        "{fenced}"
    );
    assert_eq!(fenced["sequencer_fenced_families"], json!(1), "{fenced}");
    assert_eq!(fenced["trace_roots_written"], json!(1), "{fenced}");
    assert_eq!(fenced["families"][0]["fenced"], json!(true), "{fenced}");
    assert_eq!(fenced["sequencer_stalled"], json!(false), "{fenced}");

    write_trace_frames(&mut open_trace, &[trace_atexit_frame(&sid, 0, 1_002)]);
    let started = std::time::Instant::now();
    loop {
        let snapshot = status_daemon(&repo);
        if snapshot["trace_roots_open_mutating"] == json!(0)
            && snapshot["sequencer_entries_total"] == json!(0)
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "status.daemon never reported the drained checkpoint: {snapshot}"
        );
        thread::sleep(Duration::from_millis(25));
    }
    repo.sync_daemon();
}

#[test]
fn daemon_bg_status_includes_daemon_health() {
    let repo = TestRepo::new_dedicated_daemon();
    let output = bg_command(&repo, "status", &[]);
    assert!(output.status.success(), "bg status failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("bg status must print JSON ({error}): {stdout}"));
    assert_eq!(value["ok"], json!(true), "{value}");
    assert!(value["data"]["family_key"].is_string(), "{value}");
    assert!(value["daemon"]["uptime_seconds"].is_u64(), "{value}");
    assert!(
        value["daemon"]["sequencer_entries_total"].is_u64(),
        "{value}"
    );
    assert!(value["daemon"]["families"].is_array(), "{value}");
}

#[test]
#[cfg(not(windows))]
fn daemon_sync_family_ignores_open_mutating_root_from_other_family() {
    let first_repo = TestRepo::new_dedicated_daemon();
    let second_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket = daemon_control_socket_path(&first_repo);
    let second_worktree = repo_workdir_string(&second_repo);
    let sid = "cross-family-open-mutating-root";

    let mut open_trace = first_repo.open_mutating_commit_trace_root(sid, true);
    thread::sleep(Duration::from_millis(150));

    // The root has written refs (a real commit in its worktree, as its
    // process would); from here on its own family must wait for it.
    first_repo
        .git_og_with_env(
            &[
                "commit",
                "--allow-empty",
                "-m",
                "written under the open root",
            ],
            &[("GIT_TRACE2_EVENT", "0")],
        )
        .expect("untraced commit");

    let own_control_socket = control_socket.clone();
    let own_worktree = repo_workdir_string(&first_repo);
    let (own_sync_tx, own_sync_rx) = mpsc::channel();
    let own_sync = thread::spawn(move || {
        let response = send_control_request_with_timeout(
            &own_control_socket,
            &ControlRequest::SyncFamily {
                repo_working_dir: own_worktree,
            },
            Duration::from_secs(5),
        );
        let _ = own_sync_tx.send(response);
    });
    assert!(
        own_sync_rx
            .recv_timeout(Duration::from_millis(250))
            .is_err(),
        "sync.family must still wait for an open mutating root in its own family once it has written"
    );

    let unrelated_sync = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::SyncFamily {
            repo_working_dir: second_worktree,
        },
        Duration::from_secs(1),
    )
    .expect("an open mutating root from another family must not block sync.family");
    assert!(
        unrelated_sync.ok,
        "unrelated sync.family request failed: {unrelated_sync:?}"
    );

    write_trace_frames(&mut open_trace, &[trace_atexit_frame(sid, 0, 1_002)]);
    let own_sync_response = own_sync_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("own-family sync should complete after the trace root closes")
        .expect("own-family sync request failed");
    assert!(own_sync_response.ok, "own-family sync failed");
    own_sync.join().unwrap();
}

#[test]
#[cfg(not(windows))]
fn daemon_partial_trace_line_does_not_block_checkpoint_control_request() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let control_socket = daemon_control_socket_path(&repo);

    let mut stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");
    stalled_stream
        .write_all(br#"{"event":"start""#)
        .expect("failed to write partial trace frame");
    stalled_stream
        .flush()
        .expect("failed to flush partial trace frame");
    thread::sleep(Duration::from_millis(150));

    let file_path = repo.path().join("checkpoint-after-partial-trace.txt");
    fs::write(&file_path, "checkpoint content\n").unwrap();

    let request = checkpoint_request(
        &repo,
        "checkpoint-after-partial-trace".to_string(),
        "daemon-test-session".to_string(),
    );

    let response =
        send_checkpoint_request_with_timeout(&control_socket, &request, Duration::from_millis(500))
            .expect("checkpoint control request should not block on incomplete trace frames");

    assert!(
        response.ok,
        "checkpoint control request should succeed: {:?}",
        response
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_trace_listener_partial_line_does_not_block_later_trace_connections() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");
    stalled_stream
        .write_all(br#"{"event":"start""#)
        .expect("failed to write partial trace frame");
    stalled_stream
        .flush()
        .expect("failed to flush partial trace frame");
    thread::sleep(Duration::from_millis(200));

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "partial-listener-followup",
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
                "time_ns": 10_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "partial-listener-followup",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 10_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "partial-listener-followup",
                "code": 0,
                "time_ns": 10_100u64,
            }),
            trace_atexit_frame("partial-listener-followup", 0, 10_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    panic!(
        "daemon did not process a later trace connection while an earlier trace socket held a partial line"
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_trace_read_error_does_not_leave_root_open_forever() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let control_socket = daemon_control_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to connect to trace socket");
    write_trace_frames(
        &mut stream,
        &[
            json!({
                "event": "start",
                "sid": "read-error-open-root",
                "argv": ["git", "commit", "-m", "interrupted by bad bytes"],
                "time_ns": 50_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "read-error-open-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 50_001u64,
            }),
        ],
    );
    // Invalid UTF-8 makes the reader's read_line fail. The connection's roots
    // must still be finalized, or this family's fences block forever.
    stream
        .write_all(&[0xFF, 0xFE, 0xFD, b'\n'])
        .expect("failed to write invalid bytes");
    stream.flush().expect("failed to flush invalid bytes");
    thread::sleep(Duration::from_millis(200));

    let response = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::SyncFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
        Duration::from_secs(3),
    )
    .expect("sync.family must not hang after a trace connection read error");
    assert!(response.ok, "sync.family failed: {response:?}");
    drop(stream);
}

/// Family resolution reads `.git` and canonicalizes paths — filesystem I/O.
/// One repo on a hung/slow filesystem must not stall trace draining for
/// every other repository (the I/O must run outside the shared ingress lock).
#[test]
#[serial]
#[cfg(not(windows))]
fn daemon_trace_family_resolution_io_does_not_block_other_readers() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon =
        DaemonGuard::start_with_env(&repo, &[("GIT_AI_TEST_FAMILY_RESOLVE_DELAY_MS", "3000")]);
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    // Connection A: a def_repo whose worktree path carries the stall marker;
    // its family resolution sleeps 3s, simulating a hung filesystem. The root
    // is read-only so the only thing that could delay other repos is the
    // ingress lock the reader holds while resolving.
    let mut slow_repo_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to connect slow-repo trace socket");
    write_trace_frames(
        &mut slow_repo_stream,
        &[
            json!({
                "event": "start",
                "sid": "family-resolve-slow-root",
                "argv": ["git", "status", "--short"],
                "time_ns": 60_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "family-resolve-slow-root",
                "worktree": "/tmp/git-ai-family-resolve-stall/repo",
                "time_ns": 60_001u64,
            }),
        ],
    );
    thread::sleep(Duration::from_millis(200));

    // Connection B: a healthy repo must still be processed promptly.
    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "family-resolve-healthy-root",
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
                "time_ns": 61_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "family-resolve-healthy-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 61_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "family-resolve-healthy-root",
                "code": 0,
                "time_ns": 61_100u64,
            }),
            trace_atexit_frame("family-resolve-healthy-root", 0, 61_101u64),
        ],
    );

    let start = std::time::Instant::now();
    let mut healthy_completed = false;
    while start.elapsed() < Duration::from_millis(1500) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            healthy_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    daemon.shutdown();
    assert!(
        healthy_completed,
        "a healthy repository's trace traffic must not wait behind another repo's family-resolution I/O"
    );
}

/// Only ~6 of the ~25-35 frames a mutating git command emits are ever
/// consumed downstream; the rest must be dropped at ingestion instead of
/// occupying ingest-queue slots. A queue sized to hold just the consumed
/// frames must survive a root padded with realistic trace2 noise.
#[test]
#[serial]
#[cfg(not(windows))]
fn daemon_noise_frames_do_not_consume_ingest_capacity() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            // Room for the consumed frames (start, def_repo, cmd_name, exit,
            // atexit, close marker) but not for the noise.
            ("GIT_AI_TEST_TRACE_INGEST_QUEUE_CAPACITY", "8"),
            // Nothing drains while the frames arrive.
            ("GIT_AI_TEST_TRACE_INGEST_WORKER_START_DELAY_MS", "4000"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "3600"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    let sid = "noise-heavy-root";
    let mut frames = vec![
        json!({"event": "version", "sid": sid, "evt": "3", "exe": "2.49.0"}),
        json!({
            "event": "start",
            "sid": sid,
            "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
            "time_ns": 80_000u64,
        }),
        json!({"event": "cmd_path", "sid": sid, "path": "/usr/bin/git"}),
        json!({"event": "cmd_ancestry", "sid": sid, "ancestry": ["zsh", "login"]}),
        json!({
            "event": "def_repo",
            "sid": sid,
            "worktree": worktree,
            "repo": git_dir,
            "time_ns": 80_001u64,
        }),
        json!({"event": "cmd_name", "sid": sid, "name": "commit", "hierarchy": "commit"}),
    ];
    for i in 0..10u64 {
        frames.push(json!({
            "event": if i % 2 == 0 { "region_enter" } else { "data" },
            "sid": sid,
            "time_ns": 80_010 + i,
            "category": "index",
            "label": "noise",
        }));
    }
    frames.push(json!({"event": "child_start", "sid": sid, "child_id": 0, "argv": ["hook"]}));
    frames.push(json!({"event": "child_exit", "sid": sid, "child_id": 0, "code": 0}));
    frames.push(json!({"event": "exit", "sid": sid, "code": 0, "time_ns": 80_100u64}));
    frames.push(trace_atexit_frame(sid, 0, 80_101));
    send_trace_frames(&trace_socket, &frames);

    // Once the worker wakes, the root must complete: noise frames must not
    // have overflowed the queue (which would fail closed and kill the daemon).
    let start = std::time::Instant::now();
    let mut completed = false;
    while start.elapsed() < Duration::from_secs(15) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        completed,
        "noise frames must be filtered at ingestion, not fill the queue:\n{}",
        daemon.stderr_contents()
    );
    let response = send_control_request(&control_socket_path, &ControlRequest::Ping)
        .expect("daemon should still be serving after the noise-heavy root");
    assert!(response.ok, "ping failed: {response:?}");
    daemon.shutdown();
}

#[test]
#[cfg(not(windows))]
fn daemon_trace_accept_loop_not_serialized_by_silent_connections() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    // Connections that never send a byte must not hold the accept loop
    // hostage: a later connection's frames have to be read promptly.
    let mut silent_streams = Vec::new();
    for _ in 0..20 {
        silent_streams.push(
            open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open silent trace connection"),
        );
    }

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "silent-conn-followup",
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
                "time_ns": 20_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "silent-conn-followup",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 20_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "silent-conn-followup",
                "code": 0,
                "time_ns": 20_100u64,
            }),
            trace_atexit_frame("silent-conn-followup", 0, 20_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(1) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    panic!(
        "daemon did not process a trace connection within 1s while 20 silent connections were open; \
         the accept loop must hand connections to reader threads without doing per-connection reads"
    );
}

#[test]
#[serial]
#[cfg(not(windows))]
fn daemon_trace_reader_spawn_failure_drops_connection_and_keeps_accepting() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    // The readiness wait makes exactly one successful trace-socket connect,
    // consuming one injected failure; the two test connections below consume
    // the rest.
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[("GIT_AI_TEST_TRACE_CONNECTION_SPAWN_FAILURES", "3")],
    );
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    // The first two connections hit the injected reader-spawn failure. The
    // daemon must close them promptly (a blocked git writer would otherwise
    // hang forever) instead of wedging or exiting.
    for connection in 0..2 {
        let mut stream =
            open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open trace connection");
        let started = std::time::Instant::now();
        let mut dropped = false;
        while started.elapsed() < Duration::from_secs(2) {
            if stream
                .write_all(b"\n")
                .and_then(|_| stream.flush())
                .is_err()
            {
                dropped = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            dropped,
            "connection {connection} should be dropped promptly when the reader thread cannot be spawned"
        );
    }

    // With the injected failures exhausted, the daemon must still be alive
    // and process new trace connections normally.
    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "post-spawn-failure",
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
                "time_ns": 30_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "post-spawn-failure",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 30_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "post-spawn-failure",
                "code": 0,
                "time_ns": 30_100u64,
            }),
            trace_atexit_frame("post-spawn-failure", 0, 30_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            daemon.shutdown();
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    daemon.shutdown();
    panic!("daemon did not process a trace connection after injected reader-spawn failures");
}

#[test]
#[cfg(not(windows))]
fn daemon_trace_connection_close_without_atexit_does_not_block_later_trace() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "closed-before-atexit",
                "argv": ["git", "commit", "-m", "incomplete"],
                "time_ns": 9_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "closed-before-atexit",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 9_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "closed-before-atexit",
                "code": 0,
                "time_ns": 9_100u64,
            }),
        ],
    );

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "complete-after-closed-root",
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
                "time_ns": 10_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "complete-after-closed-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 10_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "complete-after-closed-root",
                "code": 0,
                "time_ns": 10_100u64,
            }),
            trace_atexit_frame("complete-after-closed-root", 0, 10_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    panic!("daemon did not process a later trace after a mutating root closed before atexit");
}

#[test]
#[cfg(not(windows))]
fn daemon_control_listener_stalled_connection_does_not_block_later_control_requests() {
    let repo = TestRepo::new_dedicated_daemon();
    let control_socket = daemon_control_socket_path(&repo);
    let _stalled_stream =
        open_local_socket_stream_with_timeout(&control_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled control socket");
    thread::sleep(Duration::from_millis(50));

    let response = send_control_request(
        &control_socket,
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    )
    .expect("later control request should complete while an earlier control socket is stalled");

    assert!(
        response.ok,
        "later control request should return an ok response: {:?}",
        response
    );
}

#[test]
#[cfg(windows)]
fn daemon_windows_control_pipe_worker_exhaustion_does_not_block_later_control_requests() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_WINDOWS_CONTROL_PIPE_WORKERS", "2"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let control_socket = daemon_control_socket_path(&repo);

    let _stalled_streams = (0..2)
        .map(|_| {
            open_local_socket_stream_with_timeout(&control_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open stalled control pipe")
        })
        .collect::<Vec<_>>();
    thread::sleep(Duration::from_millis(100));

    let response = send_control_request(
        &control_socket,
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    )
    .expect("control request should complete after every original pipe worker is stalled");

    assert!(
        response.ok,
        "later control request should return an ok response: {:?}",
        response
    );
    daemon.shutdown();
}

#[test]
#[cfg(windows)]
fn daemon_windows_trace_pipe_worker_exhaustion_does_not_block_later_trace_connections() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_WINDOWS_TRACE_PIPE_WORKERS", "2"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let _stalled_streams = (0..2)
        .map(|_| {
            open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open stalled trace pipe")
        })
        .collect::<Vec<_>>();
    thread::sleep(Duration::from_millis(100));

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "windows-exhaustion-followup",
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
                "time_ns": 15_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "windows-exhaustion-followup",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 15_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "windows-exhaustion-followup",
                "code": 0,
                "time_ns": 15_100u64,
            }),
            trace_atexit_frame("windows-exhaustion-followup", 0, 15_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            daemon.shutdown();
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    daemon.shutdown();
    panic!(
        "daemon did not process a later trace connection after every original pipe worker was stalled"
    );
}

#[test]
#[serial]
#[cfg(not(windows))]
fn daemon_trace_ingest_backpressure_shuts_down_without_blocking_listener() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_INGEST_QUEUE_CAPACITY", "1"),
            ("GIT_AI_TEST_TRACE_INGEST_WORKER_START_DELAY_MS", "5000"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to connect trace socket");
    write_trace_frames(
        &mut stream,
        &[
            json!({
                "event": "start",
                "sid": "backpressure-root",
                "argv": ["git", "commit", "-m", "synthetic"],
                "time_ns": 20_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "backpressure-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 20_001u64,
            }),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }

    panic!("daemon did not fail closed within 2s when trace ingest queue capacity was exhausted");
}

#[test]
fn daemon_failed_rebase_does_not_consume_later_continue_reflog_entry() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut shared_file = repo.filename("shared.txt");
    shared_file.set_contents(lines!["line 1", "line 2"]);
    repo.stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");
    let mut feature_file = repo.filename("shared.txt");
    feature_file.set_contents(lines!["line 1", "AI feature line 2"]);
    feature_file
        .repo
        .git_ai(&native_checkpoint_args!(
            feature_file.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    repo.stage_all_and_commit("AI feature changes")
        .expect("feature commit should succeed");
    let feature_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse feature should succeed")
        .trim()
        .to_string();
    assert!(
        has_commit_links(&repo, &feature_sha),
        "feature commit should have a local link before rebase"
    );

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");
    let mut main_file = repo.filename("shared.txt");
    main_file.set_contents(lines!["line 1", "main change line 2"]);
    repo.stage_all_and_commit("main conflicting change")
        .expect("main commit should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    repo.sync_daemon();

    let rebase_result = repo.git_og(&["rebase", &default_branch]);
    assert!(
        rebase_result.is_err(),
        "raw rebase should fail due to conflict"
    );

    fs::write(
        repo.path().join("shared.txt"),
        "line 1\nmain change line 2\nAI feature line 2\n",
    )
    .expect("failed to write resolved conflict");
    repo.git_og(&["add", "shared.txt"])
        .expect("raw add should succeed");
    repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
        .expect("raw rebase --continue should succeed");
    let rebased_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse rebased HEAD should succeed")
        .trim()
        .to_string();
    assert_ne!(
        rebased_sha, feature_sha,
        "rebase --continue should create a rewritten commit"
    );

    let rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let rebase_session_arg = format!("git-ai.testSyncSession={rebase_session}");
    let continue_session_arg = format!("git-ai.testSyncSession={continue_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "failed-rebase-start",
                "argv": ["git", "-c", rebase_session_arg, "-C", worktree, "rebase", default_branch],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "failed-rebase-start",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "failed-rebase-start",
                "code": 1,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("failed-rebase-start", 1, 1_101u64),
            json!({
                "event": "start",
                "sid": "rebase-continue",
                "argv": ["git", "-c", continue_session_arg, "-C", worktree, "rebase", "--continue"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "rebase-continue",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "rebase-continue",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("rebase-continue", 0, 2_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[rebase_session, continue_session]);

    assert!(
        has_commit_links(&repo, &rebased_sha),
        "rebased commit should get the remapped note even when failed rebase processing is delayed until after --continue"
    );
}

#[test]
fn daemon_late_cherry_pick_trace_uses_actual_destination_not_stale_commit_entry() {
    let mut repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut file = repo.filename("picked.txt");
    file.set_contents(lines!["base"]);
    let base_commit = repo
        .stage_all_and_commit("base")
        .expect("base commit should succeed");
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "source"])
        .expect("checkout source should succeed");
    file.insert_at(1, lines!["AI picked line"]);
    file.repo
        .git_ai(&native_checkpoint_args!(
            file.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    let source_commit = repo
        .stage_all_and_commit("source change")
        .expect("source commit should succeed");
    repo.assert_commit_links(
        &source_commit.commit_sha,
        &[("codex", "daemon-test-session")],
    );

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");

    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(lines!["main branch line"]);
    let main_tip = repo
        .stage_all_and_commit("main branch advance")
        .expect("main branch advance should succeed");

    fs::write(repo.path().join("stale.txt"), "stale\n").expect("write stale file");
    repo.git_og(&["add", "stale.txt"])
        .expect("raw stale add should succeed");
    repo.git_og(&["commit", "-m", "stale plain commit"])
        .expect("raw stale commit should succeed");
    let stale_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse stale commit should succeed")
        .trim()
        .to_string();
    assert_ne!(stale_commit, base_commit.commit_sha);
    assert!(
        !has_commit_links(&repo, &stale_commit),
        "raw stale commit should not have an session link"
    );

    repo.git_og(&["reset", "--hard", &main_tip.commit_sha])
        .expect("raw reset should succeed");
    repo.restart_dedicated_daemon_for_test();

    repo.git_og(&["cherry-pick", &source_commit.commit_sha])
        .expect("raw cherry-pick should succeed");
    let picked_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse picked commit should succeed")
        .trim()
        .to_string();
    assert_ne!(picked_commit, source_commit.commit_sha);
    assert_ne!(picked_commit, stale_commit);
    assert!(
        !has_commit_links(&repo, &picked_commit),
        "raw cherry-pick should not write the link before synthetic trace processing"
    );

    let cherry_pick_session = repos::test_repo::new_daemon_test_sync_session_id();
    let cherry_pick_session_arg = format!("git-ai.testSyncSession={cherry_pick_session}");
    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "late-cherry-pick",
                "argv": ["git", "-c", cherry_pick_session_arg, "-C", worktree, "cherry-pick", source_commit.commit_sha],
                "worktree": worktree,
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "late-cherry-pick",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "late-cherry-pick",
                "code": 0,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("late-cherry-pick", 0, 1_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[cherry_pick_session]);

    assert!(
        !has_commit_links(&repo, &stale_commit),
        "stale historical commit must not receive the cherry-pick note"
    );
    let mut file = repo.filename("picked.txt");
    file.assert_committed_lines(lines!["base", "AI picked line",]);
}

#[test]
fn daemon_failed_rebase_does_not_consume_later_skip_reflog_entry() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut file = repo.filename("file.txt");
    file.set_contents(lines!["line 1"]);
    repo.stage_all_and_commit("Initial")
        .expect("initial commit should succeed");

    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");
    file.replace_at(0, "AI line 1");
    file.repo
        .git_ai(&native_checkpoint_args!(
            file.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    repo.stage_all_and_commit("AI changes")
        .expect("conflicting AI commit should succeed");

    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(lines!["// AI feature"]);
    feature_file
        .repo
        .git_ai(&native_checkpoint_args!(
            feature_file.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    let feature_commit = repo
        .stage_all_and_commit("Add feature")
        .expect("feature commit should succeed");
    assert!(
        has_commit_links(&repo, &feature_commit.commit_sha),
        "feature commit should have a local link before rebase"
    );

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");
    file.replace_at(0, "MAIN line 1");
    repo.stage_all_and_commit("Main changes")
        .expect("main commit should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    repo.sync_daemon();

    let rebase_result = repo.git_og(&["rebase", &default_branch]);
    assert!(
        rebase_result.is_err(),
        "raw rebase should fail due to conflict"
    );
    repo.git_og(&["rebase", "--skip"])
        .expect("raw rebase --skip should succeed");
    let rebased_feature_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse rebased feature should succeed")
        .trim()
        .to_string();
    assert_ne!(
        rebased_feature_sha, feature_commit.commit_sha,
        "rebase --skip should rewrite the following feature commit"
    );

    let rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let skip_session = repos::test_repo::new_daemon_test_sync_session_id();
    let rebase_session_arg = format!("git-ai.testSyncSession={rebase_session}");
    let skip_session_arg = format!("git-ai.testSyncSession={skip_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "failed-rebase-before-skip",
                "argv": ["git", "-c", rebase_session_arg, "-C", worktree, "rebase", default_branch],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "failed-rebase-before-skip",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "failed-rebase-before-skip",
                "code": 1,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("failed-rebase-before-skip", 1, 1_101u64),
            json!({
                "event": "start",
                "sid": "rebase-skip",
                "argv": ["git", "-c", skip_session_arg, "-C", worktree, "rebase", "--skip"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "rebase-skip",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "rebase-skip",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("rebase-skip", 0, 2_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[rebase_session, skip_session]);

    assert!(
        has_commit_links(&repo, &rebased_feature_sha),
        "rebased feature commit should get the remapped note when failed rebase processing is delayed until after --skip"
    );
    feature_file.assert_committed_lines(lines!["// AI feature"]);
}

#[test]
#[serial]
fn daemon_trace_ingest_treats_atexit_as_terminal_for_reflog_capture() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let sid = "atexit-commit";
    let completion_baseline = repo.daemon_total_completion_count();

    send_trace_frames(
        &trace_socket,
        &[
            serde_json::json!({
                "event":"start",
                "sid":sid,
                "ts":1,
                "argv":["git","commit","-m","x"],
                "cwd":repo.path().to_string_lossy().to_string(),
            }),
            serde_json::json!({
                "event":"atexit",
                "sid":sid,
                "ts":2,
                "code":1
            }),
        ],
    );

    wait_for_expected_top_level_completions(&repo, completion_baseline, 1);

    let commands = completion_entries_for_command(&repo, "commit");
    assert!(
        commands.iter().any(|command| command.exit_code == Some(1)
            && command.status == "ok"
            && command.seq > 0),
        "atexit terminal frames should still produce a tracked commit command"
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_checkpoint_stage_checkpoint_two_commits_preserve_ai_lines() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let file_rel = "daemon-two-ai-lines.txt";
    let file_path = repo.path().join(file_rel);
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(&file_path, "base\n").expect("failed to seed base file");
    traced_git_with_env(
        &repo,
        &["add", file_rel],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .expect("failed to open file for first append");
        writeln!(f, "test").expect("failed to append first ai line");
    }
    let checkpoint_output = repo
        .git_ai_with_env(
            &native_checkpoint_args!(file_rel),
            &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
        )
        .expect("first delegated ai checkpoint should succeed");
    expected_top_level_completions +=
        repos::test_repo::parse_checkpoint_request_count(&checkpoint_output);
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging first ai line should succeed");

    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .expect("failed to open file for second append");
        writeln!(f, "test1").expect("failed to append second ai line");
    }
    let checkpoint_output = repo
        .git_ai_with_env(
            &native_checkpoint_args!(file_rel),
            &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
        )
        .expect("second delegated ai checkpoint should succeed");
    expected_top_level_completions +=
        repos::test_repo::parse_checkpoint_request_count(&checkpoint_output);
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["commit", "-m", "first ai line"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("first commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging second ai line should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "second ai line"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    let mut file = repo.filename(file_rel);
    file.assert_committed_lines(lines!["base", "test", "test1"]);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_checkpoint_stage_checkpoint_non_adjacent_hunks_survive_split_commits() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let file_rel = "daemon-non-adjacent.md";
    let file_path = repo.path().join(file_rel);
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    let initial = "\
Top line

**Section Alpha**
alpha body

middle line 1
middle line 2

**Section Omega**
omega body
";
    fs::write(&file_path, initial).expect("failed to write initial content");
    traced_git_with_env(
        &repo,
        &["add", file_rel],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    let first_ai_hunk = "\
Top line

### Section Alpha
alpha body

middle line 1
middle line 2

**Section Omega**
omega body
";
    fs::write(&file_path, first_ai_hunk).expect("failed to write first hunk content");
    let checkpoint_output = repo
        .git_ai_with_env(
            &native_checkpoint_args!(file_rel),
            &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
        )
        .expect("first delegated checkpoint should succeed");
    expected_top_level_completions +=
        repos::test_repo::parse_checkpoint_request_count(&checkpoint_output);
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging first hunk should succeed");

    let both_hunks = "\
Top line

### Section Alpha
alpha body

middle line 1
middle line 2

### Section Omega
omega body
";
    fs::write(&file_path, both_hunks).expect("failed to write both hunks content");
    let checkpoint_output = repo
        .git_ai_with_env(
            &native_checkpoint_args!(file_rel),
            &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
        )
        .expect("second delegated checkpoint should succeed");
    expected_top_level_completions +=
        repos::test_repo::parse_checkpoint_request_count(&checkpoint_output);
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit first staged hunk"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("first split commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging remaining hunk should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit second hunk"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second split commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    let mut file = repo.filename(file_rel);
    file.assert_committed_lines(lines![
        "Top line",
        "",
        "### Section Alpha",
        "alpha body",
        "",
        "middle line 1",
        "middle line 2",
        "",
        "### Section Omega",
        "omega body",
    ]);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_write_mode_applies_amend_rewrite() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("pure-trace.txt"), "line 1\n").expect("failed to write file");
    traced_git_with_env(
        &repo,
        &["add", "pure-trace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "initial"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("commit should succeed");

    fs::write(repo.path().join("pure-trace.txt"), "line 1\nline 2\n")
        .expect("failed to update file");
    traced_git_with_env(
        &repo,
        &["add", "pure-trace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add before amend should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "--amend", "-m", "initial amended"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("amend should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_rebase_abort_emits_abort_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("rebase-conflict.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "-b", "feature"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature branch checkout should succeed");
    fs::write(repo.path().join("rebase-conflict.txt"), "feature\n")
        .expect("failed to write feature branch change");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "feature change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");
    fs::write(repo.path().join("rebase-conflict.txt"), "main\n")
        .expect("failed to write default branch change");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "feature"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout feature should succeed");
    let rebase_conflict = traced_git_with_env(
        &repo,
        &["rebase", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    );
    assert!(
        rebase_conflict.is_err(),
        "rebase should conflict for abort flow coverage"
    );
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
    traced_git_with_env(
        &repo,
        &["rebase", "--abort"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("rebase abort should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_abort_emits_abort_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("cherry-conflict.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "-b", "topic"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic branch checkout should succeed");
    fs::write(repo.path().join("cherry-conflict.txt"), "topic\n")
        .expect("failed to write topic branch change");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "topic change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic commit should succeed");
    let topic_sha = repo
        .git(&["rev-parse", "topic"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();

    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");
    fs::write(repo.path().join("cherry-conflict.txt"), "main\n")
        .expect("failed to write default branch conflicting change");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch commit should succeed");

    let cherry_pick_conflict = traced_git_with_env(
        &repo,
        &["cherry-pick", topic_sha.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    );
    assert!(
        cherry_pick_conflict.is_err(),
        "cherry-pick should conflict for abort flow coverage"
    );
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
    traced_git_with_env(
        &repo,
        &["cherry-pick", "--abort"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("cherry-pick abort should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_stash_main_ops_emit_stash_events() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("stash-case.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "stash-case.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    fs::write(repo.path().join("stash-case.txt"), "base\nchange one\n")
        .expect("failed to write stash content");
    traced_git_with_env(
        &repo,
        &["stash", "push", "-m", "save one"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash push should succeed");
    // `git stash list` is readonly — the daemon's readonly fast-path drops it
    // before it reaches the ingest queue, so we run it without incrementing
    // expected_top_level_completions and do not expect it in the rewrite log.
    repo.git_og_with_env(&["stash", "list"], &env_refs)
        .expect("stash list should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "apply", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash apply should succeed");

    traced_git_with_env(
        &repo,
        &["reset", "--hard", "HEAD"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("reset hard should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "pop", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash pop should succeed");

    traced_git_with_env(
        &repo,
        &["add", "stash-case.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add before commit should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "stash pop result"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("commit after stash pop should succeed");

    fs::write(repo.path().join("stash-case.txt"), "base\nchange two\n")
        .expect("failed to write second stash content");
    traced_git_with_env(
        &repo,
        &["stash", "push", "-m", "save two"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second stash push should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "drop", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash drop should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_continue_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = vec![
        (env[0].0, env[0].1.as_str()),
        (env[1].0, env[1].1.as_str()),
        ("GIT_EDITOR", "true"),
    ];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("cherry-continue.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["checkout", "-b", "topic"], &env_refs)
        .expect("topic checkout should succeed");
    fs::write(repo.path().join("cherry-continue.txt"), "topic\n")
        .expect("failed to write topic change");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("topic add should succeed");
    repo.git_og_with_env(&["commit", "-m", "topic change"], &env_refs)
        .expect("topic commit should succeed");
    let topic_sha = repo
        .git(&["rev-parse", "topic"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();

    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout default should succeed");
    fs::write(repo.path().join("cherry-continue.txt"), "main\n")
        .expect("failed to write main conflict change");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("main add should succeed");
    repo.git_og_with_env(&["commit", "-m", "main change"], &env_refs)
        .expect("main commit should succeed");

    let cherry_conflict = repo.git_og_with_env(&["cherry-pick", topic_sha.as_str()], &env_refs);
    assert!(
        cherry_conflict.is_err(),
        "cherry-pick should conflict before continue"
    );
    wait_for_expected_top_level_completions(&repo, 0, 9);

    fs::write(repo.path().join("cherry-continue.txt"), "resolved\n")
        .expect("failed to write resolved cherry content");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("add resolved cherry content should succeed");
    repo.git_og_with_env(&["cherry-pick", "--continue"], &env_refs)
        .expect("cherry-pick continue should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 11);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_rebase_with_short_sha_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    // Create base commit on default branch
    fs::write(repo.path().join("rebase-short.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "rebase-short.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    // Create feature branch with a commit
    traced_git_with_env(
        &repo,
        &["checkout", "-b", "feature-rebase-short"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature branch checkout should succeed");
    fs::write(repo.path().join("feature-only.txt"), "feature content\n")
        .expect("failed to write feature file");
    traced_git_with_env(
        &repo,
        &["add", "feature-only.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "feature change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature commit should succeed");

    // Go back to default branch and add a non-conflicting commit
    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default should succeed");
    fs::write(repo.path().join("main-only.txt"), "main content\n")
        .expect("failed to write main file");
    traced_git_with_env(
        &repo,
        &["add", "main-only.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("main add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main advance"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("main commit should succeed");

    // Get the short SHA of the latest main commit
    let main_full_sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD rev-parse should succeed")
        .trim()
        .to_string();
    let main_short_sha = &main_full_sha[..7];

    // Switch to feature branch and rebase onto main using SHORT SHA
    traced_git_with_env(
        &repo,
        &["checkout", "feature-rebase-short"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout feature should succeed");
    traced_git_with_env(
        &repo,
        &["rebase", main_short_sha],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("rebase with short SHA should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_with_short_sha_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    // Create base commit
    fs::write(repo.path().join("short-sha-test.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "short-sha-test.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    // Create topic branch with a commit
    traced_git_with_env(
        &repo,
        &["checkout", "-b", "topic-short-sha"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic branch checkout should succeed");
    fs::write(repo.path().join("short-sha-test.txt"), "topic content\n")
        .expect("failed to write topic change");
    traced_git_with_env(
        &repo,
        &["add", "short-sha-test.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "topic change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic commit should succeed");

    // Get the full SHA and derive a short (7-char) prefix
    let topic_full_sha = repo
        .git(&["rev-parse", "topic-short-sha"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();
    let topic_short_sha = &topic_full_sha[..7];

    // Switch back to default branch
    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");

    // Cherry-pick using the SHORT SHA -- this is the key part of the test
    traced_git_with_env(
        &repo,
        &["cherry-pick", topic_short_sha],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("cherry-pick with short SHA should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_switch_tracks_success_and_conflict_failure() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("switch-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "switch-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["switch", "-c", "feature"], &env_refs)
        .expect("switch -c feature should succeed");
    fs::write(repo.path().join("switch-case.txt"), "feature branch\n")
        .expect("failed to write feature content");
    repo.git_og_with_env(&["add", "switch-case.txt"], &env_refs)
        .expect("feature add should succeed");
    repo.git_og_with_env(&["commit", "-m", "feature"], &env_refs)
        .expect("feature commit should succeed");

    repo.git_og_with_env(&["switch", default_branch.as_str()], &env_refs)
        .expect("switch back to default branch should succeed");
    repo.git_og_with_env(&["switch", "feature"], &env_refs)
        .expect("switch to feature should succeed");
    repo.git_og_with_env(&["switch", default_branch.as_str()], &env_refs)
        .expect("switch back to default branch should succeed");

    fs::write(repo.path().join("switch-case.txt"), "dirty local change\n")
        .expect("failed to write dirty local change");
    let switch_failure = repo.git_og_with_env(&["switch", "feature"], &env_refs);
    assert!(
        switch_failure.is_err(),
        "switch should fail when local changes would be overwritten"
    );

    wait_for_expected_top_level_completions(&repo, 0, 9);

    let switch_entries = completion_entries_for_command(&repo, "switch");
    let saw_switch_success = switch_entries
        .iter()
        .any(|entry| entry.exit_code == Some(0));
    let saw_switch_failure = switch_entries
        .iter()
        .any(|entry| entry.exit_code.unwrap_or(0) != 0);
    assert!(saw_switch_success, "switch success should be tracked");
    assert!(saw_switch_failure, "switch failure should be tracked");
}

#[test]
#[serial]
fn daemon_pure_trace_socket_checkout_tracks_success_failure_and_new_branch() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("checkout-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "checkout-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["checkout", "-b", "feature"], &env_refs)
        .expect("checkout -b feature should succeed");
    fs::write(repo.path().join("checkout-case.txt"), "feature branch\n")
        .expect("failed to write feature content");
    repo.git_og_with_env(&["add", "checkout-case.txt"], &env_refs)
        .expect("feature add should succeed");
    repo.git_og_with_env(&["commit", "-m", "feature"], &env_refs)
        .expect("feature commit should succeed");

    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout default should succeed");
    repo.git_og_with_env(&["checkout", "feature"], &env_refs)
        .expect("checkout feature should succeed");
    repo.git_og_with_env(&["checkout", "-b", "hotfix"], &env_refs)
        .expect("checkout -b hotfix should succeed");
    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout back to default should succeed");

    fs::write(
        repo.path().join("checkout-case.txt"),
        "dirty local change\n",
    )
    .expect("failed to write dirty local change");
    let checkout_failure = repo.git_og_with_env(&["checkout", "feature"], &env_refs);
    assert!(
        checkout_failure.is_err(),
        "checkout should fail when local changes would be overwritten"
    );

    wait_for_expected_top_level_completions(&repo, 0, 10);

    let checkout_entries = completion_entries_for_command(&repo, "checkout");
    let saw_checkout_success = checkout_entries
        .iter()
        .any(|entry| entry.exit_code == Some(0));
    let saw_checkout_failure = checkout_entries
        .iter()
        .any(|entry| entry.exit_code.unwrap_or(0) != 0);
    assert!(saw_checkout_success, "checkout success should be tracked");
    assert!(saw_checkout_failure, "checkout failure should be tracked");
}

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_fast_forward_tracks_pull_command() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("pull-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "pull-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let remote_root = tempfile::tempdir().expect("remote tempdir should be created");
    let bare_remote = remote_root.path().join("origin.git");
    let remote_clone = remote_root.path().join("origin-work");
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("pull-case.txt"), "base\nremote update\n")
        .expect("failed to write remote update");
    run_git(&["-C", remote_clone_str.as_str(), "add", "pull-case.txt"]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote update",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    repo.git_og_with_env(
        &["pull", "--ff-only", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("fast-forward pull should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 5);

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(saw_pull_success, "pull success should be tracked");
    assert!(
        fs::read_to_string(repo.path().join("pull-case.txt"))
            .expect("pulled file should be readable")
            .contains("remote update"),
        "pull fast-forward should update the worktree contents"
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_rebase_tracks_pull_and_rebase_completion() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("pull-rebase-base.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "pull-rebase-base.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let root = repo
        .path()
        .parent()
        .expect("test repo path should have parent")
        .to_path_buf();
    let unique = repo
        .path()
        .file_name()
        .expect("test repo path should have filename")
        .to_string_lossy();
    let bare_remote = root.join(format!("origin-rebase-{unique}.git"));
    let remote_clone = root.join(format!("origin-rebase-work-{unique}"));
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("remote-only.txt"), "remote\n")
        .expect("failed to write remote file");
    run_git(&["-C", remote_clone_str.as_str(), "add", "remote-only.txt"]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote commit",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    fs::write(repo.path().join("local-only.txt"), "local\n").expect("failed to write local file");
    repo.git_og_with_env(&["add", "local-only.txt"], &env_refs)
        .expect("local add should succeed");
    repo.git_og_with_env(&["commit", "-m", "local commit"], &env_refs)
        .expect("local commit should succeed");

    repo.git_og_with_env(
        &["pull", "--rebase", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pull --rebase should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 7);

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_rebase_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(
        saw_pull_rebase_success,
        "pull --rebase success should be tracked"
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_autostash_preserves_local_changes_and_tracks_command() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("autostash-local.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "autostash-local.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let root = repo
        .path()
        .parent()
        .expect("test repo path should have parent")
        .to_path_buf();
    let bare_remote = root.join("origin-autostash.git");
    let remote_clone = root.join("origin-autostash-work");
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("autostash-remote.txt"), "remote\n")
        .expect("failed to write remote update file");
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "add",
        "autostash-remote.txt",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote update",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    fs::write(
        repo.path().join("autostash-local.txt"),
        "base\nlocal dirty change\n",
    )
    .expect("failed to write local dirty change");

    repo.git_og_with_env(
        &[
            "pull",
            "--rebase",
            "--autostash",
            "origin",
            default_branch.as_str(),
        ],
        &env_refs,
    )
    .expect("pull --rebase --autostash should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 5);

    let local_contents = fs::read_to_string(repo.path().join("autostash-local.txt"))
        .expect("local file should remain readable");
    assert!(
        local_contents.contains("local dirty change"),
        "autostash pull should preserve local dirty change content"
    );

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_autostash_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(
        saw_pull_autostash_success,
        "pull --rebase --autostash success should be tracked"
    );
}

#[test]
fn daemon_delayed_pull_rebase_autostash_does_not_consume_later_commit() {
    let (local, _upstream) =
        TestRepo::new_with_remote_with_daemon_scope(DaemonTestScope::Dedicated);
    let trace_socket = daemon_trace_socket_path(&local);
    let worktree = repo_workdir_string(&local);
    let git_dir = local.path().join(".git").to_string_lossy().to_string();

    let mut readme = local.filename("README.md");
    readme.set_contents(lines!["# Test Repo"]);
    let initial = local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    readme.assert_committed_lines(lines!["# Test Repo"]);

    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    let mut committed_ai = local.filename("ai_feature.txt");
    committed_ai.set_contents(lines![
        "AI generated feature line 1",
        "AI generated feature line 2",
    ]);
    committed_ai
        .repo
        .git_ai(&native_checkpoint_args!(
            committed_ai.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    let local_ai = local
        .stage_all_and_commit("add AI feature")
        .expect("AI feature commit should succeed");
    committed_ai.assert_committed_lines(lines![
        "AI generated feature line 1",
        "AI generated feature line 2",
    ]);

    let branch = local.current_branch();
    local
        .git(&["reset", "--hard", &initial.commit_sha])
        .expect("reset to initial commit should succeed");

    let mut upstream_file = local.filename("upstream_change.txt");
    upstream_file.set_contents(lines!["upstream content"]);
    local
        .stage_all_and_commit("upstream divergent commit")
        .expect("upstream commit should succeed");
    upstream_file.assert_committed_lines(lines!["upstream content"]);

    local
        .git(&["push", "--force", "origin", &format!("HEAD:{}", branch)])
        .expect("force push upstream commit should succeed");
    local
        .git(&["reset", "--hard", &local_ai.commit_sha])
        .expect("reset back to local AI commit should succeed");

    let mut uncommitted_ai = local.filename("uncommitted_ai.txt");
    uncommitted_ai.set_contents(lines!["Uncommitted AI line"]);
    uncommitted_ai
        .repo
        .git_ai(&native_checkpoint_args!(
            uncommitted_ai.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    local
        .git_ai(&native_checkpoint_args!("uncommitted_ai.txt"))
        .expect("checkpoint should succeed");
    local.sync_daemon();

    local
        .git_og(&["pull", "--rebase", "--autostash"])
        .expect("raw pull --rebase --autostash should succeed");
    local
        .git_og(&["add", "-A"])
        .expect("raw add should succeed");
    local
        .git_og(&["commit", "-m", "commit uncommitted AI work"])
        .expect("raw commit should succeed");
    let final_commit = local
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse final commit should succeed")
        .trim()
        .to_string();

    let pull_session = repos::test_repo::new_daemon_test_sync_session_id();
    let commit_session = repos::test_repo::new_daemon_test_sync_session_id();
    let pull_session_arg = format!("git-ai.testSyncSession={pull_session}");
    let commit_session_arg = format!("git-ai.testSyncSession={commit_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "delayed-pull-autostash",
                "argv": ["git", "-c", pull_session_arg, "-C", worktree, "pull", "--rebase", "--autostash"],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-pull-autostash",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-pull-autostash",
                "code": 0,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("delayed-pull-autostash", 0, 1_101u64),
            json!({
                "event": "start",
                "sid": "delayed-commit-after-pull",
                "argv": ["git", "-c", commit_session_arg, "-C", worktree, "commit", "-m", "commit uncommitted AI work"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-commit-after-pull",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-commit-after-pull",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("delayed-commit-after-pull", 0, 2_101u64),
        ],
    );
    local.sync_daemon_external_completion_sessions(&[pull_session, commit_session]);

    assert!(
        has_commit_links(&local, &final_commit),
        "delayed pull processing must not consume the following commit reflog entry"
    );
    uncommitted_ai.assert_committed_lines(lines!["Uncommitted AI line"]);
}

#[test]
fn daemon_delayed_failed_rebase_continue_does_not_consume_final_continue() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    fs::write(repo.path().join("config_a.py"), "FLAG_A = 0\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    repo.git_og(&["commit", "-m", "Initial config_a"]).unwrap();
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 0\nBATCH = 10\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og(&["commit", "-m", "Initial config_b"]).unwrap();
    let main_branch = repo.current_branch();

    fs::write(repo.path().join("config_a.py"), "FLAG_A = 1\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    repo.git_og(&["commit", "-m", "main sets flag_a"]).unwrap();
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 1\nBATCH = 50\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og(&["commit", "-m", "main sets config_b"])
        .unwrap();

    let base_sha = repo
        .git_og(&["rev-parse", "HEAD~2"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    let mut module_a = repo.filename("module_a.py");
    module_a.set_contents(lines!["class ModuleA:", "    pass"]);
    module_a
        .repo
        .git_ai(&native_checkpoint_args!(
            module_a.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    let original_c1 = repo.stage_all_and_commit("feat: C1 add ModuleA").unwrap();
    module_a.assert_committed_lines(lines!["class ModuleA:", "    pass"]);

    let mut config_a = repo.filename("config_a.py");
    config_a.set_contents(lines!["FLAG_A = 2"]);
    config_a
        .repo
        .git_ai(&native_checkpoint_args!(
            config_a.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    let original_c2 = repo.stage_all_and_commit("feat: C2 sets flag_a").unwrap();
    config_a.assert_committed_lines(lines!["FLAG_A = 2"]);

    let mut module_c = repo.filename("module_c.py");
    module_c.set_contents(lines!["class ModuleC:", "    pass"]);
    module_c
        .repo
        .git_ai(&native_checkpoint_args!(
            module_c.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    let original_c3 = repo.stage_all_and_commit("feat: C3 add ModuleC").unwrap();
    module_c.assert_committed_lines(lines!["class ModuleC:", "    pass"]);

    let mut config_b = repo.filename("config_b.py");
    config_b.set_contents(lines!["FLAG_B = 1", "BATCH = 200"]);
    config_b
        .repo
        .git_ai(&native_checkpoint_args!(
            config_b.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    let original_c4 = repo.stage_all_and_commit("feat: C4 sets batch").unwrap();
    config_b.assert_committed_lines(lines!["FLAG_B = 1", "BATCH = 200"]);

    let mut module_e = repo.filename("module_e.py");
    module_e.set_contents(lines!["class ModuleE:", "    pass"]);
    module_e
        .repo
        .git_ai(&native_checkpoint_args!(
            module_e.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    let original_c5 = repo.stage_all_and_commit("feat: C5 add ModuleE").unwrap();
    module_e.assert_committed_lines(lines!["class ModuleE:", "    pass"]);
    for commit in [
        &original_c1,
        &original_c2,
        &original_c3,
        &original_c4,
        &original_c5,
    ] {
        assert!(
            has_commit_links(&repo, &commit.commit_sha),
            "original feature commit should have session link"
        );
    }
    repo.sync_daemon();

    assert!(
        repo.git_og(&["rebase", &main_branch]).is_err(),
        "initial raw rebase should stop at config_a conflict"
    );
    fs::write(repo.path().join("config_a.py"), "FLAG_A = 2\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    assert!(
        repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
            .is_err(),
        "first raw rebase --continue should stop at config_b conflict"
    );
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 1\nBATCH = 75\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
        .expect("final raw rebase --continue should finish");

    let final_chain = (0..5)
        .rev()
        .map(|offset| {
            let rev = if offset == 0 {
                "HEAD".to_string()
            } else {
                format!("HEAD~{offset}")
            };
            repo.git_og(&["rev-parse", &rev])
                .unwrap()
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>();

    let initial_rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let first_continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let final_continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let initial_session_arg = format!("git-ai.testSyncSession={initial_rebase_session}");
    let first_continue_session_arg = format!("git-ai.testSyncSession={first_continue_session}");
    let final_continue_session_arg = format!("git-ai.testSyncSession={final_continue_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "delayed-rebase-start",
                "argv": ["git", "-c", initial_session_arg, "-C", worktree, "rebase", main_branch],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-rebase-start",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-rebase-start",
                "code": 1,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("delayed-rebase-start", 1, 1_101u64),
            json!({
                "event": "start",
                "sid": "delayed-first-rebase-continue",
                "argv": ["git", "-c", first_continue_session_arg, "-C", worktree, "rebase", "--continue"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-first-rebase-continue",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-first-rebase-continue",
                "code": 1,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("delayed-first-rebase-continue", 1, 2_101u64),
            json!({
                "event": "start",
                "sid": "delayed-final-rebase-continue",
                "argv": ["git", "-c", final_continue_session_arg, "-C", worktree, "rebase", "--continue"],
                "time_ns": 3_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-final-rebase-continue",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 3_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-final-rebase-continue",
                "code": 0,
                "time_ns": 3_100u64,
            }),
            trace_atexit_frame("delayed-final-rebase-continue", 0, 3_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[
        initial_rebase_session,
        first_continue_session,
        final_continue_session,
    ]);

    for (idx, sha) in final_chain.iter().enumerate() {
        assert!(
            has_commit_links(&repo, sha),
            "rebased commit {} should have session link after delayed continue processing",
            idx + 1
        );
    }
    module_e.assert_committed_lines(lines!["class ModuleE:", "    pass"]);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_concurrent_checkpoint_requests_preserve_exact_line_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let harness = WorkdirRaceHarness::new(&repo, trace_socket.clone());
    let workdir = repo.path().to_path_buf();

    let file_count = 12usize;
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected = Vec::new();
    for idx in 0..file_count {
        let file_rel = format!("daemon-race-concurrent-checkpoint-{idx}.txt");
        let line = format!("ai-line-{idx}");
        fs::write(workdir.join(file_rel.as_str()), format!("{line}\n"))
            .expect("failed to write concurrent checkpoint test file");
        expected.push((file_rel, line));
    }

    let mut expected_completions = 0;
    #[cfg(windows)]
    {
        for (file_rel, _) in &expected {
            expected_completions += harness.run_delegated_checkpoint(&workdir, file_rel.as_str());
        }
    }
    #[cfg(not(windows))]
    {
        let mut checkpoint_threads = Vec::new();
        for (file_rel, _) in &expected {
            let thread_workdir = workdir.clone();
            let harness = harness.clone();
            let file_rel = file_rel.clone();
            checkpoint_threads.push(thread::spawn(move || {
                harness.run_delegated_checkpoint(&thread_workdir, file_rel.as_str())
            }));
        }
        for handle in checkpoint_threads {
            expected_completions += handle
                .join()
                .expect("concurrent delegated checkpoint thread should not panic");
        }
    }

    // Wait for all concurrent checkpoints to complete before adding
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    repo.git_og_with_env(&["add", "."], &env_refs)
        .expect("staging concurrent checkpoint files should succeed");
    expected_completions += 1;

    repo.git_og_with_env(
        &["commit", "-m", "concurrent delegated checkpoint burst"],
        &env_refs,
    )
    .expect("commit for concurrent checkpoint files should succeed");
    expected_completions += 1;

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.assert_commit_links(head.trim(), &[("codex", "daemon-test-session")]);
    for (file_rel, line) in expected {
        let mut file = repo.filename(file_rel.as_str());
        file.assert_committed_lines(lines![line]);
    }
}

// Daemon update check decision logic is tested by unit tests in
// src/commands/upgrade.rs (check_for_update_available_*). The integration
// tests that spawned a full daemon were removed because the post-shutdown
// self-update code made real HTTP calls that caused hangs/flakes.

#[test]
#[serial]
fn daemon_memory_does_not_grow_unbounded_under_trace_load() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    // Create a base commit so the repo has a valid HEAD.
    fs::write(repo.path().join("init.txt"), "init\n").expect("write failed");
    repo.git(&["add", "init.txt"]).expect("add failed");
    repo.git(&["commit", "-m", "init"]).expect("commit failed");

    let mut guard = DaemonGuard::start(&repo);
    let pid = guard.child.id();

    // Let the daemon settle after startup.
    thread::sleep(Duration::from_millis(500));
    let baseline_rss = get_rss_kb(pid).unwrap_or_else(|| {
        eprintln!(
            "WARN: /proc/{}/status not readable, skipping RSS check",
            pid
        );
        0
    });
    eprintln!("daemon pid={} baseline RSS={}KB", pid, baseline_rss);

    let worktree_str = repo.path().to_string_lossy().to_string();

    // Send 2000 complete git trace lifecycle rounds (start + exit + atexit).
    // Each round simulates a complete `git status` invocation with a unique SID.
    for batch in 0..20 {
        let mut frames = Vec::new();
        for i in 0..100u64 {
            let sid = format!("stress-{}-{}", batch, i);
            frames.push(serde_json::json!({
                "event": "start",
                "sid": &sid,
                "argv": ["git", "status"],
                "time_ns": 1000000000u64 + (batch * 100) as u64 + i,
            }));
            frames.push(serde_json::json!({
                "event": "def_repo",
                "sid": &sid,
                "worktree": &worktree_str,
                "repo": repo.path().join(".git").to_string_lossy().to_string(),
            }));
            frames.push(serde_json::json!({
                "event": "exit",
                "sid": &sid,
                "code": 0,
                "time_ns": 1000000001u64 + (batch * 100) as u64 + i,
            }));
            frames.push(trace_atexit_frame(
                &sid,
                0,
                1000000002u64 + (batch * 100) as u64 + i,
            ));
        }
        send_trace_frames(&guard.trace_socket_path, &frames);
        // Small delay to let the daemon process frames.
        thread::sleep(Duration::from_millis(50));
    }

    // Give the daemon time to finish processing all frames.
    thread::sleep(Duration::from_millis(500));

    let final_rss = get_rss_kb(pid).unwrap_or(0);
    let growth = final_rss.saturating_sub(baseline_rss);
    eprintln!(
        "daemon pid={} final RSS={}KB growth={}KB",
        pid, final_rss, growth
    );

    if baseline_rss > 0 && final_rss > 0 {
        // Memory growth should be bounded. With the leak fixes, growth should stay
        // well under 50 MB even after 2000 trace rounds.
        assert!(
            growth < 50_000,
            "daemon RSS grew by {}KB after 2000 trace rounds; expected < 50MB",
            growth,
        );
    } else {
        eprintln!("RSS measurement unavailable, verifying daemon survived load");
    }

    guard.shutdown();
}

#[test]
#[serial]
fn daemon_memory_threshold_logs_uploads_and_aborts_without_draining() {
    let mut mock_api = MockApiServer::start();
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.patch_git_ai_config(|patch| {
        patch.daemon_memory_limit_mb = Some(1024);
    });

    let file_path = repo.path().join("memory-emergency.txt");
    fs::write(&file_path, "Untracked base\n").unwrap();
    repo.git_og(&["add", "memory-emergency.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Initial commit"]).unwrap();

    let mut samples = vec!["100"; 40];
    samples.push("900");
    let sample_sequence = samples.join(",");
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            (
                "GIT_AI_TEST_DAEMON_PEAK_RSS_MB_SEQUENCE",
                sample_sequence.as_str(),
            ),
            ("GIT_AI_TEST_DAEMON_MEMORY_POLL_MS", "25"),
            (
                "GIT_AI_TEST_DELAY_CHECKPOINT_ADMISSION",
                "memory-limit-checkpoint=10000",
            ),
            ("GIT_AI_API_BASE_URL", mock_api.base_url()),
            ("GIT_AI_API_KEY", "test-api-key"),
        ],
    );
    fs::write(&file_path, "Untracked base\nAI before emergency\n").unwrap();
    let request = checkpoint_request(
        &repo,
        "memory-limit-checkpoint".to_string(),
        "memory-limit-session".to_string(),
    );
    let checkpoint_response = send_checkpoint_request_with_timeout(
        &daemon.control_socket_path,
        &request,
        Duration::from_secs(1),
    )
    .expect("checkpoint should be acknowledged before emergency shutdown");
    assert!(
        checkpoint_response.ok,
        "checkpoint should be accepted before emergency shutdown: {checkpoint_response:?}"
    );
    let started = std::time::Instant::now();
    let status = daemon.child.wait().expect("wait for emergency daemon stop");
    assert!(!status.success(), "85% threshold should abort the daemon");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "memory emergency shutdown waited for the delayed checkpoint"
    );
    let logs = daemon.stderr_contents();
    assert!(
        logs.contains("memory emergency threshold reached"),
        "missing emergency memory diagnostic:\n{logs}"
    );
    let requests = mock_api.collect_requests();
    assert!(
        requests
            .iter()
            .filter(|request| request["path"] == "/worker/logs/upload")
            .flat_map(|request| request["body"]["events"].as_array().into_iter().flatten())
            .any(|event| event["message"] == "daemon memory emergency threshold reached"),
        "emergency diagnostic was not uploaded before shutdown: {requests:?}"
    );
}

#[test]
#[serial]
fn daemon_memory_limit_below_startup_usage_aborts_without_restart_loop() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.patch_git_ai_config(|patch| {
        patch.daemon_memory_limit_mb = Some(1024);
    });

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_DAEMON_PEAK_RSS_MB_SEQUENCE", "1024"),
            ("GIT_AI_TEST_DAEMON_MEMORY_POLL_MS", "250"),
        ],
    );
    let status = daemon.child.wait().expect("wait for daemon stop");

    assert!(!status.success(), "startup-over-limit should abort");
    thread::sleep(Duration::from_millis(300));
    assert!(
        send_control_request(&daemon.control_socket_path, &ControlRequest::Ping).is_err(),
        "startup-over-limit daemon must not respawn"
    );
    let logs = daemon.stderr_contents();
    assert!(
        logs.contains("memory emergency threshold reached"),
        "missing startup-limit diagnostic:\n{logs}"
    );
}

#[test]
#[serial]
fn daemon_memory_hard_limit_aborts_without_restart() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.patch_git_ai_config(|patch| {
        patch.daemon_memory_limit_mb = Some(1024);
    });

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_DAEMON_PEAK_RSS_MB_SEQUENCE", "100,100,1024"),
            ("GIT_AI_TEST_DAEMON_MEMORY_POLL_MS", "100"),
        ],
    );
    let status = daemon.child.wait().expect("wait for daemon abort");

    assert!(!status.success(), "hard memory limit must abort the daemon");
    thread::sleep(Duration::from_millis(300));
    assert!(
        send_control_request(&daemon.control_socket_path, &ControlRequest::Ping).is_err(),
        "hard-aborted daemon must not respawn"
    );
    let logs = daemon.stderr_contents();
    assert!(
        logs.contains("memory emergency threshold reached"),
        "missing hard-limit diagnostic:\n{logs}"
    );
}

fn bg_command(repo: &TestRepo, subcommand: &str, extra_args: &[&str]) -> Output {
    bg_command_with_env(repo, subcommand, extra_args, &[])
}

fn bg_command_with_env(
    repo: &TestRepo,
    subcommand: &str,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> Output {
    let daemon_home = repo.daemon_home_path();
    let control_socket_path = daemon_control_socket_path(repo);
    let trace_socket_path = daemon_trace_socket_path(repo);
    let mut command = Command::new(get_binary_path());
    command.arg("bg").arg(subcommand);
    for arg in extra_args {
        command.arg(arg);
    }
    command
        .current_dir(repo.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path());
    for (key, value) in env {
        command.env(key, value);
    }
    configure_test_home_env(&mut command, repo.test_home_path());
    configure_test_daemon_env(
        &mut command,
        &daemon_home,
        &control_socket_path,
        &trace_socket_path,
    );
    command.output().expect("failed to invoke bg command")
}

use std::process::Output;

#[test]
#[serial]
fn daemon_shutdown_hard_kills_process() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut guard = DaemonGuard::start(&repo);

    let config = DaemonConfig::from_home(&repo.daemon_home_path());
    let pid = read_daemon_pid(&config).expect("should read daemon pid");

    // Verify daemon process is alive.
    assert!(
        process_exists(pid),
        "daemon process {} should be alive before hard shutdown",
        pid
    );

    let output = bg_command(&repo, "shutdown", &["--hard"]);
    assert!(
        output.status.success(),
        "shutdown --hard should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Reap the child so the zombie doesn't linger (our test process is the parent).
    let _ = guard.child.wait();

    // Process should be dead.
    for _ in 0..40 {
        if !process_exists(pid) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_exists(pid),
        "daemon process {} should be dead after hard shutdown",
        pid
    );
}

#[test]
#[serial]
fn daemon_restart_brings_up_new_process() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut guard = DaemonGuard::start(&repo);

    let config = DaemonConfig::from_home(&repo.daemon_home_path());
    let old_pid = read_daemon_pid(&config).expect("should read daemon pid");

    // Reap the child first — on Linux the killed process is a zombie until we wait.
    let _ = guard.child.kill();
    let _ = guard.child.wait();

    let output = bg_command(&repo, "restart", &[]);
    assert!(
        output.status.success(),
        "restart should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // New daemon should be up with a different PID.
    let new_pid = read_daemon_pid(&config).expect("should read new daemon pid");
    assert_ne!(old_pid, new_pid, "restart should produce a new daemon PID");

    // New daemon should be responsive.
    let status = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    );
    assert!(
        status.is_ok(),
        "new daemon should respond to status request"
    );

    // Clean up the new detached daemon.
    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[test]
#[serial]
fn daemon_restart_hard_kills_and_restarts() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut guard = DaemonGuard::start(&repo);

    let config = DaemonConfig::from_home(&repo.daemon_home_path());
    let old_pid = read_daemon_pid(&config).expect("should read daemon pid");

    // Reap the child first — on Linux the killed process is a zombie until we wait.
    let _ = guard.child.kill();
    let _ = guard.child.wait();

    let output = bg_command(&repo, "restart", &["--hard"]);
    assert!(
        output.status.success(),
        "restart --hard should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // New daemon should be up.
    let new_pid = read_daemon_pid(&config).expect("should read new daemon pid");
    assert_ne!(
        old_pid, new_pid,
        "hard restart should produce a new daemon PID"
    );

    // Clean up.
    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[test]
#[serial]
fn daemon_shutdown_hard_when_not_running_fails_gracefully() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    // Don't start any daemon — just run shutdown --hard on a cold config.
    // It should not panic / crash.
    let output = bg_command(&repo, "shutdown", &["--hard"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should fail with a readable error about the service not running.
    assert!(
        !output.status.success(),
        "shutdown --hard on cold config should fail"
    );
    assert!(
        stderr.contains("not running")
            || stderr.contains("pid")
            || stderr.contains("not found")
            || stderr.contains("No such file"),
        "shutdown --hard on cold config should fail gracefully: {}",
        stderr
    );
}

#[test]
#[serial]
fn daemon_restart_when_not_running_starts_fresh() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    // No daemon running — restart should just start a new one.
    let output = bg_command(&repo, "restart", &[]);
    assert!(
        output.status.success(),
        "restart with no running daemon should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Daemon should be up.
    let status = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    );
    assert!(
        status.is_ok(),
        "daemon should be reachable after restart from cold state"
    );

    // Clean up.
    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// Regression test for issue #919: daemon must recover from panics in the
/// side-effect pipeline and continue processing subsequent commands.
///
/// This test:
/// 1. Starts a dedicated daemon with a file-based panic flag.
/// 2. Sends a git commit that triggers side-effect processing → panic.
/// 3. Verifies the daemon process is still alive (not a zombie).
/// 4. Removes the panic flag file.
/// 5. Sends another git commit and verifies the daemon processes it normally.
/// 6. Cleanly shuts down the daemon.
#[test]
#[serial]
fn daemon_recovers_from_panic_in_side_effect_pipeline() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    // Create a flag file that will trigger a panic in the side-effect pipeline.
    let panic_flag_path = repo.path().join(".panic_flag");
    fs::write(&panic_flag_path, "1").expect("failed to write panic flag");

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[(
            "GIT_AI_TEST_PANIC_IN_SIDE_EFFECT_FLAG",
            panic_flag_path
                .to_str()
                .expect("panic flag path should be utf-8"),
        )],
    );
    let daemon_pid = daemon.child.id();

    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    // Phase 1 — Send a commit while the panic flag is active.
    // The daemon will panic inside the side-effect pipeline, but catch_unwind
    // should keep it alive.  Because panicked commands do NOT emit completion
    // log entries, we cannot use wait_for_expected_top_level_completions here.
    // Instead we track these commands in a throwaway counter and poll the
    // daemon's control socket to confirm it is still responsive.
    let mut _throwaway = 0u64;

    fs::write(repo.path().join("file.txt"), "initial\n").expect("failed to write initial file");
    traced_git_with_env(&repo, &["add", "file.txt"], &env_refs, &mut _throwaway)
        .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "initial"],
        &env_refs,
        &mut _throwaway,
    )
    .expect("initial commit should succeed");

    // Give the daemon enough time to ingest the trace events and attempt
    // (and panic in) side-effect processing.  Poll the control socket to
    // confirm the daemon is still responsive.
    let mut daemon_responded = false;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if send_control_request(
            &daemon.control_socket_path,
            &ControlRequest::StatusFamily {
                repo_working_dir: daemon.repo_working_dir.clone(),
            },
        )
        .is_ok()
        {
            daemon_responded = true;
            break;
        }
    }
    assert!(
        daemon_responded,
        "daemon control socket should respond after panic in side-effect pipeline"
    );

    // Verify the daemon process is still alive after the panic.
    assert!(
        process_exists(daemon_pid),
        "daemon process should still be alive after a panic in side-effect pipeline"
    );
    assert!(
        daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_none(),
        "daemon should not have exited after panic"
    );

    // Phase 2 — Remove the panic flag and verify the daemon processes a new
    // commit end-to-end (completion log entry recorded).
    fs::remove_file(&panic_flag_path).expect("failed to remove panic flag");

    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("file.txt"), "updated\n").expect("failed to write updated file");
    traced_git_with_env(
        &repo,
        &["add", "file.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "second commit"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second commit should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    // Verify the daemon is still alive after recovering and processing normal commands.
    assert!(
        process_exists(daemon_pid),
        "daemon should still be alive after recovering and processing normal commands"
    );

    // Clean shutdown.
    daemon.shutdown();
}

/// When the daemon's socket files are deleted from the filesystem while the
/// daemon process is still running, the daemon becomes a zombie: alive but
/// unreachable. New clients cannot connect because the filesystem entries are
/// gone, even though the kernel-level socket fds are still open.
///
/// The daemon should detect that its socket files have been unlinked and
/// initiate a graceful shutdown so that the next wrapper invocation can
/// spawn a fresh daemon via ensure_daemon_running.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_shuts_down_when_socket_files_are_deleted() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let trace_socket_path = daemon_trace_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );

    // Verify the daemon is alive and both sockets exist on disk.
    assert!(
        control_socket_path.exists(),
        "control socket should exist after daemon start"
    );
    assert!(
        trace_socket_path.exists(),
        "trace socket should exist after daemon start"
    );
    assert!(
        send_control_request(
            &control_socket_path,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        )
        .is_ok(),
        "daemon should respond to status requests"
    );

    // Verify daemon is actually still running before we delete sockets.
    assert!(
        daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_none(),
        "daemon process should still be running before socket deletion"
    );

    // Delete the socket files out from under the running daemon.
    fs::remove_file(&control_socket_path).expect("failed to delete control socket");
    fs::remove_file(&trace_socket_path).expect("failed to delete trace socket");
    assert!(
        !control_socket_path.exists(),
        "control socket should be deleted"
    );
    assert!(
        !trace_socket_path.exists(),
        "trace socket should be deleted"
    );

    // Wait for the daemon to notice and shut down. With a 1-second check
    // interval, it should detect the missing sockets within a few seconds.
    let mut daemon_exited = false;
    for _ in 0..100 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            daemon_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    assert!(
        daemon_exited,
        "daemon should shut down after its socket files are deleted, \
         but the process is still running after 10 seconds"
    );

    // DaemonGuard::drop calls shutdown(), which is a no-op if already exited.
    daemon.shutdown();
}

/// After detecting that its sockets have been deleted, the daemon should
/// spawn a detached `git-ai bg restart --hard` process that reaps the
/// zombie and starts a fresh daemon. Verify that a new, reachable daemon
/// is running after the original one dies.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_self_heals_after_socket_deletion() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let trace_socket_path = daemon_trace_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );

    // Verify the daemon is alive and responsive.
    assert!(
        send_control_request(
            &control_socket_path,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        )
        .is_ok(),
        "original daemon should respond to status requests"
    );

    // Delete both socket files.
    fs::remove_file(&control_socket_path).expect("failed to delete control socket");
    fs::remove_file(&trace_socket_path).expect("failed to delete trace socket");

    // Wait for the original daemon to exit.
    let mut original_exited = false;
    for _ in 0..100 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            original_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        original_exited,
        "original daemon should shut down after socket deletion"
    );

    // Wait for a new daemon to come up with fresh sockets.
    let mut new_daemon_reachable = false;
    for _ in 0..200 {
        if control_socket_path.exists()
            && send_control_request(
                &control_socket_path,
                &ControlRequest::StatusFamily {
                    repo_working_dir: repo_workdir_string(&repo),
                },
            )
            .is_ok()
        {
            new_daemon_reachable = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    assert!(
        new_daemon_reachable,
        "a new daemon should be reachable after the original self-healed"
    );

    // Clean up the new daemon.
    let _ = send_control_request(&control_socket_path, &ControlRequest::Shutdown);
    for _ in 0..100 {
        if !control_socket_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// A wedged trace accept loop leaves the listening socket connectable (the
/// backlog keeps queueing connects) while nothing drains trace2 input, so a
/// connect-only health probe cannot see it. The drain probe must detect the
/// wedge end-to-end and self-restart the daemon.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_self_restarts_when_trace_accept_loop_wedges() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let trace_socket_path = daemon_trace_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            // The readiness wait's trace connect is the first accepted
            // connection: the stall hook wedges the accept loop right there.
            ("GIT_AI_TEST_TRACE_ACCEPT_STALL_SECS", "60"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_TRACE_DRAIN_PROBE_DEADLINE_MS", "500"),
            // One restart only: the replacement inherits the wedge hook, and
            // without this cap it would churn through more generations past
            // the end of the test.
            ("GIT_AI_DAEMON_SELF_RESTART_BUDGET_MAX", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    // The budgeted restart spawns a replacement daemon DaemonGuard does not
    // own; make sure it is shut down even when an assertion fails.
    let _stray_daemon = StrayDaemonGuard::for_repo(&repo);

    // The listening socket still accepts connects while the accept loop is
    // wedged, so a connect-only probe would pass here.
    assert!(
        local_socket_connects_with_timeout(&trace_socket_path, DAEMON_TEST_PROBE_TIMEOUT).is_ok(),
        "trace socket should still be connectable while the accept loop is wedged"
    );

    let mut original_exited = false;
    for _ in 0..100 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            original_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        original_exited,
        "daemon should detect the wedged trace drain and shut down"
    );

    let mut new_daemon_reachable = false;
    for _ in 0..200 {
        if control_socket_path.exists()
            && send_control_request(
                &control_socket_path,
                &ControlRequest::StatusFamily {
                    repo_working_dir: repo_workdir_string(&repo),
                },
            )
            .is_ok()
        {
            new_daemon_reachable = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        new_daemon_reachable,
        "a new daemon should be reachable after the wedged one self-restarted"
    );

    let _ = send_control_request(&control_socket_path, &ControlRequest::Shutdown);
    for _ in 0..100 {
        if !control_socket_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// A git process blocked writing trace2 into an undrained socket must be
/// released as soon as the wedged daemon gives up: closing the socket fds
/// turns the blocked write into an error, which git treats as "disable the
/// trace2 target and continue".
#[test]
#[serial]
#[cfg(unix)]
fn daemon_drain_probe_wedge_releases_blocked_trace_writer() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let trace_socket_path = daemon_trace_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_ACCEPT_STALL_SECS", "60"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_TRACE_DRAIN_PROBE_DEADLINE_MS", "500"),
            // No restart: this test only verifies that giving up releases the
            // blocked writer.
            ("GIT_AI_DAEMON_SELF_RESTART_BUDGET_MAX", "0"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );

    // The accept loop is already wedged (the readiness wait's trace connect
    // was the first accepted connection). This writer's connection sits in
    // the listen backlog; once the kernel buffers fill, writes block, exactly
    // like git inside tr2_dst_write_line.
    let writer_socket = trace_socket_path.clone();
    let writer = thread::spawn(move || {
        let mut stream =
            open_local_socket_stream_with_timeout(&writer_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open writer trace connection");
        let line = format!(
            "{}\n",
            json!({
                "event": "data",
                "sid": "blocked-writer",
                "payload": "x".repeat(8192),
            })
        );
        loop {
            if stream
                .write_all(line.as_bytes())
                .and_then(|_| stream.flush())
                .is_err()
            {
                return;
            }
        }
    });

    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(20) {
        if writer.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        writer.is_finished(),
        "blocked trace writer should be released when the wedged daemon shuts down"
    );
    writer.join().unwrap();
    daemon.shutdown();
}

/// Reader-thread spawn failure past the threshold takes the same budgeted
/// restart path as every other failure-driven restart: under systemic
/// thread-spawn failure (pids.max, RLIMIT_NPROC) the daemon must not
/// crash-loop through unlimited generations.
#[test]
#[serial]
#[cfg(not(windows))]
fn daemon_spawn_failure_restart_consumes_budget() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let trace_socket_path = daemon_trace_socket_path(&repo);
    let restart_history_path = daemon_lock_path(&repo)
        .parent()
        .expect("daemon lock path should have a parent")
        .join("self_restart_history.json");

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_CONNECTION_SPAWN_FAILURES", "40"),
            ("GIT_AI_DAEMON_SELF_RESTART_BUDGET_MAX", "1"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "3600"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    // The budgeted restart spawns a replacement daemon DaemonGuard does not
    // own; make sure it is shut down even when an assertion fails.
    let _stray_daemon = StrayDaemonGuard::for_repo(&repo);

    // Readiness consumed one injected failure; enough further connections
    // reach the consecutive-failure threshold.
    for _ in 0..17 {
        let _ =
            open_local_socket_stream_with_timeout(&trace_socket_path, DAEMON_TEST_PROBE_TIMEOUT);
        thread::sleep(Duration::from_millis(10));
    }

    let mut original_exited = false;
    for _ in 0..100 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            original_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        original_exited,
        "daemon should shut down after persistent reader-spawn failures"
    );

    // Exactly one budgeted restart: a replacement daemon appears...
    let mut replacement_reachable = false;
    for _ in 0..200 {
        if control_socket_path.exists()
            && send_control_request(&control_socket_path, &ControlRequest::Ping).is_ok()
        {
            replacement_reachable = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(replacement_reachable, "one budgeted restart should occur");

    // ...and when driven over the threshold again, the exhausted budget must
    // stop the chain instead of spawning a third generation.
    for _ in 0..20 {
        let _ =
            open_local_socket_stream_with_timeout(&trace_socket_path, DAEMON_TEST_PROBE_TIMEOUT);
        thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..150 {
        if !control_socket_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !control_socket_path.exists(),
        "the replacement daemon should shut down once the restart budget is exhausted"
    );
    thread::sleep(Duration::from_secs(2));
    assert!(
        !control_socket_path.exists(),
        "no third daemon may start after the restart budget is exhausted"
    );

    let history: Vec<u64> = serde_json::from_str(
        &fs::read_to_string(&restart_history_path).expect("restart history should exist"),
    )
    .expect("restart history should be valid JSON");
    assert_eq!(
        history.len(),
        1,
        "exactly one spawn-failure self-restart should have consumed budget"
    );
}

/// A `bg shutdown` racing an in-flight drain probe must not consume restart
/// budget or resurrect the daemon the user just stopped.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_shutdown_mid_probe_does_not_restart() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let restart_history_path = daemon_lock_path(&repo)
        .parent()
        .expect("daemon lock path should have a parent")
        .join("self_restart_history.json");

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            // Wedge the accept loop so every drain probe hangs in its poll
            // window, then keep teardown (and thus the health thread) alive
            // long enough for the probe to resolve after shutdown.
            ("GIT_AI_TEST_TRACE_ACCEPT_STALL_SECS", "60"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_TRACE_DRAIN_PROBE_DEADLINE_MS", "5000"),
            ("GIT_AI_TEST_SHUTDOWN_HANG_SECS", "8"),
            ("GIT_AI_DAEMON_SHUTDOWN_DEADLINE_SECS", "10"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );

    // Let a health tick begin its (doomed) probe, then shut down mid-probe.
    thread::sleep(Duration::from_millis(2000));
    let _ = send_control_request(&control_socket_path, &ControlRequest::Shutdown);

    let started = std::time::Instant::now();
    let mut exited = false;
    while started.elapsed() < Duration::from_secs(15) {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(exited, "daemon should exit after the requested shutdown");

    thread::sleep(Duration::from_secs(3));
    assert!(
        !control_socket_path.exists(),
        "a shutdown racing a drain probe must not resurrect the daemon"
    );
    assert!(
        !restart_history_path.exists(),
        "a shutdown racing a drain probe must not consume restart budget"
    );
}

/// The drain probe only proves the socket legs; a wedged ingest pipeline
/// (payloads queued, processed watermark frozen) must also trigger the same
/// budgeted self-restart, or attribution silently stops while health stays
/// green.
#[test]
#[serial]
#[cfg(not(windows))]
fn daemon_processing_stall_triggers_budgeted_restart() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let trace_socket_path = daemon_trace_socket_path(&repo);
    let restart_history_path = daemon_lock_path(&repo)
        .parent()
        .expect("daemon lock path should have a parent")
        .join("self_restart_history.json");

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_INGEST_WORKER_START_DELAY_MS", "120000"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_SELF_RESTART_BUDGET_MAX", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    // The budgeted restart spawns a replacement daemon DaemonGuard does not
    // own; make sure it is shut down even when an assertion fails.
    let _stray_daemon = StrayDaemonGuard::for_repo(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    // Queue mutating payloads that the (wedged) ingest worker never processes.
    send_trace_frames(
        &trace_socket_path,
        &[
            json!({
                "event": "start",
                "sid": "processing-stall-root",
                "argv": ["git", "commit", "-m", "stalled pipeline"],
                "time_ns": 70_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "processing-stall-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 70_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "processing-stall-root",
                "code": 0,
                "time_ns": 70_100u64,
            }),
            trace_atexit_frame("processing-stall-root", 0, 70_101u64),
        ],
    );

    // The stall window is 12 health intervals (12s at the 1s test cadence);
    // the wide 90s deadline absorbs heavily loaded CI runners.
    let mut original_exited = false;
    for _ in 0..900 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            original_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        original_exited,
        "daemon should self-restart when the ingest pipeline stops making progress"
    );

    let mut replacement_reachable = false;
    for _ in 0..200 {
        if control_socket_path.exists()
            && send_control_request(&control_socket_path, &ControlRequest::Ping).is_ok()
        {
            replacement_reachable = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        replacement_reachable,
        "a replacement daemon should serve after the processing stall"
    );
    let history: Vec<u64> = serde_json::from_str(
        &fs::read_to_string(&restart_history_path).expect("restart history should exist"),
    )
    .expect("restart history should be valid JSON");
    assert_eq!(history.len(), 1, "the restart must consume budget");

    let _ = send_control_request(&control_socket_path, &ControlRequest::Shutdown);
    for _ in 0..100 {
        if !control_socket_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Shutdown must sever accepted trace connections immediately (releasing any
/// blocked writer), not merely rely on the socket fds closing at process
/// exit.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_shutdown_severs_trace_connections_before_process_exit() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let trace_socket_path = daemon_trace_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            // Readers register, then sleep: writes pile into kernel buffers
            // until the writer blocks, like git inside tr2_dst_write_line.
            ("GIT_AI_TEST_TRACE_READER_START_DELAY_MS", "30000"),
            // Keep the process alive well past the shutdown request so the
            // writer's release can only come from the registry sever.
            ("GIT_AI_TEST_SHUTDOWN_HANG_SECS", "8"),
            ("GIT_AI_DAEMON_SHUTDOWN_DEADLINE_SECS", "10"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "3600"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );

    let writer_socket = trace_socket_path.clone();
    let writer = thread::spawn(move || {
        let mut stream =
            open_local_socket_stream_with_timeout(&writer_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open writer trace connection");
        let line = format!(
            "{}\n",
            json!({
                "event": "data",
                "sid": "sever-blocked-writer",
                "payload": "x".repeat(8192),
            })
        );
        loop {
            if stream
                .write_all(line.as_bytes())
                .and_then(|_| stream.flush())
                .is_err()
            {
                return;
            }
        }
    });
    thread::sleep(Duration::from_millis(500));

    let _ = send_control_request(&control_socket_path, &ControlRequest::Shutdown);

    // Linux wakes a blocked peer writer as soon as the daemon's end is shut
    // down, so release must precede process exit. On macOS a blocked writer
    // is only released once the reader's fd closes — the sever wakes a reader
    // parked in read, but this test's reader is deliberately asleep, so the
    // release is bounded by the shutdown deadline enforcer instead.
    #[cfg(target_os = "linux")]
    {
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            if writer.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            writer.is_finished(),
            "the blocked writer must be released by the registry sever, not by process exit"
        );
        assert!(
            daemon
                .child
                .try_wait()
                .expect("failed to poll daemon")
                .is_none(),
            "the daemon process should still be alive when the writer is released"
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            if writer.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            writer.is_finished(),
            "the blocked writer must be released within the shutdown deadline"
        );
    }
    writer.join().unwrap();
    daemon.shutdown();
}

/// Outstanding checkpoints defer a failing health check — but only up to the
/// cap, after which the daemon restarts anyway. Drives the health loop's
/// deferral increment and cap wiring end to end: a checkpoint body dribbled
/// over the control socket holds a quota reservation while the wedged accept
/// loop keeps every drain probe failing.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_health_restart_deferred_for_checkpoints_until_cap() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_ACCEPT_STALL_SECS", "60"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_TRACE_DRAIN_PROBE_DEADLINE_MS", "500"),
            ("GIT_AI_DAEMON_SELF_RESTART_BUDGET_MAX", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let _stray_daemon = StrayDaemonGuard::for_repo(&repo);

    // Hold a checkpoint ingress reservation: announce a large body, read the
    // ready ack, then dribble bytes so the body read never times out.
    let dribble_socket = control_socket_path.clone();
    let dribbler = thread::spawn(move || {
        let mut stream =
            open_local_socket_stream_with_timeout(&dribble_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open checkpoint control connection");
        stream
            .write_all(b"{\"method\":\"checkpoint.run\",\"params\":{\"body_bytes\":100000}}\n")
            .expect("failed to write checkpoint header");
        stream.flush().expect("failed to flush checkpoint header");
        // Read the ready ack line.
        use std::io::Read as _;
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => return,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => {}
            }
        }
        // Dribble the body: one byte per 200ms keeps the reservation alive.
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            if stream.write_all(b"x").and_then(|_| stream.flush()).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
    });

    // The health loop must defer while the reservation is outstanding, hit
    // the cap, and then restart anyway.
    let started = std::time::Instant::now();
    loop {
        let logs = daemon.stderr_contents();
        if logs.contains("consecutive_deferrals=4") {
            assert!(
                logs.contains("restart_deferred_for_checkpoints"),
                "deferral log should carry its reason:\n{logs}"
            );
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "health loop never reached the deferral cap:\n{logs}"
        );
        thread::sleep(Duration::from_millis(50));
    }

    let mut original_exited = false;
    for _ in 0..300 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            original_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        original_exited,
        "the daemon must restart once the deferral cap is exhausted"
    );
    dribbler.join().unwrap();
}

/// A persistent wedge (e.g. systemic filesystem breakage) must not produce an
/// endless restart loop: the sliding-window budget lets the daemon self-heal
/// a bounded number of times and then stay down.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_restart_budget_prevents_crash_loop() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let restart_history_path = daemon_lock_path(&repo)
        .parent()
        .expect("daemon lock path should have a parent")
        .join("self_restart_history.json");

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_ACCEPT_STALL_SECS", "60"),
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_TRACE_DRAIN_PROBE_DEADLINE_MS", "500"),
            ("GIT_AI_DAEMON_SELF_RESTART_BUDGET_MAX", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );

    // First daemon wedges (readiness trace connect) and restarts once.
    let mut original_exited = false;
    for _ in 0..100 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            original_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(original_exited, "wedged daemon should shut down");

    // The replacement daemon wedges on its own first health probe and, with
    // the budget exhausted, must shut down without spawning a third daemon.
    let started = std::time::Instant::now();
    let mut saw_replacement = false;
    while started.elapsed() < Duration::from_secs(30) {
        if control_socket_path.exists() {
            saw_replacement = true;
        } else if saw_replacement {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        saw_replacement,
        "one replacement daemon should have started"
    );
    for _ in 0..100 {
        if !control_socket_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !control_socket_path.exists(),
        "the replacement daemon should shut down once the restart budget is exhausted"
    );
    thread::sleep(Duration::from_secs(2));
    assert!(
        !control_socket_path.exists(),
        "no third daemon may start after the restart budget is exhausted"
    );

    let history: Vec<u64> = serde_json::from_str(
        &fs::read_to_string(&restart_history_path).expect("restart history should exist"),
    )
    .expect("restart history should be valid JSON");
    assert_eq!(
        history.len(),
        1,
        "exactly one self-restart should have been recorded"
    );
}

/// Once shutdown is requested, the process must exit within the deadline even
/// if graceful teardown wedges: process exit is what closes the socket fds
/// and releases any git still blocked on trace2 writes.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_shutdown_deadline_forces_exit() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_SHUTDOWN_HANG_SECS", "30"),
            ("GIT_AI_DAEMON_SHUTDOWN_DEADLINE_SECS", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );

    let _ = send_control_request(&control_socket_path, &ControlRequest::Shutdown);

    let started = std::time::Instant::now();
    let mut exit_status = None;
    while started.elapsed() < Duration::from_secs(5) {
        if let Some(status) = daemon.child.try_wait().expect("failed to poll daemon") {
            exit_status = Some(status);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let exit_status = exit_status
        .expect("daemon should be force-exited by the shutdown deadline enforcer within 5s");
    assert_eq!(
        exit_status.code(),
        Some(70),
        "forced exit should use the enforcer's exit code"
    );
}

/// Drain probes must be invisible to ingest ordering and attribution.
#[test]
#[cfg(not(windows))]
fn daemon_drain_probes_do_not_disturb_attribution() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket_path = daemon_trace_socket_path(&repo);

    let stop_probes = Arc::new(AtomicBool::new(false));
    let probe_stop = Arc::clone(&stop_probes);
    let probe_socket = trace_socket_path.clone();
    let prober = thread::spawn(move || {
        let mut probe_id = 0u64;
        while !probe_stop.load(Ordering::SeqCst) {
            probe_id += 1;
            if let Ok(mut stream) =
                open_local_socket_stream_with_timeout(&probe_socket, DAEMON_TEST_PROBE_TIMEOUT)
            {
                let line = format!(
                    "{}\n",
                    json!({ "event": "git_ai_drain_probe", "git_ai_probe_id": probe_id })
                );
                let _ = stream.write_all(line.as_bytes());
                let _ = stream.flush();
            }
            thread::sleep(Duration::from_millis(50));
        }
    });

    let mut file = repo.filename("probe-noise.txt");
    file.set_contents(lines!["human line", "ai line"]);
    file.repo
        .git_ai(&native_checkpoint_args!(
            file.file_path.to_string_lossy().as_ref()
        ))
        .unwrap();
    repo.stage_all_and_commit("commit under probe noise")
        .unwrap();
    file.assert_committed_lines(lines!["human line", "ai line"]);

    stop_probes.store(true, Ordering::SeqCst);
    prober.join().unwrap();
}

/// A control client that connects and vanishes (times out, dies mid-request)
/// is routine under load — it must not produce ERROR-level noise or affect
/// the daemon's health.
#[test]
#[serial]
#[cfg(unix)]
fn control_peer_disconnect_not_logged_as_error() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(&repo, &[]);
    let control_socket_path = daemon_control_socket_path(&repo);

    // Deterministic peer-gone: announce a checkpoint body and vanish before
    // sending it. The daemon's body read hits EOF mid-request, which must be
    // classified as a routine peer disconnect, not a daemon error.
    {
        let mut stream =
            open_local_socket_stream_with_timeout(&control_socket_path, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open control connection");
        stream
            .write_all(b"{\"method\":\"checkpoint.run\",\"params\":{\"body_bytes\":1000}}\n")
            .expect("failed to write checkpoint header");
        stream.flush().expect("failed to flush checkpoint header");
        // Wait for the ready ack so the daemon is definitely inside the body
        // read when the connection drops.
        let mut ready = [0u8; 1];
        use std::io::Read as _;
        let _ = stream.read(&mut ready);
    }

    // Also hammer it with valid pings that never read their responses.
    for _ in 0..20 {
        if let Ok(mut stream) =
            open_local_socket_stream_with_timeout(&control_socket_path, DAEMON_TEST_PROBE_TIMEOUT)
        {
            let _ = stream.write_all(b"{\"method\":\"ping\"}\n");
            let _ = stream.flush();
        }
    }

    let started = std::time::Instant::now();
    loop {
        if daemon
            .stderr_contents()
            .contains("daemon control connection dropped by peer")
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "peer-gone classification never ran:\n{}",
            daemon.stderr_contents()
        );
        thread::sleep(Duration::from_millis(25));
    }

    let response = send_control_request(&control_socket_path, &ControlRequest::Ping)
        .expect("daemon should still serve control requests after abandoned peers");
    assert!(
        response.ok,
        "ping after abandoned peers failed: {response:?}"
    );

    let logs = daemon.stderr_contents();
    assert!(
        !logs.contains("daemon control connection failed"),
        "abandoned control peers must not be logged as connection failures:\n{logs}"
    );
    daemon.shutdown();
}

/// Dropped trace connections are counted and reported through stats.ingest,
/// so attribution loss is observable instead of silent.
#[test]
#[serial]
#[cfg(not(windows))]
fn trace_connection_drops_reported_via_stats_ingest() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    // Readiness consumes one injected failure; the two test connections
    // below consume the rest.
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[("GIT_AI_TEST_TRACE_CONNECTION_SPAWN_FAILURES", "3")],
    );
    let trace_socket_path = daemon_trace_socket_path(&repo);
    let control_socket_path = daemon_control_socket_path(&repo);

    for _ in 0..2 {
        let mut stream =
            open_local_socket_stream_with_timeout(&trace_socket_path, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open trace connection");
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if stream
                .write_all(b"\n")
                .and_then(|_| stream.flush())
                .is_err()
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    let response = send_control_request(&control_socket_path, &ControlRequest::StatsIngest)
        .expect("stats.ingest request failed");
    assert!(response.ok, "stats.ingest returned error: {response:?}");
    let data = response.data.expect("stats.ingest should return data");
    let connections_dropped = data
        .get("trace_connections_dropped")
        .and_then(Value::as_u64)
        .expect("stats.ingest data should include trace_connections_dropped");
    assert!(
        connections_dropped >= 2,
        "expected at least 2 dropped trace connections, got {connections_dropped}: {data}"
    );
    assert_eq!(
        data.get("trace_payloads_dropped_queue_full")
            .and_then(Value::as_u64),
        Some(0),
        "no queue-full drops expected in this scenario: {data}"
    );
    daemon.shutdown();
}

/// A queue-full drop must name the root whose attribution was lost.
#[test]
#[serial]
#[cfg(not(windows))]
fn trace_queue_full_drop_logs_the_dropped_root() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_INGEST_QUEUE_CAPACITY", "1"),
            ("GIT_AI_TEST_TRACE_INGEST_WORKER_START_DELAY_MS", "5000"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to connect trace socket");
    write_trace_frames(
        &mut stream,
        &[
            json!({
                "event": "start",
                "sid": "queue-full-loss-root",
                "argv": ["git", "commit", "-m", "synthetic"],
                "time_ns": 40_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "queue-full-loss-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 40_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "queue-full-loss-root",
                "code": 0,
                "time_ns": 40_100u64,
            }),
            trace_atexit_frame("queue-full-loss-root", 0, 40_101u64),
        ],
    );

    let started = std::time::Instant::now();
    loop {
        let logs = daemon.stderr_contents();
        if logs.contains("ingest_worker_queue_full") {
            assert!(
                logs.contains("queue-full-loss-root"),
                "queue-full log should name the dropped root:\n{logs}"
            );
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "daemon never reported the queue-full drop:\n{logs}"
        );
        thread::sleep(Duration::from_millis(25));
    }
    daemon.shutdown();
}

#[test]
fn daemon_debug_logging_does_not_reupload_ureq_logs() {
    let mut mock_api = MockApiServer::start();
    let repo = TestRepo::new_with_daemon_env(&[
        ("RUST_LOG", "debug"),
        ("GIT_AI_API_BASE_URL", mock_api.base_url()),
        ("GIT_AI_API_KEY", "test-api-key"),
    ]);

    repo.git_ai(&["await", "--timeout", "10"])
        .expect("initial daemon log flush should succeed");

    let first_upload_deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut requests = Vec::new();
    while std::time::Instant::now() < first_upload_deadline {
        requests.extend(mock_api.collect_requests());
        if requests
            .iter()
            .any(|request| request["path"] == "/worker/logs/upload")
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    thread::sleep(Duration::from_millis(250));
    repo.git_ai(&["await", "--timeout", "10"])
        .expect("follow-up daemon log flush should succeed");
    thread::sleep(Duration::from_millis(250));
    requests.extend(mock_api.collect_requests());

    let uploaded_targets = requests
        .iter()
        .filter(|request| request["path"] == "/worker/logs/upload")
        .flat_map(|request| request["body"]["events"].as_array().into_iter().flatten())
        .filter_map(|event| {
            event["fields"]["log.target"]
                .as_str()
                .or_else(|| event["target"].as_str())
        })
        .collect::<Vec<_>>();

    assert!(
        !uploaded_targets.is_empty(),
        "expected the daemon to upload its startup logs"
    );
    assert!(
        uploaded_targets
            .iter()
            .all(|target| *target != "ureq" && !target.starts_with("ureq::")),
        "ureq logs generated by daemon log delivery must not be uploaded: {uploaded_targets:?}"
    );
}

#[test]
fn await_is_marked_beta_and_returns_promptly_when_idle() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    let top_level_help = repo
        .git_ai(&["--help"])
        .expect("top-level help should succeed");
    assert!(
        top_level_help.contains("await [beta]"),
        "top-level help should mark await as beta: {}",
        top_level_help
    );

    let await_help = repo
        .git_ai(&["await", "--help"])
        .expect("await help should succeed");
    assert!(
        await_help.contains("beta"),
        "await help should mark the command as beta: {}",
        await_help
    );

    let started_at = std::time::Instant::now();
    repo.git_ai(&["await", "--timeout", "10"])
        .expect("await should succeed when the daemon is idle");
    assert!(
        started_at.elapsed() < Duration::from_secs(4),
        "await should return promptly instead of waiting for the progress interval"
    );
}

#[test]
fn await_rejects_zero_timeout() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    let error = repo
        .git_ai(&["await", "--timeout", "0"])
        .expect_err("zero timeout should be rejected");

    assert!(
        error.contains("--timeout must be a positive integer"),
        "await should report an input validation error: {error}"
    );
}
