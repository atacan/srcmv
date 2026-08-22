# srcmv

[![CI](https://github.com/atacan/srcmv/actions/workflows/ci.yml/badge.svg)](https://github.com/atacan/srcmv/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/atacan/srcmv)](https://github.com/atacan/srcmv/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust 1.97+](https://img.shields.io/badge/rust-1.97%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/atacan/srcmv)

## Introduction

srcmv is a CLI for moving or copying code from one file to another. Built mostly for splitting up large source files, especially when a coding agent is doing the refactor.

srcmv was formerly known as CodeSplice; those historical releases remain published under their original names in the `atacan/code-splice` GitHub repository.

For example, you can:

- move lines 527–718 from `file1.ts` into `file2.ts`;
- move a function such as `userInfo` into a new file;
- move several different blocks into several destination files in one operation.

To elaborate on the last case:

Suppose you want to move lines 6–10 to `fileA` and lines 11–15 to `fileB`. If you do the first move normally, the original line 11 is no longer line 11. A script or coding agent then has to reread the file or recalculate every later line number.

srcmv plans all of the operations against the same original file snapshot. You can describe every move using the original line numbers and send them together.

You can select code by line or byte range. srcmv can also use a language server already installed on your machine to find a symbol, so you can select something like a function by name instead of looking up its exact lines.

The CLI accepts JSON rather than a long list of flags. That makes batches of edits easier to describe, and it also makes srcmv straightforward to expose as a tool to a coding agent later. 

The repository includes an installable agent skill under [`skills/srcmv/`](skills/srcmv/).

If you just want to see what it does, start with the runnable examples in [`examples/`](examples/). They create disposable files and show the before-and-after result, so you can try srcmv without pointing it at a real project.

The rest of this README documents the exact behavior, safety checks, protocol and current platform limits.

## Exact, byte-preserving code movement for developers and coding agents.

srcmv is a Rust command-line tool that moves or copies code already present
in a workspace. It selects line or byte ranges from an immutable snapshot and
inserts those exact bytes elsewhere—without asking an agent to reproduce the
text, reformatting it, or normalizing line endings. An optional read-only
semantic-selection command can ask an already-installed language server where a
declaration is; srcmv still owns the immutable bytes and edit preconditions.

That makes srcmv useful for refactors where textual fidelity matters:
moving a function to another file, copying a declaration, reordering blocks in
one file, splitting one source file across several destinations, and preserving
CRLF, mixed line endings, or non-UTF-8 data.

srcmv `v0.5.0` is a deliberately bounded pilot. It is qualified only for
Linux x86_64 workspaces on local ext4 and macOS arm64 workspaces on local APFS.

## Why srcmv?

Coding agents are good at deciding *what* should move, but regenerating an
existing block can introduce incidental whitespace, encoding, or line-ending
changes. srcmv separates those responsibilities: the caller chooses a
source range and destination; the CLI verifies preconditions, previews a
deterministic plan, and transfers the original bytes.

Core features include:

- exact `move` and `copy` operations using line or half-open byte ranges;
- insertion at file start/end, before/after a line, or at a byte offset;
- same-file reordering, explicit no-op detection, and new-file destinations;
- multi-operation and multi-target plans from one immutable workspace snapshot;
- read-only inspection with file digests, byte lengths, and line counts;
- read-only semantic selection by exact symbol name or source position through a
  trusted, locally installed Language Server Protocol (LSP) server;
- bounded text or binary previews with a deterministic `sha256:` plan digest;
- optimistic file preconditions and `commit --expect-plan` protection against a
  stale source, destination, or preview;
- persistent transaction records plus explicit completion or rollback; and
- strict protocol-v1 JSON schemas, stable error codes, and machine-readable
  reports suitable for automation.

Explore the runnable, before-and-after walkthroughs in [`examples/`](examples/).
They cover the user-facing feature set and are the easiest way to try the CLI
without modifying a real project.

## Install

srcmv requires Rust 1.97 or newer when building from source.

### From this checkout

```bash
git clone https://github.com/atacan/srcmv.git
cd srcmv
cargo install --locked --path crates/srcmv-cli
srcmv --version
```

Reinstall after making local changes with:

```bash
cargo install --locked --force --path crates/srcmv-cli
```

### Prebuilt binaries and Homebrew

The repository contains release packaging for these two qualified targets:

- `x86_64-unknown-linux-gnu`
- `aarch64-apple-darwin`

Native archives and checksums are published with each current GitHub Release.
Configuring the personal Homebrew tap is still forthcoming. Once available, the
intended Homebrew command is:

```bash
brew install atacan/tap/srcmv
```

Until the tap is live, download a supported archive from GitHub Releases or
install from this checkout.

## Safe quickstart

Every automated mutation should follow the same three stages:

```text
inspect -> preview -> commit --expect-plan
```

The following example moves line 2 from `source.rs` to the end of
`destination.rs`. It requires `jq` and creates a fresh disposable directory:

```bash
DEMO_DIR=$(mktemp -d "${TMPDIR:-/tmp}/srcmv-demo.XXXXXX")
mkdir "$DEMO_DIR/workspace"
printf 'fn stay() {}\nfn move_me() {}\n' > "$DEMO_DIR/workspace/source.rs"
printf 'fn destination() {}\n' > "$DEMO_DIR/workspace/destination.rs"

srcmv --workspace "$DEMO_DIR/workspace" inspect \
  --path source.rs --path destination.rs --json > "$DEMO_DIR/inspection.json"

SOURCE_SHA=$(jq -r '.paths[] | select(.path == "source.rs") | .sha256' "$DEMO_DIR/inspection.json")
DESTINATION_SHA=$(jq -r '.paths[] | select(.path == "destination.rs") | .sha256' "$DEMO_DIR/inspection.json")

jq -n --arg source_sha "$SOURCE_SHA" --arg destination_sha "$DESTINATION_SHA" '{
  protocol_version: 1,
  operations: [{
    kind: "move",
    source: {
      path: "source.rs",
      selector: {kind: "lines", start: 2, end: 2},
      precondition: {kind: "sha256", value: $source_sha}
    },
    destination: {
      path: "destination.rs",
      anchor: {kind: "file_end"},
      precondition: {kind: "sha256", value: $destination_sha}
    }
  }]
}' > "$DEMO_DIR/request.json"

srcmv --workspace "$DEMO_DIR/workspace" apply \
  --request "$DEMO_DIR/request.json" --preview --json > "$DEMO_DIR/preview.json"

PLAN_SHA=$(jq -r '.plan_sha256' "$DEMO_DIR/preview.json")

srcmv --workspace "$DEMO_DIR/workspace" apply \
  --request "$DEMO_DIR/request.json" --commit --expect-plan "$PLAN_SHA" --json
```

Review `$DEMO_DIR/preview.json` before committing. If the workspace or plan
changes, srcmv rejects the commit; inspect and preview again. Coding agents
should never bypass this check with `--accept-current-plan`.

## Semantic selection with an installed language server

`srcmv select` turns a language server's hierarchical document symbols into
validated, half-open byte selectors. Language servers are not bundled. For the
built-in Rust descriptor, for example, `rust-analyzer` must already be installed
and discoverable on a trusted absolute `PATH` entry:

```bash
srcmv selection-capabilities --json
```

This target-independent discovery command reports the static selection-v1
feature surface without claiming that a compatible server is installed.

```bash
srcmv --workspace /path/to/repo select \
  --path src/lib.rs --name parse_request --kind function --json \
  > selection.json
```

Name matching is exact, case-sensitive, and unqualified. Position queries are
also available:

```bash
# Exact zero-based byte insertion offset.
srcmv --workspace /path/to/repo select \
  --path src/lib.rs --at-byte 42711 --json

# One-based line and Unicode-scalar insertion column; column defaults to 1.
srcmv --workspace /path/to/repo select \
  --path src/lib.rs --at-line 120 --at-column 9 --json
```

By default, zero matches are `SELECTION_NOT_FOUND` and multiple name matches (or
equally small position matches) are `SELECTION_AMBIGUOUS`. Add `--all` to return
every bounded match, including an empty list. `--extent declaration_lines` is
the default and includes a declaration's complete line boundaries only when the
extra bytes are spaces, tabs, or its line terminator; use `--extent symbol` for
the server's exact enclosing symbol range.

Each JSON match includes `request_source`, a copy-ready source fragment for an
ordinary protocol-v1 `move` or `copy` request. Copy it unchanged, add the
operation and destination, then use the normal preview and expected-plan commit
workflow:

```bash
jq -n --slurpfile selected selection.json \
  --arg destination_sha 'sha256:DESTINATION_DIGEST' '{
    protocol_version: 1,
    operations: [{
      kind: "move",
      source: $selected[0].matches[0].request_source,
      destination: {
        path: "src/destination.rs",
        anchor: {kind: "file_end"},
        precondition: {kind: "sha256", value: $destination_sha}
      }
    }]
  }' > request.json

srcmv --workspace /path/to/repo apply \
  --request request.json --preview --summary --no-diff --json
```

The source SHA-256 in `request_source` makes later preview or commit fail if the
selected file changed. See [agent integration](docs/agent-integration.md) for a
complete composition recipe and [the protocol contract](docs/protocol.md) for
selection semantics, server configuration, and error exits.

Beginning with `v0.2.0`, add `--summary --no-diff` to preview for a concise,
typed review record without detailed diff text. Use `--summary` alone to retain
the bounded diff alongside the review metadata. Both modes preserve the same
plan digest as ordinary preview.

For interrupted work, inspect persistent transactions before choosing an
explicit action:

```bash
srcmv --workspace "$DEMO_DIR/workspace" recover --list --json
srcmv --workspace "$DEMO_DIR/workspace" recover TRANSACTION_ID --status --json
srcmv --workspace "$DEMO_DIR/workspace" recover TRANSACTION_ID --complete --json
# Or: srcmv --workspace "$DEMO_DIR/workspace" recover TRANSACTION_ID --rollback --json
```

### Listing every symbol with `srcmv outline`

`srcmv outline` runs one read-only `textDocument/documentSymbol` request against
a trusted, installed language server and lists every symbol in one file: name,
standardized kind, complete breadcrumb, nesting depth, one-based inclusive
lines, one-based exclusive scalar columns, the raw zero-based LSP audit ranges,
and the validated half-open byte selector that `select --at-byte` consumes.
On very large files, use `outline` first to discover entries, then `select`
second to obtain the copy-ready edit fragment:

```bash
# Human tree view; two-space indentation per nesting level.
srcmv --workspace /path/to/repo outline --path src/huge.rs

# Machine-readable listing filtered to functions and methods.
srcmv --workspace /path/to/repo outline --path src/huge.rs \
  --kind function --kind method --json > outline.json
```

Human output for the standard fixture looks like:

```text
source.rs: 2 document symbols
class Outer lines 3..7 lsp=2:0..6:1 bytes 19..77
  function alpha lines 4..6 lsp=3:4..5:5 bytes 36..75
```

The JSON response carries `outline_protocol_version: 1`; its normative schema
is `docs/schema/outline-v1/response.schema.json`, with hand-authored examples
under `tests/golden/outline-v1`. An empty result is a success, never an error,
and an optional repeatable `--kind KIND` filter narrows large listings cheaply.
See [the protocol contract](docs/protocol.md) for ordering rules, limits, and
error exits.

## Coding-agent skill

The repository includes a progressively disclosed agent skill under
[`skills/srcmv/`](skills/srcmv/). Its short `SKILL.md` routes agents to
focused references only when a task needs them.

Install it for Codex with the open [Skills CLI](https://skills.sh/docs/cli):

```bash
npx skills add https://github.com/atacan/srcmv --skill srcmv -g -a codex
```

From a local checkout, omit `-g` to install it for the current project:

```bash
npx skills add . --skill srcmv -a codex
```

## Guarantees and boundaries

srcmv guarantees the equality of selected and inserted content bytes for
effectful exact-mode operations. It preserves the POSIX permission bits of an
existing changed target and assigns a new target according to the startup umask.

The edit engine does **not** parse code, update imports, format output, normalize
newlines, or create parent directories. Semantic selection delegates parsing to
a trusted external language server and does not bundle grammars or servers. The
edit engine also does not preserve ownership, ACLs, extended attributes,
resource forks, timestamps, platform flags, or hard-link relationships. Changed
files with multiple hard links are rejected.

Multi-target commit is **recoverable, not atomically visible**: unrelated readers
may temporarily observe a mixture of old and new files. Recovery after abrupt
process termination is supported; power-loss durability is not claimed.

The `v0.1.0` threat model assumes a trusted local user. srcmv rejects
absolute or escaping paths, symlink traversal, unsupported file types and
filesystems, cross-device transactions, and detected concurrent edits, but it is
not a sandbox or a defense against a malicious same-user process racing the
workspace.

See the frozen contracts for exact details:

- [editing semantics](docs/specification.md)
- [protocol and error registry](docs/protocol.md)
- [agent workflow](docs/agent-integration.md)
- [transaction and recovery model](docs/transaction-model.md)
- [resource limits](docs/resource-limits.md)
- [metadata contract](docs/metadata.md)
- [security boundary](docs/security.md)
- [qualified platforms](docs/platform-support.md)
- [`v0.1.0` release contract](docs/release-v0.1.0.md)
- [`v0.1.1` release notes](docs/release-v0.1.1.md)
- [`v0.2.0` release notes](docs/release-v0.2.0.md)
- [`v0.2.1` release notes](docs/release-v0.2.1.md)
- [`v0.3.0` release notes](docs/release-v0.3.0.md)
- [`v0.5.0` release notes](docs/release-v0.5.0.md)
- [release automation and Homebrew handoff](docs/releasing.md)

Protocol version 1, plan-hash version 1, and transaction-record version 1 are
frozen at the `v0.1.0` tag. Breaking wire-format changes require a new protocol
version.

## Repository layout

- `crates/srcmv-core`: immutable domain model and pure planning.
- `crates/srcmv-fs`: workspace snapshots, transactions, and recovery.
- `crates/srcmv-protocol`: strict JSON protocol and reports.
- `crates/srcmv-lsp`: bounded LSP transport, lifecycle, configuration,
  position conversion, and symbol resolution.
- `crates/srcmv-cli`: argument parsing, orchestration, and rendering.
- `crates/srcmv-test-support`: test-only fixtures and helpers.
- `examples/`: runnable user-facing demonstrations.
- `skills/srcmv/`: reusable instructions for coding agents.
- `docs/`: public behavior, support, and release contracts.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- \
  -D warnings -D clippy::perf
cargo test --workspace --all-features
cargo build --workspace --all-features
```

Run the complete platform qualification suite only on a qualified host and
filesystem:

```bash
scripts/qualify-platform.sh
```

srcmv is licensed under [Apache-2.0](LICENSE).
