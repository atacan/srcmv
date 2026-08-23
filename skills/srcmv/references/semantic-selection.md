# Semantic source selection

Use `select` only to discover an exact source range. It is read-only and does
not change the protocol-v1 inspect, preview, and guarded-commit workflow.

On very large files, run the read-only outline command first to list every
symbol with its breadcrumb, depth, lines, and validated byte selector, then use
that selector here (`select --at-byte SELECTOR_START`) instead of searching by
name. See [cli-protocol.md](cli-protocol.md) for the outline grammar and
response fields.

First confirm the selection surface and an appropriate trusted language server:

```bash
srcmv selection-capabilities --json
srcmv --workspace /absolute/repo select \
  --path src/lib.rs --name parse_request --kind function --json \
  > selection.json
```

Use exactly one query: exact case-sensitive unqualified `--name`; zero-based
UTF-8-boundary `--at-byte`; or one-based `--at-line` with optional one-based
Unicode-scalar `--at-column` (default 1). Add `--kind` to disambiguate. Prefer a
precise position over choosing the first ambiguous result. Use `--all` only when
the task explicitly needs every match; it may return none.

Accepted `--kind` values are exact lowercase spellings:
`file`, `module`, `namespace`, `package`, `class`, `method`, `property`, `field`,
`constructor`, `enum`, `interface`, `function`, `variable`, `constant`, `string`,
`number`, `boolean`, `array`, `object`, `key`, `null`, `enum_member`, `struct`,
`event`, `operator`, and `type_parameter`. There is no `trait`, `protocol`,
`impl`, or `extension`; use the server's standardized mapping or a position query.

The default `--extent declaration_lines` suits moving a standalone declaration.
Use `--extent symbol` for the language server's exact symbol range.

Require one intended match, then copy `matches[0].request_source` unchanged into
the operation's `source`:

```bash
jq -n --slurpfile selection selection.json --arg sha "$DESTINATION_SHA" '{
  protocol_version: 1,
  operations: [{
    kind: "move",
    source: $selection[0].matches[0].request_source,
    destination: {
      path: "src/destination.rs",
      anchor: {kind: "file_end"},
      precondition: {kind: "sha256", value: $sha}
    }
  }]
}' > request.json
```

Do not recalculate its byte selector or source digest. Inspect the destination,
compose the request structurally, then follow [workflow.md](workflow.md). If the
source changes, rerun selection instead of weakening its precondition.

For several declarations, inspect every destination and run every `select`
before applying anything, for example:

```bash
srcmv --workspace "$WORKSPACE" select --path "$SOURCE" \
  --at-line 4 --at-column 1 --json > protocol.json
srcmv --workspace "$WORKSPACE" select --path "$SOURCE" \
  --at-line 13 --at-column 1 --json > extension.json
```

Require one intended match in each response and create one request operation per
match, assigning its `source` from that response's `request_source` unchanged.
Repeated selections of one file must carry the same source SHA-256, and moved
byte ranges must not overlap. Preview the complete multi-operation request once,
then commit that unchanged request with its reviewed `plan_sha256`.

Automatic discovery supports installed trusted server descriptors; language
servers are not bundled or sandboxed. Treat `--server-program` and user-enabled
workspace programs as explicit trust decisions. See the repository's
`docs/agent-integration.md` for complete composition and server configuration,
and `examples/10-lsp-semantic-selection/` for runnable multilingual batching.
