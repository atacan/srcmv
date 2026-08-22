# LSP-backed file outline command implementation plan

Status: plan only; nothing implemented

Scope: a read-only `srcmv outline` command that lists every document
definition/symbol in one workspace file, with nesting support, using the
existing language-server session machinery

Editing protocol impact: none; protocol v1, plan-hash v1, selection-v1, and all
frozen schemas and registries remain unchanged

Naming note: the feature request refers to `codesplice-*` paths; this repository
was renamed (`66d51fd rename: CodeSplice to srcmv`). All paths below use the
current names — `crates/srcmv-cli`, `crates/srcmv-lsp`, `crates/srcmv-protocol`,
`crates/srcmv-test-support`, `skills/srcmv/...` — and the binary is `srcmv`.
Line references were verified against the current tree.

## 1. Decision summary

Add a new read-only subcommand, `srcmv outline`, that runs exactly one
`textDocument/documentSymbol` request against an installed language server over
the existing session lifecycle, validates and normalizes the hierarchical
result with the existing symbol layer, and emits every symbol with name, kind,
containing path, depth, one-based start/end lines, optional one-based columns,
raw zero-based LSP audit ranges, and the validated half-open byte selector.

Rationale for a new command rather than extending `select`:

- The `select` grammar requires exactly one query (`--name`, `--at-byte`, or
  `--at-line`; enforced by clap in `crates/srcmv-cli/src/select.rs:52-102`).
  A query-less whole-file listing cannot be added without breaking that frozen
  contract.
- The selection-v1 response schema (`docs/schema/selection-v1/response.schema.json`)
  is versioned and frozen. Its `query` member is mandatory and its match shape
  is edit-composition oriented (`request_source`, payload digest). An outline
  response has no query, different required members, and different cardinality
  expectations.
- `select --all` means "all matches for the supplied query"; reusing it as
  "list all symbols" would silently change documented semantics.
- Precedent exists for independent surfaces: selection-v1 was deliberately
  independently versioned from protocol-v1 (see
  `notes/lsp-semantic-selection-implementation-plan.md`). Outline-v1 follows
  the same pattern.

The implementation is LSP-only. Tree-sitter is not introduced; see §12 for the
seam that keeps a future parser backend possible without wire changes.

## 2. Answers to the requested investigation points

1. **New command or `select` extension?** New subcommand `srcmv outline`
   (§1). `symbols` was considered as the verb; `outline` better matches the
   editor-outline semantics of `textDocument/documentSymbol` and avoids
   implying grep-like symbol search. See open decision #1.
2. **Is the existing request + normalization sufficient?** Yes.
   `run_session` (`crates/srcmv-lsp/src/session.rs:261-468`) already performs
   initialize → didOpen → `textDocument/documentSymbol` (lines 396-417) →
   didClose → shutdown → exit and returns the raw hierarchical result plus
   negotiated capabilities. `normalize_document_symbols`
   (`crates/srcmv-lsp/src/symbols.rs:435-608`) already decodes, rejects flat
   responses, bounds strings/depth/storage, converts both ranges per symbol,
   and enforces containment. Neither function needs behavioral changes.
3. **Flattened vs tree-preserving representation?** Keep the existing
   flattened `Vec<NormalizedSymbol>` as the single canonical model. Each
   record already carries the full `symbol_path`; add a derived `depth`
   (`symbol_path.len() - 1`) at emission time. The human renderer indents by
   `depth`; JSON consumers reconstruct the tree from `symbol_path`. This
   avoids a second hierarchy type, keeps output order deterministic by document
   position regardless of server child ordering, and keeps limit enforcement
   simple. Server hierarchies are not trusted to be ordered or contiguous;
   flattening plus sorting removes that variability from the contract.
4. **Human nesting representation?** Two-space indentation per depth level
   with root symbols at column zero, one line per symbol, terminal-escaped
   like existing human output (`escape_terminal_text` is exported from
   `srcmv-protocol`; see `render_human`, select.rs:576-612, for the bounded
   accumulation pattern). Example in §5.2.
