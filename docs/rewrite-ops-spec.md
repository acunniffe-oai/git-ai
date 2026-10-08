# Git AI v2 operation semantics

The trace2 normalizer, family sequencer, and reflog cursor establish immutable operation boundaries. Downstream processing updates session sets in SQLite; no operation reads or writes Git Notes or reconstructs line attribution.

| Operation | Local link behavior |
|---|---|
| Commit | Consume pending worktree sessions into the created commit. |
| Amend | Union old-commit links and pending sessions onto the replacement SHA. |
| Rebase / pull-rebase | Match old/new sequences and transfer locally known sessions; ambiguous source mappings union onto established replacement commits. |
| Cherry-pick | Transfer source links to created commits, including completed commits before a conflict. |
| Squash / fixup | Union source sessions into the resulting SHA. |
| No-commit cherry-pick / squash merge | Defer source sessions to the next commit. |
| Ordinary merge / fast-forward | Keep existing links; new merge commits may consume pending sessions, without inheriting all ancestry. |
| Revert | Link pending sessions for new work; do not credit the source being undone. |
| Soft/mixed reset | Restore sessions from removed commits to pending state. |
| Hard reset | Clear pending sessions, including when HEAD does not change. |
| Stash | Save and restore pending sets by captured stash OID; partial stash conservatively retains the worktree set too. |

Failed stash apply/pop/branch attempts conservatively restore a locally known stash session set, since conflicts can leave changes in the worktree. Only a successful pop/branch removes the saved set. This favors retaining links even when a preflight failure made no changes.

Ordinary rebase and update-ref rewrites preserve pending worktree sessions; amend, cherry-pick, and conflict-resolution continuation commits consume their applicable pending set.

Old commit links remain available after rewrites. Operation IDs prevent replay from consuming later contribution windows. Stored rewrite edges propagate links confirmed by a late shell post-hook.

All Git metadata work uses a fixed number of batched calls per operation, independent of commit/file/object/ref count. No extra Git work or file reads are added to trace ingestion. Unestablished boundaries fail closed; processing-time HEAD is never substituted for captured state.

The native write signal is repository-level. Partial staging or later removal of an agent edit does not retract it. This deliberately differs from v1 attribution semantics. The implementation contract is covered by the v2 TestRepo suites and the retained trace2 transport/cursor tests.
