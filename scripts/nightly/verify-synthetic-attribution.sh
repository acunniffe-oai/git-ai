#!/usr/bin/env bash
# Verify exact local session-to-commit links from the synthetic checkpoint.
# The script name is retained for existing workflow callers.
set -euo pipefail
AGENT="${1:?Usage: $0 <agent> [repo-dir]}"
REPO_DIR="${2:-/tmp/test-repo}"
RESULTS_DIR="${RESULTS_DIR:-/tmp/test-results}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$RESULTS_DIR"
cd "$REPO_DIR"
REPO_DIR="$(pwd -P)"
git-ai await --timeout 30
python3 "$SCRIPT_DIR/verify-commit-links.py" --agent "$AGENT" --repo "$REPO_DIR" \
  --session-id-file "$RESULTS_DIR/synthetic-session-${AGENT}.txt" \
  | tee "$RESULTS_DIR/synthetic-attribution-${AGENT}.txt"
