# Independent conservation audit implementation plan

Status: proposal; not yet implemented

Scope: a new read-only `srcmv audit-conservation` command that compares two
arbitrary directory trees (typically a PR base commit tree and head commit tree)
and verifies that every substantial byte region removed from the before tree
reappears exactly somewhere in the after tree.

Editing protocol impact: none; protocol v1, plan-hash version 1,
transaction-record version 1, the frozen edit error/warning registries, and all
existing command outputs remain unchanged. The audit is an independently
versioned read-only surface (`conservation-audit-v1`) that follows the
established `selection-v1` precedent.

## 1. Decision summary

Add a pure, reviewer-facing conservation check: given a before tree and an
after tree, prove at the byte level that removed content was conserved
elsewhere, and clearly report every byte that is genuinely new or unmatched.

The audit deliberately shares nothing with srcmv's edit machinery:

- it reads no srcmv requests, transaction journals, plan digests, runtime
  identity hashes, control directories, or agent logs;
- its inputs are exactly two trees supplied on the command line (directories or
  explicit path manifests), so it works across many coding-agent iterations
  regardless of how the changes were produced; and
- it is a deterministic function from `(base_tree, head_tree, options)` to one
  bounded report with a conservative `pass`, `fail`, or `inconclusive` verdict.

Internally everything is accounted in exact bytes. Line numbers are derived for
presentation only, using the existing `LineIndex` semantics (LF, CRLF, lone CR;
unterminated final suffix is a line). No parsing, symbol resolution, language
servers, or semantic correctness claims are involved.

Matching is global across the whole after tree, so blocks that moved between
files, were reordered inside one file, or were split into pieces are still
credited. One-to-one consumption accounting prevents one shared occurrence
from satisfying two removals. Identical repeated blocks are counted soundly
and flagged; content too ubiquitous to anchor anywhere is never credited or
forgiven — it forces `inconclusive`. Anything that cannot be decided within
the configured resource budget also forces `inconclusive`. The command must
never report success when ambiguity or incomplete matching prevents a reliable
conservation claim; its only incompleteness is conservative (rare greedy
overlap conflicts may `fail` where a perfect packing exists, never the
reverse).

Git integration stays optional. Version 1 accepts plain directories and path
manifests only; the documented workflow materializes commits with ordinary Git
plumbing outside srcmv. This keeps the core git-free, dependency-free, and
shell-free.

## 2. Goals

- Verify, byte-for-byte, that content deleted between two trees reappears
  somewhere in the after tree.
- Work identically whether or not srcmv performed any of the edits; the audit
  never consults srcmv state.
- Support PR-style review of coding-agent iterations: run once against the base
  and head commits of a branch.
- Credit moves across files, same-file reordering, splits into multiple
  destinations, and multi-target reorganizations.
- Keep exact byte accounting internally and present one-based line ranges using
  existing line semantics for human review.
- Produce a bounded, deterministic JSON report plus a human-readable summary,
  both suitable for long-term comparison across iterations.
- Fail closed: resource exhaustion, truncation, unreadable inputs, or
  unresolvable ambiguity yield `fail`-free but non-`pass` outcomes, specifically
  `inconclusive`, with machine-readable reasons.

## 3. Non-goals

- Proving semantic correctness, compilability, behavioral equivalence, or that
  the conserved bytes are *used* correctly. The report states exactly what it
  proves: bytes removed from the before tree exist again in the after tree.
- Interpreting code. No parsers, grammars, language servers, or symbol ranges.
  Symbol-oriented presentation is explicitly out of scope; line ranges are the
  only human-oriented coordinates in v1.
- Detecting renames as first-class entities. A renamed file is simply "all
  content removed there, reappearing elsewhere"; rename labeling may be added
  later as presentation sugar.
- Modifying any file, creating transactions, or touching the `.srcmv` control
  tree. The command is read-only over its two input roots and writes nothing.
- Replacing per-edit guarantees. srcmv's preview/commit pipeline remains the
  authority for individual operations; this audit reviews aggregate outcomes.
- Bundling Git support, shelling out to `git`, or reading packfiles in v1.

## 4. What the audit does and does not prove

A `pass` verdict means:

> Every maximal byte region that the before tree contained and the after tree
> lost was found, in exact byte equality, at some consumed location in the
> after tree, under one-to-one accounting, with all work completed within
> configured bounds — except for two explicitly reported carve-outs that do
> not defeat a pass by default: bytes below the substantiality floor
> (`insubstantial_residue`, §9.4) and line-edge-equivalent rewrites
> (`formatting_only`, §8.7). Every other ledger term is zero for a pass:
> no `unmatched_substantial`, no `ambiguous_common`, no `unanalyzed` bytes.

It does not mean the code is correct, complete, or behaves the same. A block
can be conserved verbatim into a place where it breaks compilation; duplicated
into two places while being deleted from one; or conserved while unrelated new
code introduces regressions. The report therefore always includes:

- total bytes removed, conserved, unmatched, formatting-classified, ambiguous,
  and new;
- per-region records with source/destination paths, byte ranges, line ranges,
  lengths, and SHA-256 digests; and
- the full list of substantial unmatched regions, bounded by size caps with
  deterministic truncation markers.

A `fail` verdict means at least one substantial removed region has no remaining
exact occurrence available in the after tree under exclusive consumption. An
`inconclusive` verdict means the tool could not complete the analysis reliably
(truncated work, skipped input, or unverifiable ambiguity); it is distinct
from `fail` because no conservation claim is being made either way.

**Known incompleteness (conservative direction).** Matching is greedy
longest-first without backtracking. Rare overlap conflicts between *distinct*
region contents competing for intersecting occurrences can produce a false
`fail` even though a perfect exclusive assignment exists (a small block whose
only surviving copy sits inside a span greedily taken by a longer region).
Greedy assignment never produces a false `pass`: every credited byte is
byte-verified against an exclusively consumed interval. The tool therefore may
under-credit in rare packings but never over-credits; acceptance criterion 2 is
stated as this soundness property, and known-hard packing cases are pinned as
labeled fixtures rather than promised solvable (§15). Bounded backtracking is a
listed future enhancement (§17).

## 5. Repository fit

Investigation summary the implementation can rely on:

- `crates/srcmv-core`: pure domain model with `#![forbid(unsafe_code)]` and
  `deny(missing_docs)`; owns `FileSnapshot` (path, immutable
  `bytes: Arc<[u8]>`, `Sha256Digest`, `LineIndex`, plus physical-identity
  metadata the audit does not need), `LineIndex::from_bytes_with_limits`,
  `metrics_for_range`, `line_start`/`line_end` (one-based lines, zero-based
  half-open `ByteRange`), planner, deterministic CBOR plan-hash v1, and the
  lowerable `ResourceBudget` pattern. The audit's matching engine belongs here
  as a new pure module; it needs no new dependencies beyond `sha2`.
- `crates/srcmv-fs`: canonical workspace opening (`Workspace::open`), strict
  relative-path validation (`parse_relative_path`: rejects absolute, escaping,
  NUL-containing, over-long paths; no symlink traversal; regular files only),
  bounded stable reads (re-read, re-hash, and metadata-signature stability
  checks across up to three attempts), and `SnapshotLimits`
  (per-file 256 MiB, total 1 GiB, 5M lines, 256 MiB line-index memory). There
  is currently **no whole-tree walker**; snapshots are acquired from explicit
  path requirements. The audit adds a bounded recursive reader here, reusing the
  same validation vocabulary. Note `.srcmv` (control) and `.codesplice`
  (legacy control) directory names live in `srcmv-fs`.
