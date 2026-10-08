#!/usr/bin/env bash
# Feed a content-free native session signal through the installed daemon.
# Usage: test-synthetic-checkpoint.sh <agent> [repo-dir]
set -euo pipefail
AGENT="${1:?Usage: $0 <agent> [repo-dir]}"
REPO_DIR="${2:-/tmp/test-repo}"
RESULTS_DIR="${RESULTS_DIR:-/tmp/test-results}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$RESULTS_DIR"
LOG="$RESULTS_DIR/synthetic-checkpoint-${AGENT}.txt"
SESSION_ID="synthetic-${AGENT}-$(date +%s)"
printf '%s\n' "$SESSION_ID" > "$RESULTS_DIR/synthetic-session-${AGENT}.txt"
cd "$REPO_DIR"
REPO_DIR="$(pwd -P)"
TEST_FILE="agent-test-${AGENT}.txt"
printf 'Hello from %s synthetic test\n' "$AGENT" > "$TEST_FILE"
git add "$TEST_FILE"
python3 - "$AGENT" "$REPO_DIR" "$TEST_FILE" "$SESSION_ID" <<'PAYLOAD' | git-ai checkpoint agent-v1 --hook-input stdin
import json, sys
agent, repo, path, session = sys.argv[1:]
print(json.dumps({"type":"ai_agent", "repo_working_dir":repo,
                  "edited_filepaths":[path], "agent_name":agent,
                  "conversation_id":session}))
PAYLOAD
git commit -m "Synthetic $AGENT checkpoint test"
git-ai await --timeout 30
python3 "$SCRIPT_DIR/verify-commit-links.py" --agent "$AGENT" --repo "$REPO_DIR" \
  --session-id-file "$RESULTS_DIR/synthetic-session-${AGENT}.txt" | tee "$LOG"