5. **JSON schema?** New independent `outline_protocol_version: 1` response;
   normative schema at `docs/schema/outline-v1/response.schema.json`. Lines
   are one-based physical lines; end line is the line containing the exclusive
   end offset (inclusive as displayed). Columns are one-based Unicode-scalar
   columns, exclusive/past-end, and **nullable**: an end offset inside a line
   terminator or EOF after a final terminator has no scalar-column position
   (mirrors `PositionError::ByteNotRepresentable`), but the line number always
   exists and is reported. Raw zero-based LSP ranges are retained verbatim for
   audit, matching selection-v1 conventions. Kinds reuse the frozen
   selection-v1 spellings (`SelectionSymbolKindDto`, `"unknown"` for numeric
   kinds outside the standard set). Full example in §5.2.
6. **All kinds or definitions only?** List everything the server returns.
   `documentSymbol` already reports definitions; filtering to "definitions"
   would require heuristics or a parser (non-goal). Provide an optional,
   repeatable `--kind KIND` filter reusing `KnownSymbolKind` parsing so agents
   can narrow output cheaply on very large files.
7. **Degenerate server outputs?**
   - No symbols (`null` / `[]`): success with `"symbols": []` (human:
     `no document symbols`), consistent with `--all` zero-match behavior.
   - Legacy flat `SymbolInformation[]`: typed failure
     `LSP_FLAT_SYMBOLS_UNSUPPORTED` (exit 4) — same fail-closed stance as
     `select`; no silent degradation.
   - Malformed ranges, invalid containment, oversized names/details/paths:
     existing `SymbolError` mapping unchanged → `LSP_PROTOCOL_ERROR` /
     `LSP_RESOURCE_LIMIT_EXCEEDED`.
   - Duplicate symbols: identical records coalesced by the existing key
     `(lsp_range, kind, symbol_path, name)` (`duplicate_key_equal`,
     symbols.rs:900-905); genuinely distinct duplicates are both listed.
   - Missing capability: `LSP_CAPABILITY_UNAVAILABLE` via existing
     `validate_initialize_result`.
   - Empty/oversized server identity: same checks as `select`
     (`validate_server_identity`, select.rs:621-638).
8. **Limits?** All normalization/session/transport limits apply unchanged.
   The frozen 1,000-match selection limit is untouched and does not apply.
   Add one new bound: `DEFAULT_MAXIMUM_OUTLINE_SYMBOLS = 10_000`, enforced
   after ordering/dedup and before serialization, failing closed with
   `LSP_RESOURCE_LIMIT_EXCEEDED`, resource `"outline_symbols"`. Rationale: an
   18k-line file commonly has far fewer than 10k top-level+nested definitions;
   the exact-bound 16 MiB serialized-response cap still backstops output size;
   and keeping the number separate preserves the frozen selection semantics.
9. **Fields useful for future selection/edit workflows?** Include the byte
   `selector` now (it is what `select --at-byte` consumes, so an agent can go
   straight from an outline entry to a precise `select` query or a future
   edit). Deliberately exclude `request_source` and the selected-payload
   digest in v1: hashing every symbol's payload on an 18k-line listing is
   wasted work and implies edit composition. A future additive field can
   reintroduce them opt-in. `lsp_selection_range` is kept because it is the
   natural `--at-byte` target.
10. **Tree-sitter fallback design now?** Record the seam only: keep
    `normalize_hierarchical_symbols` as the single ingestion boundary into
    `NormalizedSymbol`, and keep the outline surface free of any "LSP" wording
    in its own schema fields (server metadata stays generic). No parser code,
    grammars, dependencies, or fallback flags in this release.
11. **Exact files/tests/docs:** §9 (file-by-file changes + tests), §10
    (documentation updates), §11 (verification gates).
12. **Backwards compatibility?** Purely additive. No change to `select`
    grammar, behavior, schemas, goldens, error registry, exit codes,
    capability responses, protocol v1, or configuration format. See §8.

## 3. Goals

- List every symbol in one file with name, kind, containing path/tree, lines,
  columns, raw LSP ranges, and byte selector, in deterministic order.
- Support deep nesting (methods in classes/impls, structs in modules,
  namespaces) via path + depth, rendered as an indented tree for humans.
- Work against any installed, configured language server through the existing
  trusted-descriptor discovery system, with no new trust model.
- Scale to very large files (the 18,000-line motivating case) within the
  existing snapshot, conversion-work, normalization, and response-size bounds.
- Fail closed on malformed, flat, or oversized server output; never guess.
- Remain fully read-only: snapshot acquisition holds the shared diagnostic
  lock only until capture, and the language server never receives write
  capabilities.

