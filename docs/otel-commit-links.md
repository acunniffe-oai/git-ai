# OTLP commit links

Git AI v2 records local native-agent session-to-commit links in SQLite and sends `git_ai.session.linked_commit`. Git Notes and local transcript processing are not used.

## Contribution model

A confirmed agent write adds its original session ID to the current worktree’s pending set. The next successful commit consumes that set and records links. Repeated write hooks in the same window skip further work. The signal is repository-level: partial staging, excluded files, and later reverted edits do not remove it. Separate linked worktrees have separate pending sets.

Shell hooks use bounded pre/post filesystem metadata, without content reads. Once attached, they skip the scan. A writing shell call links its session to the commits created within that call, including when the post-hook arrives after those commits. It does not also attach the next unrelated commit. Missing write evidence fails closed.

Rewrites copy locally known links to new SHAs, and squash or ambiguous rewrites union affected sessions. Old SHA links remain. Stash and reset preserve pending sets where work is retained. No historical v1 notes are imported and no cross-machine note synchronization occurs.

## Export

Configuring `api_key` enables delivery to the origin of `api_base_url` plus `/api/otel/git-ai/v1/logs`. Requests use OTLP/JSON and `Authorization: Bearer <api_key>`. OAuth login alone does not enable delivery. Local links persist even without a key; enabling it does not backfill history.

The existing payload contains canonical agent name, original session ID, commit SHA, stable patch ID, author email, subject, body, and repository URL when known. Claude is named `claude-code`. Native agent presets are supported without reading transcripts. Missing/placeholder IDs, empty patches, and missing required commit metadata are not exported; local links remain available.

Commit metadata and patch IDs are prepared with batched Git commands outside trace ingestion. The SQLite database remains `~/.git-ai/internal/otel-db`; its schema migration preserves existing queued and delivered outbox records. Link changes, operation completion, pending-set consumption, and new export work are transactional.

Network failures, 429 and 5xx retry with capped backoff; 413 splits batches; an oversized single record is parked. A 401 waits for a changed API key. Other failures are retained without automatic retries. Records remain bound to their original endpoint. Retries preserve the original payload and event time. Delivery is at least once.

`git-ai await` drains processing and OTEL delivery and reports remaining or failed work. `git-ai debug` checks trace2 transport and local session linking. Old notes, working logs, and transcript databases are left untouched but unused.
