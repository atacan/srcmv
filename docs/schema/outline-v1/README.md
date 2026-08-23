# Read-only outline protocol version 1

`response.schema.json` is the normative JSON Schema Draft 2020-12 contract for
the standalone, read-only `srcmv outline` surface. It does not modify srcmv
edit protocol version 1, plan-hash version 1, selection protocol version 1,
capability output, or any frozen registry.

Successful responses carry `outline_protocol_version: 1`. Outline errors that
occur after CLI parsing carry the same version and use the independent outline
error registry (see the error-envelope note below). Top-level command-line
grammar failures occur before outline dispatch and retain the global
`INVALID_CLI` response.

## Response contract

The response describes the immutable source snapshot sent to the language
server, the negotiated server identity and position encoding, and zero or more
flattened symbol records in deterministic order. An empty `symbols` array is a
successful response (no document symbols, or none surviving `--kind`
filtering); it is never an error.

Symbol records are flat: each carries its complete `symbol_path` breadcrumb and
a derived `depth` (`symbol_path.len() - 1`, roots at `0`). There is no nested
`children` representation in v1; JSON consumers reconstruct the tree from
`symbol_path`. Records are ordered by the frozen candidate comparator —
enclosing-range start/end bytes, kind spelling then numeric value, symbol path,
name, reveal-range start/end, then detail — with exact duplicates coalesced by
`(lsp_range, kind, symbol_path, name)`.

Line and column semantics:

- `start_line` is the one-based physical line containing the symbol's converted
  start byte.
- `end_line` is the one-based physical line containing the exclusive end byte;
  displayed as an inclusive end line.
- `start_column` is the one-based Unicode-scalar column of the start byte and
  is never null.
- `end_column` is the one-based Unicode-scalar column just past the end content
  (exclusive, matching half-open selectors) and is nullable in the schema. A
  raw byte offset inside a line terminator, or EOF after a final terminator,
  has no scalar-column position. Such offsets cannot arise from
  `documentSymbol`-derived ranges because range conversion clamps both
  endpoints into line content before validation, so every record emitted by the
  frozen v1 LSP pipeline serializes concrete columns; `null` is retained only
  as the seam for a future non-LSP backend and consumers must not code for it.

`lsp_range` and `lsp_selection_range` preserve the raw, zero-based LSP
coordinates returned by the server for audit only. `selector` is the
authoritative validated half-open byte range of the enclosing symbol range —
selection-v1 `extent = "symbol"` semantics — and is what `select --at-byte`
consumes. v1 deliberately omits `request_source` and selected-payload digests:
an outline listing does not compose edits, and hashing every payload would be
wasted work on large files. A future additive revision may reintroduce them
opt-in.

`detail` is the bounded server-provided detail or `null`. The server object
never exposes a process command, arguments, environment, initialization
options, stderr, or an absolute workspace path. The warning array reuses the
existing `WarningDto` object shape but permits only the already-registered
`OBSERVATION_MAY_BE_STALE` warning; outline v1 does not extend the frozen edit
warning registry.

## Error envelope

Outline failures use a dedicated error envelope with
`outline_protocol_version: 1` and an outline-specific code registry rather than
reusing the selection envelope verbatim: labeling an outline failure with
`selection_protocol_version: 1` would misstate the failing surface. Registry
codes are `INVALID_OUTLINE_QUERY`, `OUTLINE_INTERNAL_ERROR`, plus the shared
`LSP_*` support spellings reused verbatim from selection v1 (`LSP_*` codes keep
their existing categories, exit numbers, and retryability; there are no
conflict-category codes because outline performs no query matching). The
registry is not separately schema-frozen in v1; `docs/protocol.md` documents
the complete table.

## Limits and validation

Schema `maxLength` counts Unicode characters, not encoded bytes. Runtime code
must enforce the corresponding UTF-8 byte limits before allocation and must
also enforce the global 16 MiB serialized-response limit, checked arithmetic,
and duplicate-key rejection. The outline-specific release cap is 10,000 emitted
symbols (resource `outline_symbols`), enforced after ordering, deduplication,
and kind filtering, before any serialization; all normalization, snapshot,
position-conversion, session, and transport limits apply unchanged.

The golden vectors in `tests/golden/outline-v1` are hand-authored contract
examples. `workspace_identity_hash` values are placeholders because the hash
covers the physical identity of the workspace root, which differs per machine;
runtime tests substitute the observed value before comparing full structures.
