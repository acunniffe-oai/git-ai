//! Batched immutable Git object reads for commit/session linking.
use std::collections::{HashMap, HashSet};

use crate::git::repository::{Repository, exec_git_stdin};

/// Pairs source commits with their cherry-picked counterparts using a two-pass algorithm.
///
/// Pass 1: patch-id anchoring — identical patches get paired by stable patch-id.
/// Pass 2: positional gap-fill — remaining unmatched commits are paired by order.
/// Sources with no corresponding new commit (skipped) produce no pair.
pub fn match_cherry_pick_pairs(
    repo: &Repository,
    sources: &[String],
    new_commits: &[String],
) -> Result<Vec<(String, String)>, crate::error::GitAiError> {
    if sources.is_empty() || new_commits.is_empty() {
        return Ok(Vec::new());
    }

    let patch_ids = compute_patch_ids(repo, sources, new_commits)?;

    // Compute patch-ids for both sides
    let source_patch_ids: Vec<Option<String>> = sources
        .iter()
        .map(|sha| patch_ids.get(sha).cloned())
        .collect();

    let new_patch_ids: Vec<Option<String>> = new_commits
        .iter()
        .map(|sha| patch_ids.get(sha).cloned())
        .collect();

    // Build map: patch_id -> list of indices in new_commits
    let mut new_by_patch_id: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, pid) in new_patch_ids.iter().enumerate() {
        if let Some(id) = pid {
            new_by_patch_id.entry(id.clone()).or_default().push(idx);
        }
    }

    let mut matched_sources: Vec<bool> = vec![false; sources.len()];
    let mut matched_new: Vec<bool> = vec![false; new_commits.len()];
    let mut pairs: Vec<(String, String)> = Vec::new();

    // Pass 1: patch-id anchoring
    for (src_idx, src_pid) in source_patch_ids.iter().enumerate() {
        let Some(pid) = src_pid else {
            continue;
        };
        let Some(candidates) = new_by_patch_id.get_mut(pid) else {
            continue;
        };
        // Take the first unmatched candidate
        if let Some(pos) = candidates.iter().position(|&idx| !matched_new[idx]) {
            let new_idx = candidates[pos];
            pairs.push((sources[src_idx].clone(), new_commits[new_idx].clone()));
            matched_sources[src_idx] = true;
            matched_new[new_idx] = true;
        }
    }

    // Pass 2: positional gap-fill
    let unmatched_sources: Vec<usize> = matched_sources
        .iter()
        .enumerate()
        .filter(|(_, m)| !**m)
        .map(|(i, _)| i)
        .collect();

    let unmatched_new: Vec<usize> = matched_new
        .iter()
        .enumerate()
        .filter(|(_, m)| !**m)
        .map(|(i, _)| i)
        .collect();

    for (src_pos, new_pos) in unmatched_sources.iter().zip(unmatched_new.iter()) {
        pairs.push((sources[*src_pos].clone(), new_commits[*new_pos].clone()));
    }

    Ok(pairs)
}

fn compute_patch_ids(
    repo: &Repository,
    sources: &[String],
    new_commits: &[String],
) -> Result<HashMap<String, String>, crate::error::GitAiError> {
    let mut commits = Vec::new();
    let mut seen = HashSet::new();
    for sha in sources.iter().chain(new_commits.iter()) {
        if seen.insert(sha.clone()) {
            commits.push(sha.clone());
        }
    }
    if commits.is_empty() {
        return Ok(HashMap::new());
    }

    stable_patch_ids_for_commits(repo, &commits)
}

pub fn stable_patch_ids_for_commits(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, crate::error::GitAiError> {
    let mut commits = Vec::new();
    let mut seen = HashSet::new();
    for sha in commit_shas {
        if seen.insert(sha.clone()) {
            commits.push(sha.clone());
        }
    }
    if commits.is_empty() {
        return Ok(HashMap::new());
    }

    let mut log_args = repo.global_args_for_exec();
    log_args.extend([
        "log".to_string(),
        "--stdin".to_string(),
        "--no-walk".to_string(),
        "--reverse".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--no-color".to_string(),
        "--format=medium".to_string(),
        "-p".to_string(),
    ]);
    let stdin_data = commits.join("\n") + "\n";
    let log_output = exec_git_stdin(&log_args, stdin_data.as_bytes())?;
    if log_output.stdout.is_empty() {
        return Ok(HashMap::new());
    }

    let mut patch_args = repo.global_args_for_exec();
    patch_args.extend(["patch-id".to_string(), "--stable".to_string()]);
    let patch_output = exec_git_stdin(&patch_args, &log_output.stdout)?;

    let stdout = String::from_utf8_lossy(&patch_output.stdout);
    let mut patch_ids = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(patch_id) = parts.next() else {
            continue;
        };
        let Some(commit_sha) = parts.next() else {
            continue;
        };
        patch_ids.insert(commit_sha.to_string(), patch_id.to_string());
    }

    Ok(patch_ids)
}

/// List immutable history between captured object IDs in one Git process.
pub fn commits_in_range(
    repo: &Repository,
    base: &str,
    tip: &str,
) -> Result<Vec<String>, crate::error::GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".into(),
        "--reverse".into(),
        format!("{base}..{tip}"),
    ]);
    let output = crate::git::repository::exec_git(&args)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

pub fn merge_base(
    repo: &Repository,
    old: &str,
    new: &str,
) -> Result<Option<String>, crate::error::GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend(["merge-base".into(), old.into(), new.into()]);
    let output = crate::git::repository::exec_git_allow_nonzero(&args)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

/// Return only range-diff's explicit pairs. Ambiguity is handled at set level by
/// the caller, without loading files or reconstructing line attribution.
pub fn range_pairs(
    repo: &Repository,
    base: &str,
    old: &str,
    new_base: &str,
    new: &str,
) -> Result<Vec<(String, String)>, crate::error::GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "-c".into(),
        "core.abbrev=no".into(),
        "range-diff".into(),
        "--no-color".into(),
        "--no-abbrev".into(),
        "-s".into(),
        "--creation-factor=100".into(),
        format!("{base}..{old}"),
        format!("{new_base}..{new}"),
    ]);
    let output = crate::git::repository::exec_git(&args)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().take(5).collect();
            if fields.len() != 5 || !matches!(fields[2], "=" | "!") {
                return None;
            }
            let old = fields[1];
            let new = fields[4];
            (crate::git::repo_state::is_valid_git_oid(old)
                && crate::git::repo_state::is_valid_git_oid(new))
            .then(|| (old.to_owned(), new.to_owned()))
        })
        .collect())
}
