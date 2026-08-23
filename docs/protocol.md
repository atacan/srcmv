# Protocol contract

Protocol version 1 accepts JSON batches of `move` and `copy` operations. Each
operation contains a source path, selector, and existing-file digest precondition,
plus a destination path, anchor, and existing-file digest or `must_not_exist`
precondition. The request never contains the workspace root.

Objects reject unknown fields and duplicate keys. Integers are nonnegative and
must fit `u64`. Existing-file digests use exactly `sha256:` followed by 64
lowercase hexadecimal digits. The normative Draft 2020-12 request and response
schemas are `docs/schema/v1/request.schema.json` and
`docs/schema/v1/response.schema.json`.

## Command surface

The `srcmv` binary provides the complete grammar for `inspect`, `select`,
`outline`, `apply`, `recover`, `capabilities`, `selection-capabilities`, and
`protocol-version`. `apply` is the only mutation interface. A commit must use
exactly one of an expected plan digest or the explicit human convenience that
accepts the current plan. Agents use the expected digest.

In `v0.1.0`, `inspect`, preview, multi-target commit, recovery list
and status, and explicit transaction-wide completion/rollback are implemented.
Read-only commands create nothing and retain the existing shared control lock
through their scan, workspace observation, and report when it exists. Commit uses
two planning passes, requires explicit plan intent, prepares every candidate before
mutation, and commits changed targets in normalized UTF-8 path order.

Preview reports resolved byte coordinates, selected payload digests, output
before/after lengths and digests, plan-hash version 1, the plan digest, and an
opaque workspace identity hash. `--no-diff` changes only the diff field. Text
diffs label `LF`, `CRLF`, lone `CR`, and unterminated (`NONE`) lines; binary data
uses digest, length, and bounded base64 head/tail samples.

Beginning with srcmv `v0.2.0`, preview also accepts opt-in `--summary`.
Without that flag, JSON and human preview output are unchanged. `--summary`
retains the bounded diff and adds review-summary-v1 metadata at
`diff.summary.review`; `--summary --no-diff` is the concise review mode and uses
`diff.kind = "omitted"` while retaining the complete review metadata. The
independent nested schema is `docs/schema/review-summary-v1/schema.json`.

Review operation indices join `resolved_operations` in request order, and output
indices join `outputs` in normalized UTF-8 path order. Logical lines use the same
LF, CRLF, lone-CR, and final-unterminated-line semantics as inspection. Selected
ranges are interpreted as standalone byte sequences. Each output lists only
effectful payload insertion groups in final segment traversal order; a reported
same-file `no_op` has no fabricated insertion event. Existing binary and
truncation summary keys remain siblings of `review`.

Commit reports include a transaction ID (or `null` for a no-op), terminal state,
changed paths, preserved existing-target permission modes, and an inserted payload
digest for every effectful operation (`null` for a reported same-file no-op). The
`visibility` field states `recoverable_not_atomic`: unrelated readers can observe
mixed old/new targets during commit or rollback. Recovery list/status reports use
`mixed_old_new_possible` for those in-progress states, and `all_original` or
`all_planned` when the journal proves a uniform view.

JSON mode writes exactly one UTF-8 JSON value followed by LF to stdout. Human
diagnostics use stderr and visibly escape terminal control and bidi characters.

## Read-only semantic selection protocol v1

Semantic selection is an independently versioned, read-only surface. It does
not change protocol version 1, plan-hash version 1, transaction-record version
1, `capabilities --json`, or the frozen edit error and warning registries. Its
normative schemas are
`docs/schema/selection-v1/response.schema.json` and
`docs/schema/selection-v1/error.schema.json`.

Automation can discover this surface without a workspace or an installed
server:

```bash
srcmv selection-capabilities --json
```

The bounded response reports `selection_protocol_version: 1`, supported query
and extent spellings, position encodings, copy-ready composition support, and
that language-server availability is runtime-dependent. This command is
independently versioned and does not probe configuration or `PATH`.

The CLI grammar is:

```text
srcmv [--workspace PATH] select --path RELATIVE QUERY [OPTIONS]

QUERY:
  --name NAME
  --at-byte OFFSET
  --at-line LINE [--at-column COLUMN]

OPTIONS:
  --kind KIND
  --all
  --extent symbol|declaration_lines
  --server-id ID
  --server-program PROGRAM --language-id ID [--server-arg ARG]...
  --json
```

