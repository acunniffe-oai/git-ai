use crate::config;
use crate::error::GitAiError;
use crate::git::repo_state::{
    common_dir_for_git_dir, git_dir_for_worktree, is_valid_git_dir, worktree_root_for_path,
};
#[cfg(windows)]
use crate::utils::is_interactive_terminal;

use regex::Regex;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(windows)]
use crate::utils::CREATE_NO_WINDOW;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

// Keep a thread-local depth for low-overhead checks on the active thread and a process-global
// depth so internal git spawned from background threads inherits suppression state.
thread_local! {
    static INTERNAL_GIT_HOOKS_DISABLED_DEPTH: Cell<usize> = const { Cell::new(0) };
}
static INTERNAL_GIT_HOOKS_DISABLED_DEPTH_GLOBAL: AtomicUsize = AtomicUsize::new(0);

pub struct InternalGitHooksGuard;

impl Drop for InternalGitHooksGuard {
    fn drop(&mut self) {
        INTERNAL_GIT_HOOKS_DISABLED_DEPTH.with(|depth| {
            let current = depth.get();
            if current > 0 {
                depth.set(current - 1);
            }
        });
        INTERNAL_GIT_HOOKS_DISABLED_DEPTH_GLOBAL.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Disable managed git hooks for internal `git` subprocesses executed through `exec_git*`.
/// Use this guard around higher-level operations that already execute hook logic explicitly.
pub fn disable_internal_git_hooks() -> InternalGitHooksGuard {
    INTERNAL_GIT_HOOKS_DISABLED_DEPTH.with(|depth| depth.set(depth.get() + 1));
    INTERNAL_GIT_HOOKS_DISABLED_DEPTH_GLOBAL.fetch_add(1, Ordering::Relaxed);
    InternalGitHooksGuard
}

fn should_disable_internal_git_hooks() -> bool {
    INTERNAL_GIT_HOOKS_DISABLED_DEPTH.with(|depth| depth.get() > 0)
        || INTERNAL_GIT_HOOKS_DISABLED_DEPTH_GLOBAL.load(Ordering::Relaxed) > 0
}

#[cfg(windows)]
fn null_hooks_path() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn null_hooks_path() -> &'static str {
    "/dev/null"
}

#[doc(hidden)]
pub fn args_with_disabled_hooks_if_needed(args: &[String]) -> Vec<String> {
    if !should_disable_internal_git_hooks() {
        return args.to_vec();
    }

    // Respect explicit hook-path overrides if a caller already set one.
    let already_overrides_hooks = args
        .windows(2)
        .any(|pair| pair[0] == "-c" && pair[1].starts_with("core.hooksPath="))
        || args.iter().any(|arg| {
            arg.starts_with("-ccore.hooksPath=") || arg.starts_with("--config=core.hooksPath=")
        });

    if already_overrides_hooks {
        return args.to_vec();
    }

    let mut out = Vec::with_capacity(args.len() + 2);
    out.push("-c".to_string());
    out.push(format!("core.hooksPath={}", null_hooks_path()));
    out.extend(args.iter().cloned());
    out
}

fn first_git_subcommand_index(args: &[String]) -> Option<usize> {
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];

        if !arg.starts_with('-') {
            return Some(index);
        }

        let takes_value = matches!(
            arg.as_str(),
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--config-env"
        );

        index += if takes_value { 2 } else { 1 };
    }

    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalGitProfile {
    General,
    PatchParse,
    NumstatParse,
    RawDiffParse,
}

fn strip_profile_conflicts(args: Vec<String>, profile: InternalGitProfile) -> Vec<String> {
    if profile == InternalGitProfile::General {
        return args;
    }

    let Some(command_index) = first_git_subcommand_index(&args) else {
        return args;
    };

    let should_drop = |arg: &str| -> bool {
        match profile {
            InternalGitProfile::General => false,
            InternalGitProfile::PatchParse => {
                arg == "--ext-diff"
                    || arg == "--textconv"
                    || arg == "--relative"
                    || arg.starts_with("--relative=")
                    || arg == "--color"
                    || arg.starts_with("--color=")
                    || arg == "--no-prefix"
                    || arg == "--src-prefix"
                    || arg == "--dst-prefix"
                    || arg.starts_with("--src-prefix=")
                    || arg.starts_with("--dst-prefix=")
                    || arg.starts_with("--diff-algorithm=")
                    || arg == "--no-indent-heuristic"
                    || arg.starts_with("--inter-hunk-context=")
            }
            InternalGitProfile::NumstatParse => {
                arg == "--ext-diff"
                    || arg == "--textconv"
                    || arg == "--relative"
                    || arg.starts_with("--relative=")
                    || arg == "--color"
                    || arg.starts_with("--color=")
                    || arg == "--find-renames"
                    || arg.starts_with("--find-renames=")
                    || arg == "--find-copies"
                    || arg.starts_with("--find-copies=")
                    || arg == "--find-copies-harder"
                    || arg == "-M"
                    || arg.starts_with("-M")
                    || arg == "-C"
                    || arg.starts_with("-C")
            }
            InternalGitProfile::RawDiffParse => {
                arg == "--ext-diff"
                    || arg == "--textconv"
                    || arg == "--relative"
                    || arg.starts_with("--relative=")
                    || arg == "--color"
                    || arg.starts_with("--color=")
            }
        }
    };

    let mut out = Vec::with_capacity(args.len());
    out.extend(args[..=command_index].iter().cloned());

    let mut index = command_index + 1;
    while index < args.len() {
        if args[index] == "--" {
            out.extend(args[index..].iter().cloned());
            return out;
        }

        let drop_current = should_drop(&args[index]);
        if !drop_current {
            out.push(args[index].clone());
            index += 1;
            continue;
        }

        // Handle split-arg forms we intentionally strip (e.g. --src-prefix X).
        if matches!(profile, InternalGitProfile::PatchParse)
            && (args[index] == "--src-prefix" || args[index] == "--dst-prefix")
        {
            index += 1;
            if index < args.len() && args[index] != "--" {
                index += 1;
            }
            continue;
        }

        index += 1;
    }

    out
}

fn profile_options(profile: InternalGitProfile) -> &'static [&'static str] {
    match profile {
        InternalGitProfile::General => &[],
        InternalGitProfile::PatchParse => &[
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-relative",
            "--no-color",
            "--diff-algorithm=default",
            "--indent-heuristic",
            "--inter-hunk-context=0",
        ],
        InternalGitProfile::NumstatParse => &[
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-relative",
            "--no-renames",
        ],
        InternalGitProfile::RawDiffParse => &[
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-relative",
        ],
    }
}

#[doc(hidden)]
pub fn args_with_internal_git_profile(args: &[String], profile: InternalGitProfile) -> Vec<String> {
    if profile == InternalGitProfile::General {
        return args.to_vec();
    }

    let args = strip_profile_conflicts(args.to_vec(), profile);
    let Some(command_index) = first_git_subcommand_index(&args) else {
        return args;
    };

    let options = profile_options(profile);
    if options.is_empty() {
        return args;
    }

    let mut out = Vec::with_capacity(args.len() + options.len());
    out.extend(args[..=command_index].iter().cloned());
    for option in options {
        if !args.iter().any(|arg| arg == option) {
            out.push((*option).to_string());
        }
    }
    out.extend(args[command_index + 1..].iter().cloned());
    out
}

pub struct Reference<'a> {
    repo: &'a Repository,
    ref_name: String,
}

impl<'a> Reference<'a> {
    pub fn name(&self) -> Option<&str> {
        Some(&self.ref_name)
    }

