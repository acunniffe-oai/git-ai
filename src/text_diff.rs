//! Textual change previews for installer configuration.
use imara_diff::{Algorithm, Diff, InternedInput};

/// Represents a change in a line-based diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineChangeTag {
    /// Line was inserted.
    Insert,
    /// Line was deleted.
    Delete,
    /// Line was unchanged.
    Equal,
}

/// A single line change from a diff.
#[derive(Debug, Clone)]
pub struct LineChange<'a> {
    tag: LineChangeTag,
    value: &'a str,
}

impl<'a> LineChange<'a> {
    /// Returns the tag indicating what kind of change this is.
    pub fn tag(&self) -> &LineChangeTag {
        &self.tag
    }

    /// Returns the line content (including trailing newline if present).
    pub fn value(&self) -> &'a str {
        self.value
    }
}

/// Computes line changes between two strings, similar to `TextDiff::iter_all_changes`.
///
/// Uses imara-diff with Myers algorithm and git-like post-processing.
///
/// # Arguments
/// * `old` - The original string
/// * `new` - The new string
///
/// # Returns
/// A vector of `LineChange` representing each line's change status.
pub fn compute_line_changes<'a>(old: &'a str, new: &'a str) -> Vec<LineChange<'a>> {
    let old_lines: Vec<&str> = split_lines_with_terminators(old);
    let new_lines: Vec<&str> = split_lines_with_terminators(new);

    // Normalize CRLF→LF for comparison so that line-ending differences alone
    // don't cause every line to appear as changed (fixes inflated stats when
    // files switch between CRLF and LF, e.g. on Windows or across editors).
    let old_norm = normalize_line_endings(old);
    let new_norm = normalize_line_endings(new);

    let input = InternedInput::new(old_norm.as_ref(), new_norm.as_ref());
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    diff.postprocess_lines(&input);

    let mut changes = Vec::new();
    let mut old_idx: usize = 0;
    let mut new_idx: usize = 0;

    for hunk in diff.hunks() {
        let hunk_old_start = hunk.before.start as usize;
        let hunk_old_end = hunk.before.end as usize;
        let hunk_new_start = hunk.after.start as usize;
        let hunk_new_end = hunk.after.end as usize;

        // Add equal lines before this hunk
        while old_idx < hunk_old_start && new_idx < hunk_new_start {
            if let Some(line) = new_lines.get(new_idx) {
                changes.push(LineChange {
                    tag: LineChangeTag::Equal,
                    value: line,
                });
            }
            old_idx += 1;
            new_idx += 1;
        }

        // Add deleted lines
        for i in hunk_old_start..hunk_old_end {
            if let Some(line) = old_lines.get(i) {
                changes.push(LineChange {
                    tag: LineChangeTag::Delete,
                    value: line,
                });
            }
        }

        // Add inserted lines
        for i in hunk_new_start..hunk_new_end {
            if let Some(line) = new_lines.get(i) {
                changes.push(LineChange {
                    tag: LineChangeTag::Insert,
                    value: line,
                });
            }
        }

        old_idx = hunk_old_end;
        new_idx = hunk_new_end;
    }

    // Add remaining equal lines after last hunk
    while new_idx < new_lines.len() {
        if let Some(line) = new_lines.get(new_idx) {
            changes.push(LineChange {
                tag: LineChangeTag::Equal,
                value: line,
            });
        }
        new_idx += 1;
    }

    changes
}

/// Normalize line endings: strip `\r` from `\r\n` pairs so that CRLF and LF
/// content compare identically at the line level. Returns a borrowed `Cow` when
/// no `\r` is present (zero-copy fast path).
///
/// Only handles `\r\n` → `\n` (Windows CRLF). Bare `\r` is left unchanged
/// because converting it to `\n` would increase the line count, breaking the
/// index alignment between normalized diff hunks and original line arrays in
/// `compute_line_changes`.
pub(crate) fn normalize_line_endings(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains("\r\n") {
        return std::borrow::Cow::Borrowed(s);
    }
    std::borrow::Cow::Owned(s.replace("\r\n", "\n"))
}

