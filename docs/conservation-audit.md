# Conservation audit contract

`srcmv audit-conservation` is a read-only command that compares two arbitrary
directory trees — typically a PR base commit tree and a head commit tree — and
verifies at the byte level that every substantial region removed from the
before tree reappears exactly somewhere in the after tree. It is an
independently versioned surface (`conservation-audit-v1`) following the
`selection-v1` precedent; protocol v1, plan-hash v1, transaction-record v1,
and all frozen registries are unchanged. The normative report schema is
[`docs/schema/conservation-audit-v1/report.schema.json`](schema/conservation-audit-v1/report.schema.json)
and the complete implementation plan lives in
[`docs/conservation-audit-plan.md`](conservation-audit-plan.md).

The audit shares nothing with srcmv's edit machinery: it reads no requests,
journals, plan digests, identity hashes, or control directories, and it writes
nothing. It is a deterministic function from `(base_tree, head_tree, options)`
to one bounded report.

## Command surface

```text
srcmv audit-conservation --base TREE --head TREE [OPTIONS]

TREE:
  PATH                    an existing readable directory (recursive)
  @MANIFEST               '@' prefix: text file listing one normalized
                          workspace-relative path per line

OPTIONS:
  --json                  emit the machine report instead of the human summary
  --min-substantial-bytes N   default 32
  --min-fragment-bytes N      default 16
  --strict-uniqueness         downgrade multiplicity-counted matches to
                              inconclusive contributors
  --count-formatting          treat formatting-only deltas as failures too
```

The command is target-independent like `capabilities`: `--workspace` is
rejected with `INVALID_CLI`. Exactly one `--base` and one `--head` are
required; both must resolve to existing readable directories (a root that
exists but is not a directory is `INVALID_REQUEST`). Option floors are
validated at parse time: `--min-fragment-bytes` must be at least the index
window `K = 16`, and `--min-substantial-bytes` must be at least
`--min-fragment-bytes`; violations are `INVALID_REQUEST`.

Directory ingestion walks deterministically in sorted byte order, accepts
regular files only, rejects symbolic links and special files fail-closed, and
skips exactly three root-level control names: `.git`, `.srcmv`, `.codesplice`.
Manifest files are UTF-8 text with one validated relative path per line (`#`
comments and blank lines allowed); each entry resolves against the manifest's
own parent directory, and missing entries are hard errors knowable before any
analysis. Both sides accept either form independently.

Git integration stays external and dependency-free:

```bash
BASE=$(mktemp -d); HEAD=$(mktemp -d)
git -C repo worktree add --detach "$BASE" "$PR_BASE_SHA"
git -C repo worktree add --detach "$HEAD" "$PR_HEAD_SHA"
srcmv audit-conservation --base "$BASE" --head "$HEAD" --json > audit.json
git -C repo worktree remove "$BASE"; git -C repo worktree remove "$HEAD"
```

or losslessly with plumbing: `git archive "$SHA" | tar -x -C "$DIR"`.

## Output contract

Identical to every other command: `--json` writes exactly one bounded UTF-8
JSON value plus LF to stdout — the audit report itself, never wrapped in any
protocol success envelope; human mode writes a plain-text summary to stdout;
all diagnostics go to stderr through the standard error envelope with terminal
control and bidi characters visibly escaped.

The JSON report validates against the frozen schema. Its `totals` object
carries exactly the §9.2 ledger terms plus counts, and the identity
`removed_bytes = Σ ledger terms` holds on every completed run. Listings are
ordered largest-first with deterministic tie-breaks; byte ranges are zero-based
half-open pairs, line ranges are one-based inclusive and exactly `null` for
binary content (`is_binary` reused from preview: contains NUL or invalid
UTF-8). The serialized report is bounded at 16 MiB under the §11.3 policy
table: dropping any unmatched/formatting listing sets the fatal
`report_truncated` reason and downgrades the verdict to `inconclusive`, so
truncation can never manufacture a `pass`.

The human summary mirrors the same data, bounded at 4 MiB like preview's
rendered diff:

```text
conservation audit v1
base:  /tmp/base  (142 files, 3204551 bytes)
head:  @list.txt  (manifest; 143 files, 3211002 bytes)
verdict: FAIL
removed 12344 B | conserved 12002 B | unmatched 110 B | formatting 210 B | new 1905 B
unmatched regions (1):
  - src/legacy/parser.ts [220..330) (lines 9..11) 110 B sha256:ab41…
formatting-only regions (1):
  - src/legacy/parser.ts [512..722) (lines 21..27) 210 B sha256:c3d4…
top new content:
  - src/parse/index.ts [0..431) 431 B
inconclusive reasons: (none)
warnings: (none)
```

Inconclusive reasons are always rendered when present (`(none)` otherwise);
overflow truncates listings behind an explicit `(truncated)` marker while the
fixed skeleton stays intact.

## Verdict semantics

Completed audits exit **0** on all three verdicts; the verdict is data, not a
process failure, so automation selects with e.g.
`jq -e '.verdict == "pass"'`.

- `pass` — every maximal removed byte region was found, in exact byte
  equality, at some consumed head location under one-to-one accounting,
  except two reported carve-outs that do not defeat a pass by default:
  sub-floor residue (`insubstantial_residue_bytes`) and line-edge-equivalent
  rewrites (`formatting_only_bytes`, which fail under `--count-formatting`).
- `fail` — at least one substantial removed region has no remaining exact
  occurrence available in the after tree under exclusive consumption.
- `inconclusive` — no conservation claim either way, with machine-readable
  reasons: `work_budget_exceeded`, `memory_budget_exceeded`,
  `unreadable_input_skipped`, `strict_uniqueness_ambiguity`,
  `ambiguous_common`, `report_truncated`.

Matching is greedy longest-first without backtracking: it never produces a
false `pass`, but rare overlap conflicts between distinct contents may
conservatively `fail` where a perfect packing exists (plan §4).

## Errors and exit codes

Completed audits never use nonzero exits. Command failures reuse the shared
frozen registry unchanged:

| Exit | Codes used by the audit |
|---:|---|
| 2 | `INVALID_CLI`, `INVALID_REQUEST` (bad manifest syntax, missing manifest entries, bad option values, root not a directory) |
| 4 | `RESOURCE_LIMIT_EXCEEDED`, `SYMLINK_NOT_ALLOWED`, `UNSUPPORTED_FILE_TYPE` |
| 8 | `IO_ERROR` (missing or unreadable root/manifest, canonicalization failures, walk-time non-UTF-8 entry names), `INTERNAL_ERROR` |

An integration test pins the actual exit code for a missing root (exit 8 via
`IO_ERROR`) so this table and the binary cannot drift. If hard memory caps
trip before any analysis completes, the command fails closed with
`RESOURCE_LIMIT_EXCEEDED` rather than emitting a hollow `inconclusive`.

Resource bounds for ingestion, matching, and rendering are listed in
[resource limits](resource-limits.md).
