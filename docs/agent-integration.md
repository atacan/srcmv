# Agent integration

Agents follow an inspect, preview, commit sequence:

```bash
srcmv --workspace /path/to/repo inspect \
  --path src/source.rs --path src/destination.rs --json

srcmv --workspace /path/to/repo apply \
  --request split.json --preview --summary --no-diff --json

srcmv --workspace /path/to/repo apply \
  --request split.json --commit \
  --expect-plan sha256:PREVIEWED_PLAN --json
```

An agent never uses `--accept-current-plan`. If the expected plan changes, the
agent inspects and previews again rather than bypassing the digest precondition.
It treats recovery-required and conflict outcomes as failures needing explicit
inspection; it does not reproduce selected source bytes by hand as a fallback.

srcmv `v0.2.0` adds the opt-in concise preview shown above. The complete
typed review record is at `diff.summary.review`; it reports selected byte/line
metrics, before/after output line counts, and effectful insertion groups without
embedding source text. Omit `--no-diff` when detailed bounded diff evidence is
also useful. Omitting `--summary` preserves the earlier preview output exactly.

On `TRANSACTION_BUSY`, the agent waits and never polls tightly, bypasses the
lock, removes the lock file, or guesses whether a mutation or recovery is active.
Retrying `inspect` or preview takes a fresh observation. The unchanged commit
request may be retried with the same `--expect-plan`: srcmv replans and the
expected-plan gate prevents an unreviewed changed plan from committing. If that
retry reports a precondition or plan mismatch, the agent inspects and previews
again. Before starting an unrelated normal mutation after contention, it runs
`recover --list --json` as the authoritative point-in-time workspace status
check.

In `v0.1.0`, this workflow is available for plans with up to 100 changed
targets. Every candidate is prepared before mutation, targets commit in normalized
path order, and explicit recovery completes forward or rolls back in reverse order.
The commit is recoverable but not atomically visible across files.

The Phase 10 qualification pilot executed all 15 release scenarios with this
workflow on Linux x86_64/ext4 and macOS arm64/APFS. Negative path, stale-input,
and mismatch scenarios stopped at their documented rejection boundary; no pilot
invocation used `--accept-current-plan`.

## Discovering a source semantically

When exact line or byte coordinates are not already known, an agent may use the
read-only selection-v1 surface to obtain them from a trusted, installed language
server. This is discovery before the normal edit workflow; it does not alter the
protocol-v1 request or combine selection and editing into one invocation.

On very large files, discover with `outline` first and select second. The
outline listing gives every symbol's breadcrumb, depth, lines, and validated
half-open byte selector; pick the intended entry and feed its selector straight
into a position query to obtain the copy-ready `request_source` fragment:

```bash
srcmv --workspace /path/to/repo outline --path src/huge.rs --json > outline.json
srcmv --workspace /path/to/repo select \
  --path src/huge.rs \
  --at-byte "$(jq -r '.symbols[0].selector.start' outline.json)" \
  --json > selection.json
```

An optional repeatable `--kind function --kind method` filter narrows large
listings cheaply before picking an entry.

Use exactly one query form:

```bash
# Exact, case-sensitive, unqualified name.
srcmv --workspace /path/to/repo select \
  --path src/source.rs --name parse_request --kind function --json

# Or a zero-based UTF-8-boundary byte insertion offset.
srcmv --workspace /path/to/repo select \
  --path src/source.rs --at-byte 42711 --kind function --json

# Or a one-based line and Unicode-scalar insertion column (default column: 1).
srcmv --workspace /path/to/repo select \
  --path src/source.rs --at-line 120 --at-column 9 --kind function --json
```

The default `--extent declaration_lines` is appropriate for moving a standalone
declaration: it includes leading indentation and a line terminator only when the
bytes outside the server range are whitespace. Use `--extent symbol` when the
server's exact enclosing range is required. The authoritative result is the
half-open byte `selector`, not the raw zero-based `lsp_range` or
`lsp_selection_range` audit coordinates.

Without `--all`, treat `SELECTION_NOT_FOUND` and `SELECTION_AMBIGUOUS` as prompts
to refine the query. Prefer adding a standardized `--kind` or using a precise
position. Use `--all` only when the task really intends to review every match;
it may succeed with zero matches and is bounded to 1,000 results. Never choose
the first ambiguity candidate merely because it is first.

### Copy-ready request composition

Every match includes `request_source`, containing the workspace-relative path,
the selected byte range, and the captured source SHA-256 precondition. Copy that
object unchanged into the ordinary protocol-v1 request. The following runnable
shape uses `jq` to move one selected Rust function to the end of another existing
file:

```bash
WORKSPACE=/path/to/repo

srcmv --workspace "$WORKSPACE" select \
  --path src/source.rs --name parse_request --kind function --json \
  > selection.json

srcmv --workspace "$WORKSPACE" inspect \
  --path src/destination.rs --json > destination-inspection.json

DESTINATION_SHA=$(jq -r \
  '.paths[] | select(.path == "src/destination.rs") | .sha256' \
  destination-inspection.json)

jq -n --slurpfile selection selection.json \
  --arg destination_sha "$DESTINATION_SHA" '{
    protocol_version: 1,
    operations: [{
      kind: "move",
      source: $selection[0].matches[0].request_source,
      destination: {
        path: "src/destination.rs",
        anchor: {kind: "file_end"},
        precondition: {kind: "sha256", value: $destination_sha}
      }
    }]
  }' > request.json

srcmv --workspace "$WORKSPACE" apply \
  --request request.json --preview --summary --no-diff --json > preview.json

PLAN_SHA=$(jq -r '.plan_sha256' preview.json)

srcmv --workspace "$WORKSPACE" apply \
  --request request.json --commit --expect-plan "$PLAN_SHA" --json
```

Before composing, require `.matches | length == 1` unless the task explicitly
selected multiple results. Preserve `request_source` structurally; do not
recalculate offsets, rewrite the source digest, convert the selector to lines,
or reproduce the selected bytes in the request. The ordinary parser and preview
provide the next validation boundary.

If the selected source changes after `select`, preview or commit fails its
SHA-256 precondition. Rerun selection rather than editing the emitted digest. No
language server is started during `apply`.

### Language-server choice

Automatic selection uses trusted user configuration first, then built-in
metadata for installed `rust-analyzer`, `gopls`, `pylsp`, `clangd`, or
`typescript-language-server --stdio`. It does not bundle these programs and
does not guess ambiguous extensions such as `h`. Use a configured
`--server-id`, or make the trust decision explicit:

```bash
srcmv --workspace "$WORKSPACE" select \
  --path unusual/source.ext --name target \
  --server-program /trusted/tools/my-language-server \
  --language-id my-language --server-arg=--stdio --json
```

Each `--server-arg` is one literal argument; no shell interprets it. Automatic
discovery ignores empty and relative `PATH` entries and rejects workspace-local
executables. Explicit server programs and user configuration with
`allow_workspace_program = true` are trusted escape hatches, not sandboxed
plugins.

Selection snapshots and unlocks before starting the server. The selected file is
therefore exact, but the server may read other project files at a later time.
Agents should treat project-wide meaning as a mixed-time observation and rerun
selection after relevant project-context changes.

For a real-server smoke test on a development or release host, run:

```bash
scripts/qualify-lsp.sh
```

The script exercises installed `clangd` and `rust-analyzer`; it prints a notice
and succeeds when neither is installed because fake-server CI is the normative
protocol qualification.