Exactly one query form is required. `--name` performs exact, case-sensitive,
unqualified matching. `--kind` optionally restricts either query to one standard
LSP symbol kind; accepted spellings are the lowercase values in the response
schema, such as `function`, `method`, `class`, `interface`, `enum`, and `struct`.

`--at-byte` is a zero-based insertion offset into the captured snapshot. It must
be at a UTF-8 boundary; EOF is valid. `--at-line` and `--at-column` are one-based,
and the column counts Unicode scalar insertion positions rather than bytes,
UTF-16 code units, or grapheme clusters. Column 1 is line start, the position
immediately after the last scalar is valid, line terminators are not columns,
and `--at-column` defaults to 1. Coordinates are exact and never clamped.

A position query matches nonempty half-open symbol ranges containing the byte
offset. At ordinary boundaries this means `start <= offset < end`; EOF also
matches a symbol whose nonempty range ends at EOF. Without `--all`, the shortest
containing range wins, and distinct equally short candidates are ambiguous.
Without `--all`, a name query must likewise have exactly one candidate. With
`--all`, all bounded candidates are returned in deterministic byte-range,
kind/path/name order; no matches is a successful empty response. Duplicate
server symbols with the same range, kind, path, and name are coalesced.

`--extent symbol` returns the converted `DocumentSymbol.range` exactly.
`--extent declaration_lines`, the default, expands the start to physical line
start only when the preceding bytes on that line are spaces or tabs. It expands
the end through spaces/tabs and one LF, CRLF, or lone-CR terminator only when no
other content follows the symbol on that line. Existing terminators and an
unterminated final line are preserved byte-for-byte. The server's
`selectionRange` is validated as nonempty and contained by `range`, but it is
reported for audit rather than used as the payload extent.

srcmv accepts hierarchical `DocumentSymbol[]`, `null`, and an empty array.
Legacy flat `SymbolInformation[]` responses are rejected rather than assigned
weaker semantics. LSP ranges are zero-based and may use negotiated UTF-8,
UTF-16, or UTF-32 character units. srcmv converts them against the exact
UTF-8 source snapshot, rejecting nonexistent lines, split code points or
surrogate pairs, reversed/empty/out-of-file ranges, and invalid
`selectionRange` containment.

### Success and composition

A successful JSON response contains:

- `selection_protocol_version: 1` and an opaque workspace identity hash;
- the workspace-relative source path, snapshot SHA-256, and byte length;
- the normalized name query or canonical byte-position query;
- a non-secret server descriptor ID, reported server identity, and negotiated
  position encoding;
- deterministic matches with symbol breadcrumbs, both raw LSP ranges, the
  authoritative byte selector, selected-payload digest and length; and
- existing observation warnings, currently only `OBSERVATION_MAY_BE_STALE`.

Every match contains a `request_source` object whose shape is the unchanged
protocol-v1 edit request's `source` definition:

```json
{
  "path": "src/input.rs",
  "selector": {"kind": "bytes", "start": 0, "end": 42},
  "precondition": {
    "kind": "sha256",
    "value": "sha256:SOURCE_SNAPSHOT_DIGEST"
  }
}
```

Insert that object unchanged into a normal `move` or `copy` operation, provide a
destination, and run the ordinary `apply --preview` followed by
`apply --commit --expect-plan`. Selection neither creates an edit request nor
commits anything. A source change after selection is rejected by the embedded
SHA-256 precondition; no language server runs during `apply`.

`--json` writes exactly one bounded JSON value plus LF. Without it, one line per
match reports the path, half-open bytes, kind, name, breadcrumb, enclosing LSP
range, and LSP selection range. A successful empty `--all` result prints
`no matching document symbols`.

### Server choice and trusted configuration

Language servers are installed and maintained independently; srcmv does
not bundle them. `--server-program` selects its program directly. `--server-id`
looks up that normalized ID in trusted user descriptors and then built-ins. With
neither option, automatic selection first looks for user descriptors matching
the extension and then consults the built-in table. `--server-program` requires
`--language-id`, conflicts with `--server-id`, and accepts repeated
`--server-arg` values as literal arguments.
The executable and arguments are passed directly to `std::process::Command`:
there is no shell parsing, interpolation, glob expansion, or command
substitution.