- `crates/srcmv-protocol`: strict JSON boundary. Unknown fields and duplicate
  keys are rejected; integers are nonnegative `u64`; digests spell
  `sha256:` + 64 lowercase hex digits; `to_json_line` emits exactly one UTF-8
  JSON value plus LF; `escape_terminal_text` visibly escapes terminal-control
  and bidi characters; response cap is 16 MiB. Selection-v1 demonstrates the
  sanctioned pattern for an independently versioned read-only surface with its
  own schema directory, capabilities discovery, and golden vectors — the audit
  adopts those versioning/schema-directory/golden conventions but deliberately
  reuses the shared frozen error registry for command failures instead of
  selection's independent error envelope (see §12–§13 for the reasoning).
- `crates/srcmv-cli/src/lib.rs`: clap grammar (`Cli`, `Command` enum), global
  `--workspace`, target-independent commands reject `--workspace`
  (`reject_workspace_for_target_independent`), exit-code table (2 request,
  3 conflict, 4 limit/support, 5 transaction, 6 corruption, 8 internal),
  `render_error`/`render_success` output discipline.
- `crates/srcmv-cli/src/preview.rs`: bounded diff precedent — common
  prefix/suffix trimming only, `DETAILED_DIFF_INPUT_BYTES = 8 MiB` per side,
  `DIFF_WORK_UNITS = 10_000_000`, 4 MiB rendered-diff cap, `DIFF_TRUNCATED`
  warning reasons, binary detection (`is_binary`: contains NUL or invalid
  UTF-8), base64 head/tail samples. The audit reuses the binary-detection rule
  and the work-unit accounting style but implements its own matcher.
- `crates/srcmv-cli/src/select.rs`: `SelectionFailure` envelope showing how an
  independently versioned surface plugs into `run_with_io`. The audit module
  mirrors the module shape (own grammar struct, own orchestration function,
  own rendering) but renders failures through the shared `ErrorDto` path.
- Tests: in-crate unit tests named `*_should_*`; `proptest!` property tests with
  committed `proptest-regressions/` files; integration tests drive the real
  binary via `env!("CARGO_BIN_EXE_srcmv")` and `tempfile` workspaces;
  golden vectors under `tests/golden/<surface>-vN/`; fuzz regression seeds in
  `tests/fuzz-regressions/seeds.json` executed by `cargo test --workspace
  --all-features fuzz_regression` (campaign scale via `PROPTEST_CASES=4096`);
  cargo-fuzz targets live beside the crate they stress (`srcmv-lsp/fuzz`);
  below/at/above limit tests follow the `phase9_*` naming style; runnable
  examples live in `examples/NN-name/` wired into `examples/run.sh`.
- Docs: frozen contracts in `docs/*.md` linked from `README.md`; normative
  Draft 2020-12 schemas under `docs/schema/<surface>/` named per artifact
  (`request.schema.json`, `response.schema.json`, `error.schema.json`), each
  surface with a README; prior feature plans lived in `notes/`. This plan
  intentionally lives in `docs/` alongside the other public behavior contracts
  it proposes to join.

## 6. User-facing command

### 6.1 Grammar

```text
srcmv audit-conservation --base TREE --head TREE [OPTIONS]

TREE:
  PATH                    an existing readable directory (recursive)
  @MANIFEST               '@' prefix: text file listing one normalized
                          workspace-relative path per line

OPTIONS:
  --json                  emit the machine report instead of the human summary
  --min-substantial-bytes N   default 32; see §9.4
  --min-fragment-bytes N      default 16; see §8.5
  --strict-uniqueness         downgrade multiplicity-counted matches to
                              inconclusive contributors; see §8.6
  --count-formatting          treat formatting-only deltas as failures too
  --help
```

- The command is target-independent like `capabilities` and
  `selection-capabilities`: `--workspace` is rejected with `INVALID_CLI`
  through the existing helper.
