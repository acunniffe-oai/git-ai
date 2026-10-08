//! Apply trace-derived Git operations to session sets, downstream of ingestion.
//!
//! Only immutable OIDs supplied by the normalized operation are accepted here.
//! Git commands are batched before the SQLite transaction; never execute Git
//! while iterating over commits, files, objects, or refs.
use std::collections::{BTreeSet, HashSet};

use super::git;
use super::store::{LinkStore, OperationContext};
use crate::daemon::domain::{
    NormalizedCommand, PullStrategy, ResetKind, SemanticEvent, StashOpKind,
};
use crate::error::GitAiError;
use crate::git::repository::Repository;

#[derive(Debug)]
enum Change {
    Commit(Vec<String>),
    Rewrite {
        pairs: Vec<(String, String)>,
        created: Vec<String>,
        consume_pending: bool,
    },
    Restore(Vec<String>),
    Defer(&'static str, Vec<String>, Option<String>),
    Abort(&'static str),
    Clear,
    StashSave(String, bool),
    StashRestore(String, bool),
}

fn valid_oid(oid: &str) -> bool {
    crate::git::repo_state::is_valid_git_oid(oid) && !oid.bytes().all(|b| b == b'0')
}

/// Prefer the complete branch transition for rebases, where HEAD also visits
/// the new base and every intermediate commit. Multiple changed branches are
/// ambiguous unless an exact HEAD span identifies the affected worktree.
fn head_transition(command: &NormalizedCommand) -> Option<(String, String)> {
    let branches: Vec<_> = command
        .ref_changes
        .iter()
        .filter(|c| {
            c.reference.starts_with("refs/heads/")
                && valid_oid(&c.old)
                && valid_oid(&c.new)
                && c.old != c.new
        })
        .collect();
    if let Some(first) = branches.first()
        && branches.iter().all(|c| c.reference == first.reference)
    {
        return Some((first.old.clone(), branches.last()?.new.clone()));
    }
    let heads: Vec<_> = command
        .ref_changes
        .iter()
        .filter(|c| {
            c.reference == "HEAD" && valid_oid(&c.old) && valid_oid(&c.new) && c.old != c.new
        })
        .collect();
    Some((heads.first()?.old.clone(), heads.last()?.new.clone()))
}

/// Range-diff anchors preserve precise pairs. Any unmatched source sessions
/// reach the replacement set together, covering squash/fixup and edited patches.
fn rewritten_commits(
    repo: &Repository,
    old: &str,
    new: &str,
    complete: bool,
    onto: Option<&str>,
) -> Result<Option<Change>, GitAiError> {
    if !valid_oid(old) || !valid_oid(new) || old == new {
        return Ok(None);
    }
    let Some(base) = git::merge_base(repo, old, new)? else {
        tracing::warn!(
            old,
            new,
            "commit links: rewrite has no established common boundary"
        );
        return Ok(None);
    };
    if base == old || base == new {
        return Ok(None);
    }
    let destination_base = match onto.filter(|onto| valid_oid(onto) && *onto != old && *onto != new)
    {
        Some(onto) if git::merge_base(repo, onto, new)?.as_deref() == Some(onto) => onto,
        _ => &base,
    };
    let sources = git::commits_in_range(repo, &base, old)?;
    let destinations = git::commits_in_range(repo, destination_base, new)?;
    let mut pairs = git::range_pairs(repo, &base, old, destination_base, new)?;
    if pairs.is_empty() && !complete {
        return Ok(None);
    }
    let matched_sources: HashSet<_> = pairs.iter().map(|(source, _)| source.clone()).collect();
    let matched_destinations: BTreeSet<_> = pairs
        .iter()
        .map(|(_, destination)| destination.clone())
        .collect();
    // Unpaired new-side commits may be upstream commits. Once an anchor exists,
    // do not spread local sessions onto those unrelated ancestors.
    let replacements: Vec<_> = if destination_base != base {
        // A captured checkout/continuation boundary excludes the upstream range.
        // New-side unmatched commits here are replacements (for example splits).
        for destination in destinations
            .iter()
            .filter(|sha| !matched_destinations.contains(*sha))
        {
            for source in &sources {
                pairs.push((source.clone(), destination.clone()));
            }
        }
        destinations
    } else if matched_destinations.is_empty() {
        // With no anchors, the sequence cannot be separated from upstream work.
        // Its tip is nevertheless the captured replacement of the old tip.
        vec![new.to_owned()]
    } else {
        destinations
            .into_iter()
            .filter(|sha| matched_destinations.contains(sha))
            .collect()
    };
    for source in sources
        .iter()
        .filter(|sha| complete && !matched_sources.contains(*sha))
    {
        for destination in &replacements {
            pairs.push((source.clone(), destination.clone()));
        }
    }
    Ok(Some(Change::Rewrite {
        pairs,
        created: replacements,
        consume_pending: false,
    }))
}

fn rebase_change(
    repo: &Repository,
    command: &NormalizedCommand,
    old: &str,
    new: &str,
) -> Result<Option<Change>, GitAiError> {
    // A conflicted rebase initially checks out the upstream tip. That is not
    // a newly created commit and must never receive the local branch's links.
    let completed = command.exit_code == 0;
    let created: HashSet<_> = command
        .ref_changes
        .iter()
        .filter(|change| change.reference == "HEAD")
        .skip(1)
        .map(|change| change.new.clone())
        .collect();
    if !completed && created.is_empty() {
        return Ok(None);
    }
    let continuing = command
        .invoked_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--continue" | "--skip"));
    let first_head = command
        .ref_changes
        .iter()
        .find(|change| change.reference == "HEAD");
    let onto = first_head.map(|change| {
        if continuing {
            change.old.as_str()
        } else {
            change.new.as_str()
        }
    });
    let mut change = rewritten_commits(repo, old, new, completed, onto)?;
    if let Some(Change::Rewrite {
        consume_pending, ..
    }) = change.as_mut()
    {
        *consume_pending = continuing;
    }
    if !completed
        && let Some(Change::Rewrite {
            pairs,
            created: replacements,
            ..
        }) = change.as_mut()
    {
        pairs.retain(|(_, destination)| created.contains(destination));
        replacements.retain(|destination| created.contains(destination));
    }
    Ok(change)
}

fn created_merge(command: &NormalizedCommand, new: &str) -> Option<Change> {
    (valid_oid(new) && command.merge_commit_oid.as_deref() == Some(new))
        .then(|| Change::Commit(vec![new.to_owned()]))
}

fn stash_retains_pending(args: &[String]) -> bool {
    let mut args = args.iter().map(String::as_str).peekable();
    let save = args.peek() == Some(&"save");
    if matches!(args.peek(), Some(&"push" | &"save")) {
        args.next();
    }
    while let Some(arg) = args.next() {
        if arg == "-m" || arg == "--message" {
            args.next();
            continue;
        }
        if matches!(
            arg,
            "--keep-index" | "-k" | "--patch" | "-p" | "--staged" | "-S" | "--"
        ) || arg.starts_with("--pathspec")
        {
            return true;
        }
        if !save && !arg.starts_with('-') {
            return true;
        }
    }
    false
}

fn changes(
    repo: &Repository,
    command: &NormalizedCommand,
    events: &[SemanticEvent],
    cherry_boundary: Option<&str>,
) -> Result<Vec<Change>, GitAiError> {
    let mut changes = Vec::new();
    let primary = command.primary_command.as_deref().unwrap_or_default();
    if primary == "revert" && command.invoked_args.iter().any(|a| a == "--abort") {
        return Ok(Vec::new());
    }
    // A same-HEAD hard reset has no ref transition but still discards edits.
    if primary == "reset"
        && command.exit_code == 0
        && command.invoked_args.iter().any(|a| a == "--hard")
    {
        return Ok(vec![Change::Clear]);
    }
    for event in events {
        match event {
            SemanticEvent::CommitCreated { new_head, .. } if valid_oid(new_head) => {
                let mut seen = HashSet::new();
                let mut created: Vec<_> = command
                    .ref_changes
                    .iter()
                    .filter(|c| c.reference == "HEAD" && c.old != c.new && valid_oid(&c.new))
                    .filter_map(|c| seen.insert(c.new.clone()).then_some(c.new.clone()))
                    .collect();
                if created.is_empty() {
                    created.push(new_head.clone());
                }
                changes.push(Change::Commit(created));
            }
            SemanticEvent::CommitAmended { old_head, new_head }
                if valid_oid(old_head) && valid_oid(new_head) =>
            {
                changes.push(Change::Rewrite {
                    pairs: vec![(old_head.clone(), new_head.clone())],
                    created: vec![new_head.clone()],
                    consume_pending: true,
                });
            }
            SemanticEvent::RebaseComplete {
                old_head, new_head, ..
            } => {
                if let Some(change) = rebase_change(repo, command, old_head, new_head)? {
                    changes.push(change);
                }
            }
            SemanticEvent::CherryPickComplete {
                original_head,
                new_head,
                source_commits,
                new_commits,
            } => {
                let mut created =
                    if new_commits.is_empty() && valid_oid(original_head) && valid_oid(new_head) {
                        git::commits_in_range(repo, original_head, new_head)?
                    } else {
                        new_commits
                            .iter()
                            .filter(|sha| valid_oid(sha))
                            .cloned()
                            .collect()
                    };
                if let Some(boundary) = cherry_boundary {
                    let heads: Vec<_> = command
                        .ref_changes
                        .iter()
                        .filter(|change| change.reference == "HEAD")
                        .collect();
                    if let Some(index) = heads.iter().position(|change| change.old == boundary) {
                        created = heads[index..]
                            .iter()
                            .filter(|change| valid_oid(&change.new))
                            .map(|change| change.new.clone())
                            .collect();
                    } else if heads.iter().any(|change| change.new == boundary) {
                        created.clear();
                    }
                }
                let source_commits = if source_commits.is_empty() {
                    &command.cherry_pick_source_oids
                } else {
                    source_commits
                };
                let sources: Vec<_> = source_commits
                    .iter()
                    .filter(|sha| valid_oid(sha))
                    .cloned()
                    .collect();
                let pairs = git::match_cherry_pick_pairs(repo, &sources, &created)?;
                changes.push(Change::Rewrite {
                    pairs,
                    created,
                    consume_pending: true,
                });
            }
            SemanticEvent::CherryPickNoCommit {
                source_commits,
                head,
            } if command.exit_code == 0 => {
                changes.push(Change::Defer(
                    "cherry-pick",
                    source_commits
                        .iter()
                        .filter(|s| valid_oid(s))
                        .cloned()
                        .collect(),
                    valid_oid(head).then(|| head.clone()),
                ));
            }
            SemanticEvent::CherryPickAbort { .. } => changes.push(Change::Abort("cherry-pick")),
            SemanticEvent::RebaseAbort { .. } => changes.push(Change::Abort("rebase")),
            SemanticEvent::MergeSquash { source_head, onto }
                if command.exit_code == 0 && valid_oid(source_head) && valid_oid(onto) =>
            {
                changes.push(Change::Defer(
                    "merge",
                    git::commits_in_range(repo, onto, source_head)?,
                    None,
                ));
            }
            SemanticEvent::Reset {
                kind: ResetKind::Soft | ResetKind::Mixed,
                old_head,
                new_head,
            } if command.exit_code == 0 && valid_oid(old_head) && valid_oid(new_head) => {
                changes.push(Change::Restore(git::commits_in_range(
                    repo, new_head, old_head,
                )?));
            }
            SemanticEvent::StashOperation {
                kind: StashOpKind::Push,
                ..
            } if command.exit_code == 0 => {
                if let Some(oid) = command
                    .ref_changes
                    .iter()
                    .rev()
                    .find(|c| c.reference == "refs/stash")
                    .map(|c| &c.new)
                    .filter(|s| valid_oid(s))
                {
                    let retain = stash_retains_pending(&command.invoked_args);
                    changes.push(Change::StashSave(oid.clone(), retain));
                }
            }
            SemanticEvent::StashOperation {
                kind: StashOpKind::Apply | StashOpKind::Pop | StashOpKind::Branch,
                ..
            } => {
                if let Some(oid) = command.stash_target_oid.as_ref().filter(|s| valid_oid(s)) {
                    // Conflict exits can leave applied changes. Conservatively
                    // restore this locally known set, without inspecting the index.
                    let remove = command.exit_code == 0
                        && matches!(
                            event,
                            SemanticEvent::StashOperation {
                                kind: StashOpKind::Pop | StashOpKind::Branch,
                                ..
                            }
                        );
                    changes.push(Change::StashRestore(oid.clone(), remove));
                }
            }
            SemanticEvent::PullCompleted { strategy, .. } => {
                if let Some((old, new)) = head_transition(command) {
                    if matches!(strategy, PullStrategy::Rebase | PullStrategy::RebaseMerges) {
                        if let Some(change) = rebase_change(repo, command, &old, &new)? {
                            changes.push(change);
                        }
                    } else if let Some(change) = created_merge(command, &new) {
                        changes.push(change);
                    }
                }
            }
            // Only one captured transition is considered below. Never launch
            // Git once per ref for multi-ref update transactions.
            SemanticEvent::RefUpdated { .. } => {}
            _ => {}
        }
    }
    if primary == "cherry-pick"
        && command.exit_code == 0
        && changes.is_empty()
        && command
            .invoked_args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--continue" | "--skip"))
    {
        changes.push(Change::Abort("cherry-pick"));
    }
    if primary == "cherry-pick" && command.exit_code != 0 {
        let applied: HashSet<_> = changes
            .iter()
            .filter_map(|change| match change {
                Change::Rewrite { pairs, .. } => {
                    Some(pairs.iter().map(|(source, _)| source.clone()))
                }
                _ => None,
            })
            .flatten()
            .collect();
        let remaining = command
            .cherry_pick_source_oids
            .iter()
            .filter(|source| valid_oid(source) && !applied.contains(*source))
            .cloned()
            .collect();
        let boundary = command
            .ref_changes
            .iter()
            .rev()
            .find(|change| change.reference == "HEAD" && valid_oid(&change.new))
            .map(|change| change.new.clone())
            .or_else(|| cherry_boundary.map(str::to_owned));
        changes.push(Change::Defer("cherry-pick", remaining, boundary));
    }
    if primary == "merge"
        && command.invoked_args.iter().any(|a| a == "--abort")
        && command.exit_code == 0
    {
        changes.push(Change::Abort("merge"));
    } else if primary == "merge" && !command.invoked_args.iter().any(|a| a == "--squash") {
        if let Some((_, new)) = head_transition(command)
            && let Some(change) = created_merge(command, &new)
        {
            changes.push(change);
        }
    } else if primary == "update-ref"
        && let Some((old, new)) = head_transition(command)
        && let Some(change) = rewritten_commits(repo, &old, &new, command.exit_code == 0, None)?
    {
        changes.push(change);
    }
    Ok(changes)
}

/// Called by the daemon's existing per-family asynchronous side-effect worker.
pub fn process_command(
    repo: &Repository,
    command: &NormalizedCommand,
    events: &[SemanticEvent],
) -> Result<(), GitAiError> {
    let Some(worktree_path) = command.worktree.as_ref() else {
        return Ok(());
    };
    // Synthetic/replayed trace2 may use a path alias (for example /var on macOS).
    // Match checkpoint scope without consulting mutable Git state.
    let worktree = worktree_path
        .canonicalize()
        .unwrap_or_else(|_| worktree_path.clone())
        .to_string_lossy()
        .into_owned();
    let mut recovered_command;
    let mut cherry_boundary = None;
    let command = if command.primary_command.as_deref() == Some("cherry-pick")
        && command.cherry_pick_source_oids.is_empty()
        && command
            .invoked_args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--continue" | "--skip"))
        && let Some(family) = &command.family_key
    {
        recovered_command = command.clone();
        let store = LinkStore::open()?;
        let mut sources = store.operation_sources(&family.0, &worktree, "cherry-pick")?;
        cherry_boundary = store
            .operation_sources(&family.0, &worktree, "cherry-pick-head")?
            .into_iter()
            .next();
        if command.invoked_args.iter().any(|arg| arg == "--skip") && !sources.is_empty() {
            sources.remove(0);
        }
        recovered_command.cherry_pick_source_oids = sources;
        &recovered_command
    } else {
        command
    };
    let changes = changes(repo, command, events, cherry_boundary.as_deref())?;
    if changes.is_empty() {
        return Ok(());
    }
    let Some(family) = command.family_key.as_ref() else {
        return Ok(());
    };
    let repo_url = repo.get_default_remote().ok().flatten().and_then(|name| {
        repo.remotes_with_urls()
            .ok()?
            .into_iter()
            .find(|(n, _)| n == &name)
            .map(|(_, url)| url)
    });
    let context = OperationContext {
        id: command.root_sid.clone(),
        family: family.0.clone(),
        worktree,
        git_dir: repo.common_dir().to_path_buf(),
        repo_url,
        time_ns: command.finished_at_ns,
    };
    LinkStore::open()?.apply_operation(&context, |tx| {
        for change in &changes {
            match change {
                Change::Commit(commits) => {
                    let pending = tx.pending_sessions()?;
                    for commit in commits {
                        tx.link_commit(commit, &pending)?;
                        tx.record_created_commit(
                            commit,
                            command.started_at_ns,
                            command.finished_at_ns,
                        )?;
                    }
                    if !commits.is_empty() {
                        tx.clear_pending()?;
                    }
                }
                Change::Rewrite {
                    pairs,
                    created,
                    consume_pending,
                } => {
                    if command.primary_command.as_deref() == Some("cherry-pick") {
                        // Recovered ordered sources, rather than the previous
                        // conflict's union, decide what survived --skip.
                        tx.clear_deferred("cherry-pick")?;
                        tx.clear_operation_sources("cherry-pick-head")?;
                    }
                    for (source, destination) in pairs {
                        tx.record_rewrite(source, destination)?;
                    }
                    let pending = if *consume_pending {
                        tx.pending_sessions()?
                    } else {
                        BTreeSet::new()
                    };
                    for commit in created {
                        tx.link_commit(commit, &pending)?;
                        tx.record_created_commit(
                            commit,
                            command.started_at_ns,
                            command.finished_at_ns,
                        )?;
                    }
                    if *consume_pending && !created.is_empty() {
                        tx.clear_pending()?;
                    }
                }
                Change::Restore(commits) => {
                    let sessions = tx.sessions_for_commits(commits)?;
                    tx.add_pending(&sessions)?;
                }
                Change::Defer(kind, commits, boundary) => {
                    let accumulate = *kind == "merge"
                        || command
                            .invoked_args
                            .iter()
                            .any(|arg| arg == "--no-commit" || arg == "-n");
                    let mut sources = if accumulate {
                        tx.operation_sources(kind)?
                    } else {
                        Vec::new()
                    };
                    let mut seen: HashSet<_> = sources.iter().cloned().collect();
                    sources.extend(
                        commits
                            .iter()
                            .filter(|sha| seen.insert((*sha).clone()))
                            .cloned(),
                    );
                    let sessions = tx.sessions_for_commits(&sources)?;
                    tx.clear_deferred(kind)?;
                    tx.add_deferred(kind, &sessions)?;
                    tx.set_operation_sources(kind, &sources)?;
                    if *kind == "cherry-pick" {
                        let boundary: Vec<_> = boundary.iter().cloned().collect();
                        tx.set_operation_sources("cherry-pick-head", &boundary)?;
                    }
                }
                Change::Abort(kind) => {
                    tx.clear_deferred(kind)?;
                    if *kind == "cherry-pick" {
                        tx.clear_operation_sources("cherry-pick-head")?;
                    }
                }
                Change::Clear => tx.clear_pending()?,
                Change::StashSave(oid, retain) => tx.save_stash(oid, *retain)?,
                Change::StashRestore(oid, remove) => tx.restore_stash(oid, *remove)?,
            }
        }
        Ok(())
    })?;
    Ok(())
}