    pub fn shorthand(&self) -> Result<String, GitAiError> {
        let mut args = self.repo.global_args_for_exec();
        args.push("rev-parse".to_string());
        args.push("--abbrev-ref".to_string());
        args.push(self.ref_name.clone());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    pub fn target(&self) -> Result<String, GitAiError> {
        use crate::git::fast_reader::{FastRefReader, HeadKind};
        let reader = FastRefReader::new(&self.repo.git_dir, &self.repo.git_common_dir);
        if self.ref_name == "HEAD" {
            match reader.try_read_head() {
                Some(HeadKind::Detached(oid)) => return Ok(oid),
                Some(HeadKind::Symbolic(refname)) => {
                    if let Some(sha) = reader.try_resolve_ref(&refname) {
                        return Ok(sha);
                    }
                }
                None => {}
            }
        } else if let Some(sha) = reader.try_resolve_ref(&self.ref_name) {
            return Ok(sha);
        }

        let mut args = self.repo.global_args_for_exec();
        args.push("rev-parse".to_string());
        args.push(self.ref_name.clone());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }
}

/// A Git identity (name + email) for the current repository.
///
/// Resolved via `git var GIT_COMMITTER_IDENT` which respects the full git precedence
/// chain (env vars > config > system defaults), unlike a raw `git config user.name`
/// lookup which can miss identities configured via environment variables or system-level
/// defaults.
#[derive(Debug, Clone, Default)]
pub struct GitAuthorIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl GitAuthorIdentity {
    /// Format as `"Name <email>"`, `"Name"`, `"<email>"`, or `None`.
    pub fn formatted(&self) -> Option<String> {
        match (&self.name, &self.email) {
            (Some(n), Some(e)) => Some(format!("{} <{}>", n, e)),
            (Some(n), None) => Some(n.clone()),
            (None, Some(e)) => Some(format!("<{}>", e)),
            (None, None) => None,
        }
    }

    /// Return the full identity (`"Name <email>"`) or fall back to name-only / `"unknown"`.
    pub fn formatted_or_unknown(&self) -> String {
        self.formatted().unwrap_or_else(|| "unknown".to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct GitIdentityResolution {
    pub raw_git_var: Option<String>,
    pub identity: GitAuthorIdentity,
}

#[derive(Debug, Clone, Default)]
pub struct GitConfigIdentityResolution {
    pub raw_name: Option<String>,
    pub raw_email: Option<String>,
    pub identity: GitAuthorIdentity,
}

/// Parse `git var GIT_COMMITTER_IDENT` output into name and email.
///
/// The output format is: `Name <email> unix-timestamp timezone`
/// For example: `John Doe <john@example.com> 1234567890 +0000`
pub fn parse_git_var_identity(output: &str) -> GitAuthorIdentity {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return GitAuthorIdentity::default();
    }

    // Find email in angle brackets
    let email_start = trimmed.find('<');
    let email_end = trimmed.find('>');

    match (email_start, email_end) {
        (Some(start), Some(end)) if end > start => {
            let name = trimmed[..start].trim();
            let email = trimmed[start + 1..end].trim();
            GitAuthorIdentity {
                name: if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                },
                email: if email.is_empty() {
                    None
                } else {
                    Some(email.to_string())
                },
            }
        }
        _ => {
            // No angle brackets - just treat the whole string as a name
            GitAuthorIdentity {
                name: Some(trimmed.to_string()),
                email: None,
            }
        }
    }
}

pub fn global_git_config_committer_identity() -> Result<GitAuthorIdentity, GitAiError> {
    Ok(global_git_config_identity_resolution()?.identity)
}

pub fn global_git_config_identity_resolution() -> Result<GitConfigIdentityResolution, GitAiError> {
    let config =
        gix_config::File::from_globals().map_err(|e| GitAiError::GixError(e.to_string()))?;
    Ok(git_config_identity_resolution_from_config(&config))
}

pub fn current_git_committer_identity_resolution() -> GitIdentityResolution {
    resolve_git_var_identity_with_args(Vec::new(), "GIT_COMMITTER_IDENT", || {
        global_git_config_committer_identity().unwrap_or_default()
    })
}

fn git_config_identity_resolution_from_config(
    config: &gix_config::File<'_>,
) -> GitConfigIdentityResolution {
    let raw_name = config.string("user.name").map(|cow| cow.to_string());
    let raw_email = config.string("user.email").map(|cow| cow.to_string());
    let name = raw_name
        .as_deref()
        .map(ToOwned::to_owned)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());
    let email = raw_email
        .as_deref()
        .map(ToOwned::to_owned)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());

    GitConfigIdentityResolution {
        raw_name,
        raw_email,
        identity: GitAuthorIdentity { name, email },
    }
}

fn resolve_git_var_identity_with_args<F>(
    mut args: Vec<String>,
    git_var: &str,
    fallback_identity: F,
) -> GitIdentityResolution
where
    F: FnOnce() -> GitAuthorIdentity,
{
    args.push("var".to_string());
    args.push(git_var.to_string());

    if let Ok(output) = exec_git(&args)
        && let Ok(stdout) = String::from_utf8(output.stdout)
    {
        let identity = parse_git_var_identity(&stdout);
        if identity.name.is_some() || identity.email.is_some() {
            return GitIdentityResolution {
                raw_git_var: Some(stdout.trim().to_string()),
                identity,
            };
        }
    }

    GitIdentityResolution {
        raw_git_var: None,
        identity: fallback_identity(),
    }
}

#[derive(Debug, Clone)]
pub struct Repository {
    global_args: Vec<String>,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
    workdir: PathBuf,
    /// Canonical (absolute, resolved) version of workdir for reliable path comparisons
    /// On Windows, this uses the \\?\ UNC prefix format
    canonical_workdir: PathBuf,
    /// Cached git author identity resolved via `git var GIT_COMMITTER_IDENT`.
    cached_author_identity: std::sync::OnceLock<GitAuthorIdentity>,
}

impl Repository {
    // Util for preparing global args for execution
    pub fn global_args_for_exec(&self) -> Vec<String> {
        let mut args = self.global_args.clone();
        if !args.iter().any(|arg| arg == "--no-pager") {
            args.push("--no-pager".to_string());
        }
        args
    }