`SRCMV_CONFIG` overrides the configuration path exactly. Otherwise the
file is `srcmv/config.toml` under the platform configuration directory:
typically `$XDG_CONFIG_HOME/srcmv/config.toml` (or
`~/.config/srcmv/config.toml`) on Linux and
`~/Library/Application Support/srcmv/config.toml` on macOS. An absent
default file means no user descriptors; an explicitly named missing or invalid
file is an error. Loading configuration creates no file or directory.

A descriptor can use this shape; `initialization_options` and `settings` are
optional and are omitted here, while the other optional members show their
defaults:

```toml
version = 1

[[servers]]
id = "rust-custom"
extensions = ["rs"]
language_id = "rust"
program = "rust-analyzer"
args = []
project_root = "."
allow_workspace_program = false
startup_timeout_ms = 10000
request_timeout_ms = 30000
```

The file is trusted user configuration, bounded to 1 MiB and nesting depth 32,
and rejects unknown fields. IDs are trimmed, ASCII-case-normalized, and limited
to letters, digits, `.`, `_`, and `-`; extensions are trimmed, lowercased, and
may have one leading dot. IDs must be unique, extensions within one descriptor
must be unique, and automatic selection rejects multiple user descriptors for
the same extension. `project_root` is workspace-relative, cannot escape, must
resolve to an existing directory, and becomes the server's working directory.
Configured timeouts must be from 100 through 300000 milliseconds.

Automatic and built-in discovery searches only absolute `PATH` entries, ignores
empty and relative entries, requires a regular executable file, canonicalizes
it, and rejects candidates inside the workspace. This prevents a workspace
`bin` directory or `PATH=.` from silently selecting repository code. A trusted
user descriptor may opt into a workspace-local executable with
`allow_workspace_program = true`. Explicit `--server-program` is the direct
escape hatch and may name a relative or workspace-local program; using it is an
explicit trust decision.

Built-in descriptors are convenience metadata, not bundled servers or support
guarantees:

| ID | Program and arguments | Extensions / language IDs |
|---|---|---|
| `rust` | `rust-analyzer` | `rs` / `rust` |
| `go` | `gopls` | `go` / `go` |
| `python` | `pylsp` | `py` / `python` |
| `clangd-c` | `clangd` | `c` / `c` |
| `clangd-cpp` | `clangd` | `cc`, `cpp`, `cxx` / `cpp` |
| `typescript` | `typescript-language-server --stdio` | `ts` / `typescript`, `tsx` / `typescriptreact`, `js` / `javascript`, `jsx` / `javascriptreact` |

Ambiguous header extensions such as `h` are intentionally not guessed. Select
a trusted descriptor with `--server-id` or use an explicit program and language
ID.

### Selection errors and exits

Command-line grammar failures still use global `INVALID_CLI`. Failures after
selection dispatch use the independent selection-v1 error envelope and these
exit categories:

| Exit | Category | Codes |
|---:|---|---|
| 2 | Request | `INVALID_SELECTION_QUERY` |
| 3 | Conflict | `SELECTION_NOT_FOUND`, `SELECTION_AMBIGUOUS` |
| 4 | Support | `LSP_SERVER_NOT_CONFIGURED`, `UNSUPPORTED_TEXT_ENCODING`, `LSP_CAPABILITY_UNAVAILABLE`, `LSP_FLAT_SYMBOLS_UNSUPPORTED`, `LSP_DOCUMENT_SYNC_UNAVAILABLE`, `LSP_RESOURCE_LIMIT_EXCEEDED`, `LSP_TIMEOUT`, `LSP_START_FAILED`, `LSP_EXITED`, `LSP_PROTOCOL_ERROR`, `LSP_REQUEST_FAILED` |
| 8 | Internal | `SELECTION_INTERNAL_ERROR` |

Only `LSP_TIMEOUT`, `LSP_START_FAILED`, `LSP_EXITED`, and
`LSP_REQUEST_FAILED` are marked retryable. `SELECTION_AMBIGUOUS` includes a
bounded deterministic candidate list; use a more specific `--kind` or position,
or intentionally request `--all`.

