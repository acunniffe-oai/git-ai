# Git AI v2

Git AI links coding-agent sessions to Git commits and exports those links over OpenTelemetry.

Native agent hooks report whether a session wrote to a repository. A shared daemon observes real Git through trace2, stores local session/commit links in SQLite, and follows those links through rebases, cherry-picks, amendments, squash, stash, and reset operations.

## Install and configure

Use the project’s platform installer, then configure your backend endpoint and API key with `git-ai install --api-base <url> --api-key <key>`. Installation configures native agent hooks, Git trace2, and supported native agent OTEL settings.

```sh
git-ai config          # inspect configuration
git-ai daemon status   # inspect the background service
git-ai debug           # check transport and session linking
git-ai await           # wait for queued links to be delivered
```

Agent integrations invoke `git-ai checkpoint <agent> --hook-input stdin`. Keep the original native session ID in hook payloads. No transcript parsing is needed.

## What a link means

One confirmed write attaches a session to the current worktree’s next successful commit. Multiple sessions can contribute to one commit. After the commit, another write is needed to attach that session again. Repeated hooks within a window skip expensive processing.

This is a repository-level signal, including when changes were partially staged or later reverted. Git AI does not calculate line attribution or human contributions. It does not provide blame/usage dashboards, Git Notes, or IDE extensions. Native agent adapters remain supported; extension-only legacy coverage and tab completions are retired.

Local links survive restart and supply rewrite propagation. No v1 historical import or cross-machine note synchronization is performed. Existing queued OTEL records survive the v2 database migration.

See [OTEL commit links](docs/otel-commit-links.md) for the payload, delivery, and storage contract, and [agent OTEL setup](docs/agent-otel-setup.md) for native telemetry configuration.

## Development

```sh
task build                        # compile
task test                         # run the suite
task test TEST_FILTER=v2_          # session-link integration tests
task lint
task fmt
task dev                          # install the development build and restart the daemon
```

Tests use real repositories and isolated trace2 daemons. Read [AGENTS.md](AGENTS.md) before contributing: no additional work on critical ingestion, no Git spawns per commit/file/object/ref, and strict TestRepo-based TDD.

## License
Apache 2.0