    // Retrieve and resolve the reference pointed at by HEAD.
    // If HEAD is a symbolic ref, return the refname (e.g., "refs/heads/main").
    // Otherwise, return "HEAD".
    pub fn head<'a>(&'a self) -> Result<Reference<'a>, GitAiError> {
        use crate::git::fast_reader::{FastRefReader, HeadKind};
        let reader = FastRefReader::new(&self.git_dir, &self.git_common_dir);
        match reader.try_read_head() {
            Some(HeadKind::Symbolic(refname)) => {
                return Ok(Reference {
                    repo: self,
                    ref_name: refname,
                });
            }
            Some(HeadKind::Detached(_)) => {
                return Ok(Reference {
                    repo: self,
                    ref_name: "HEAD".to_string(),
                });
            }
            None => {}
        }

        let mut args = self.global_args_for_exec();
        args.push("symbolic-ref".to_string());
        args.push("HEAD".to_string());

        let output = exec_git(&args);

        match output {
            Ok(output) if output.status.success() => {
                let refname = String::from_utf8(output.stdout)?;
                Ok(Reference {
                    repo: self,
                    ref_name: refname.trim().to_string(),
                })
            }
            _ => Ok(Reference {
                repo: self,
                ref_name: "HEAD".to_string(),
            }),
        }
    }

    // Returns the path to the .git folder for normal repositories or the repository itself for bare repositories.
    // TODO Test on bare repositories.
    pub fn path(&self) -> &Path {
        self.git_dir.as_path()
    }

    /// Returns the common git directory shared by linked worktrees.
    /// For non-worktree repositories, this is the same as `path()`.
    pub fn common_dir(&self) -> &Path {
        self.git_common_dir.as_path()
    }

    // Get the path of the working directory for this repository.
    // If this repository is bare, then None is returned.
    pub fn workdir(&self) -> Result<PathBuf, GitAiError> {
        // TODO Remove Result since this is determined at initialization now
        Ok(self.workdir.clone())
    }

    /// Returns true when this repository is bare.
    pub fn is_bare_repository(&self) -> Result<bool, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("rev-parse".to_string());
        args.push("--is-bare-repository".to_string());
        let output = exec_git(&args)?;
        let value = String::from_utf8(output.stdout)?;
        Ok(value.trim() == "true")
    }

    /// Get the canonical (absolute, resolved) path of the working directory
    /// Check if a path is within the repository's working directory.
    ///
    /// Returns `false` for paths inside nested independent git repos (subdirectories
    /// with their own `.git/` directory), since those files belong to the nested repo,
    /// not this one. Submodules (`.git` file, not directory) are transparent and still
    /// considered part of this repo.
    pub fn path_is_in_workdir(&self, path: &Path) -> bool {
        // Try canonical comparison first (most reliable, especially on Windows)
        if let Ok(canonical_path) = path.canonicalize() {
            if !canonical_path.starts_with(&self.canonical_workdir) {
                return false;
            }
            return !has_intervening_git_dir(&canonical_path, &self.canonical_workdir);
        }

        // Fallback for paths that don't exist yet: try to canonicalize the parent directory
        // and append the filename. This handles cases where the path contains symlinks
        // (e.g., /var -> /private/var on macOS).
        if let Some(parent) = path.parent()
            && let Some(filename) = path.file_name()
            && let Ok(canonical_parent) = parent.canonicalize()
        {
            let canonical_path = canonical_parent.join(filename);
            if !canonical_path.starts_with(&self.canonical_workdir) {
                return false;
            }
            return !has_intervening_git_dir(&canonical_path, &self.canonical_workdir);
        }

        // Final fallback: normalize by resolving .. and . and check against both
        // canonical and non-canonical workdir (in case of symlinks)
        let normalized = path
            .components()
            .fold(std::path::PathBuf::new(), |mut acc, component| {
                match component {
                    std::path::Component::ParentDir => {
                        acc.pop();
                    }
                    std::path::Component::CurDir => {}
                    _ => acc.push(component),
                }
                acc
            });

        // Try both canonical and non-canonical workdir to handle symlinks
        let in_canonical = normalized.starts_with(&self.canonical_workdir);
        let in_workdir = normalized.starts_with(&self.workdir);

        if !in_canonical && !in_workdir {
            return false;
        }

        // Use canonical_workdir if path matches it, otherwise use workdir
        let base = if in_canonical {
            &self.canonical_workdir
        } else {
            &self.workdir
        };

        !has_intervening_git_dir(&normalized, base)
    }

    pub fn remotes(&self) -> Result<Vec<String>, GitAiError> {
        Ok(self
            .remotes_with_urls()?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    // List all remotes with their URLs as tuples (name, url)
    pub fn remotes_with_urls(&self) -> Result<Vec<(String, String)>, GitAiError> {
        let config = self.get_git_config_file()?;
        let mut remotes = Vec::new();

        for section in config.sections() {
            if !section.header().name().eq_ignore_ascii_case(b"remote") {
                continue;
            }
            let Some(name) = section.header().subsection_name() else {
                continue;
            };
            let Some(url) = section.body().value("url") else {
                continue;
            };
            remotes.push((name.to_string(), url.to_string()));
        }

        Ok(remotes)
    }

    fn load_optional_config_file(
        path: &Path,
        source: gix_config::Source,
    ) -> Result<Option<gix_config::File<'static>>, GitAiError> {
        if !path.exists() {
            return Ok(None);
        }
        gix_config::File::from_path_no_includes(path.to_path_buf(), source)
            .map(Some)
            .map_err(|e| GitAiError::GixError(e.to_string()))
    }

    pub(crate) fn get_git_config_file(&self) -> Result<gix_config::File<'static>, GitAiError> {
        git_config_file_for_repo_paths(self.path(), self.common_dir())
    }

    /// Get config value for a given key as a String.
    pub fn config_get_str(&self, key: &str) -> Result<Option<String>, GitAiError> {
        self.get_git_config_file()
            .map(|cfg| cfg.string(key).map(|cow| cow.to_string()))
    }

    /// Get the effective raw Git user identity for this repository.
    ///
    /// Uses `git var GIT_COMMITTER_IDENT` which respects the full git identity precedence:
    /// `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL` env vars > `user.name`/`user.email` config >
    /// system defaults.
    ///
    /// Falls back to `git config user.name` / `user.email` if `git var` fails.
    /// The result is cached per Repository instance for performance.
    ///
    /// For git-ai authorship metadata, use [`Self::effective_author_identity`] so the
    /// git-ai author config can override this raw Git identity.
    pub fn git_author_identity(&self) -> &GitAuthorIdentity {
        self.cached_author_identity
            .get_or_init(|| self.resolve_git_var_identity("GIT_COMMITTER_IDENT"))
    }

    pub fn git_author_identity_resolution(&self) -> GitIdentityResolution {
        self.resolve_git_var_identity_resolution("GIT_COMMITTER_IDENT")
    }

    /// Get the effective git commit author identity for this repository.
    ///
    /// Uses `git var GIT_AUTHOR_IDENT` which respects:
    /// `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` env vars > `user.name`/`user.email` config >
    /// system defaults.
    ///
    /// Falls back to `git config user.name` / `user.email` if `git var` fails.
    ///
    /// This is the correct method to use when resolving the commit **author** identity
    /// (as opposed to committer), e.g. in commit hooks.
    pub fn git_commit_author_identity(&self) -> GitAuthorIdentity {
        self.resolve_git_var_identity("GIT_AUTHOR_IDENT")
    }

    /// Internal: resolve git identity via the specified `git var` variable.
    fn resolve_git_var_identity(&self, git_var: &str) -> GitAuthorIdentity {
        self.resolve_git_var_identity_resolution(git_var).identity
    }

    fn resolve_git_var_identity_resolution(&self, git_var: &str) -> GitIdentityResolution {
        resolve_git_var_identity_with_args(self.global_args_for_exec(), git_var, || {
            self.get_git_config_file()
                .ok()
                .map(|config| git_config_identity_resolution_from_config(&config).identity)
                .unwrap_or_default()
        })
    }

    /// Get all config values matching a regex pattern.
    ///
    /// Regular expression matching is currently case-sensitive
    /// and done against a canonicalized version of the key
    /// in which section and variable names are lowercased, but subsection names are not.
    ///
    /// Returns a HashMap of key -> value for all matching config entries.
    pub fn config_get_regexp(
        &self,
        pattern: &str,
    ) -> Result<std::collections::HashMap<String, String>, GitAiError> {
        let re = Regex::new(pattern)
            .map_err(|e| GitAiError::Generic(format!("Invalid regex pattern: {}", e)))?;

        let config = self.get_git_config_file()?;
        let mut matches: HashMap<String, String> = HashMap::new();

        for section in config.sections() {
            let section_name = section.header().name().to_string().to_lowercase();
            let subsection = section.header().subsection_name();

            for value_name in section.body().value_names() {
                let value_name_str = value_name.to_string().to_lowercase();
                let full_key = if let Some(sub) = subsection {
                    format!("{}.{}.{}", section_name, sub, value_name_str)
                } else {
                    format!("{}.{}", section_name, value_name_str)
                };

                if re.is_match(&full_key)
                    && let Some(value) = section.body().value(value_name).map(|c| c.to_string())
                {
                    matches.insert(full_key, value);
                }
            }
        }

        Ok(matches)
    }

    /// Get the git version as a tuple (major, minor, patch).
    /// Returns None if the version cannot be parsed.
    pub fn git_version(&self) -> Option<(u32, u32, u32)> {
        let args = vec!["--version".to_string()];
        let output = exec_git(&args).ok()?;
        let version_str = String::from_utf8(output.stdout).ok()?;
        parse_git_version(&version_str)
    }

    /// Check if the current git version supports --ignore-revs-file flag for blame.
    /// This flag was added in git 2.23.0.
    pub fn get_default_remote(&self) -> Result<Option<String>, GitAiError> {
        let remotes = self.remotes()?;
        if remotes.is_empty() {
            return Ok(None);
        }
        // Prefer 'origin' if it exists
        for i in 0..remotes.len() {
            if let Some(name) = remotes.get(i)
                && name == "origin"
            {
                return Ok(Some("origin".to_string()));
            }
        }
        // Otherwise, just use the first remote
        Ok(remotes.first().map(|s| s.to_string()))
    }
}

