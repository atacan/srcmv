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

## Language servers

srcmv does not bundle or sandbox language servers. Without `--server-id` or
`--server-program`, it uses a matching descriptor from the user's trusted
config file first, then a built-in descriptor whose program is installed on an
absolute `PATH` entry outside the workspace:

| ID | Program | Extensions |
|---|---|---|
| `rust` | `rust-analyzer` | `rs` |
| `go` | `gopls` | `go` |
| `python` | `pylsp` | `py` |
| `clangd-c` | `clangd` | `c` |
| `clangd-cpp` | `clangd` | `cc`, `cpp`, `cxx` |
| `typescript` | `typescript-language-server --stdio` | `ts`, `tsx`, `js`, `jsx` |
| `swift` | `sourcekit-lsp` | `swift` |

Older srcmv binaries may lack some rows (the `swift` row arrived in
v0.7.0). Ambiguous extensions such as `h` are never guessed: pass
`--server-id clangd-c` or `--server-id clangd-cpp`. `LSP_SERVER_NOT_CONFIGURED`
(exit 4) means no descriptor matched the extension or its program is not
installed. Do not fall back to line ranges silently; configure a server with
one of the options below or tell the user which server is missing.

**Explicit program (one invocation).** Naming a program is an explicit trust
decision. `--language-id` is required; each `--server-arg` is one literal
argument and no shell interprets it. The same flags work with `outline`.

```bash
srcmv --workspace "$WORKSPACE" select \
  --path unusual/source.ext --name target \
  --server-program /trusted/tools/my-language-server \
  --language-id my-language --server-arg=--stdio --json
```

**User config (persistent).** The trusted config file is `$SRCMV_CONFIG` when
set, otherwise `~/Library/Application Support/srcmv/config.toml` on macOS or
`$XDG_CONFIG_HOME/srcmv/config.toml` (`~/.config/srcmv/config.toml`) on Linux.
Unknown fields are rejected. Optional fields, shown with defaults:
`args = []`, `project_root = "."`, `allow_workspace_program = false`,
`startup_timeout_ms = 10000`, `request_timeout_ms = 30000`.

```toml
version = 1

[[servers]]
id = "my-language"
extensions = ["ext"]
language_id = "my-language"
program = "my-language-server"
args = ["--stdio"]
```

Select it with `--server-id my-language` or let the extension match it.
Discovery rejects executables inside the workspace unless the descriptor sets
`allow_workspace_program = true`; treat that as a trust decision too.

The server starts only after srcmv snapshots the selected file, so the selected
bytes are exact, but project-wide meaning is a mixed-time observation. Rerun
selection after relevant project changes. No language server runs during
`apply`.

### Swift

On macOS with Xcode or the Command Line Tools, `/usr/bin/sourcekit-lsp` is on
`PATH`, so the built-in `swift` descriptor works with no flags. It works on a
Swift package, an Xcode project, or a loose `.swift` file, because document
symbols are syntactic. On Linux, install a Swift toolchain that provides
`sourcekit-lsp`.

With srcmv before v0.7.0, which has no `swift` built-in, use explicit flags:

```bash
srcmv --workspace "$WORKSPACE" select --path Sources/App/Parser.swift \
  --name 'isControl(_:)' \
  --server-program "$(xcrun --find sourcekit-lsp)" --language-id swift --json
```

or this user config entry:

```toml
version = 1

[[servers]]
id = "swift-sourcekit"
extensions = ["swift"]
language_id = "swift"
program = "sourcekit-lsp"
```

sourcekit-lsp names symbols differently from most servers:

- Functions, methods, and initializers use their full Swift name, including
  argument labels: `--name 'isControl(_:)'`, `--name 'greet(with:loudly:)'`,
  `--name 'shout()'`, `--name 'init(name:)'`. A bare `--name isControl` returns
  `SELECTION_NOT_FOUND`. Quote these names in the shell.
- Types and properties use the plain name: `--name Person`, `--name name`.
- A protocol reports `interface`; an `extension Person` reports `namespace`
  with the name `Person`. So `--name Person` matches both the type and each
  extension. Disambiguate with `--kind struct`, `--kind class`, or
  `--kind namespace`, or use a position query.
- If you don't know the exact name, run `outline` first and copy the `name`, or
  use `--at-line`/`--at-column` on the declaration.

See `examples/10-lsp-semantic-selection/` in the srcmv repository for
runnable multilingual batching.
