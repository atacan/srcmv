# CLI and protocol reference

## Availability

```bash
srcmv --version
srcmv capabilities --json
srcmv selection-capabilities --json
srcmv protocol-version --json
```

Release v0.1.0 implements protocol version 1 and plan-hash version 1. Query capabilities instead of assuming a newer installation has the same surface.

## Command grammar

```text
srcmv [--workspace PATH] inspect --path RELATIVE [--path RELATIVE ...] --json
srcmv [--workspace PATH] select --path RELATIVE (--name NAME | --at-byte OFFSET | --at-line LINE [--at-column COLUMN]) [--kind KIND] [--all] [--extent declaration_lines|symbol] --json
srcmv [--workspace PATH] outline --path RELATIVE [--kind KIND ...] [--server-id ID | --server-program PROGRAM --language-id ID [--server-arg ARG ...]] --json
srcmv [--workspace PATH] apply --request FILE_OR_DASH --preview [--json] [--no-diff] [--summary]
srcmv [--workspace PATH] apply --request FILE_OR_DASH --commit --expect-plan DIGEST [--json]
srcmv [--workspace PATH] recover --list [--json]
srcmv [--workspace PATH] recover ID --status [--json]
srcmv [--workspace PATH] recover ID --complete [--json]
srcmv [--workspace PATH] recover ID --rollback [--json]
srcmv capabilities --json
srcmv selection-capabilities --json
srcmv protocol-version --json
```

`--request -` reads one request from stdin. Prefer `--json` for agent use: stdout is exactly one UTF-8 JSON value followed by LF. Human diagnostics use stderr.

## Response fields

`inspect` returns each path's existence, regular/absent type, content digest, byte length, line count, and physical identity hash.

`select` returns validated document-symbol matches from a trusted, installed
language server. Each match's `request_source` is directly composable as a
protocol-v1 operation source; preserve it unchanged. See
[semantic-selection.md](semantic-selection.md).

`outline` lists every document symbol in one file with `outline_protocol_version:
1`: name, standardized `symbol_kind`, complete `symbol_path`, derived `depth`,
one-based inclusive `start_line`/`end_line`, one-based scalar columns (the
exclusive `end_column` is schema-nullable but always populated in v1), raw
zero-based `lsp_range`/`lsp_selection_range` audit coordinates, and the
validated half-open byte `selector`. Records are deterministically ordered and
deduplicated; empty listings are successes. Feed a selector into
`select --at-byte` to obtain the copy-ready edit fragment.

Preview returns:

- `protocol_version`, `plan_hash_version`, `plan_sha256`, and `workspace_identity_hash`.
- `resolved_operations` with byte coordinates, destination offsets, effects, and selected payload digests.
- `outputs` with change kinds and before/after lengths and digests.
- A text diff, bounded binary summary, or omitted diff.
- Structured warnings.

Beginning with v0.2.0, opt-in `--summary` adds review-summary-v1 at
`diff.summary.review`. Combine it with `--no-diff` for a complete concise review
without detailed diff text. Operation and output indices join the existing
report arrays; no top-level field or capability entry is added.

Commit adds `transaction_id`, `transaction_state`, `files_changed`, preserved permission modes, recoverability status, and visibility. Each effectful resolved operation reports `inserted_payload_sha256`; a same-file no-op reports `null`.

## Exit categories

| Exit | Category | Representative codes |
|---:|---|---|
| 2 | Request | `INVALID_CLI`, `INVALID_JSON`, `UNSUPPORTED_PROTOCOL_VERSION`, `INVALID_REQUEST`, `INVALID_DIGEST` |
| 3 | Conflict | `PRECONDITION_FAILED`, `FILE_CHANGED`, `EXPECTED_PLAN_MISMATCH`, `PLAN_CHANGED_DURING_COMMIT`, `EDIT_CONFLICT`, `RECOVERY_CONFLICT` |
| 4 | Limit/support | `RESOURCE_LIMIT_EXCEEDED`, `UNSUPPORTED_PLATFORM`, `UNSUPPORTED_FILESYSTEM`, `SYMLINK_NOT_ALLOWED`, `HARD_LINK_NOT_SUPPORTED`, `CROSS_DEVICE_TRANSACTION` |
| 5 | Transaction | `TRANSACTION_BUSY`, `TRANSACTION_RECOVERY_REQUIRED`, `TRANSACTION_NOT_FOUND`, `RECOVERY_ACTION_NOT_ALLOWED` |
| 6 | Corruption | `CONTROL_DIRECTORY_INVALID`, `TRANSACTION_RECORD_CORRUPT` |
| 8 | Internal | `IO_ERROR`, `INTERNAL_ERROR` |

Every JSON error includes `code`, `category`, `retryable`, `message`, and `context`. Interpret the code and context; do not retry solely because `retryable` is true without re-establishing current workspace state.

`TRANSACTION_BUSY` keeps exit 5 and `retryable: true` and always reports:

```json
{
  "lock_state": "contended",
  "recovery_required": "unknown",
  "safe_next_action": "wait_then_retry"
}
```

This context proves only that an incompatible workspace lock is held. It does not identify the holder or show whether a mutation or recovery is active. Wait before retrying; never poll tightly, bypass the lock, or remove the lock file. Retrying inspect or preview takes a fresh observation. An agent may retry the unchanged commit request with the same `--expect-plan`, then must inspect and preview again if a precondition or plan mismatch follows. A human using `--accept-current-plan` previews again before retrying.

`recover --list --json` is the workspace-level status operation, and `recover ID --status --json` is the transaction-level operation. Both are read-only, create nothing, and hold a shared lock through their validated scan and report construction. An empty list means no recorded transaction needing recovery was observed. Any `orphan_record`, `manifest_only`, or `active` entry requires recovery before a new mutation. `cleanup_only` alone is terminal cleanup state. An incompatible exclusive holder produces `TRANSACTION_BUSY` instead of a scan.

Protocol-v1 request and response objects reject unknown fields. Integers are nonnegative `u64` values, and existing-file digests must be lowercase SHA-256 strings. The v0.1.0 protocol, plan-hash format, error/warning registry, and transaction-record version are frozen; do not invent fields or commands.