pub fn find_repository(global_args: &[String]) -> Result<Repository, GitAiError> {
    let mut rev_parse_args = global_args.to_owned();
    rev_parse_args.push("rev-parse".to_string());
    // Use --git-dir instead of --absolute-git-dir for compatibility with Git < 2.13
    // (--absolute-git-dir was added in Git 2.13; older versions output the literal
    // string "absolute-git-dir" instead of the resolved path).
    rev_parse_args.push("--is-bare-repository".to_string());
    rev_parse_args.push("--git-dir".to_string());
    rev_parse_args.push("--git-common-dir".to_string());

    let rev_parse_output = exec_git(&rev_parse_args)?;
    let rev_parse_stdout = String::from_utf8(rev_parse_output.stdout)?;
    let mut lines = rev_parse_stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());

    let is_bare = match lines.next() {
        Some("true") => true,
        Some("false") => false,
        Some(other) => {
            return Err(GitAiError::Generic(format!(
                "Unexpected --is-bare-repository output: {}",
                other
            )));
        }
        None => {
            return Err(GitAiError::Generic(
                "Missing --is-bare-repository output from git rev-parse".to_string(),
            ));
        }
    };

    let git_dir_str = lines.next().ok_or_else(|| {
        GitAiError::Generic("Missing --git-dir output from git rev-parse".to_string())
    })?;
    let git_common_dir_str = lines.next().ok_or_else(|| {
        GitAiError::Generic("Missing --git-common-dir output from git rev-parse".to_string())
    })?;
    let command_base_dir = resolve_command_base_dir(global_args)?;
    let git_dir = if Path::new(git_dir_str).is_relative() {
        command_base_dir.join(git_dir_str)
    } else {
        PathBuf::from(git_dir_str)
    };
    let git_common_dir = if Path::new(git_common_dir_str).is_relative() {
        command_base_dir.join(git_common_dir_str)
    } else {
        PathBuf::from(git_common_dir_str)
    };

    if !git_dir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Git directory does not exist: {}",
            git_dir.display()
        )));
    }
    if !git_common_dir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Git common directory does not exist: {}",
            git_common_dir.display()
        )));
    }

    let workdir = if is_bare {
        git_dir.parent().map(Path::to_path_buf).ok_or_else(|| {
            GitAiError::Generic(format!(
                "Git directory has no parent: {}",
                git_dir.display()
            ))
        })?
    } else {
        let mut top_level_args = global_args.to_owned();
        top_level_args.push("rev-parse".to_string());
        top_level_args.push("--show-toplevel".to_string());
        let output = exec_git(&top_level_args)?;
        PathBuf::from(String::from_utf8(output.stdout)?.trim())
    };

    if !workdir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Work directory does not exist: {}",
            workdir.display()
        )));
    }

    // Ensure all internal git commands use a stable repository root consistently.
    let mut normalized_global_args = global_args.to_owned();
    let command_root = if is_bare {
        git_dir.display().to_string()
    } else {
        workdir.display().to_string()
    };

    if normalized_global_args.is_empty() {
        normalized_global_args = vec!["-C".to_string(), command_root];
    } else if normalized_global_args.len() == 2
        && normalized_global_args[0] == "-C"
        && normalized_global_args[1] != command_root
    {
        normalized_global_args[1] = command_root;
    }

    // Canonicalize workdir for reliable path comparisons (especially on Windows)
    // On Windows, canonical paths use the \\?\ UNC prefix, which makes path.starts_with()
    // comparisons work correctly. We store both regular and canonical versions.
    let canonical_workdir = workdir.canonicalize().map_err(|e| {
        GitAiError::Generic(format!(
            "Failed to canonicalize working directory {}: {}",
            workdir.display(),
            e
        ))
    })?;

    Ok(Repository {
        global_args: normalized_global_args,
        git_dir,
        git_common_dir,
        workdir,
        canonical_workdir,
        cached_author_identity: std::sync::OnceLock::new(),
    })
}

#[doc(hidden)]
pub fn resolve_command_base_dir(global_args: &[String]) -> Result<PathBuf, GitAiError> {
    let mut base: Option<PathBuf> = None;
    let mut idx = 0usize;

    while idx < global_args.len() {
        if global_args[idx] == "-C" {
            let path_arg = global_args.get(idx + 1).ok_or_else(|| {
                GitAiError::Generic("Missing path after -C in global git args".to_string())
            })?;

            let next_base = PathBuf::from(path_arg);
            base = Some(if next_base.is_absolute() {
                next_base
            } else {
                let current = match &base {
                    Some(existing) => existing.clone(),
                    None => std::env::current_dir().map_err(GitAiError::IoError)?,
                };
                current.join(next_base)
            });
            idx += 2;
            continue;
        }
        idx += 1;
    }

    match base {
        Some(base) => Ok(base),
        None => std::env::current_dir().map_err(GitAiError::IoError),
    }
}

struct DiscoveredRepositoryPaths {
    command_root: PathBuf,
    workdir: PathBuf,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
}