## Read-only outline protocol v1

The outline surface is independently versioned like selection v1. It runs one
read-only `textDocument/documentSymbol` request through the same session
lifecycle, trusted-descriptor resolution, snapshot limits, and document-symbol
normalization as selection, then emits every symbol as a flat record. It does
not modify edit protocol v1, plan-hash v1, capability output, or any frozen
registry. The normative Draft 2020-12 success schema is
`docs/schema/outline-v1/response.schema.json`.

### Grammar

```text
srcmv [--workspace PATH] outline --path RELATIVE [--kind KIND ...]
  [--server-id ID | --server-program PROGRAM --language-id ID [--server-arg ARG] ...]
  [--json]
```

There is no name or position query because everything is listed, no `--all`
because the listing is already complete, and no extent choice because each
record's byte selector always uses the server's enclosing symbol range —
selection-v1 `extent = "symbol"` semantics. An optional, repeatable
`--kind KIND` filter accepts only standardized spellings and applies after
ordering and deduplication; an unknown spelling is `INVALID_OUTLINE_QUERY`
(exit 2).

### Field semantics

Successful responses carry `outline_protocol_version: 1`, an opaque workspace
identity hash, the immutable snapshot description (`path`, `sha256`,
`byte_length`), negotiated server identity and position encoding, flat symbol
records, and structured warnings (only `OBSERVATION_MAY_BE_STALE`).

| Field | Semantics |
|---|---|
| `start_line` | one-based physical line of the symbol's converted start byte |
| `end_line` | one-based physical line containing the exclusive end byte (display-inclusive end) |
| `start_column` | one-based Unicode-scalar column of the start byte; always populated in v1 |
| `end_column` | one-based Unicode-scalar column just past the end content; nullable in the schema but unreachable through the frozen v1 LSP pipeline |
| `depth` | `symbol_path.len() - 1`; root symbols are `0` |
| `symbol_kind` | selection-v1 spelling; `"unknown"` for non-standard numeric kinds |
| `lsp_range`, `lsp_selection_range` | raw zero-based server coordinates in the negotiated encoding, audit only |
| `selector` | authoritative validated half-open byte range of the enclosing symbol range |

v1 deliberately omits `request_source` and selected-payload digests: hashing
every symbol on a large listing is wasted work and implies edit composition.
Use the byte selector with `select --at-byte` when composition is intended.

### Ordering, deduplication, and degenerate inputs

Records are ordered by the frozen candidate comparator — enclosing-range
start/end bytes, kind spelling then numeric value, symbol path, name,
reveal-range start/end bytes, then detail — regardless of server child order,
and exact duplicates coalesce by `(lsp_range, kind, symbol_path, name)`.

| Situation | Result |
|---|---|
| No symbols (`null` / `[]`) or none surviving `--kind` filtering | success with `"symbols": []`; human output prints `no document symbols` |
| Legacy flat `SymbolInformation[]` | `LSP_FLAT_SYMBOLS_UNSUPPORTED` (exit 4) |
| Malformed ranges, invalid containment, bad payloads | `LSP_PROTOCOL_ERROR` (exit 4) |
| Missing `documentSymbolProvider` | `LSP_CAPABILITY_UNAVAILABLE` (exit 4) |
| Server not resolvable or ambiguous descriptors | `LSP_SERVER_NOT_CONFIGURED` (exit 4) |
| Timeouts, spawn failure, early exit, request errors | `LSP_TIMEOUT` / `LSP_START_FAILED` / `LSP_EXITED` / `LSP_REQUEST_FAILED` (exit 4, retryable per the shared table) |
| Normalization, session, transport, serialization, or outline-count bounds exceeded | `LSP_RESOURCE_LIMIT_EXCEEDED` (exit 4) |

Errors after outline dispatch carry `outline_protocol_version: 1` and the
outline registry: `INVALID_OUTLINE_QUERY`, `OUTLINE_INTERNAL_ERROR`, plus the
shared `LSP_*` support spellings reused verbatim from selection v1. Categories
map to exits 2 (request), 4 (support), and 8 (internal); there is no conflict
category because outline performs no query matching.

### Limits