## 4. Non-goals

- Emitting `request_source` fragments or payload digests (future opt-in).
- Multi-file or workspace-wide outlines; one invocation lists one file.
- Fuzzy, substring, regex, or textual symbol search (only exact `--kind`
  filtering).
- Keeping a server alive across invocations; one process per invocation.
- Any parser/tree-sitter fallback, bundled grammars, or plugin loading.
- Changes to `select`, selection-v1, protocol-v1, plan-hash v1, transaction
  records, capability responses, or the user configuration schema.
- Folding ranges, call hierarchy, go-to-definition, or workspace/symbol.
- Watching, caching, or incremental re-listing.

## 5. Command syntax and output

### 5.1 Grammar

```text
srcmv [--workspace PATH] outline --path RELATIVE [OPTIONS]

OPTIONS:
  --kind KIND                     repeatable filter; standardized spelling only
  --server-id ID
  --server-program PROGRAM --language-id ID [--server-arg ARG]...
  --json
```

Examples:

```bash
# Human outline of a large file.
srcmv --workspace /path/to/repo outline --path src/huge.rs

# Only functions and methods, machine-readable.
srcmv --workspace /path/to/repo outline --path src/huge.rs \
  --kind function --kind method --json > outline.json

# Explicit trusted server, mirroring select's escape hatch.
srcmv --workspace . outline --path x.py \
  --server-program pyright-langserver --language-id python --json
```