fn discover_repository_paths_no_git_exec(
    path: &Path,
) -> Result<DiscoveredRepositoryPaths, GitAiError> {
    let start = if path.file_name().and_then(|name| name.to_str()) == Some(".git") || path.is_dir()
    {
        path.to_path_buf()
    } else {
        path.parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf())
    };

    if start.file_name().and_then(|name| name.to_str()) == Some(".git") {
        if is_valid_git_dir(&start) {
            let workdir = start.parent().ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Git directory has no parent workdir: {}",
                    start.display()
                ))
            })?;
            let git_common_dir = common_dir_for_git_dir(&start).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Unable to resolve common dir for git dir: {}",
                    start.display()
                ))
            })?;
            return Ok(DiscoveredRepositoryPaths {
                command_root: workdir.to_path_buf(),
                workdir: workdir.to_path_buf(),
                git_dir: start,
                git_common_dir,
            });
        }

        if start.is_file() {
            let workdir = start.parent().ok_or_else(|| {
                GitAiError::Generic(format!(
                    ".git file has no parent workdir: {}",
                    start.display()
                ))
            })?;
            let git_dir = git_dir_for_worktree(workdir).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Unable to resolve git dir for worktree: {}",
                    workdir.display()
                ))
            })?;
            let git_common_dir = common_dir_for_git_dir(&git_dir).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Unable to resolve common dir for git dir: {}",
                    git_dir.display()
                ))
            })?;
            return Ok(DiscoveredRepositoryPaths {
                command_root: workdir.to_path_buf(),
                workdir: workdir.to_path_buf(),
                git_dir,
                git_common_dir,
            });
        }
    }

    if let Some(worktree_root) = worktree_root_for_path(&start) {
        let git_dir = git_dir_for_worktree(&worktree_root).ok_or_else(|| {
            GitAiError::Generic(format!(
                "Unable to resolve git dir for worktree: {}",
                worktree_root.display()
            ))
        })?;
        let git_common_dir = common_dir_for_git_dir(&git_dir).ok_or_else(|| {
            GitAiError::Generic(format!(
                "Unable to resolve common dir for git dir: {}",
                git_dir.display()
            ))
        })?;
        return Ok(DiscoveredRepositoryPaths {
            command_root: worktree_root.clone(),
            workdir: worktree_root,
            git_dir,
            git_common_dir,
        });
    }

    let mut current = Some(start.as_path());
    while let Some(dir) = current {
        if dir.join("HEAD").is_file() && dir.join("objects").is_dir() {
            let workdir = dir.parent().ok_or_else(|| {
                GitAiError::Generic(format!("Git directory has no parent: {}", dir.display()))
            })?;
            return Ok(DiscoveredRepositoryPaths {
                command_root: dir.to_path_buf(),
                workdir: workdir.to_path_buf(),
                git_dir: dir.to_path_buf(),
                git_common_dir: dir.to_path_buf(),
            });
        }
        current = dir.parent();
    }

    Err(GitAiError::Generic(format!(
        "No git repository found for path without exec: {}",
        path.display()
    )))
}

/// The effective git config for a repository given its git dir and common
/// dir (globals, `<common>/config`, `config.worktree`, environment), read
/// without spawning git. Callers that already know the git dir (the daemon's
/// family registry) use this instead of discovery from a path, which would
/// climb out of a submodule's `.git/modules/<name>` into the superproject.
pub(crate) fn git_config_file_for_repo_paths(
    git_dir: &Path,
    git_common_dir: &Path,
) -> Result<gix_config::File<'static>, GitAiError> {
    let mut config =
        gix_config::File::from_globals().map_err(|e| GitAiError::GixError(e.to_string()))?;

    let home = dirs::home_dir();
    let options = gix_config::file::init::Options {
        includes: gix_config::file::includes::Options::follow(
            gix_config::path::interpolate::Context {
                home_dir: home.as_deref(),
                ..Default::default()
            },
            gix_config::file::includes::conditional::Context {
                git_dir: Some(git_dir),
                branch_name: None,
            },
        ),
        ..Default::default()
    };

    config
        .resolve_includes(options)
        .map_err(|e| GitAiError::GixError(e.to_string()))?;

    let local_config_path = git_common_dir.join("config");
    let local_config =
        Repository::load_optional_config_file(&local_config_path, gix_config::Source::Local)?;
    let worktree_config_enabled = local_config
        .as_ref()
        .and_then(|cfg| cfg.boolean("extensions.worktreeConfig"))
        .and_then(Result::ok)
        .unwrap_or(false);

    if let Some(mut local_config) = local_config {
        local_config
            .resolve_includes(options)
            .map_err(|e| GitAiError::GixError(e.to_string()))?;
        config.append(local_config);
    }

    if worktree_config_enabled {
        let worktree_config_path = git_dir.join("config.worktree");
        if let Some(mut worktree_config) = Repository::load_optional_config_file(
            &worktree_config_path,
            gix_config::Source::Worktree,
        )? {
            worktree_config
                .resolve_includes(options)
                .map_err(|e| GitAiError::GixError(e.to_string()))?;
            config.append(worktree_config);
        }
    }

    config.append(
        gix_config::File::from_environment_overrides()
            .map_err(|e| GitAiError::GixError(e.to_string()))?,
    );

    Ok(config)
}

pub fn config_get_str_for_path_no_git_exec(
    path: &Path,
    key: &str,
) -> Result<Option<String>, GitAiError> {
    let paths = discover_repository_paths_no_git_exec(path)?;
    git_config_file_for_repo_paths(&paths.git_dir, &paths.git_common_dir)
        .map(|cfg| cfg.string(key).map(|cow| cow.to_string()))
}

#[allow(dead_code)]
pub fn from_bare_repository(git_dir: &Path) -> Result<Repository, GitAiError> {
    let workdir = git_dir
        .parent()
        .ok_or_else(|| GitAiError::Generic("Git directory has no parent".to_string()))?
        .to_path_buf();
    let global_args = vec!["-C".to_string(), git_dir.to_string_lossy().to_string()];

    let canonical_workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.clone());

    Ok(Repository {
        global_args,
        git_dir: git_dir.to_path_buf(),
        git_common_dir: git_dir.to_path_buf(),
        workdir,
        canonical_workdir,
        cached_author_identity: std::sync::OnceLock::new(),
    })
}

fn repository_from_discovered_paths(
    command_root: &Path,
    workdir: &Path,
    git_dir: &Path,
    git_common_dir: &Path,
) -> Result<Repository, GitAiError> {
    if !git_dir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Git directory does not exist: {}",
            git_dir.display()
        )));
    }
    if !git_common_dir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Git common directory does not exist: {}",
            git_common_dir.display()
        )));
    }
    if !workdir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Work directory does not exist: {}",
            workdir.display()
        )));
    }

    let canonical_workdir = workdir.canonicalize().map_err(|e| {
        GitAiError::Generic(format!(
            "Failed to canonicalize working directory {}: {}",
            workdir.display(),
            e
        ))
    })?;

    Ok(Repository {
        global_args: vec!["-C".to_string(), command_root.to_string_lossy().to_string()],
        git_dir: git_dir.to_path_buf(),
        git_common_dir: git_common_dir.to_path_buf(),
        workdir: workdir.to_path_buf(),
        canonical_workdir,
        cached_author_identity: std::sync::OnceLock::new(),
    })
}

pub fn discover_repository_in_path_no_git_exec(path: &Path) -> Result<Repository, GitAiError> {
    let paths = discover_repository_paths_no_git_exec(path)?;
    repository_from_discovered_paths(
        &paths.command_root,
        &paths.workdir,
        &paths.git_dir,
        &paths.git_common_dir,
    )
}