- Exactly one `--base` and one `--head` are required; both must resolve to
  existing readable directories (the `@MANIFEST` form resolves paths against
  the manifest's own directory).
- Base and head roles matter only for direction of accounting (removed-from vs
  reappeared-in); swapping them audits the reverse diff.
- Option validation at parse time (`INVALID_REQUEST` on violation):
  `--min-fragment-bytes` must be ≥ the index window `K = 16` (§8.2) and
  `--min-substantial-bytes` must be ≥ `--min-fragment-bytes`. The defaults
  (16 and 32) satisfy both. This keeps every matchable span reachable through
  the index; the floor is expressible down to 16 bytes, and stricter behavior
  beyond that is achieved with `--count-formatting` plus
  `--strict-uniqueness`.
- **Optional `--json` is a novel combination** for this CLI: existing
  target-independent commands require `--json`, while human-default commands
  are workspace-scoped. The audit is deliberately dual-output like preview.
  Note that `run_with_io`'s pre-scan of raw arguments for `--json` governs
  error rendering, so an invalid invocation containing `--json` renders its
  error as JSON — covered by an integration test asserting that flag-less
  invocations print the human summary to stdout and nothing to stderr, and
  that error rendering follows the `--json` pre-scan in both modes.

### 6.2 Output discipline

Identical to every other command: `--json` writes exactly one bounded UTF-8
JSON value plus LF to stdout; human mode writes a plain-text summary to stdout;
all diagnostics go to stderr with terminal-control and bidi characters escaped
through `escape_terminal_text`. On any failure the process emits the standard
error envelope (see §12) and writes no partial report.

Exit codes for completed audits: **0**, always. Verdict is data, not process
failure, so automation selects with e.g.
`jq -e '.verdict == "pass"'. Rationale: the existing code table conflates
nonzero with "command did not complete", and inventing verdict-exits would
collide with request/conflict/limit semantics used by callers today. (Revisit
if CI ergonomics demand otherwise; see open questions.)

Failure exits reuse the existing table unchanged (§12).

### 6.3 Supplying before/after trees

Two ingestion forms share one validated internal representation:

1. **Directory mode (default).** Recursively read the directory. Rules:
   - canonicalize the root once (same contract as `Workspace::open`);
   - traverse deterministically in sorted byte order, depth-first;
   - regular files only; symbolic links, fifos, sockets, devices are rejected
     fail-closed with `SYMLINK_NOT_ALLOWED` / `UNSUPPORTED_FILE_TYPE`;
   - skip exactly three root-level entries by name: `.git`, `.srcmv`,
     `.codesplice` (tooling metadata, not content); everything else — including
     other dotfiles — is audited content;
   - empty files are valid; empty directories contribute nothing.
2. **Manifest mode (`@FILE`).** A UTF-8 text file with one normalized relative
   path per line (`#` comments and blank lines allowed). Each path is validated
   by the existing relative-path rules and resolved against the manifest's
   parent directory; missing paths are hard errors. Manifests bound large
   repositories to a reviewable subset and keep runs reproducible.

Both sides accept either form independently, enabling mixed audits (for
example, a full head checkout against a manifest of touched base files).

Git workflows remain external and documented (README + docs page):

```bash
BASE=$(mktemp -d); HEAD=$(mktemp -d)
git -C repo worktree add --detach "$BASE" "$PR_BASE_SHA"
git -C repo worktree add --detach "$HEAD" "$PR_HEAD_SHA"
srcmv audit-conservation --base "$BASE" --head "$HEAD" --json > audit.json
git -C repo worktree remove "$BASE"; git -C repo worktree remove "$HEAD"
```

or losslessly with plumbing: `git archive "$SHA" | tar -x -C "$DIR"`. Native
ref support (`--base-ref origin/main`) is deferred; it would require either a
Git crate or spawning `git` without a shell, both of which deserve their own
trust review (see open questions).

## 7. Ingestion model

New public API in `srcmv-fs`:

```rust
pub struct AuditTreeLimits {
    // Private fields, matching house style: SnapshotLimits / RequestLimits.
    // Constructed with #[must_use] pub const fn new(...) -> Self plus
    // impl Default for release values.
}
pub struct AuditTreeFile {
    pub path: WorkspaceRelativePath,
    pub bytes: Arc<[u8]>,
    pub digest: Sha256Digest,       // SHA-256, computed during stable read
}
/// A file observed but not ingested, with the reason it was skipped.
pub struct SkippedInput {
    pub path: WorkspaceRelativePath,
    pub reason: SkipReason,         // Vanished | UnstableReads | ...
}
pub fn read_audit_tree(root: &Path, limits: AuditTreeLimits)
    -> Result<(Vec<AuditTreeFile>, Vec<SkippedInput>), FsError>;
// Files sorted by normalized path bytes; skips reported alongside.
```

Skip-versus-error boundary, stated mechanically:

- **Per-file instability becomes a skip**, not an abort: a file that vanishes
  mid-walk or fails stable-read verification across all three attempts is
  recorded in `SkippedInput` and produces the `unreadable_input_skipped`
  inconclusive reason (§9.3). A partially read tree cannot support a `pass`,
  so any skip forces that verdict downstream.
- **Everything else stays a hard error** through the standard envelope:
  root-level failures (missing/unreadable root, canonicalization),
  symlink/special-file rejection, limit violations, and manifest problems
  (including missing entries — knowable before any analysis, so the audit
  fails fast rather than reporting inconclusive). The asymmetry versus
  per-file skips is deliberate: policy violations and structural problems
  abort, while mid-read instability of otherwise-valid content merely makes
  results unreliable.
- Limit-set discipline matches house rules exactly: trusted *user
  configuration* may lower limits but never raise them; trusted *test*
  constructors may pass arbitrary values (including `u64::MAX`) for boundary
  tests, as `LineIndex` tests already do with
  `from_bytes_with_limits(bytes, u64::MAX, u64::MAX)`.
- `max_files = 20_000` deliberately exceeds `MAX_SNAPSHOT_IDENTITIES = 1,000`:
  edit batches touch explicit paths, while an audit must admit whole checkouts;
  the per-tree byte cap (1 GiB) remains the primary bound. The divergence and
  its rationale are recorded here and in `docs/resource-limits.md`.

- Files are read with the existing bounded-stable-read discipline (re-read,
  re-hash, metadata-signature stability verification) even though the audit is
  offline; this keeps behavior uniform if a tree is mutated mid-read.
- All limits use checked arithmetic charged before allocation or I/O and are
  lowerable through the constructor for boundary tests, mirroring
  `SnapshotLimits::new`.
- Line indexes are **not** built during ingestion. They are constructed lazily,
  per file, only when a report needs line numbers for a specific range, under
  the existing per-call line/memory caps. This avoids paying 5M-line indexing
  cost for audits that end in `fail` early.

Path classification (pure function in core, trivially unit-testable):

| Class | Condition | Role |
|---|---|---|
| `unchanged` | same path, equal digest | none |
| `modified` | same path, different digest | source of removal regions |
| `deleted` | path only in base | whole file becomes removal regions |
| `created` | path only in head | destination-only; feeds corpus and new-byte totals |

Case sensitivity follows the filesystem's spelling as captured; no folding.
Files identical in name but differing only in case on case-insensitive local
filesystems behave like any modified pair.

## 8. Matching engine (pure, in `srcmv-core::conservation`)

No filesystem access, no clocks, no randomness: identical inputs produce
byte-identical reports. Everything below is expressed in exact bytes over
`Arc<[u8]>` slices.

### 8.1 Removed-region extraction (per modified/deleted file)

Goal: enumerate maximal byte spans present in the before bytes that the paired
after bytes lost, aligned to true edit boundaries rather than line guesses.

Pipeline per changed pair `(before, after)`:

1. Trim the common byte prefix and suffix (existing preview approach).
2. Split both middles into logical lines using `LineIndex` semantics.
3. Build anchor points from **unique lines**: a line whose trimmed-content hash
   occurs exactly once in the before middle and exactly once in the after
   middle is an anchor pair. Take the longest increasing subsequence of
   anchor pairs (by before-position) — patience-diff style — breaking ties by
   choosing the lexicographically smallest sequence of after-positions via the
   standard patience-stack reconstruction, so the anchors are deterministic.
   Unique-line anchoring is O(n) expected with a hash map plus O(a log a) for
   the LIS.
4. Between consecutive anchors lies a *gap*: before-gap bytes and after-gap
   bytes. For each gap, trim common byte prefix/suffix of the two gap slices;
   whatever remains on the before side is one candidate removed region (the
   after-side remainder is recorded as this region's paired insertion context,
   used only by the formatting check in §8.7). Only gap-derived regions from
   modified pairs carry paired context; deleted-file and fallback regions have
   none and are therefore never formatting-classified.
5. **Fallback applies per maximal anchor-free stretch**, not per gap size: if a
   stretch of the before middle has no enclosing anchor pair on both sides
   (anchors too sparse to bracket it) or the anchor pass exceeds its work
   budget, that stretch's before-span becomes one coarse removed region.
   Bracketed gaps are unaffected regardless of their size — a large but
   bracketed gap costs only linear trimming. Coarseness costs match precision,
   never soundness, because matching happens globally next.
6. Deleted files contribute one region covering the whole file (empty files
   contribute nothing); such regions carry no paired insertion context.

Regions carry: source path, half-open byte range, length, SHA-256 of the exact
bytes, lazily derivable line span. Adjacent regions separated by fewer than
`merge_gap_bytes` (default 0) are not merged in v1; merging is unnecessary
because fragment matching tolerates interior gaps (§8.5).

Complexity: linear in input plus LIS cost of anchor count; worst-case fallback
is constant time per file. Total extraction work charges the budget in §10.

### 8.2 Head-corpus index (built once)

Build a single index over every created + unchanged + modified-after file in
the head tree (deleted files obviously cannot host content):

- Concatenate virtually: positions are `(file_index, offset)` pairs; matches
  never cross file boundaries.
- Rolling-hash table keyed on fixed windows of `K = 16` bytes at stride 1,
  mapping `u64` window hash → sorted vector of positions. Hash function: a
  dependency-free multiply-xor mix (wyhash-style finalizer) over raw bytes,
  implemented locally; collisions are harmless because every candidate is
  verified byte-for-byte.
- **Normative resolution constraint:** `K = 16` equals the default
  `--min-fragment-bytes`, and §6.1 requires `--min-fragment-bytes ≥ K` and
  `--min-substantial-bytes ≥ --min-fragment-bytes`. Every matchable span is
  therefore ≥ `K` bytes and contains at least one full index window, so
  probing is always defined. Whole regions shorter than `K` are necessarily
  below the substantiality floor (≥ 16): they are never probed and their full
  length flows directly to the `insubstantial_residue` ledger term (§8.5,
  §9.2), except that gap-derived modified-pair regions are first offered to
  the formatting check of §8.7, which runs before any length-based
  short-circuit.
- **Near-tail windows:** a window exists at every offset with
  `offset + K ≤ file_len`. An occurrence flush against EOF is still indexed
  through its interior windows; candidate verification then reads exactly
  `region_len` bytes, so end-of-file positions match correctly.
- **Hot-bucket suppression:** any key whose position vector exceeds
  `hot_bucket_limit` (default 64) is marked hot and excluded from anchoring.
  This makes pathological inputs (runs of `' '`, minified bundles, lockfiles)
  linear instead of quadratic; hot stretches can still be matched through
  neighboring cold windows (§8.3–§8.5). Note that hotness depends on head-tree
  frequency: identical removed content may anchor cleanly in one repository
  and be unanchorable in another that contains many unrelated copies.
- Memory for the index charges the audit budget; exceeding it aborts with
  `RESOURCE_LIMIT_EXCEEDED` before allocation completes (checked arithmetic,
  consistent with house style). Expected footprint ≈ 16 bytes/position × ≤
  total-head-bytes positions, so the default 1 GiB head cap dominates; the
  index memory cap defaults to 2 GiB with the total-bytes cap effectively
  bounding it lower.

### 8.3 Exact whole-region matching

Process regions in deterministic priority order: longer length first, ties by
(source path bytes, then start offset ascending). For each region `r`:

1. Probe the index with the first cold K-window of `r`: scan forward through
   `r` to the first non-hot window. If no window of `r` is cold **after this
   forward scan is exhausted** (the neighbor fallback having been attempted),
   classify `r` `ambiguous_common` per §8.6 — never silently skip it.
2. Verify candidates in ascending `(path, offset)` order by comparing all
   `r.len()` bytes. Skip candidates whose span intersects any already-consumed
   interval in that file (§8.6) — overlap means the occurrence is spent.
3. First verified, unconsumed candidate wins: mark `[start, start+len)`
   consumed, record the match, continue with the next region.
4. If candidates existed but none verified unconsumed, defer `r` to fragment
   matching.

Greedy priority is complete for identical duplicates (every copy of duplicated
content contains an escape copy for later equal regions), but not for distinct
contents competing for intersecting spans; §4 documents this conservative
incompleteness and §15 pins labeled fixtures for it.

### 8.4 Splits, reorderings, cross-file moves

Because matching is global and order-free, these cases need no special logic:

- *Moved block:* exact occurrence exists somewhere → matched wherever it is.
- *Same-file reorder:* identical; consumption tracks the surviving copy.
- *Block split across files:* whole-region match fails, fragment matching
  succeeds piecewise (§8.5).
- *Renamed file:* old path contributes whole-file regions; they match inside
  the new path's bytes.

### 8.5 Fragment matching (split tolerance)

For each deferred region, walk it left to right collecting disjoint matched
fragments:

1. From current offset `o`, probe the index with the first cold window at or
   after `o`; extend each candidate rightward byte-wise (charging work units)
   while bytes equal and the span `[candidate_start, candidate_start + t)`
   stays unconsumed; also extend leftward into `[region_start, o)` only while
   those before-side bytes are not yet covered by an earlier accepted fragment
   **and** the mirrored head interval is unconsumed — leftward extension
   performs the same consumption checks as rightward and never re-covers a
   consumed or already-fragment-matched byte.
2. Accept the longest verified extension ≥ `--min-fragment-bytes` (default 16
   = `K`, so every accepted fragment contained at least one full probe window),
   consume its head span, jump `o` past the fragment's region-side end.
3. If no acceptable extension, advance `o` by one byte (bounded scans only;
   every compared byte charges work units).
4. Terminate at region end or when the global matching work budget is
   exhausted; leftover bytes become the region's residue.

Because extensions stop at consumed bytes, self-overlapping periodic strings
yield one maximal fragment per surviving run and are classified honestly;
fragments can never overlap each other (`o` monotonically advances past each
acceptance) nor previously consumed spans.

After fragment matching:

- `conserved_bytes(r) = Σ fragment lengths`
- residue `< --min-substantial-bytes` → classified `insubstantial_residue`
  (reported, does not affect verdict; see §9.4)
- residue `≥ --min-substantial-bytes` → `unmatched_substantial`

(The acceptance threshold for individual fragments remains
`--min-fragment-bytes`; only the residue verdict boundary uses the
substantiality floor.)

Fragments record their own destinations; one region may list several
destination spans, including spans in different files — this is how a block
split across two new files is presented to a reviewer.

### 8.6 One-to-one accounting and duplicates

Consumption is the soundness core:

- Per head file, consumed intervals live in a sorted `Vec<(u64, u64)>` merged
  on insert; overlap checks are binary searches. A candidate overlapping any
  consumed byte is rejected outright — one physical occurrence can satisfy at
  most one region, so shared boilerplate cannot be double-counted.
- Identical repeated blocks are handled by counting, not guessing: if the same
  region content was removed twice and two distinct occurrences remain, each
  consumes its own occurrence; the pairing of which-copy-to-which is arbitrary
  and flagged `pairing_ambiguous: true` on the affected records. Byte-level
  conservation is still provable (multiset containment with exclusive
  consumption — every copy of duplicated content contains an escape copy for
  later equal regions, so ascending-order probing always finds one), so these
  matches count toward `pass`.
- `--strict-uniqueness` flips this: any record relying on multiplicity
  counting (its content had more removed copies than one, or more surviving
  candidates than consumed) contributes `strict_uniqueness_ambiguity` reasons
  instead, pushing the verdict toward `inconclusive`. Default off; reviewers
  who need copy-identity claims should enable it.
- **`ambiguous_common` is verdict-visible.** A region whose every window is
  hot after the §8.3 forward scan cannot be anchored, and no byte of it may be
  credited or forgiven: its full length enters the ledger as
  `ambiguous_common_bytes`, and its presence forces `verdict = INCONCLUSIVE`
  with reason `ambiguous_common` (§9.1–§9.3). This is deliberately stricter
  than the multiplicity case above: with anchoring impossible there is no way
  to verify any occurrence, so claiming conservation would be unsound.
  Deleting a whitespace-heavy or lockfile-like file therefore yields a
  truthful "cannot decide" rather than a manufactured pass; scoping such
  content out via manifests is the reviewer's remedy.

### 8.7 Formatting changes and partial edits

- Byte purity is preserved: a CRLF→LF rewrite or indent change is *not* a
  conservation match. Exactness is never weakened by an option.
- **Sequencing is normative:** the formatting check runs at extraction time,
  immediately after gap trimming (§8.1 step 4) and *before* any probing,
  length-based short-circuit, or matching. Every gap-derived modified-pair
  region is compared with its paired insertion context under line-edge
  normalization (below). Qualifying regions contribute their **full length**
  to the `formatting_only` ledger term and never enter §8.3/§8.5, so no byte
  is ever counted twice (§9.2 holds by construction). Non-qualifying regions
  proceed through exact and fragment matching normally; their post-matching
  residue is classified per §8.5.
- **Line-edge normalization** is the transformation `T`: replace every CRLF
  and lone CR with LF, split on LF, strip leading and trailing ASCII
  whitespace from each line, then join the lines with LF. A region qualifies
  iff `T(region) == T(paired_remainder)`. This catches re-indentation,
  trailing-space churn, EOL conversion, and final-newline additions —
  including the degenerate gaps of a pure EOL conversion, where each removed
  region is a single CR byte whose `T` image is empty and equals its empty
  remainder. It deliberately does *not* equate intra-line spacing changes or
  deleted line breaks: joining two statements changes the joined sequence and
  stays unmatched.
- Applicability is narrow by construction: deleted-file regions and fallback
  regions carry no paired remainder (§8.1) and are never formatting-
  classified. Known caveat, documented for reviewers: content inside multi-
  line string/template literals whose lines begin or end with spaces can be
  misclassified as formatting; `--count-formatting` restores strict failure
  semantics for reviewers who care. A second consequence of extraction-time
  classification is deliberate under-crediting: a formatting-equivalent block
  that also survives verbatim elsewhere is reported as rewritten rather than
  conserved; the extra copy surfaces in new content instead.
- Formatting bytes count toward neither conservation nor failure by default;
  they appear in their own totals and listings so rewritten-but-equivalent
  edits are clearly visible. `--count-formatting` promotes them to failures.
- Partial edits decompose under this ordering without special cases: the
  anchor/gap extraction trims to exact changed bytes; an edited tail whose
  bytes equal its counterpart under `T` qualifies at extraction time;
  anything else proceeds to matching, and only genuinely unmatched residue
  remains.

### 8.8 New content

After matching completes, compute insertion spans (after-side gap remainders of
modified pairs, plus whole created files), subtract every consumed interval
that lies inside them, and sum the survivors into `new_bytes` — a removed block
landing amid an insertion counts as conserved there, not as new. The report
lists the largest new spans (deterministic top-N, default 50) with
path/range/digest. New content never affects the verdict; it is reported
because "clearly reporting genuinely new content" is a stated purpose of the
audit. A report-level consistency check mirrors §9.2: per head file,
`consumed ∪ new_spans` never overlaps, and their union stays within file
bounds.

## 9. Result model

### 9.1 Verdict derivation

Evaluated strictly in this order after matching completes:

```text
if any fatal condition (§9.3):            verdict = INCONCLUSIVE
else if unmatched_substantial_bytes > 0:  verdict = FAIL
else if count_formatting && formatting_bytes > 0: verdict = FAIL
else:                                     verdict = PASS
```

`ambiguous_common_bytes > 0` is itself a fatal condition (reason
`ambiguous_common`, §8.6), so it can never reach the pass branch.
`unmatched_substantial_bytes == 0` requires every removed region to have been
exactly matched wholly or fragmentarily with zero substantial residue. There
is no partial credit threshold: conservation is byte-complete or the audit
says `fail`.

### 9.2 Machine-checkable invariant

For every completed run (any verdict):

```text
Σ region lengths
  = conserved_exact + conserved_fragments
  + unmatched_substantial + insubstantial_residue + formatting_only
  + ambiguous_common + unanalyzed
```

Every removed byte lands in exactly one term; a region cut short by budget
exhaustion contributes its unprocessed remainder to `unanalyzed`. Per head
file, consumed intervals are pairwise disjoint and within file bounds, and
`consumed ∪ new_spans` never overlaps (§8.8). All invariants are asserted in
debug builds, tested by property tests, and recomputed in the report builder
so a bug fails loudly instead of fabricating a verdict.

### 9.3 Fatal conditions forcing `inconclusive`

Each carries a structured reason string in `report.inconclusive_reasons[]`:

- `work_budget_exceeded` — extraction, index build, or matching ran out of
  work units (§10). Processing stops deterministically at the first exhausted
  pool; totals reflect completed work only and remaining bytes are ledgered as
  `unanalyzed`.
- `memory_budget_exceeded` — with a mechanical boundary: index-build failures
  occurring before any region has been matched surface as the standard
  `RESOURCE_LIMIT_EXCEEDED` envelope error (nothing worth reporting was
  learned); failures after matching began downgrade to this inconclusive
  reason so partial results remain inspectable.
- `unreadable_input_skipped` — one or more files were skipped during ingestion
  (`SkippedInput`, §7) because they vanished mid-walk or failed stable-read
  verification; the tree changed underneath, so no verdict is reliable;
- `strict_uniqueness_ambiguity` — only under `--strict-uniqueness`;
- `ambiguous_common` — at least one region could not be anchored anywhere in
  the head tree after hot-bucket suppression and neighbor-window fallback
  (§8.3, §8.6);
- `report_truncated` — the serialized report overflowed its byte cap and
  listings were dropped (§11.3); absence of listed unmatched regions must
  never imply their absence in fact.

### 9.4 Substantiality floor

`--min-substantial-bytes` (default 32) defines the smallest residue worth
failing over. Defaults chosen so incidental churn (an import line tweak, a
brace) surfaces in reports without flipping branch audits to `fail`, while any
real block survives the floor. The floor applies only to residue and to whole
regions too short to probe: a region containing at least one index window
(length ≥ `K`, §8.2) is matched and reported normally regardless of its size,
while shorter regions skip probing entirely — after the §8.7 formatting check
— and land in `insubstantial_residue`. Per §6.1 the floor cannot be set below
`--min-fragment-bytes`, which cannot be set below `K = 16`; maximal practical
strictness is therefore 16-byte floors, tightened further with
`--count-formatting` and `--strict-uniqueness`.

### 9.5 Binary content lifecycle

Binary files participate in every stage unchanged: extraction splits bytes on
terminators regardless of content, head files are indexed as match
destinations regardless of content (a moved `.png` passes), and digest-equal
pairs are exempt as always. Binary-ness (`is_binary`: contains NUL or invalid
UTF-8, reused from preview) affects presentation only — line ranges are
reported as `null` for binary files, and human listings show byte ranges and
digests without line numbers.

## 10. Resource limits and denial-of-service protection

New table for `docs/resource-limits.md` (defaults; trusted test constructors
may lower but not raise, matching house rules):

| Resource | Limit |
|---|---:|
| Files per tree | 20,000 |
| Individual file bytes | 256 MiB |
| Total bytes per tree | 1 GiB |
| Path bytes | 4,096 |
| Manifest entries / bytes | 20,000 / 4 MiB |
| Removed regions retained | 200,000 |
| Index memory | 2 GiB (effectively bounded by tree bytes) |
| Hot-bucket positions | 64 |
| Extraction work units | 200,000,000 (≈ bytes scanned/hashed) |
| Matching work units | 400,000,000 (≈ bytes compared/probed) |
| Report regions listed | 2,000 (largest first, then source order) |
| Report new-content spans | 50 |
| Serialized report | 16 MiB |

DoS posture:

- Symlink cycles are impossible (symlinks rejected); deep trees are bounded by
  file count and path-length validation inherited from the existing walker
  rules.
- Quadratic blowups are prevented structurally: hot-bucket suppression caps
  candidate lists; every probe/comparison charges work units; budgets convert
  attacks into `inconclusive` with a reason, not hangs.
- **Work-budget mechanics:** the extraction and matching budgets are global
  pools charged by every phase; there are no per-region quotas. When a pool is
  exhausted, processing stops at that point in the deterministic region order,
  remaining bytes are ledgered as `unanalyzed`, and the verdict becomes
  `inconclusive` with `work_budget_exceeded` (§9.3). No ordering or option
  combination can turn exhaustion into a verdict-bearing pass.
- Checked arithmetic precedes every allocation and I/O step; limits are charged
  at first knowledge, exactly like snapshot/planner accounting.
- The serialized report is assembled under a running byte budget; overflow
  truncates listings per the §11.3 policy table and trips `report_truncated`.
- Nothing writes to disk; the command holds no locks; concurrent audits of the
  same trees are safe.

Performance targets on qualified hardware (recorded later in
`docs/performance-baseline.json`; new entries must respect the invariants the
existing hardening tests assert over that file): ingest + classify 100 MB/tree
in seconds; index build ~1 s per 100 MB; typical agent-iteration PRs
(< 5 MB changed, < 2,000 regions) complete well under one second.

## 11. Report formats

### 11.1 JSON report (`--json`)

Normative schema: `docs/schema/conservation-audit-v1/report.schema.json`
(Draft 2020-12, unknown fields rejected, one value + LF on the wire).
Illustrative shape:

```json
{
  "conservation_audit_version": 1,
  "verdict": "pass",
  "inconclusive_reasons": [],
  "options": {
    "min_substantial_bytes": 32,
    "min_fragment_bytes": 16,
    "strict_uniqueness": false,
    "count_formatting": false
  },
  "inputs": {
    "base":  {"kind": "directory", "files": 142, "bytes": 3204551},
    "head":  {"kind": "manifest",  "files": 143, "bytes": 3211002}
  },
  "totals": {
    "removed_regions": 57,
    "removed_bytes": 12344,
    "conserved_bytes": 12002,
    "conserved_exact_bytes": 11670,
    "conserved_fragment_bytes": 332,
    "formatting_bytes": 210,
    "insubstantial_residue_bytes": 22,
    "unmatched_substantial_bytes": 110,
    "ambiguous_common_bytes": 0,
    "unanalyzed_bytes": 0,
    "new_bytes": 1905,
    "pairing_ambiguous_regions": 3,
    "ambiguous_common_regions": 0
  },
  "regions": [
    {
      "id": 0,
      "source_path": "src/legacy/parser.ts",
      "source_byte_range": [10240, 11792],
      "source_line_range": [411, 468],
      "length": 1552,
      "sha256": "sha256:9f2c…",
      "status": "conserved",
      "pairing_ambiguous": false,
      "destinations": [
        {"path": "src/parse/core.ts", "byte_range": [512, 2064], "line_range": [19, 76]},
        {"path": "src/parse/util.ts", "byte_range": [88, 96], "line_range": [4, 4]}
      ]
    }
  ],
  "unmatched": [
    {
      "id": 7,
      "source_path": "src/legacy/parser.ts",
      "source_byte_range": [220, 260],
      "source_line_range": [9, 9],
      "length": 40,
      "sha256": "sha256:ab41…",
      "status": "unmatched_substantial"
    }
  ],
  "formatting": [],
  "new_content": [
    {"path": "src/parse/index.ts", "byte_range": [0, 431], "line_range": [1, 12],
     "length": 431, "sha256": "sha256:0be3…"}
  ],
  "warnings": []
}
```

Field rules:

- Byte ranges are zero-based half-open pairs `[start, end)`; line ranges are
  one-based inclusive `[first_line, last_line]` under existing terminator
  semantics, computed lazily and omitted (`null`) for binary files
  (`is_binary` rule reused: contains NUL or invalid UTF-8; see §9.5).
- `sha256` values use the standard prefixed lowercase spelling.
- The `totals` object contains exactly the §9.2 ledger terms plus counts;
  the identity `removed_bytes = Σ ledger terms` holds on every report.
- Destinations within one region are ordered by `(path, offset)`; fragments
  appear as separate destination entries, making splits visible.
- Digests enable reviewers to verify any claim independently:
  `sha256(head_file[dest_range]) == sha256(base_file[src_range])`.

### 11.2 Human report (default)

Compact summary followed by actionable listings; paths and any echoed content
fragments pass through `escape_terminal_text`:

```text
conservation audit v1
base:  /tmp/base  (142 files, 3204551 bytes)
head:  /tmp/head  (143 files, 3211002 bytes)
verdict: FAIL
removed 12344 B | conserved 12002 B | unmatched 110 B | formatting 210 B | new 1905 B
unmatched regions (1):
  - src/legacy/parser.ts [220..260) (lines 9..9) 40 B sha256:ab41…
top new content:
  - src/parse/index.ts [0..431) 431 B
inconclusive reasons: (none)
warnings: (none)
```

The human renderer is bounded at 4 MiB like preview's rendered diff
(`DIFF_OUTPUT_BYTES` precedent); overflow truncates listings with an explicit
`(truncated)` marker. Inconclusive reasons are always rendered when present —
fail-closed UX depends on showing *why* an audit could not decide.

### 11.3 Bounded rendering

Report assembly mirrors preview's budget discipline: totals, verdict, and
reason lists are reserved first; listings fill the remainder under the 16 MiB
cap. One policy table governs every cap:

| Cap exceeded | Mechanism | Consequence |
|---|---|---|
| `regions[]` entry cap (2,000) | deterministic drop, smallest conserved regions first | silent drop + `omitted_region_count` sibling; **not** fatal |
| `unmatched[]` / `formatting[]` caps | deterministic drop of lowest-priority entries | sets `report_truncated`; **fatal** (§9.3) — unlisted unmatched regions must never imply absence |
| serialized report > 16 MiB | running byte budget during assembly | same as above: drop listings; if even the reserved skeleton (totals, verdict, reasons) cannot fit, emit the standard error envelope with `RESOURCE_LIMIT_EXCEEDED` instead of a report |

Truncation therefore can never manufacture a `pass`: any fatal row downgrades
the verdict to `inconclusive`.

## 12. Errors and diagnostics

Completed audits exit 0 with a verdict-bearing report. Command failures reuse
the shared `ErrorDto` envelope and the frozen protocol-v1 registry codes
unchanged — deliberately *not* a selection-style independent envelope, because
every failure mode here maps cleanly onto existing codes and §13 forbids
registry additions:

| Exit | Codes used by the audit |
|---:|---|
| 2 | `INVALID_CLI`, `INVALID_REQUEST` (bad manifest syntax, missing manifest entries, bad option values) |
| 4 | `RESOURCE_LIMIT_EXCEEDED`, `SYMLINK_NOT_ALLOWED`, `UNSUPPORTED_FILE_TYPE` |
| 8 | `IO_ERROR` (root missing/unreadable — Internal category per the frozen registry), `INTERNAL_ERROR` |

An integration test asserts the actual exit code for `--base /nonexistent`
(exit 8 via `IO_ERROR`) so the plan's table and the binary cannot drift.
`IO_ERROR` redacts absolute paths in structured context per existing policy.
No new warning codes are required; observation-staleness does not apply to an
offline two-tree comparison. If hard memory caps trip before any analysis
completes, the command fails closed with `RESOURCE_LIMIT_EXCEEDED` rather than
emitting a hollow `inconclusive` (§9.3 states the exact boundary).

## 13. Protocol, versioning, and backward compatibility

- **Independent surface.** `conservation_audit_version` starts at 1 with its
  own schema directory and goldens, exactly like `selection-v1`. The frozen
  protocol-v1 request/response schemas, error/warning registries, plan-hash
  v1, and transaction-record v1 gain no fields and no codes.
- **Evolution rule.** Any wire-visible change (new field, new status enum
  value) bumps `conservation_audit_version` and adds a new schema file;
  unknown-field rejection makes silent extension impossible, which is the
  desired strictness.
- **Discovery.** `srcmv --version` continues to answer globally; no separate
  capabilities command is needed because the surface has no runtime-dependent
  features — no servers to discover, and its filesystem behavior inherits the
  existing unix-only platform support from `srcmv-fs`.
- **Backward compatibility.** Purely additive: older clients never invoke the
  subcommand; newer binaries change no existing command's output byte-for-byte.
  CI workflows pinning behavior see no diffs. The only shared-code changes are
  additive exports (`read_audit_tree`, audit DTOs) and one new `Command`
  variant.

## 14. Crate and module design

```text
crates/srcmv-core/src/conservation/
  mod.rs        public engine API, doc-commented types
  classify.rs   path classification (pure)
  extract.rs    region extraction: trim, lines, unique-anchor LIS, gaps
  index.rs      rolling-hash corpus index, hot buckets, work accounting
  match.rs      exact + fragment matching, interval consumption
  format.rs     line-edge normalization (EOL/indent) classification
  verdict.rs    derivation + invariant recomputation
crates/srcmv-fs/src/audit_tree.rs     read_audit_tree, manifests, limits, skips
crates/srcmv-protocol/src/audit.rs    report DTOs + serialization budget
crates/srcmv-cli/src/audit.rs         grammar + orchestration + rendering
docs/schema/conservation-audit-v1/{report.schema.json,README.md}
tests/golden/conservation-audit-v1/*.json
tests/fixtures/conservation/…         small hand-built trees (see §15)
crates/srcmv-core/fuzz/               cargo-fuzz target for the engine
examples/11-conservation-audit/       runnable walkthrough
```

Engine types sketch:

```rust
pub struct ConservationInput<'a> {
    pub base: &'a [AuditTreeFile],
    pub head: &'a [AuditTreeFile],
    pub options: ConservationOptions,
    pub budget: ConservationBudget,   // lowerable, Default = release values
}
pub struct ConservationOutcome {
    pub verdict: Verdict,                 // Pass | Fail | Inconclusive
    pub reasons: Vec<InconclusiveReason>,
    pub totals: Totals,
    pub records: Vec<RegionRecord>,       // bounded upstream of serialization
}
pub fn audit_conservation(input: ConservationInput<'_>) -> ConservationOutcome;
```

Dependency direction: cli → fs/protocol/core; protocol → core; core depends on
nothing new. The engine never touches `std::fs`.

## 15. Test plan

Unit tests (each module, `*_should_*` names):

- classification table incl. digest ties, created/deleted edges;
- extraction: known small diffs produce exact expected regions; per-stretch
  fallback triggers on sparse-anchor synthetic input while bracketed gaps stay
  fine-grained; LIS tie-breaking determinism; CRLF/CR/LF mixes; unterminated
  tails; empty files; fully-deleted files;
- index: hot-bucket suppression thresholds, collision safety (forced via
  crafted keys), file-boundary isolation, near-EOF window placement;
- matching: exact hit, miss→fragment, split across two files, reorder, overlap
  rejection, longest-first priority, deterministic tie-breaking;
- **length boundaries at `K`:** whole regions and fragments of `K-1`, `K`,
  and `K+1` bytes (16/17 with defaults), including occurrences flush against
  file end;
- consumption: disjoint-interval invariant under adversarial orders;
- normalization: line-edge equivalence classes (indent churn, EOL swap,
  final-newline) and near-misses that must NOT classify (intra-line spacing,
  deleted line breaks); a lone-CR region shorter than `K` from a pure
  CRLF→LF conversion must classify `formatting_only` and flip the audit to
  `fail` under `--count-formatting`;
- verdict truth table incl. every inconclusive reason; §9.2 ledger identity
  recomputation incl. `ambiguous_common` and `unanalyzed` terms.

Property tests (proptest, committed regressions), split by direction:

- **Soundness property (no false pass):** for randomized inputs constructed by
  scatter-plus-corruption, a `pass` verdict implies every substantial removed
  byte is byte-verified against exclusively consumed intervals — recheck the
  invariant from the report alone.
- **Scatter property (completeness on friendly packings):** partition random
  base bytes into chunks, permute and scatter them into random head files with
  padding noise ⇒ verdict `pass`, conserved == removed minus declared
  formatting/residue. Friendly packings only: scattered copies never overlap
  each other, which is the domain where greedy matching is complete.
- **Labeled hard-packing fixtures:** deterministic cases where distinct region
  contents compete for intersecting occurrences, with lengths above every
  floor so outcomes are verdict-visible. Canonical fixture: base file `f1`
  removes `r1` (48 bytes: `"0123456789abcdef"` repeated three times) and
  `r2` (32 bytes: the last 32 bytes of the 64-byte composite
  `C = r1 ++ "y"×16`, i.e., `C[16..48]`, which intersects `r1`'s span); head
  contains `C` once inside `a.txt` at a low offset plus one escape copy of
  `r1` inside `z.txt` (sorting after `a.txt`). Greedy longest-first takes
  `r1`'s ascending-first candidate — the intersecting copy inside `C` — after
  which `r2`'s only occurrence overlaps consumed bytes, leaving 32 bytes of
  substantial residue ⇒ `fail`, although the perfect assignment (`r1 → z.txt`,
  `r2 → C[16..48]`) exists. Assert the documented conservative outcome
  (`fail`, or `inconclusive` under strict options), never a fabricated pass;
  never promise solvability.
- **Mutation property:** flip/copy/delete bytes in the scattered copies ⇒
  outcome ∈ {fail} ∪ inconclusive-with-reason, never `pass` with
  `unmatched_substantial_bytes == 0` unless the mutation was provably absorbed
  (assert which).
- **Determinism:** two runs over identical inputs ⇒ identical serialized
  reports.
- **Budget conversion:** lowered budgets ⇒ clean `inconclusive` reasons, no
  panics, valid partial totals satisfying §9.2.

Integration tests (`crates/srcmv-cli/tests/audit_cli.rs`, real-binary style of
`preview_cli.rs` with two temp dirs):

- move-across-files pass; split-block pass with multi-destination record;
- reorder-only pass; rename-as-delete+create pass;
- deleted-block fail with correct unmatched record;
- duplicate boilerplate: two removals vs two survivors pass with
  `pairing_ambiguous`; one survivor fails; `--strict-uniqueness` downgrades;
- formatting-only delta: default pass-with-report, `--count-formatting` fails;
- new file/deleted file accounting; empty trees; identical trees;
- symlink in tree → exit 4; oversized file with lowered limits → exit 4;
- missing root → exit 8 via `IO_ERROR` (pins the §12 table against drift);
- manifest mode happy path; missing entry and bad syntax are hard errors —
  exit 2 via `INVALID_REQUEST` (missing entries are knowable before any
  analysis, so the audit fails fast rather than reporting inconclusive);
- flag-less invocation prints the human summary to stdout and nothing to
  stderr; error rendering follows `run_with_io`'s `--json` pre-scan;
- `--json` single-line validity, schema conformance (via `jsonschema`
  dev-dependency pattern already used in protocol tests), human output
  escaping of hostile filenames;
- exit code 0 on all three verdicts.

Golden vectors: `tests/golden/conservation-audit-v1/` generated from fixtures
in `tests/fixtures/conservation/`; CI compares full serialized reports.
Goldens pin JSON only (human output embeds absolute temp paths by design).
The set covers the documented scenario matrix:
`pass.json`, `pass-pairing-ambiguous.json`, `pass-formatting.json`,
`fail.json`, `fail-count-formatting.json`, `manifest-mode.json`,
`inconclusive-work-budget.json`, `inconclusive-report-truncated.json`,
`inconclusive-ambiguous-common.json`, `inconclusive-strict-uniqueness.json`.

Boundary suite (house style): below/at/above every `AuditTreeLimits` and
budget field using lowered constructors, asserting fail-closed behavior.

Fuzz: cargo-fuzz target `conservation_matcher` under `crates/srcmv-core/fuzz/`
(beside the stressed crate, per the `srcmv-lsp/fuzz` precedent) feeding
arbitrary (region bytes, corpus bytes, option bits) into the engine asserting
no panic plus the §9.2 invariant. Regression seeds are appended to
`tests/fuzz-regressions/seeds.json` with a defined encoding for the triple —
hex-concatenated corpus and region plus an options bitmask — and consumed by a
named proptest campaign,
`conservation_engine_fuzz_regression_preserves_ledger_invariant`, runnable via
the existing `cargo test --workspace --all-features fuzz_regression` and
`PROPTEST_CASES=4096` workflow.

Example: `examples/11-conservation-audit/` — scripted before/expected trees
demonstrating move + split + one formatting tweak, wired into
`examples/run.sh`'s dispatch table (or its own runner if it needs two input
trees; match whatever layout example 10 settles on).

## 16. Implementation phases

| Phase | Deliverables | Depends on |
|---|---|---|
| 0. Contract freeze | this document accepted; schema draft; fixture trees; golden placeholders | — |
| 1. Core engine | `conservation/*` modules, unit + property tests, fuzz target scaffold | Phase 0 |
| 2. Tree reading | `audit_tree.rs`, limits + boundary tests, manifest parser | Phase 0 (validation rules exist) |
| 3. Protocol layer | DTOs, serialization budget, schema-conformance tests, goldens finalized | Phases 1–2 |
| 4. CLI integration | grammar, orchestration, rendering, integration suite, example, README + docs page | Phase 3 |
| 5. Hardening | fuzz campaigns, performance baseline entry, `qualify-platform.sh` run on qualified hosts, release notes | Phase 4 |

Each phase merges green under the standard gate: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
-D clippy::perf`, `cargo test --workspace --all-features`.

## 17. Risks and open questions

Risks and mitigations:

- **Pathological corpora** (minified JS, generated locks): hot-bucket
  suppression + work budgets degrade gracefully to `inconclusive`;
  performance baseline will record realistic ceilings.
- **Memory pressure** on huge trees: 1 GiB/tree cap aligns with existing
  snapshot limits; manifest mode scopes big monorepos; index memory is
  released before report assembly.
- **Anchor-quality variance across languages:** uniqueness anchoring is
  language-agnostic; sparse-anchor fallback keeps results sound (coarser
  regions) rather than wrong.
- **User confusion between `fail` and `inconclusive`:** docs lead with the
  one-sentence meaning of each; reasons arrays make inconclusive self-
  describing.

Open questions (default answers chosen for v1; revisit before Phase 0 freeze):

1. Exit-code ergonomics for CI (default 0 + jq recipe vs opt-in strict flag) —
   default: stay 0, consistent with "verdict is data".
2. Native Git ref inputs (`--base-ref`) — deferred; requires trust-reviewed
   process spawning or a new dependency; directory/manifest contract suffices.
3. Whether `formatting_only` should ever satisfy conservation under a
   normalization flag (`--match-normalized-eol`) — deferred; byte purity wins
   for v1.
4. Region merge heuristics for presentation (joining fragments separated by
   < N bytes) — cosmetic; measure on real PRs first.
5. Bounded backtracking to recover greedy overlap conflicts (retry a blocking
   region against alternative earlier assignments within a work-unit budget) —
   deferred; the failure direction is conservative (`fail`, never false
   `pass`), and labeled fixtures pin today's behavior.
6. Parallel ingestion/indexing via rayon — deferred; single-threaded targets
   first, parallelism is pure speedup behind an unchanged output contract
   provided ordering stays deterministic.

## 18. Acceptance criteria

1. `srcmv audit-conservation --base B --head H [--json]` completes on the two
   qualified platforms, writing either a report (exit 0) or a standard error
   envelope (exit ≠ 0), never partial output.
2. **Soundness (no false pass):** a `pass` verdict is emitted only when every
   substantial removed byte is byte-verified against exclusively consumed head
   intervals; the soundness property test rechecks this invariant from the
   report alone across randomized inputs. On *friendly* packings (non-
   overlapping scattered copies, randomized partitions, permutations, and file
   layouts including splits across ≥ 2 files) the verdict is additionally
   always `pass` — the scatter property test demonstrates this. Rare greedy
   overlap conflicts between distinct contents may conservatively `fail`;
   labeled hard-packing fixtures pin that behavior as specified in §4.
3. Deleting any substantial span without recreation yields `fail` with that
   span listed in `unmatched[]` with path, byte range, line range, length, and
   digest; the mutation property test demonstrates detection across randomized
   corruptions.
4. Duplicate-content scenarios produce exclusive one-to-one consumption with
   `pairing_ambiguous` flags; `--strict-uniqueness` converts them into
   `inconclusive` with `strict_uniqueness_ambiguity` reasons.
5. Formatting-only rewrites appear under `formatting[]` and totals, affect no
   verdict by default, and fail under `--count-formatting`.
6. Any exceeded work/memory budget or truncated report produces
   `inconclusive` with a precise reason; no code path can emit `pass` after
   truncation, enforced by tests that lower every budget below/at/above its
   boundary.
7. The JSON report validates against
   `docs/schema/conservation-audit-v1/report.schema.json`, is exactly one
   line, and is byte-identical across repeated runs and across the golden
   fixtures covering the documented scenario set (§15).
8. The engine performs zero syscalls by construction: it accepts only borrowed
   tree slices, `srcmv-core` gains no `std::fs` usage or new dependencies
   (enforced by the existing unsafe-audit-style source-scan script extended to
   check `std::fs` imports in `core::conservation`), and no request, journal,
   plan-hash, identity-hash, or log type appears anywhere in
   `core::conservation`.
9. All existing commands' golden outputs remain byte-identical; the frozen v1
   schemas, error registry, plan-hash, and transaction formats are untouched
   (CI goldens prove it).
10. Limits table added to `docs/resource-limits.md` with the house below/at/
    above test coverage; README documents the command and the external Git
    worktree/archive recipe; the example runs green end-to-end via
    `examples/run.sh 11-conservation-audit`.
11. Failure-exit mapping matches §12's table exactly, pinned by integration
    tests (including exit 8 for a missing root); completed audits exit 0 on
    all three verdicts.
12. `cargo fmt --all --check`, the clippy gate, `cargo test --workspace
    --all-features`, and the fuzz regression campaign pass on CI.

## 19. References

- Frozen editing contracts: `docs/specification.md`, `docs/protocol.md`,
  `docs/resource-limits.md`, `docs/security.md`
- Independently versioned surface precedent: `docs/schema/selection-v1/`,
  `crates/srcmv-protocol/src/selection.rs`, `crates/srcmv-cli/src/select.rs`
- Diff/bounding precedents: `crates/srcmv-cli/src/preview.rs`
- Snapshot/path discipline: `crates/srcmv-fs/src/lib.rs`
- Line semantics: `crates/srcmv-core/src/line_metrics.rs`,
  `docs/specification.md` ("Coordinates")
- Plan-format precedent: `notes/lsp-semantic-selection-implementation-plan.md`
