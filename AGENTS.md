## Non-Negotiable Rules

These are hard constraints. Violating any of them will get a PR rejected outright.

1. **All Git integration/processing is trace2-driven -- we do NOT wrap git.** All git command processing is based on trace2, so EVERYTHING we do must be fully async. As a result, we cannot rely on repo/git state at processing time being reflective of the state whenever the given operation actually occurred. We have a highly latency-sensitive trace2 ingestion flow that gathers/estimates the minimal possible state and orders events for later async processing.

2. **No new work on the critical ingestion path.** You CANNOT add git spawns, git object lookups, ref checks, etc. in the critical ingestion path of the daemon. It is EXTREMELY latency-sensitive -- even additional file reads have meaningful overhead on this path, which is sensitive to sub-millisecond latency increases.

3. **No non-constant-time git work anywhere.** You CANNOT add git spawns, git object lookups, ref checks, etc. that are not constant-time (e.g., must be O(2)/O(3), not O(n)). You cannot have any operation that calls git per commit, per file, per object, or per ref -- that is not constant time. You can still call git outside of latency-sensitive paths, but instead of calling it once per item, find a constant-time alternative: a plumbing command, a command that accepts batched inputs, etc., so you avoid the massive overhead of N process spawns. Rule of thumb: if you're calling git or looking up objects/refs "for each `<thing>`," you're doing it wrong.

4. **Unbounded git spawns are an automatic rejection.** Changes/PRs with unbounded git spawns will NOT be accepted.

5. **Reuse existing code.** This is a large codebase. For nearly any operation you need, there is almost certainly a helper function already available and tested. Reuse code as much as practical -- it also keeps diffs small. PRs that fail to reuse existing code where applicable will be rejected immediately.

6. **Follow STRICT TDD.** PRs that are not clearly TDD-driven with high-quality `TestRepo`-based tests will be rejected immediately.

## Build & Test Commands

```bash
# Install a git-ai debug build for local dev on the system so that all git commands will route through it.
# Installs to the same location as real release builds, so it overrides system-wide. It also runs `git-ai install`
# and restarts the daemon to ensure all latest code changes are fully installed and propagated system-wide.
# Use this for trying out changes locally -- do not use any other approaches for runing git-ai locally. They will
# not work, interfere, and break things.
task dev

# Build (only use this for checking that your changes compile)
task build

# Test (use these commands to run the test suite -- these calls are optimized for your system; all flags/args can be combined)
task test # Run the full test suite
task test TEST_FILTER=foo # run specific test
task test NO_CAPTURE=true # Run with Cargo's --no-capture flag
task test EXTRA_TEST_BINARY_ARGS="--ignored" # ignored / exact / other flags
task test CARGO_TEST_ARGS="--lib" # cargo-level flags (rare)

# Lint & Format
task lint
task fmt

# Snapshot management (insta crate)
cargo insta review                       # interactively review snapshot changes
cargo insta accept                       # accept all pending snapshots
```

## PR Workflow

Before opening a PR, make sure to run `task lint` and `task fmt` and resolve any formatting/lint issues as they will fail in CI.

When opening a PR, make sure to monitor the ubuntu-based CI jobs first. They are the fastest (roughly 15mins) and if they fail, you should quickly iterate based on those failures and update the PR -- iterating there until those jobs are all green. Additionally, while you're checking on the ubuntu-based jobs, our automated PR review bot, Devin, should have had time to leave feedback. Make sure to read all of Devin's PR review feedback commits and address them. Address them means review, understand, evaluate, and fix if necessary or comment with your thoughts if you don't the feedback is a real issue. Once the lint, fmt, and Ubuntu-based tests have passed and you have addressed all Devin PR review feedback, you can stop monitoring CI for the Mac (~35mins) and Windows (up to 3.5 hours) checks unless the user has explicitly asked for you to wait for those or you're working on a specific OS-based bug.

## Architecture

Git AI v2 links native agent sessions to local commits and exports `git_ai.session.linked_commit` through OTEL. There is no line attribution, Git Note storage, human-edit detection, IDE extension, local transcript processing, or token/usage pipeline.

### Checkpoints and local links

Native hooks parse the original agent/session ID and confirm a write. Post-file hooks carry repository-level evidence without file contents. Shell hooks compare bounded filesystem metadata until the session is attached; subsequent calls in that commit window skip the scan. Shell invocation correlation allows post-hooks to link commits made inside a writing tool call.

`src/commit_links/store.rs` persists pending session sets per repository family/worktree/window and committed session links in SQLite alongside the existing OTEL outbox. A commit consumes pending sessions transactionally with its links and delivery work. Replays are idempotent. Partial staging or reverting an edit does not retract the repository-level signal.

### Trace2 processing

Keep trace ingestion minimal. `TraceNormalizer`, family actors, and `RefCursor` establish ordered immutable transitions. Only downstream workers apply commit/rewrite effects. `src/commit_links/rewrite.rs` copies or unions session sets across captured commit sequences; stash and reset operate on sets, never file contents. Do not substitute processing-time HEAD for historical operation state.