/// Check if any directory between `workdir` and `file_path` contains a `.git`
/// entry that represents a **separate** git repository boundary.
///
/// `.git` directories (nested independent repos) and `.git` files that point
/// to a *linked worktree* (i.e., `gitdir: .../worktrees/…`) are treated as
/// boundaries — a file inside such a directory belongs to a different repo.
///
/// `.git` files that point to a *submodule* (i.e., `gitdir: .git/modules/…`)
/// are intentionally transparent: the parent repo tracks the submodule's
/// files, so they should still be considered part of the parent's workdir.
fn has_intervening_git_dir(file_path: &Path, workdir: &Path) -> bool {
    let Ok(relative) = file_path.strip_prefix(workdir) else {
        return false;
    };

    // Walk parent directories of the relative path (excluding the file itself
    // and the empty path). For "subrepo/src/file.ts" we check:
    //   workdir/subrepo/src/.git
    //   workdir/subrepo/.git
    let mut current = relative;
    while let Some(parent) = current.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        let potential_git = workdir.join(parent).join(".git");
        if is_valid_git_dir(&potential_git) {
            // A valid .git directory indicates a separate independent repo.
            return true;
        }
        if potential_git.is_file() {
            // A .git file is either a submodule pointer or a linked-worktree
            // pointer.  Only linked worktrees (gitdir points to …/worktrees/…)
            // represent a separate working-tree boundary; submodule pointers
            // (gitdir points to …/modules/…) are transparent to the parent.
            if is_linked_worktree_git_file(&potential_git) {
                return true;
            }
        }
        current = parent;
    }
    false
}

/// Returns `true` if `git_file` is a `.git` file that points to a linked
/// worktree (i.e., the `gitdir:` target path contains `/worktrees/`).
fn is_linked_worktree_git_file(git_file: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(git_file) else {
        return false;
    };
    // Format: "gitdir: <path>\n"
    let Some(gitdir) = contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
    else {
        return false;
    };
    // A linked worktree's gitdir resolves to something like
    // `/repo/.git/worktrees/<name>`.  A submodule's gitdir looks like
    // `../.git/modules/<name>`.
    gitdir.contains("/.git/worktrees/")
}

pub fn find_repository_in_path(path: &str) -> Result<Repository, GitAiError> {
    let global_args = vec!["-C".to_string(), path.to_string()];
    find_repository(&global_args)
}

/// Find the git repository that contains the given file path by walking up the directory tree.
///
/// This function is useful when working with multi-repository workspaces where the workspace
/// root itself may not be a git repository, but contains multiple independent git repositories.
///
/// # Arguments
///  * `file_path` - Absolute path to a file
///  * `workspace_root` - Optional workspace root path. If provided, the search will stop at this
///    boundary to avoid finding repositories outside the workspace.
///
/// # Returns
/// * `Ok(Repository)` - The repository containing the file
/// * `Err(GitAiError)` - If no repository is found or other errors occur
pub fn find_repository_for_file(
    file_path: &str,
    workspace_root: Option<&str>,
) -> Result<Repository, GitAiError> {
    let file_path = PathBuf::from(file_path);

    // Get the directory containing the file (or the path itself if it's a directory)
    let start_dir = if file_path.is_dir() {
        file_path.clone()
    } else {
        file_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| file_path.clone())
    };

    // Canonicalize paths for consistent comparison
    let start_dir = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.clone());

    let workspace_boundary = workspace_root.map(|root| {
        PathBuf::from(root)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(root))
    });

    // Walk up the directory tree looking for a .git directory
    let mut current_dir = Some(start_dir.as_path());

    while let Some(dir) = current_dir {
        // Check if we've reached the workspace boundary
        if let Some(ref boundary) = workspace_boundary {
            // Stop if we've gone above the workspace root
            if !dir.starts_with(boundary) && dir != boundary.as_path() {
                break;
            }
        }

        // Check for .git directory or file (file for submodules/worktrees)
        let git_path = dir.join(".git");
        let git_path_metadata = std::fs::metadata(&git_path).ok();
        let git_path_is_file = git_path_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.is_file());
        let git_path_is_dir = git_path_metadata.is_some_and(|metadata| metadata.is_dir());
        if git_path_is_file || (git_path_is_dir && is_valid_git_dir(&git_path)) {
            // Found a .git - but we need to check if this is a submodule
            // Submodules have a .git file (not directory) that points to the parent's .git/modules
            if git_path_is_file {
                // This is a submodule - read the file to check if it points to modules/
                if let Ok(content) = std::fs::read_to_string(&git_path)
                    && content.contains("gitdir:")
                    && content.contains("/modules/")
                {
                    // This is a submodule, skip it and continue searching up
                    current_dir = dir.parent();
                    continue;
                }
            }

            // Found a real git repository, use find_repository_in_path
            return find_repository_in_path(&dir.to_string_lossy());
        }

        current_dir = dir.parent();
    }

    Err(GitAiError::Generic(format!(
        "No git repository found for file: {}",
        file_path.display()
    )))
}

/// Group edited file paths by their containing git repository.
///
/// This function takes a list of file paths and groups them by the git repository
/// they belong to. Files that don't belong to any repository are collected separately.
///
/// # Arguments
/// * `file_paths` - List of absolute file paths to group
/// * `workspace_root` - Optional workspace root to limit repository detection
///
/// # Returns
/// A tuple of:
/// * `HashMap<PathBuf, (Repository, Vec<String>)>` - Map of repo root to (repo, file paths)
/// * `Vec<String>` - Files that couldn't be associated with any repository
#[allow(clippy::type_complexity)]
pub fn group_files_by_repository(
    file_paths: &[String],
    workspace_root: Option<&str>,
) -> (HashMap<PathBuf, (Repository, Vec<String>)>, Vec<String>) {
    let mut repo_files: HashMap<PathBuf, (Repository, Vec<String>)> = HashMap::new();
    let mut orphan_files: Vec<String> = Vec::new();

    for file_path in file_paths {
        match find_repository_for_file(file_path, workspace_root) {
            Ok(repo) => {
                let workdir = match repo.workdir() {
                    Ok(dir) => dir,
                    Err(_) => {
                        orphan_files.push(file_path.clone());
                        continue;
                    }
                };

                repo_files
                    .entry(workdir.clone())
                    .or_insert_with(|| (repo, Vec::new()))
                    .1
                    .push(file_path.clone());
            }
            Err(_) => {
                orphan_files.push(file_path.clone());
            }
        }
    }

    (repo_files, orphan_files)
}

/// Helper to execute a git command
pub fn exec_git(args: &[String]) -> Result<Output, GitAiError> {
    exec_git_with_profile(args, InternalGitProfile::General)
}

/// Helper to execute a git command and return output regardless of exit status.
/// Callers that need success-only behavior should use `exec_git*`.
pub fn exec_git_allow_nonzero(args: &[String]) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile(args, InternalGitProfile::General)
}

/// Helper to execute a git command with an explicit internal profile and return output
/// regardless of exit status.
pub fn exec_git_allow_nonzero_with_profile(
    args: &[String],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile_and_env(args, profile, &[])
}

pub fn exec_git_allow_nonzero_with_env(
    args: &[String],
    envs: &[(&str, &OsStr)],
) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile_and_env(args, InternalGitProfile::General, envs)
}

#[cfg(feature = "test-support")]
fn spawn_probe_log(effective_args: &[String]) {
    let Ok(path) = std::env::var("GIT_AI_SPAWN_LOG") else {
        return;
    };
    let sub = effective_args
        .iter()
        .find(|a| !a.starts_with('-') && !a.contains('=') && !a.contains('/') && !a.contains('\\'))
        .cloned()
        .unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", sub);
    }
}

#[cfg(not(feature = "test-support"))]
#[inline]
fn spawn_probe_log(_effective_args: &[String]) {}

fn exec_git_allow_nonzero_with_profile_and_env(
    args: &[String],
    profile: InternalGitProfile,
    envs: &[(&str, &OsStr)],
) -> Result<Output, GitAiError> {
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    spawn_probe_log(&effective_args);
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args);
    apply_internal_git_machine_env(&mut cmd);
    for (key, value) in envs {
        cmd.env(key, value);
    }

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.output().map_err(GitAiError::IoError)
}

