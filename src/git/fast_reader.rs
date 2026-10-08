use std::fs;
use std::path::Path;

fn is_valid_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadKind {
    Symbolic(String),
    Detached(String),
}

/// Fast worktree-aware ref resolution by reading .git/ files directly.
///
/// Handles loose refs, packed-refs, and symbolic refs (one level of indirection).
/// Returns None when the fast path cannot resolve (caller falls back to git CLI).
pub struct FastRefReader<'a> {
    git_dir: &'a Path,
    common_dir: &'a Path,
}

impl<'a> FastRefReader<'a> {
    pub fn new(git_dir: &'a Path, common_dir: &'a Path) -> Self {
        Self {
            git_dir,
            common_dir,
        }
    }

    /// Read HEAD and determine if it's symbolic or detached.
    ///
    /// Returns `Some(Symbolic(...))` for symbolic HEAD, `Some(Detached(...))` for
    /// detached HEAD, or `None` if HEAD can't be read or has unexpected format.
    pub fn try_read_head(&self) -> Option<HeadKind> {
        let head_path = self.git_dir.join("HEAD");
        let content = fs::read_to_string(&head_path).ok()?;
        let trimmed = content.trim();

        if let Some(refname) = trimmed.strip_prefix("ref: ") {
            let refname = refname.trim();
            if !refname.is_empty() {
                return Some(HeadKind::Symbolic(refname.to_string()));
            }
        }

        if is_valid_git_oid(trimmed) {
            return Some(HeadKind::Detached(trimmed.to_string()));
        }

        None
    }

    /// Resolve a refname (e.g., "refs/heads/main") to its OID.
    ///
    /// Checks loose refs in both common_dir and git_dir, then packed-refs.
    /// Handles one level of symbolic ref indirection.
    /// Returns None if the ref cannot be resolved via filesystem.
    pub fn try_resolve_ref(&self, refname: &str) -> Option<String> {
        if refname == "HEAD" {
            match self.try_read_head()? {
                HeadKind::Detached(oid) => return Some(oid),
                HeadKind::Symbolic(target) => return self.try_resolve_ref(&target),
            }
        }

        // Check loose refs: common_dir first, then git_dir
        for base in [self.common_dir, self.git_dir] {
            let path = base.join(refname);
            if let Ok(contents) = fs::read_to_string(&path) {
                let candidate = contents.trim();
                if is_valid_git_oid(candidate) {
                    return Some(candidate.to_string());
                }
                // One level of symbolic ref indirection
                if let Some(target) = candidate.strip_prefix("ref: ") {
                    let target = target.trim();
                    return self.resolve_without_recursion(target);
                }
            }
        }

        // Check packed-refs in common_dir
        self.try_packed_ref(refname)
    }

    fn resolve_without_recursion(&self, refname: &str) -> Option<String> {
        for base in [self.common_dir, self.git_dir] {
            let path = base.join(refname);
            if let Ok(contents) = fs::read_to_string(&path) {
                let candidate = contents.trim();
                if is_valid_git_oid(candidate) {
                    return Some(candidate.to_string());
                }
            }
        }
        self.try_packed_ref(refname)
    }

