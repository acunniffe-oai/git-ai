# Pi support

`git-ai install-hooks` installs a managed Pi extension at:

- `~/.pi/agent/extensions/git-ai.ts`

The extension emits an `after_edit` checkpoint after a successful file edit, and `before_command` / `after_command` checkpoints for shell calls. It uses Pi’s native session ID and records paths without reading edited files or transcripts. It runs:

```bash
git-ai checkpoint pi --hook-input stdin
```

## Override file

Optional user-owned override file:

- `~/.pi/agent/git-ai.override.json`

`install-hooks` does not rewrite that file.

## Override contract

```json
{
  "version": 1,
  "tools": {
    "edit": {
      "kind": "mutating",
      "canonical": "edit",
      "filepath_fields": ["path"]
    },
    "bash": {
      "kind": "ignore"
    }
  }
}
```

### Rules

- tool key = raw Pi tool name
- `kind: "mutating"` requires:
  - `canonical`: `edit|write|replace|rename`
  - `filepath_fields`: top-level input field names used to extract touched paths
- `kind: "ignore"` explicitly disables tracking for that raw tool
- override entry replaces the built-in policy for the same raw tool
- missing override file means built-ins only

## Built-in defaults

Without an override file, the managed Pi extension only treats these as mutating:

- `edit` -> canonical `edit`, filepath field `path`
- `write` -> canonical `write`, filepath field `path`

The built-in `bash` tool also sends pre/post shell signals. Failed shell calls still send their post signal because they may have written files before failing.

## Example: add Serena mutators

```json
{
  "version": 1,
  "tools": {
    "serena_replace_symbol_body": {
      "kind": "mutating",
      "canonical": "replace",
      "filepath_fields": ["relative_path"]
    },
    "serena_insert_after_symbol": {
      "kind": "mutating",
      "canonical": "edit",
      "filepath_fields": ["relative_path"]
    },
    "serena_insert_before_symbol": {
      "kind": "mutating",
      "canonical": "edit",
      "filepath_fields": ["relative_path"]
    },
    "serena_replace_content": {
      "kind": "mutating",
      "canonical": "replace",
      "filepath_fields": ["relative_path"]
    },
    "serena_rename_symbol": {
      "kind": "mutating",
      "canonical": "rename",
      "filepath_fields": ["relative_path"]
    }
  }
}
```

## Troubleshooting

Check generated extension:

```bash
grep -n "checkpoint', 'pi'\|git-ai.override.json" ~/.pi/agent/extensions/git-ai.ts
```

Check the daemon and local session-link pipeline:

```bash
git-ai daemon status
git-ai debug
git-ai await
```

Git AI v2 stores repository-level session/commit links in `~/.git-ai/internal/otel-db`. It does not create working logs or Git Notes, and the adapter does not read local transcripts.