All snapshot, normalization, position-conversion, session, and transport limits
apply unchanged; the frozen 1,000-match selection limit does not apply. One new
bound exists: at most 10,000 symbols are emitted (resource `outline_symbols`),
checked after ordering, deduplication, and kind filtering, before any
serialization. Serialized JSON or human output stays bounded by the global
16 MiB exact-response limit. Inherited quirk unchanged from selection: the
transport's 64-level JSON-depth cap practically bounds representable hierarchy
depth to roughly 31 levels even though normalization allows 256.

## Errors and warnings

Version 1 reserves the error identifiers and exit categories below.

| Exit | Category | Codes |
|---:|---|---|
| 2 | Request | `INVALID_CLI`, `INVALID_JSON`, `UNSUPPORTED_PROTOCOL_VERSION`, `INVALID_REQUEST`, `INVALID_DIGEST` |
| 3 | Conflict | `PRECONDITION_FAILED`, `FILE_CHANGED`, `FILE_ALIAS`, `EXPECTED_PLAN_REQUIRED`, `EXPECTED_PLAN_MISMATCH`, `PLAN_CHANGED_DURING_COMMIT`, `EDIT_CONFLICT`, `RECOVERY_CONFLICT` |
| 4 | Limit/support | `RESOURCE_LIMIT_EXCEEDED`, `UNSUPPORTED_PLATFORM`, `UNSUPPORTED_FILESYSTEM`, `UNSUPPORTED_FILE_TYPE`, `SYMLINK_NOT_ALLOWED`, `HARD_LINK_NOT_SUPPORTED`, `CROSS_DEVICE_TRANSACTION`, `NO_REPLACE_UNAVAILABLE` |
| 5 | Transaction | `TRANSACTION_BUSY`, `TRANSACTION_RECOVERY_REQUIRED`, `TRANSACTION_NOT_FOUND`, `RECOVERY_ACTION_NOT_ALLOWED` |
| 6 | Corruption | `CONTROL_DIRECTORY_INVALID`, `TRANSACTION_RECORD_CORRUPT` |
| 8 | Internal | `IO_ERROR`, `INTERNAL_ERROR` |

Warnings are `OBSERVATION_MAY_BE_STALE`, `METADATA_NOT_PRESERVED`, and
`DIFF_TRUNCATED`. Every error report includes its code, category, retryability,
message, and structured context.

Absolute request-file paths are redacted from structured I/O errors. Human error
messages visibly escape terminal controls and Unicode bidirectional-formatting
characters.

Every `TRANSACTION_BUSY` response has `retryable: true` and this exact context:

```json
{
  "lock_state": "contended",
  "recovery_required": "unknown",
  "safe_next_action": "wait_then_retry"
}
```

The error means that the command's nonblocking lock attempt encountered an
incompatible workspace lock. It does not identify the holder, prove that a
mutation is active, or establish whether recovery is required. `retryable` means
that an external state change may allow a later invocation to succeed; it does
not direct clients to poll or retry in a tight loop. Wait before retrying and
never bypass, remove, or break the lock.

Retry behavior depends on the command. A retried `inspect` or preview obtains a
fresh observation. An unchanged commit may be retried with the same
`--expect-plan`; the command replans and rejects a changed plan before mutation.
After a precondition or plan mismatch, inspect and preview again. A human using
`--accept-current-plan` previews again before retrying because that option can
accept a plan different from the earlier preview. Before an unrelated normal
mutation after contention, `recover --list --json` is the authoritative
point-in-time workspace status operation.

`recover --list --json` and `recover ID --status --json` are read-only and hold a
shared lock through validation, scanning, and report construction. An empty list
means that no recorded transaction needing recovery was observed. Any
`orphan_record`, `manifest_only`, or `active` entry requires recovery before a
new mutation. `cleanup_only` by itself is terminal cleanup state, not unfinished
recovery. If an exclusive holder prevents a safe scan, these status operations
return `TRANSACTION_BUSY` instead of observing a journal being published.

## Version freeze

The `v0.1.0` tag closes protocol version 1. The request and response schemas,
error and warning registries above, plan-hash version 1, and transaction-record
version 1 are the frozen release contract. A breaking wire-format change requires
a new protocol version.