    fn try_packed_ref(&self, refname: &str) -> Option<String> {
        let packed_refs_path = self.common_dir.join("packed-refs");
        let contents = fs::read_to_string(packed_refs_path).ok()?;

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let oid = parts.next()?;
            let name = parts.next()?;
            if name == refname && is_valid_git_oid(oid) {
                return Some(oid.to_string());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_git_dir() -> TempDir {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("refs/heads")).unwrap();
        fs::create_dir_all(temp.path().join("objects")).unwrap();
        temp
    }

    #[test]
    fn test_read_head_symbolic_ref() {
        let temp = setup_git_dir();
        fs::write(temp.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let reader = FastRefReader::new(temp.path(), temp.path());
        let result = reader.try_read_head();
        assert_eq!(
            result,
            Some(HeadKind::Symbolic("refs/heads/main".to_string()))
        );
    }

    #[test]
    fn test_read_head_detached() {
        let temp = setup_git_dir();
        let sha = "abc123def456789012345678901234567890abcd";
        fs::write(temp.path().join("HEAD"), format!("{}\n", sha)).unwrap();

        let reader = FastRefReader::new(temp.path(), temp.path());
        let result = reader.try_read_head();
        assert_eq!(result, Some(HeadKind::Detached(sha.to_string())));
    }

    #[test]
    fn test_read_head_invalid_format_returns_none() {
        let temp = setup_git_dir();
        fs::write(temp.path().join("HEAD"), "garbage content\n").unwrap();

        let reader = FastRefReader::new(temp.path(), temp.path());
        assert_eq!(reader.try_read_head(), None);
    }

    #[test]
    fn test_read_head_missing_returns_none() {
        let temp = TempDir::new().unwrap();
        let reader = FastRefReader::new(temp.path(), temp.path());
        assert_eq!(reader.try_read_head(), None);
    }

    #[test]
    fn test_resolve_loose_ref() {
        let temp = setup_git_dir();
        let sha = "abc123def456789012345678901234567890abcd";
        fs::write(temp.path().join("refs/heads/main"), format!("{}\n", sha)).unwrap();

        let reader = FastRefReader::new(temp.path(), temp.path());
        let result = reader.try_resolve_ref("refs/heads/main");
        assert_eq!(result, Some(sha.to_string()));
    }

    #[test]
    fn test_resolve_packed_ref() {
        let temp = setup_git_dir();
        let sha = "abc123def456789012345678901234567890abcd";
        let packed_content = format!(
            "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/packed-branch\n",
            sha
        );
        fs::write(temp.path().join("packed-refs"), packed_content).unwrap();

        let reader = FastRefReader::new(temp.path(), temp.path());
        let result = reader.try_resolve_ref("refs/heads/packed-branch");
        assert_eq!(result, Some(sha.to_string()));
    }

    #[test]
    fn test_resolve_ref_not_found() {
        let temp = setup_git_dir();
        let reader = FastRefReader::new(temp.path(), temp.path());
        assert_eq!(reader.try_resolve_ref("refs/heads/nonexistent"), None);
    }

    #[test]
    fn test_resolve_symbolic_ref_indirection() {
        let temp = setup_git_dir();
        let sha = "abc123def456789012345678901234567890abcd";
        fs::create_dir_all(temp.path().join("refs/remotes/origin")).unwrap();
        fs::write(
            temp.path().join("refs/remotes/origin/HEAD"),
            "ref: refs/remotes/origin/main\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("refs/remotes/origin/main"),
            format!("{}\n", sha),
        )
        .unwrap();

        let reader = FastRefReader::new(temp.path(), temp.path());
        let result = reader.try_resolve_ref("refs/remotes/origin/HEAD");
        assert_eq!(result, Some(sha.to_string()));
    }

    #[test]
    fn test_resolve_head_resolves_through_symbolic() {
        let temp = setup_git_dir();
        let sha = "abc123def456789012345678901234567890abcd";
        fs::write(temp.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(temp.path().join("refs/heads/main"), format!("{}\n", sha)).unwrap();

        let reader = FastRefReader::new(temp.path(), temp.path());
        let result = reader.try_resolve_ref("HEAD");
        assert_eq!(result, Some(sha.to_string()));
    }

    #[test]
    fn test_resolve_ref_worktree_common_dir_priority() {
        // Simulate a linked worktree: refs live in common_dir, not git_dir
        let common = setup_git_dir();
        let worktree_git_dir = TempDir::new().unwrap();
        fs::create_dir_all(worktree_git_dir.path()).unwrap();

        let sha = "abc123def456789012345678901234567890abcd";
        fs::write(common.path().join("refs/heads/main"), format!("{}\n", sha)).unwrap();

        let reader = FastRefReader::new(worktree_git_dir.path(), common.path());
        let result = reader.try_resolve_ref("refs/heads/main");
        assert_eq!(result, Some(sha.to_string()));
    }

    #[test]
    fn test_resolve_ref_loose_in_git_dir_over_packed() {
        let temp = setup_git_dir();
        let loose_sha = "1111111111111111111111111111111111111111";
        let packed_sha = "2222222222222222222222222222222222222222";

        fs::write(
            temp.path().join("refs/heads/main"),
            format!("{}\n", loose_sha),
        )
        .unwrap();
        let packed_content = format!("# pack-refs with: peeled\n{} refs/heads/main\n", packed_sha);
        fs::write(temp.path().join("packed-refs"), packed_content).unwrap();

        let reader = FastRefReader::new(temp.path(), temp.path());
        let result = reader.try_resolve_ref("refs/heads/main");
        assert_eq!(result, Some(loose_sha.to_string()));
    }
}