/// Spawn a git command with stdout piped and stderr inherited.
///
/// This is used by streaming consumers that cannot call `exec_git*` without
/// buffering all stdout in memory.
pub fn spawn_git_stdout(args: &[String]) -> Result<Child, GitAiError> {
    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    apply_internal_git_machine_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.spawn().map_err(GitAiError::IoError)
}

/// Spawn a git command with stdin/stdout/stderr inherited from git-ai.
///
/// This is used when a command intentionally delegates rendering and paging
/// behavior to git instead of consuming output internally.
pub fn spawn_git_passthrough(args: &[String]) -> Result<Child, GitAiError> {
    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    apply_internal_git_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.spawn().map_err(GitAiError::IoError)
}

pub(crate) const INTERNAL_GIT_ENV_REMOVE: &[&str] = &[
    "GIT_EXTERNAL_DIFF",
    "GIT_DIFF_OPTS",
    "GIT_TRACE",
    "GIT_TRACE2_BRIEF",
    "GIT_TRACE2_CONFIG_PARAMS",
    "GIT_TRACE2_ENV_VARS",
    "GIT_TRACE2_EVENT_NESTING",
    "GIT_TRACE2_PARENT_NAME",
    "GIT_TRACE2_PARENT_SID",
];

pub(crate) const INTERNAL_GIT_ENV_SET: &[(&str, &str)] = &[
    ("GIT_TRACE2", "0"),
    ("GIT_TRACE2_EVENT", "0"),
    ("GIT_TRACE2_PERF", "0"),
];

pub(crate) fn apply_internal_git_env(cmd: &mut Command) {
    for key in INTERNAL_GIT_ENV_REMOVE {
        cmd.env_remove(key);
    }
    for (key, value) in INTERNAL_GIT_ENV_SET {
        cmd.env(key, value);
    }
}

/// Normalize Git commands whose output is consumed by git-ai.
///
/// Keep this separate from `apply_internal_git_env`: passthrough commands render
/// directly to the user's terminal and should continue to honor the user's locale.
pub(crate) fn apply_internal_git_machine_env(cmd: &mut Command) {
    apply_internal_git_env(cmd);
    cmd.env("LC_ALL", "C");
}

/// Helper to execute a git command with an explicit internal profile.
pub fn exec_git_with_profile(
    args: &[String],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    let output = exec_git_allow_nonzero_with_profile(args, profile)?;

    if !output.status.success() {
        let code = output.status.code();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(GitAiError::GitCliError {
            code,
            stderr,
            args: effective_args,
        });
    }

    Ok(output)
}

/// Helper to execute a git command with data provided on stdin
pub fn exec_git_stdin(args: &[String], stdin_data: &[u8]) -> Result<Output, GitAiError> {
    exec_git_stdin_with_profile(args, stdin_data, InternalGitProfile::General)
}

/// Spawn a fully-piped git child for `effective_args` and start a thread
/// writing `stdin_data` to it. Writing stdin in a separate thread avoids
/// deadlock: if we wrote all stdin before reading stdout, the child's stdout
/// pipe buffer could fill up, causing the child to block on write, which
/// prevents it from consuming more stdin, which would block our write_all.
type StdinWriterHandle = std::thread::JoinHandle<std::io::Result<()>>;

fn spawn_git_piped(effective_args: &[String]) -> Result<Child, GitAiError> {
    spawn_probe_log(effective_args);
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(effective_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_internal_git_machine_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.spawn().map_err(GitAiError::IoError)
}

fn spawn_git_stdin_piped(
    effective_args: &[String],
    stdin_data: &[u8],
) -> Result<(Child, Option<StdinWriterHandle>), GitAiError> {
    let mut child = spawn_git_piped(effective_args)?;
    let stdin_handle = child.stdin.take().map(|mut stdin| {
        let data = stdin_data.to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            stdin.write_all(&data)
        })
    });

    Ok((child, stdin_handle))
}

/// Execute Git with batched stdin and consume stdout one line at a time.
/// Strips a trailing newline and optional preceding carriage return.
pub fn exec_git_stdin_streaming(
    args: &[String],
    stdin_data: &[u8],
    mut on_line: impl FnMut(&str),
) -> Result<(), GitAiError> {
    use std::io::BufRead;

    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let (mut child, stdin_handle) = spawn_git_stdin_piped(&effective_args, stdin_data)?;

    // Drain stderr concurrently so the child can never block on a full stderr
    // pipe while we are still reading stdout.
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });

    let stdout = child.stdout.take().expect("child stdout is piped");
    let mut reader = std::io::BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    let read_result = loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break Ok(()),
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                }
                on_line(&String::from_utf8_lossy(&buf));
            }
            Err(e) => break Err(e),
        }
    };
    if let Err(e) = read_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(GitAiError::IoError(e));
    }

    let status = child.wait().map_err(GitAiError::IoError)?;

    if let Some(handle) = stdin_handle
        && let Err(e) = handle.join().expect("stdin writer thread panicked")
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(GitAiError::IoError(e));
    }

    if !status.success() {
        let stderr_bytes = stderr_handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        return Err(GitAiError::GitCliError {
            code: status.code(),
            stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
            args: effective_args,
        });
    }

    Ok(())
}

/// Helper to execute a git command with data provided on stdin and an explicit profile.
pub fn exec_git_stdin_with_profile(
    args: &[String],
    stdin_data: &[u8],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    // TODO Make sure to handle process signals, etc.
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    let (child, stdin_handle) = spawn_git_stdin_piped(&effective_args, stdin_data)?;

    let output = child.wait_with_output().map_err(GitAiError::IoError)?;

    if let Some(handle) = stdin_handle
        && let Err(e) = handle.join().expect("stdin writer thread panicked")
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(GitAiError::IoError(e));
    }

    if !output.status.success() {
        let code = output.status.code();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(GitAiError::GitCliError {
            code,
            stderr,
            args: effective_args,
        });
    }

    Ok(output)
}

