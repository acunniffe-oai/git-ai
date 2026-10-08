#!/usr/bin/env python3
"""Inspect local commit links after the caller has run `git-ai await`."""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess


def normalized_path(value):
    return os.path.normcase(str(Path(value).resolve()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--agent", required=True)
    parser.add_argument("--session-id-file")
    args = parser.parse_args()
    # A single read-only Git command, independent of the number of links.
    output = subprocess.check_output(
        ["git", "-C", args.repo, "rev-parse", "--path-format=absolute", "--git-common-dir", "HEAD"],
        text=True,
    ).splitlines()
    family, commit = output
    if os.environ.get("GIT_AI_TEST_OTEL_DB_PATH"):
        database = Path(os.environ["GIT_AI_TEST_OTEL_DB_PATH"])
    elif os.environ.get("GIT_AI_TEST_DB_PATH"):
        database = Path(os.environ["GIT_AI_TEST_DB_PATH"]).with_suffix(".otel-db")
    else:
        database = Path.home() / ".git-ai/internal/otel-db"
    if not database.is_file():
        raise SystemExit(f"FAIL: local link database is missing: {database}")
    with sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True) as connection:
        rows = connection.execute(
            "SELECT family, agent, session_id FROM commit_sessions WHERE commit_sha = ?",
            (commit,),
        ).fetchall()
    links = sorted({(agent, session) for repo, agent, session in rows
                    if normalized_path(repo) == normalized_path(family)})
    expected_agent = {"claude": "claude-code", "cursor-background": "cursor"}.get(args.agent, args.agent)
    matches = [(agent, session) for agent, session in links if agent == expected_agent]
    if not matches:
        raise SystemExit(f"FAIL: {commit} has no links for {expected_agent}; links={links}")
    if any(not session.strip() or session == "unknown" or session.startswith("DEVIN_SESSION")
           for _, session in matches):
        raise SystemExit("FAIL: commit link contains a missing or placeholder external session ID")
    if args.session_id_file:
        expected_id = Path(args.session_id_file).read_text(encoding="utf-8-sig").strip()
        if matches != [(expected_agent, expected_id)]:
            raise SystemExit(f"FAIL: original session ID was not preserved: {matches}")
    print(json.dumps({"commit": commit, "repository": family,
                      "links": [{"agent": agent, "session_id": session} for agent, session in links]}, indent=2))
    print(f"PASS: {commit} links to {len(matches)} native {expected_agent} session(s)")


if __name__ == "__main__":
    main()
