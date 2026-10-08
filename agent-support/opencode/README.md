# Git AI support for OpenCode

The managed OpenCode adapter links native OpenCode sessions to local commits. Install it with `git-ai install-hooks`. The installer supplies the absolute Git AI binary path.

The adapter records edited paths and the original session ID when file tools finish. It supports `edit`, `write`, `patch`, `multiedit`, `apply_patch`, and `applypatch`. File pre-hooks keep only the information needed to match the result; they do not launch a checkpoint. Shell tools retain pre/post checkpoints so Git AI can detect a write and link commits made within the call.

Repository discovery, pending-session deduplication, and trace2-driven commit processing run in Git AI. The adapter does not read edited file contents or local transcripts, and it does not spawn Git.

If Git AI cannot be launched, the adapter lets OpenCode continue. Set `GIT_AI_OPENCODE_DEBUG=1` to inspect hook errors.

## Development

Use `task dev` from the repository root to install a local debug build. To check the adapter's TypeScript types, run these commands in this directory:

```bash
npm ci --ignore-scripts
npm run type-check
```

Runtime protocol regressions live in `tests/integration/v2_adapters.rs` and run through `task test`.