fn split_lines_with_terminators(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;

    for (idx, ch) in s.char_indices() {
        if ch == '\n' {
            lines.push(&s[start..idx + 1]);
            start = idx + 1;
        }
    }

    // Handle last line without trailing newline
    if start < s.len() {
        lines.push(&s[start..]);
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_compute_line_changes() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nmodified\nline3\n";

        let changes = compute_line_changes(old, new);

        let tags: Vec<_> = changes.iter().map(|c| c.tag().clone()).collect();
        assert_eq!(
            tags,
            vec![
                LineChangeTag::Equal,
                LineChangeTag::Delete,
                LineChangeTag::Insert,
                LineChangeTag::Equal,
            ]
        );
    }

    #[test]
    fn test_compute_line_changes_insert_only() {
        let old = "line1\nline2\n";
        let new = "line1\nline2\nline3\n";

        let changes = compute_line_changes(old, new);

        let tags: Vec<_> = changes.iter().map(|c| c.tag().clone()).collect();
        assert_eq!(
            tags,
            vec![
                LineChangeTag::Equal,
                LineChangeTag::Equal,
                LineChangeTag::Insert,
            ]
        );
    }

    #[test]
    fn test_split_lines_with_terminators() {
        let s = "line1\nline2\nline3";
        let lines = split_lines_with_terminators(s);
        assert_eq!(lines, vec!["line1\n", "line2\n", "line3"]);

        let s_trailing = "line1\nline2\n";
        let lines_trailing = split_lines_with_terminators(s_trailing);
        assert_eq!(lines_trailing, vec!["line1\n", "line2\n"]);
    }

    // ====================================================================
    // CRLF / LF normalization tests
    // ====================================================================

    #[test]
    fn test_normalize_line_endings_does_not_allocate_for_bare_carriage_returns() {
        let normalized = normalize_line_endings("one\rtwo");

        assert!(matches!(normalized, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn test_compute_line_changes_crlf_to_lf_identical_content() {
        // Old file has CRLF, new file has LF. Content is identical otherwise.
        // Should produce NO changes (all Equal).
        let old = "line1\r\nline2\r\nline3\r\n";
        let new = "line1\nline2\nline3\n";

        let changes = compute_line_changes(old, new);

        let tags: Vec<_> = changes.iter().map(|c| c.tag().clone()).collect();
        assert_eq!(
            tags,
            vec![
                LineChangeTag::Equal,
                LineChangeTag::Equal,
                LineChangeTag::Equal,
            ],
            "CRLF→LF conversion with identical content should produce no changes"
        );
    }

    #[test]
    fn test_compute_line_changes_lf_to_crlf_identical_content() {
        // Old file has LF, new file has CRLF. Content is identical otherwise.
        // Should produce NO changes (all Equal).
        let old = "line1\nline2\nline3\n";
        let new = "line1\r\nline2\r\nline3\r\n";

        let changes = compute_line_changes(old, new);

        let tags: Vec<_> = changes.iter().map(|c| c.tag().clone()).collect();
        assert_eq!(
            tags,
            vec![
                LineChangeTag::Equal,
                LineChangeTag::Equal,
                LineChangeTag::Equal,
            ],
            "LF→CRLF conversion with identical content should produce no changes"
        );
    }

    #[test]
    fn test_compute_line_changes_crlf_old_with_real_addition() {
        // Old file has CRLF (100-line-like scenario), new file has LF with real additions.
        // Only the actual new lines should show as Insert.
        let old = "line1\r\nline2\r\nline3\r\n";
        let new = "line1\nline2\nnew_line\nline3\n";

        let changes = compute_line_changes(old, new);

        let tags: Vec<_> = changes.iter().map(|c| c.tag().clone()).collect();
        assert_eq!(
            tags,
            vec![
                LineChangeTag::Equal,
                LineChangeTag::Equal,
                LineChangeTag::Insert,
                LineChangeTag::Equal,
            ],
            "Only the genuinely new line should be an Insert, not CRLF→LF conversions"
        );
    }

    #[test]
    fn test_compute_line_changes_mixed_crlf_with_modification() {
        // Old has CRLF, new has LF. One line is actually modified.
        let old = "line1\r\nline2\r\nline3\r\n";
        let new = "line1\nmodified\nline3\n";

        let changes = compute_line_changes(old, new);

        let tags: Vec<_> = changes.iter().map(|c| c.tag().clone()).collect();
        assert_eq!(
            tags,
            vec![
                LineChangeTag::Equal,
                LineChangeTag::Delete,
                LineChangeTag::Insert,
                LineChangeTag::Equal,
            ],
            "Only the actually-modified line should show as Delete+Insert"
        );
    }

    #[test]
    fn test_compute_line_changes_crlf_large_file_few_additions() {
        // Simulates the user-reported bug: 100-line CRLF file with 5 LF additions.
        // Should show exactly 5 inserts, NOT 105 inserts + 100 deletes.
        let mut old_lines = String::new();
        for i in 1..=10 {
            old_lines.push_str(&format!("line{}\r\n", i));
        }

        let mut new_lines = String::new();
        for i in 1..=10 {
            new_lines.push_str(&format!("line{}\n", i));
        }
        // Add 2 new lines at the end
        new_lines.push_str("new_line_a\n");
        new_lines.push_str("new_line_b\n");

        let changes = compute_line_changes(&old_lines, &new_lines);

        let insert_count = changes
            .iter()
            .filter(|c| *c.tag() == LineChangeTag::Insert)
            .count();
        let delete_count = changes
            .iter()
            .filter(|c| *c.tag() == LineChangeTag::Delete)
            .count();

        assert_eq!(insert_count, 2, "Should have exactly 2 inserts (new lines)");
        assert_eq!(delete_count, 0, "Should have 0 deletes (no lines removed)");
    }

    #[test]
    fn test_split_lines_with_terminators_crlf() {
        // CRLF lines should be split the same way as LF lines
        // (the \r should be treated as part of the line ending, not content)
        let crlf = "line1\r\nline2\r\nline3\r\n";
        let lf = "line1\nline2\nline3\n";

        let crlf_lines = split_lines_with_terminators(crlf);
        let lf_lines = split_lines_with_terminators(lf);

        // After normalization, both should produce the same number of lines
        assert_eq!(
            crlf_lines.len(),
            lf_lines.len(),
            "CRLF and LF content should produce the same number of lines"
        );
    }
}