/// Parse git version string (e.g., "git version 2.39.3 (Apple Git-146)") to extract major, minor, patch.
/// Returns None if the version cannot be parsed.
#[doc(hidden)]
pub fn parse_git_version(version_str: &str) -> Option<(u32, u32, u32)> {
    // Expected format: "git version X.Y.Z" or "git version X.Y.Z.windows.N" etc.
    let version_str = version_str.trim();
    let parts: Vec<&str> = version_str.split_whitespace().collect();

    // Find the version number part (usually the 3rd element)
    let version_part = parts.get(2)?;

    // Parse version like "2.39.3" or "2.39.3.windows.1"
    let version_nums: Vec<&str> = version_part.split('.').collect();
    if version_nums.len() < 2 {
        return None;
    }

    let major = version_nums.first()?.parse::<u32>().ok()?;
    let minor = version_nums.get(1)?.parse::<u32>().ok()?;
    let patch = version_nums
        .get(2)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn explicit_command_env(cmd: &Command, key: &str) -> Option<Option<String>> {
        cmd.get_envs()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.map(|v| v.to_string_lossy().to_string()))
    }

    #[test]
    fn internal_git_env_disables_trace2_targets() {
        let mut cmd = Command::new("git");
        for key in INTERNAL_GIT_ENV_REMOVE {
            cmd.env(key, "inherited");
        }
        for (key, _) in INTERNAL_GIT_ENV_SET {
            cmd.env(key, "inherited");
        }

        apply_internal_git_env(&mut cmd);

        for key in INTERNAL_GIT_ENV_REMOVE {
            assert_eq!(explicit_command_env(&cmd, key), Some(None));
        }
        for (key, value) in INTERNAL_GIT_ENV_SET {
            assert_eq!(
                explicit_command_env(&cmd, key),
                Some(Some((*value).to_string()))
            );
        }
    }

    #[test]
    fn machine_consumed_internal_git_forces_c_locale() {
        let mut cmd = Command::new("git");

        apply_internal_git_machine_env(&mut cmd);

        assert_eq!(
            explicit_command_env(&cmd, "LC_ALL"),
            Some(Some("C".to_string()))
        );
    }

    #[test]
    fn user_facing_internal_git_does_not_override_locale() {
        let mut cmd = Command::new("git");

        apply_internal_git_env(&mut cmd);

        assert_eq!(explicit_command_env(&cmd, "LC_ALL"), None);
    }

    #[test]
    fn test_parse_git_version_standard() {
        // Standard git version format
        assert_eq!(parse_git_version("git version 2.39.3"), Some((2, 39, 3)));
        assert_eq!(parse_git_version("git version 2.23.0"), Some((2, 23, 0)));
        assert_eq!(parse_git_version("git version 1.8.5"), Some((1, 8, 5)));
    }

    #[test]
    fn test_parse_git_version_apple_git() {
        // macOS Apple Git format
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn test_parse_git_version_windows() {
        // Windows git format
        assert_eq!(
            parse_git_version("git version 2.42.0.windows.2"),
            Some((2, 42, 0))
        );
    }

    #[test]
    fn test_parse_git_version_no_patch() {
        // Version without patch number
        assert_eq!(parse_git_version("git version 2.39"), Some((2, 39, 0)));
    }

    #[test]
    fn test_parse_git_version_with_newline() {
        // Version string with trailing newline
        assert_eq!(parse_git_version("git version 2.39.3\n"), Some((2, 39, 3)));
    }

    #[test]
    fn test_parse_git_version_invalid() {
        // Invalid formats should return None
        assert_eq!(parse_git_version(""), None);
        assert_eq!(parse_git_version("not a version"), None);
        assert_eq!(parse_git_version("git version"), None);
        assert_eq!(parse_git_version("git version x.y.z"), None);
    }

    #[test]
    fn disable_internal_git_hooks_guard_applies_to_spawned_threads() {
        let args = vec!["status".to_string()];
        let _guard = disable_internal_git_hooks();

        let spawned_args = args.clone();
        let forwarded =
            std::thread::spawn(move || args_with_disabled_hooks_if_needed(&spawned_args))
                .join()
                .expect("thread should join");

        assert_eq!(forwarded[0], "-c");
        assert!(forwarded[1].starts_with("core.hooksPath="));
    }

    #[test]
    fn patch_profile_applies_canonical_machine_parse_flags() {
        let args = vec!["diff".to_string(), "HEAD^".to_string(), "HEAD".to_string()];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::PatchParse);

        assert!(rewritten.iter().any(|arg| arg == "--no-ext-diff"));
        assert!(rewritten.iter().any(|arg| arg == "--no-textconv"));
        assert!(rewritten.iter().any(|arg| arg == "--src-prefix=a/"));
        assert!(rewritten.iter().any(|arg| arg == "--dst-prefix=b/"));
        assert!(rewritten.iter().any(|arg| arg == "--no-relative"));
        assert!(rewritten.iter().any(|arg| arg == "--no-color"));
        assert!(
            rewritten
                .iter()
                .any(|arg| arg == "--diff-algorithm=default")
        );
        assert!(rewritten.iter().any(|arg| arg == "--indent-heuristic"));
        assert!(rewritten.iter().any(|arg| arg == "--inter-hunk-context=0"));
    }

    #[test]
    fn numstat_profile_disables_renames_and_external_renderers() {
        let args = vec![
            "diff".to_string(),
            "--numstat".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::NumstatParse);
        assert!(rewritten.iter().any(|arg| arg == "--no-ext-diff"));
        assert!(rewritten.iter().any(|arg| arg == "--no-textconv"));
        assert!(rewritten.iter().any(|arg| arg == "--no-color"));
        assert!(rewritten.iter().any(|arg| arg == "--no-relative"));
        assert!(rewritten.iter().any(|arg| arg == "--no-renames"));
    }

    #[test]
    fn numstat_profile_strips_short_rename_and_copy_flags() {
        let args = vec![
            "diff".to_string(),
            "--numstat".to_string(),
            "-M90%".to_string(),
            "-C".to_string(),
            "-C75%".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::NumstatParse);
        assert!(!rewritten.iter().any(|arg| arg == "-C"));
        assert!(!rewritten.iter().any(|arg| arg.starts_with("-M")));
        assert!(!rewritten.iter().any(|arg| arg.starts_with("-C")));
        assert!(rewritten.iter().any(|arg| arg == "--no-renames"));
    }

    #[test]
    fn general_profile_is_noop() {
        let args = vec!["status".to_string(), "--porcelain=v2".to_string()];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::General);
        assert_eq!(rewritten, args);
    }

    #[test]
    fn patch_profile_strips_conflicting_ext_diff_and_color_flags() {
        let args = vec![
            "diff".to_string(),
            "--ext-diff".to_string(),
            "--color=always".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::PatchParse);

        assert!(rewritten.iter().any(|arg| arg == "--no-ext-diff"));
        assert!(!rewritten.iter().any(|arg| arg == "--ext-diff"));
        assert!(!rewritten.iter().any(|arg| arg.starts_with("--color")));
        assert!(rewritten.iter().any(|arg| arg == "--no-color"));
    }

    #[test]
    fn patch_profile_strips_split_prefix_args() {
        let args = vec![
            "diff".to_string(),
            "--src-prefix".to_string(),
            "SRC/".to_string(),
            "--dst-prefix".to_string(),
            "DST/".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::PatchParse);

        assert!(!rewritten.iter().any(|arg| arg == "--src-prefix"));
        assert!(!rewritten.iter().any(|arg| arg == "--dst-prefix"));
        assert!(!rewritten.iter().any(|arg| arg == "SRC/"));
        assert!(!rewritten.iter().any(|arg| arg == "DST/"));
        assert!(rewritten.iter().any(|arg| arg == "--src-prefix=a/"));
        assert!(rewritten.iter().any(|arg| arg == "--dst-prefix=b/"));
    }

    #[test]
    fn profile_rewrite_does_not_strip_pathspec_tokens_after_double_dash() {
        let args = vec![
            "diff".to_string(),
            "--color=always".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
            "--".to_string(),
            "--color".to_string(),
            "--relative".to_string(),
            "file.txt".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::PatchParse);
        let separator = rewritten
            .iter()
            .position(|arg| arg == "--")
            .expect("rewritten args should keep pathspec separator");
        assert_eq!(
            rewritten[separator + 1..],
            [
                "--color".to_string(),
                "--relative".to_string(),
                "file.txt".to_string()
            ]
        );
    }

    #[test]
    fn raw_diff_profile_keeps_rename_flags_untouched() {
        let args = vec![
            "diff".to_string(),
            "--raw".to_string(),
            "-z".to_string(),
            "-M".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::RawDiffParse);
        assert!(rewritten.iter().any(|arg| arg == "-M"));
        assert!(rewritten.iter().any(|arg| arg == "--no-ext-diff"));
        assert!(rewritten.iter().any(|arg| arg == "--no-textconv"));
        assert!(rewritten.iter().any(|arg| arg == "--no-color"));
        assert!(rewritten.iter().any(|arg| arg == "--no-relative"));
    }
}