OTEL enrichment uses a fixed number of batched Git calls. Delivery retains existing endpoint/authentication, payload, retry, deduplication, and `await` behavior. No remote notes or historical attribution import is involved.

### Commands and configuration

Retain checkpoint, daemon, install/uninstall, configuration, upgrade, diagnostics, authentication/bootstrap, and await commands. Legacy `git` invocation compatibility is a transparent passthrough with signal forwarding; it performs no pre/post attribution work. Configuration precedence remains environment > config file > defaults.

## Test Infrastructure

Use `TestRepo` to run real Git with trace2 connected to isolated test daemons. Follow strict TDD: write and run a failing behavioral test before implementing each change. Explicitly write files and invoke native hook payloads; do not synthesize hidden human checkpoints.

After every commit, assert the exact local session-link set with `repo.assert_commit_links(&sha, &[(agent, session_id)])`, including the empty set. Assert committed file content where relevant. `TestFile` is a content/staging helper only. OTEL tests must also check the actual collector payload and retries.

Preserve transport, causal ordering, restart, worktree isolation, subdirectory/-C, and cross-platform coverage. Instrument total Git spawns for whole operations so counts stay fixed when commit/file counts increase. Warm checkpoint tests must show no content reads or scans.

### Test isolation

- Each `TestRepo` gets a random temp directory and a separate `GIT_AI_TEST_DB_PATH`.
- `GIT_AI_TEST_CONFIG_PATCH` env var passes `ConfigPatch` JSON to override config in subprocess.
- Background flush is skipped when `GIT_AI_TEST_DB_PATH` is set (prevents race conditions on temp dir cleanup).
- Use `#[serial_test::serial]` for tests that conflict on shared env vars. Do your best to avoid needing this though by using the config patch, etc.

### Snapshot tests

Uses `insta` crate. Snapshots live in `tests/integration/snapshots/` and `tests/integration/repos/snapshots/`. Run `cargo insta review` to update.

## Key Conventions

- **Rust 2024 edition** with Rust 1.93.0 -- uses let-chains (`if let Some(x) = foo && condition`), which are stable in edition 2024.
- **Git CLI only**: All git operations use `std::process::Command` to call the real git binary. The `git2`/libgit2 dependency has been fully removed. The binary acts as a transparent git proxy.
- **`debug_log()`** for conditional debug output: prints `[git-ai]` prefixed messages to stderr when `cfg!(debug_assertions)` or `GIT_AI_DEBUG=1`. Set `GIT_AI_DEBUG=0` to suppress in debug builds.
- **`GIT_AI_DEBUG_PERFORMANCE=1`** (or `=2` for JSON) enables performance timing output.
- **Paths are POSIX-normalized**: `normalize_to_posix()` utility converts Windows backslashes. File paths in authorship logs and working logs always use forward slashes.
- **`GIT_AI_VERSION` constant** changes between debug/release/test modes via `cfg` attributes in `version.rs`.
- **Cross-platform**: `#[cfg(unix)]` / `#[cfg(windows)]` conditional compilation is used extensively (well over a hundred `#[cfg(windows)]` annotations across ~two dozen files) for signal handling, process creation flags (`CREATE_NO_WINDOW`), path handling, terminal detection, and named-pipe vs unix-socket transport (e.g. the daemon control/trace sockets are named pipes on Windows, so `Path::exists()` checks are gated to non-Windows).

## Optimize for Human Review

- Always write code optimized for human review. No code can be merged without a greenlight from a human, so make it easy for humans to review your code. This means clear naming, clear refactors as needed, and, most importantly, minimal and simple code. Clean, DRY, simple, maintainable code is your true north star.
- Always submit work with GitHub Stacks (if the `gh stack` CLI is available) as a stack of pull requests, with each pull request representing a logical, self-contained chunk of the problem. This is how you present your work for human review.
- Before stopping, ensure every submitted pull request passes all CI checks and all Devin review feedback has been addressed and resolved.

## Gotchas

- **Test binary auto-compilation**: Integration tests trigger `cargo build --bin git-ai` on first test run via `OnceLock`. If you change code and run tests, the test harness recompiles. This can cause confusion if you're debugging -- the test binary is always a debug build at `target/debug/git-ai`.

- **argv[0] dispatch is load-bearing**: The binary's behavior is entirely determined by how it's invoked. In production, symlinking as `git` makes it a proxy. The `GIT_AI=git` env var forces proxy mode (debug builds only). Breaking this dispatch breaks everything.

- **Feature flag debug/release divergence**: Some flags have different debug/release defaults (see `define_feature_flags!` macro). Tests run debug builds, so a test passing in debug may behave differently in release if it depends on a flag that diverges.

- **Repository identity**: Canonicalize repository-family and worktree keys consistently; symlink spellings must not create separate pending sets. Commit-window changes are sequenced with trace2 operations.

- **Large source files**: Several core files exceed 5-10k lines. Navigate with grep, not scrolling.