There is no `--name`/`--at-byte`/`--at-line` (that is `select`), no `--all`
(everything is listed), and no `--extent` (the byte selector always uses the
server's enclosing range, i.e. selection-v1 `extent = "symbol"` semantics).

### 5.2 Human output

For the standard CLI fixture
(`pub struct Outer;\n\nimpl Outer {\n    pub fn alpha() -> u32 {\n        1\n    }\n}\n`,
78 bytes):

```text
source.rs: 2 document symbols
class Outer lines 3..7 lsp=2:0..6:1 bytes 19..77
  function alpha lines 4..6 lsp=3:4..5:5 bytes 36..75
```

- Header line: escaped relative path and the total count **after** `--kind`
  filtering is applied.
- One line per symbol: two spaces per depth level (root symbols at column
  zero), then kind, name, one-based inclusive `lines <start>..<end>`, raw
  zero-based `lsp=` range, and half-open byte span. Names and paths containing
  control/bidi characters are visibly escaped (`escape_terminal_text` from
  `srcmv-protocol`) as elsewhere.
- Empty result — no symbols in the file, or none surviving `--kind`
  filtering — prints `no document symbols` (no header), analogous to select's
  empty-result convention (whose exact string is
  `no matching document symbols`, select.rs:608-610).
- Output size is bounded by `MAX_RESPONSE_BYTES` with checked accumulation,
  exactly like `render_human`.

### 5.3 JSON output (single line; pretty-printed here)

```json
{
  "outline_protocol_version": 1,
  "workspace_identity_hash": "sha256:WORKSPACE_IDENTITY",
  "source": {
    "path": "source.rs",
    "sha256": "sha256:SNAPSHOT_DIGEST",
    "byte_length": 78
  },
  "server": {
    "configuration_id": null,
    "reported_name": "srcmv-fake-lsp",
    "reported_version": "1",
    "position_encoding": "utf-16"
  },
  "symbols": [
    {
      "name": "Outer",
      "symbol_kind": "class",
      "symbol_path": ["Outer"],
      "depth": 0,
      "detail": "fixture class",
      "start_line": 3,
      "start_column": 1,
      "end_line": 7,
      "end_column": 2,
      "lsp_range": {
        "start": {"line": 2, "character": 0},
        "end": {"line": 6, "character": 1}
      },
      "lsp_selection_range": {
        "start": {"line": 2, "character": 5},
        "end": {"line": 2, "character": 10}
      },
      "selector": {"kind": "bytes", "start": 19, "end": 77}
    },
    {
      "name": "alpha",
      "symbol_kind": "function",
      "symbol_path": ["Outer", "alpha"],
      "depth": 1,
      "detail": "fixture function",
      "start_line": 4,
      "start_column": 5,
      "end_line": 6,
      "end_column": 6,
      "lsp_range": {
        "start": {"line": 3, "character": 4},
        "end": {"line": 5, "character": 5}
      },
      "lsp_selection_range": {
        "start": {"line": 3, "character": 11},
        "end": {"line": 3, "character": 16}
      },
      "selector": {"kind": "bytes", "start": 36, "end": 75}
    }
  ],
  "warnings": []
}
```

Field semantics:

| Field | Semantics |
|---|---|
| `start_line` | one-based physical line of the symbol's converted start byte |
| `end_line` | one-based physical line containing the exclusive end byte (display-inclusive end) |
| `start_column` | one-based Unicode-scalar column of the start byte; never null |
| `end_column` | one-based Unicode-scalar column just past the end content (exclusive); see the nullability note below |
| `lsp_range`, `lsp_selection_range` | raw zero-based server coordinates in the negotiated encoding, audit-only (identical to selection-v1) |
| `selector` | authoritative validated half-open byte range of the enclosing symbol range (extent `symbol`) |
| `depth` | `symbol_path.len() - 1`; root symbols are `0` |
| `symbol_kind` | selection-v1 spelling; `"unknown"` for non-standard numeric kinds |

Column nullability: the converter helper returns an *optional* column because
a raw byte offset inside a line terminator, or EOF after a final terminator,
has no scalar-column position. Such offsets cannot arise from
`documentSymbol`-derived ranges: `lsp_range_to_byte_range`
(position.rs:166-173) clamps both endpoints into line content before
validation, so **every entry emitted by the frozen v1 pipeline serializes
concrete columns and `null` never occurs in practice**. The `Option` exists
solely as the §12 future-backend seam; consumers must not code for a `null`
column they cannot observe. See open decision #4.

## 6. Data flow

Identical skeleton to `select` (select.rs:158-273); steps marked **new** are
the only new logic.

1. Parse args; validate each `--kind` via `KnownSymbolKind::from_str`
   (reject unknown spellings with `INVALID_OUTLINE_QUERY`, exit 2).
2. Open workspace; take `diagnostic_context`; acquire immutable snapshot with
   the same limits as select (`selection_snapshot_limits`, select.rs:275-284);
   drop lock before spawning the server (read-only observation discipline,
   covered by the existing slow-server test pattern).
3. Require valid UTF-8 (`UNSUPPORTED_TEXT_ENCODING` otherwise).
4. Resolve the server through the unchanged trusted-descriptor flow
   (`load_optional_configuration` + `resolve_selection_server`,
   select.rs:347-392). Reuse, do not duplicate (§9).
5. Build `SessionInput` (same deadlines policy, `session_deadlines`,
   select.rs:394-410) and run `run_session`.
6. Validate reported server identity; build one `PositionConverter` over the
   snapshot with negotiated encoding.
7. `normalize_document_symbols(output.symbols, converter, SymbolLimits::default())`
   — unchanged validation, bounding, and range conversion.
8. **New** `order_unique_candidates(&symbols)` (symbols.rs addition): sort by
   the full frozen candidate comparator — enclosing-range start/end bytes,
   kind spelling then numeric value, symbol path, name
   (`compare_candidates`, symbols.rs:881-886), followed by the reveal-range
   start/end and `detail` tiebreakers (`prepare_candidates`,
   symbols.rs:795-806) — and coalesce exact duplicates by
   `(lsp_range, kind, symbol_path, name)`. This is exactly the treatment
   `resolve_name --all` applies, extracted so both callers share it.
9. **New** filter by requested kinds if `--kind` was supplied.
10. **New** enforce `maximum_outline_symbols` (fail closed) and convert each
    record to an `OutlineSymbolDto`: byte→(one-based line, optional scalar
    column) for both endpoints via the **new** converter helper (§7), raw LSP
    ranges copied, selector from the validated `byte_range`.
11. **New** assemble `OutlineResponse`; serialize with a bounded writer
    (`to_outline_json_line`, enforcing `MAX_RESPONSE_BYTES` before any stdout
    write) or render human output with checked accumulation.
12. Errors map through the same mapper families used today
    (`map_config_error`, `map_filesystem_error`, `map_session_error`,
    `map_transport_error`, `map_symbol_error`), retargeted to the outline
    error envelope (open decision #2).

Scaling note for the 18k-line case: per-position conversion cost is
proportional to the scanned line length (not file length); cumulative scalar
scans across thousands of symbols stay far below the existing
16,777,216-scalar budget; normalization is O(nodes) against the 100,000-node
bounds; the 16 MiB exact-bound serializer backstops output size.

## 7. Proposed types and ownership

| Type / constant | Crate & location | Owner rationale |
|---|---|---|
| `OUTLINE_PROTOCOL_VERSION: u64 = 1` | `srcmv-protocol/src/outline.rs` (new) | wire-surface versioning lives beside `SELECTION_PROTOCOL_VERSION` |
| `OutlineResponse { outline_protocol_version, workspace_identity_hash, source: SelectionSourceDto, server: SelectionServerDto, symbols: Vec<OutlineSymbolDto>, warnings: Vec<WarningDto> }` | `srcmv-protocol/src/outline.rs` | source/server/warning DTOs reused verbatim from selection.rs (identical semantics; no duplication) |
| `OutlineSymbolDto { name, symbol_kind: SelectionSymbolKindDto, symbol_path, depth, detail, start_line, start_column: Option<u64>, end_line, end_column: Option<u64>, lsp_range: SelectionLspRangeDto, lsp_selection_range: SelectionLspRangeDto, selector: SelectionByteSelectorDto }` | `srcmv-protocol/src/outline.rs` | kind/range/selector enums reused from selection.rs |
| `OutlineErrorCode` (`INVALID_OUTLINE_QUERY`, `OUTLINE_INTERNAL_ERROR`, plus the `LSP_*` support spellings reused verbatim), `OutlineErrorCategory`, `OutlineErrorDto { outline_protocol_version, code, category, retryable, message, context }`, `OutlineProtocolError`, `to_outline_json_line` | `srcmv-protocol/src/outline.rs` | mirrors selection.rs's registry/bounded-writer pattern; category→exit mapping identical (2 request / 4 support / 8 internal; no Conflict codes exist because there is no query matching) |
| `DEFAULT_MAXIMUM_OUTLINE_SYMBOLS: usize = 10_000` | `srcmv-lsp/src/symbols.rs` next to `DEFAULT_MAXIMUM_MATCHES` (line ~29) | sits with its sibling output bound; enforced by the CLI |
| `order_unique_candidates(symbols: &[NormalizedSymbol]) -> Vec<&NormalizedSymbol>` | `srcmv-lsp/src/symbols.rs` near `prepare_candidates` (~line 795) | shares the frozen comparator/dedup with resolution; pure refactor + wrapper |
| `PositionConverter::byte_to_user_line_scalar(byte_offset) -> Result<(u64, Option<u64>), PositionError>` returning `(one_based_line, Option<one_based_scalar_column>)` | `srcmv-lsp/src/position.rs` after `user_line_scalar_to_lsp_position` (~line 295) | needs physical-line lookup tolerant of offsets inside terminators (existing `line_for_byte` rejects those; outline must not) and scalar counting, which must charge the cumulative work budget only the converter owns |
| `OutlineArgs`, `execute`, `OutlineFailure`, human renderer, limit enforcement, kind-filter application | `srcmv-cli/src/outline.rs` (new module) | command orchestration stays in the CLI crate like `select.rs` |
| Shared private helpers made crate-visible: `resolve_selection_server`, `session_deadlines`, `validate_server_identity`, `selection_snapshot_limits`, and the five error mappers | `srcmv-cli/src/select.rs` (`pub(crate)` visibility change only) | smallest-diff reuse; a full extraction into `srcmv-cli/src/lsp_common.rs` is acceptable if reviewers prefer, but is a pure move with no behavior change either way |
| `FakeLspScenario::{ManySymbols, SymbolCountLimitExceeded}` | `srcmv-test-support/src/fake_lsp.rs` | fixture scenarios live with the fake server |

Serialization invariant: like selection-v1, `to_outline_json_line` serializes
into a bounded buffer and fails with `LSP_RESOURCE_LIMIT_EXCEEDED`
(resource `serialized_json_response`) before emitting partial output; human
output accumulates with checked adds and the same cap.

## 8. Error and limit behavior

Exit categories reuse the established numbers: 2 request, 4 support, 8
internal (no conflict category). Grammar failures before dispatch remain
global `INVALID_CLI`.

| Situation | Code (exit) |
|---|---|
| Unknown `--kind` spelling | `INVALID_OUTLINE_QUERY` (2) |
| Path/encoding/workspace failures | identical mapping to select (`INVALID_SELECTION_QUERY`-equivalent becomes `INVALID_OUTLINE_QUERY`; `UNSUPPORTED_TEXT_ENCODING` unchanged) |
| Server not resolvable / ambiguous descriptors | `LSP_SERVER_NOT_CONFIGURED` (4) |
| No `documentSymbolProvider` | `LSP_CAPABILITY_UNAVAILABLE` (4) |
| Flat `SymbolInformation[]` | `LSP_FLAT_SYMBOLS_UNSUPPORTED` (4) |
| Malformed ranges, bad containment, bad payloads | `LSP_PROTOCOL_ERROR` (4) |
| Timeouts, spawn failure, early exit, request errors | `LSP_TIMEOUT` / `LSP_START_FAILED` / `LSP_EXITED` / `LSP_REQUEST_FAILED` (4, retryable per existing table) |
| Normalization/session/transport/serialization/outline-count bounds | `LSP_RESOURCE_LIMIT_EXCEEDED` (4) |
| Internal invariants | `OUTLINE_INTERNAL_ERROR` (8) |

Limits applied to one invocation (unchanged unless noted):

- Source snapshot: 8 MiB, UTF-8, 1 file, 5,000,000 lines (same helper as select).
- Raw/flattened nodes: 100,000 each; depth 256; name 4 KiB; detail 16 KiB;
  breadcrumb 64 KiB; candidate storage 64 MiB.
- Position-conversion work: 16,777,216 scalars cumulative.
- Session/transport/configuration bounds: unchanged.
- **New** outline symbols emitted: 10,000 (`resource: "outline_symbols"`),
  checked after dedup/filtering, before any serialization.
- Serialized JSON or human output: 16 MiB exact-bound.

Inherited quirk to document (verified empirically): the transport's 64-level
JSON-depth cap bounds practically representable hierarchy depth to roughly 31
levels even though `symbol_nesting_depth` allows 256; the existing
`deep-symbols` scenario fails with `LSP_PROTOCOL_ERROR` ("transport failed")
for exactly this reason. Outline inherits this unchanged; do not "fix" it in
this feature.

Addendum (post-implementation, with sign-off): the `SymbolCountLimitExceeded`
fixture (~1 MiB response frame) exposed an unrelated pre-existing race in
`Transport::next_incoming`: a chunk observed mid-frame could find no further
queued event and fall through to a premature `DeadlineExceeded`, surfacing as
spurious `LSP_TIMEOUT` for multi-chunk frames on both outline and select while
most of the phase deadline remained. Fixed separately in its own commit
(`fix(lsp): keep awaiting incomplete frames within the active deadline`) by
looping back to the blocking wait while the frame decoder holds partial input
and time remains; precedence and genuine deadline expiry are unchanged, and
select benefits identically. Regression coverage lives beside the existing
transport suites (split frame delivered under an adequate deadline, full-deadline
wait before expiry, expiry without waiting past the deadline).

Backwards compatibility checklist (point 12):

- `Command::Select` variant, clap grammar, and all select code paths untouched
  except `pub(crate)` visibility on reused helpers.
- Selection-v1 request/response/error schemas, goldens, and
  `selection-capabilities` output byte-identical.
- Protocol v1, plan-hash v1, recovery, and configuration schema untouched.
- New subcommand is additive; `--help` output grows one line.
- No existing test expectations change.

## 9. File-by-file change list (approximate locations)

Implementation order within each file follows the phases in §13.

**`crates/srcmv-protocol/src/lib.rs`**
- Add `mod outline;` and `pub use outline::{...}` alongside the selection
  exports (module list around lines 25-33).

**`crates/srcmv-protocol/src/outline.rs` (new, ≈350-450 lines)**
- Version constant, response/symbol DTOs, error-code registry, error DTO,
  bounded serializer `to_outline_json_line` (modeled on selection.rs:779-859).

**`crates/srcmv-lsp/src/position.rs`**
- `byte_to_user_line_scalar` after line ~295; reuses `LineIndex` binary
  lookup patterns from `line_for_byte` (lines 347-386) but tolerates
  terminator-interior and EOF-after-final-newline offsets for the *line*
  while returning `None` columns; charges `charge_code_point` (line ~459)
  during column scans.

**`crates/srcmv-lsp/src/symbols.rs`**
- `DEFAULT_MAXIMUM_OUTLINE_SYMBOLS` near line 29.
- `order_unique_candidates` near `prepare_candidates` (line ~795);
  refactor `prepare_candidates` internals so both share
  `compare_candidates` (881-886) / `duplicate_key_equal` (900-905).

**`crates/srcmv-cli/src/select.rs`**
- Visibility-only changes: mark `resolve_selection_server` (361-392),
  `session_deadlines` (394-410), `validate_server_identity` (621-638),
  `selection_snapshot_limits` (275-284), and the mappers
  `map_config_error` (640-666), `map_filesystem_error` (668-682),
  `map_session_error` (684-710), `map_transport_error` (712-747),
  `map_symbol_error` (749-775) as `pub(crate)`.

**`crates/srcmv-cli/src/outline.rs` (new, ≈450-550 lines incl. unit tests)**
- `OutlineArgs` (clap), execute pipeline per §6, kind filter, limit check,
  DTO construction, human renderer, `OutlineFailure`, thin wrappers that
  retarget shared mapper outputs to the outline envelope where messages
  mention "selection".

**`crates/srcmv-cli/src/lib.rs`**
- `Command::Outline(OutlineArgs)` in the enum (lines 68-77).
- Dispatch arm in `execute` (lines 204-241).
- `CommandFailure::Outline(...)` variant (lines 199-202) plus
  `render_outline_error` mirroring `render_selection_error` (1141-1162).

**`crates/srcmv-test-support/src/fake_lsp.rs`**
- `ManySymbols` scenario: ~120 well-formed nested/top-level symbols spanning
  several depths and kinds with distinct ranges (exercises ordering,
  depth rendering, multi-page human output).
- `SymbolCountLimitExceeded` scenario: >10,000 tiny symbols (~2 MB frame,
  under the 16 MiB transport caps) to exercise the new count bound cheaply.
- Register both in `ALL` and `as_str`/`FromStr` (lists at lines 97-175).

**Tests**

- `crates/srcmv-cli/tests/outline_cli.rs` (new; modeled directly on
  `select_cli.rs`'s self-re-entry harness): success listing assertions
  (names/kinds/path/depth/order/columns), empty (`null-symbols`),
  flat typed failure (exit 4), `malformed-range`, `invalid-selection-range`,
  `duplicate-symbols` coalescing, `deep-symbols` bounded failure,
  `many-symbols` ordering, `symbol-count-limit-exceeded`, non-UTF-8 source,
  source-size boundaries, timeout phase reporting, human-output
  escaping/indentation (including the column-zero root convention),
  `--kind` filtering with post-filter header count and all-filtered-out empty
  phrasing, workspace-read-only snapshot comparison, slow server releases
  diagnostic lock, JSON golden comparisons.
  Server-identity bounds are deliberately **not** re-tested end-to-end: the
  fake server hardcodes a valid identity (`serverInfo`,
  fake_lsp.rs:586/1071) and outline reuses `validate_server_identity`
  unchanged, whose empty/oversized cases are already pinned by the existing
  select.rs unit test `server_identity_must_be_nonempty_and_bounded_for_the_wire_schema`.
- `crates/srcmv-lsp/tests/position.rs`: below/at/above boundaries for
  `byte_to_user_line_scalar`; LF, CRLF, lone CR; astral-plane scalar columns
  under UTF-8/UTF-16/UTF-32 negotiation; terminator-interior and
  EOF-after-final-terminator offsets → `(line, None)`; out-of-range → error.
  These unit tests are also the only place the unreachable-in-practice
  `(line, None)` branch can be exercised (see §5.3 nullability note).
- `crates/srcmv-lsp/tests/symbols.rs`: `order_unique_candidates` ordering and
  dedup parity with `resolve_name(.., MatchMode::All, ..)` results.
- `crates/srcmv-protocol` unit tests (in-module): bounded-serializer
  below/at/above 16 MiB accounting using small injected maxima; error-registry
  completeness (`ALL`, exit codes, retryable flags).
- Golden fixtures: `tests/golden/outline-v1/{README.md,success.json,empty.json}`
  following the `selection-v1` golden layout.

## 10. Documentation updates

- `README.md`: new subsection immediately before `## Coding-agent skill`
  (~line 254), closing the "Semantic selection with an installed language
  server" section (which spans lines 172-253):
  one-paragraph intro, human example, JSON pointer to schema, guidance to use
  outline first and `select` second on huge files.
- `docs/protocol.md`: add `outline` to the command surface enumeration
  (~line 14-20); new "Read-only outline protocol v1" section after the
  selection section (~after line 267) covering grammar, field semantics table
  from §5.3, ordering/dedup rules, degenerate-input table from §8, limits,
  and exit codes.
- `docs/agent-integration.md`: extend "Discovering a source semantically"
  (~lines 48-83) with an outline-first recipe: `outline --json` → pick entry →
  `select --at-byte <selector.start or lsp_selection_range start>` → compose
  `request_source` via select.
- `docs/resource-limits.md`: add the outline row(s) to the semantic-selection
  limits table (or a sibling "Outline limits" paragraph):
  `Successful outline symbols | 10,000`.
- `docs/schema/outline-v1/response.schema.json` (new, Draft 2020-12) and
  `docs/schema/outline-v1/README.md` describing the surface, the nullable
  column rule, and the error-envelope decision (§ open decision 2).
- `skills/srcmv/references/cli-protocol.md`: add the outline line to the
  grammar block (lines 16-28) and a short response-fields paragraph.
- `skills/srcmv/references/semantic-selection.md`: cross-reference outline as
  the discovery step for large files.
- `skills/srcmv/SKILL.md`: mention `outline` in the inspection workflow if its
  command list enumerates commands.

## 11. Verification

Standard repo gates, run after each phase:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Feature-specific gates: the new `outline_cli` integration suite (including
fake-LSP re-entry scenarios), golden-file equality tests for
`tests/golden/outline-v1/`, and confirmation that the pre-existing
`select_cli`, `session_lifecycle`, `capabilities`, `resource_limits`,
and `symbols` suites pass unmodified.

## 12. Future Tree-sitter seam (design note only)

No parser code ships now. To keep a backend swap possible:

- Outline wire fields avoid LSP-specific vocabulary except the explicitly
  named audit ranges (`lsp_range`, `lsp_selection_range`), which are already
  part of the established selection vocabulary.
- All outline entries derive from `NormalizedSymbol`; a future backend would
  produce `NormalizedSymbol`s (directly, or via a parallel ingestion function)
  without touching `srcmv-protocol` or the CLI.
- Any future fallback must be explicit (flag or descriptor), never automatic,
  consistent with the trust posture of the descriptor system.

## 13. Suggested phases

0. Contract freeze: this plan + `docs/schema/outline-v1/*` + goldens accepted.
1. `srcmv-lsp`: `byte_to_user_line_scalar`, `order_unique_candidates`,
   outline-symbol constant, with boundary tests.
2. `srcmv-protocol`: `outline.rs` module + serializer/error tests.
3. `srcmv-cli`: visibility refactor, `outline.rs`, registration, rendering;
   unit tests.
4. Fixtures + integration: fake-LSP scenarios, `outline_cli.rs`, goldens.
5. Docs and skill references; final full-suite verification.

## 14. Open design decisions

1. **Command name**: `outline` (recommended) vs `symbols`. A hidden alias can
   be added later without wire impact.
2. **Error envelope**: dedicated `OutlineErrorDto` with
   `outline_protocol_version` and a mirrored code registry (recommended,
   above) versus reusing `SelectionErrorDto` verbatim (would label outline
   failures `selection_protocol_version: 1`, which misstates the surface but
   maximizes reuse).
3. **Nested JSON representation**: flat records with `depth`/`symbol_path` in
   v1 (recommended); a future minor revision could add an opt-in nested view
   without breaking the flat contract only if fields are purely additive —
   decide before freezing the schema whether `children` will ever be wanted.
4. **`end_column` polarity and nullability**: v1 pins exclusive/past-end
   (matching half-open selectors). The schema keeps `end_column` nullable, but
   the null variant is unreachable through the frozen v1 LSP pipeline (see
   §5.3); it is retained only for the §12 backend seam. If reviewers prefer,
   v1 may instead declare both columns non-nullable `u64` and relax them only
   when a second backend actually arrives — either choice must be settled
   before the schema freezes since it changes every consumer.
5. **Capability advertisement**: leave `capabilities` /
   `selection-capabilities` untouched in v1 (recommended; both are described
   as static/frozen surfaces) versus adding an outline section later.
6. **Whether `detail` should appear in human output** when present (v1
   proposal omits it for line stability; servers emit noisy details).
