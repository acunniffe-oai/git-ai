#!/usr/bin/env bash
# Verify a live native agent completed the task and linked its session locally.
set -euo pipefail
AGENT="${1:?Usage: $0 <agent>}"
REPO_DIR="${TEST_REPO_DIR:-/tmp/test-repo}"
RESULTS_DIR="${RESULTS_DIR:-/tmp/test-results}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$RESULTS_DIR"
cd "$REPO_DIR"
REPO_DIR="$(pwd -P)"
[ -f utils/math_utils.py ] || { echo "FAIL: agent did not create utils/math_utils.py"; exit 1; }
grep -q "def fibonacci" utils/math_utils.py || { echo "FAIL: fibonacci implementation missing"; exit 1; }
COMMITS=$(git rev-list HEAD --count)
[ "$COMMITS" -ge 3 ] || { echo "FAIL: agent did not create the expected commit"; exit 1; }
git-ai await --timeout 30
python3 "$SCRIPT_DIR/verify-commit-links.py" --agent "$AGENT" --repo "$REPO_DIR" \
  | tee "$RESULTS_DIR/attribution-${AGENT}.txt"
