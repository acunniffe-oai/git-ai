# Agent telemetry during hook installation

`git-ai install-hooks` (also `git-ai install`) configures OpenTelemetry for detected
Codex, Claude Code, and Copilot installations when Git AI has a nonempty `api_key`.
OAuth login alone does not enable this discovery.

The installer makes one `GET /api/client-config` request on the origin configured
by `api_base_url`, authenticated with `Authorization: Bearer <api_key>`. It applies
the server’s version-1 `agents.codex.otel`, `agents.claude.env`, and `agents.copilot` settings when the
respective entry has `enabled: true`. Prompt and tool-content capture follow the
server-provided settings exactly.

An API token and API base URL can be supplied as optional positional arguments
in either order. URLs must start with `http://` or `https://`. The existing
`--api-key`, `--api-base`, `API_KEY`, and `API_BASE` forms also work; named flags
take precedence over positional arguments. Installer values are saved and used
during that same installation. `GIT_AI_API_KEY` takes precedence for discovery
over the saved key, including a key just supplied by the installer.

```sh
git-ai install-hooks --dry-run --verbose
git-ai install-hooks https://api.example.com my-api-token
```

Dry runs fetch and preview the configuration without saving agent or Git AI
settings. Previews redact authentication headers. Rerunning the installer
refreshes OTEL even when hooks are already current, including token rotations
and changes to content capture policy.

Server-provided OTEL settings replace conflicting settings while preserving
unrelated configuration and hooks. Claude per-signal endpoint, protocol, and
header overrides and `otelHeadersHelper` are cleared so they cannot override the
server configuration. Custom `CLAUDE_CONFIG_DIR` and `CODEX_HOME` directories and
configuration-file symlinks are respected. Credential-bearing agent files are
written atomically with owner-only permissions on Unix.

Missing tokens, missing/disabled agent entries, and undetected agents leave
existing OTEL settings alone. Discovery or per-agent configuration failures warn
and allow hook installation to continue. Discovery has a five-second timeout,
no retries, and no redirects. This does not change uninstall behavior.


## Copilot CLI and VS Code

`agents.copilot.vscode.settings` supplies native dotted
`github.copilot.chat.otel.*` settings for detected Code and Code Insiders user
settings files. JSONC comments and unrelated settings are preserved. The panel
uses protobuf OTLP/HTTP with an authorization header.

`agents.copilot.cli.env` supplies the CLI's native environment configuration.
For a detected CLI installation, Git AI stores private shell files under
`~/.git-ai/copilot/` and adds a managed block to Bash/Zsh, Fish, or PowerShell
profiles. Continue using `copilot` normally in a new terminal. The environment
applies during that invocation and does not leak to unrelated parent-shell
commands. Existing Copilot functions and aliases are preserved; conflicts in a
profile produce an installer warning. Explicit binary paths, shells that skip
profiles, background/SDK agents, and cloud agents bypass this setup.

Reload VS Code after setup. Install on the host running Copilot; workspace
settings, nondefault editor profiles, and externally configured OTEL environment
variables can take precedence over the user settings. Use versions with native
OTEL endpoint, protocol, and header support. `COPILOT_HOME` is respected when
installing CLI checkpoint hooks. Neither discovery nor setup reads Copilot's
internal authentication data.

Commit links preserve the raw hook session ID and distinguish `github-copilot`
(panel) from `github-copilot-cli` (CLI). Only visible live telemetry is available;
there is no Copilot compliance backfill. Raw IDs from the VS Code window are not
chat IDs. Server content capture rules still apply before durable writes.
