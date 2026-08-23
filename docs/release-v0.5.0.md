# srcmv v0.5.0 release notes

This release adds `srcmv outline`, a read-only companion to `select` that lists
every document symbol in one file through a trusted, installed language server.
It is a minor release: purely additive, with no changes to the edit protocol,
existing commands, or frozen registries.

## The outline command

`srcmv outline` runs exactly one `textDocument/documentSymbol` request over the
same session lifecycle, trusted-descriptor resolution, and snapshot limits as
semantic selection, then emits every symbol as flat outline-v1 records:

- name, standardized `symbol_kind` (selection-v1 spelling; `"unknown"` for
  non-standard numeric kinds), complete `symbol_path`, and derived `depth`;
- one-based inclusive `start_line`/`end_line`, one-based scalar columns with an
  exclusive past-end `end_column` (schema-nullable, but always populated by the
  frozen v1 pipeline);
- raw zero-based `lsp_range`/`lsp_selection_range` audit coordinates;
- the validated half-open byte `selector` of the enclosing symbol range — the
  same coordinate `select --at-byte` consumes, so an agent can go from an
  outline entry straight to a copy-ready selection.

Records are ordered by a deterministic comparator regardless of server child
order, exact duplicates coalesce, an optional repeatable `--kind KIND` filter
narrow large listings, and an empty listing is a success. Human output renders
an indented tree; `--json` emits a single line carrying
`outline_protocol_version: 1`.

```console
srcmv --workspace /path/to/repo outline --path src/huge.rs \
  --kind function --kind method --json > outline.json
```

The normative Draft 2020-12 success schema is
`docs/schema/outline-v1/response.schema.json`; hand-authored contract examples
live under `tests/golden/outline-v1`. v1 deliberately omits `request_source`
and payload digests: pick an entry, then run `select --at-byte` for composition.

## Errors and limits

Outline failures carry `outline_protocol_version: 1` and their own registry:
`INVALID_OUTLINE_QUERY` (exit 2), the shared `LSP_*` support spellings reused
verbatim (exit 4), and `OUTLINE_INTERNAL_ERROR` (exit 8). There is no conflict
category because outline performs no query matching; see `docs/protocol.md`
for the complete tables. All selection snapshot, normalization,
position-conversion, session, and transport limits apply unchanged, plus one
new fail-closed bound: at most 10,000 emitted symbols (resource
`outline_symbols`). Serialized output stays bounded by the global 16 MiB
exact-response limit.

## Transport robustness shared with select

Release qualification exposed a pre-existing race in the language-server
transport: a response frame split across multiple reads could intermittently
fail as `LSP_TIMEOUT` while most of the deadline remained. The transport now
keeps awaiting incomplete frames within the active deadline. This fixes
spurious timeouts for `select` identically; precedence rules, resource limits,
and genuine deadline expiry are unchanged, and regression coverage pins the
wait-versus-expiry boundary.

## Compatibility

Purely additive. `capabilities --json` and `selection-capabilities --json`
output is unchanged; outline is not advertised there in this release. Protocol
version 1, plan-hash version 1, transaction-record version 1, the edit error
and warning registries, and all selection-v1 contracts are untouched.
Language servers remain unbundled; outline uses the same trusted descriptors
or explicit `--server-program` choice as selection.

## Qualification

Bounded fake-server suites cover lifecycle, ranges, ordering, filtering,
limits, degenerate server outputs, and failures on both supported OS rows, and
remain authoritative for compatibility and failure qualification. Real-server
smoke qualification follows `docs/releasing.md` (`scripts/qualify-lsp.sh`,
best-effort) at publish time; recorded server versions apply to that evidence
only and do not bundle or certify every language server version.

## Assets

The release contains exactly these assets:

- `srcmv-v0.5.0-aarch64-apple-darwin.tar.gz`
- `srcmv-v0.5.0-x86_64-unknown-linux-gnu.tar.gz`
- `SHA256SUMS`

The two archives are built and tested on their matching qualified native GitHub
runner. Publishing an archive does not extend support beyond Linux x86_64 on
local ext4 and macOS arm64 on local APFS.
