# OTLP commit links

Configuring the backend `api_key` automatically enables the independent OTLP
outbox. Git AI sends only `git_ai.session.linked_commit`, one record per supported
agent session and commit SHA. Rebase, amend, cherry-pick, squash, and other
authorship-note rewrites produce new links even when the stable patch ID matches
an earlier commit.

The destination is the origin of `api_base_url` plus
`/api/otel/git-ai/v1/logs`. Any base URL path prefix is replaced. Requests use
OTLP/JSON and `Authorization: Bearer <api_key>`; OAuth login alone does not enable
this outbox. The notes backend URL does not select the OTLP destination.

Both HTTP and Git notes trigger links. Claude (`claude` internally) is sent as
`claude-code`; Codex, Devin, `github-copilot` (VS Code), and `github-copilot-cli` retain their tool names. Session IDs are the
agents' original IDs. Unsupported agents, known placeholder IDs, empty patches,
and records missing required commit metadata are skipped. Optional branch and
user-email attributes are omitted. Prompts, diffs, usage data, and custom
attributes are not sent.

The daemon captures successful note writes and the trace command's completion
time, then enriches the whole operation in a fixed number of Git calls. Neither
enrichment nor delivery runs on trace ingestion. Source batches and prepared
records live in `~/.git-ai/internal/otel-db`; delivery is independent of notes and
metrics uploads. A separate bounded pool keeps durable source writes moving even
when both uploaders stall. There is no historical backfill.

Successful records are retained for deduplication. Network errors, HTTP 429, and
5xx responses retry with capped exponential backoff. HTTP 413 splits batches;
an oversized individual record is parked. HTTP 401 waits for a changed API key.
Other unsuccessful responses are retained as failures without automatic retry.
Queued records stay bound to their original endpoint if configuration changes.

`git-ai await` flushes this queue too and reports remaining and failed OTLP work.
It does not report completion while the active destination has undelivered
records, including records waiting for retry or credential repair. The daemon
log records preparation, persistence, and delivery errors. Transport retries
reuse the original payload and timestamp; delivery is at least once.
