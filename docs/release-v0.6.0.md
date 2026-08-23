# srcmv v0.6.0 release notes

This release adds `srcmv audit-conservation`, a read-only command that compares
two arbitrary directory trees — typically a pull-request base commit tree and
head commit tree — and verifies at the byte level that every substantial byte
region removed from the before tree reappears exactly somewhere in the after
tree. The audit works identically whether or not srcmv performed any of the
edits: it consults no workspace state, accepts exactly two trees supplied on
the command line, writes nothing, and holds no locks.

## Highlights

- Byte-exact conservation verification. Removed regions are matched by exact
  byte equality against an index of the after tree, with one-to-one exclusive
  consumption so one surviving occurrence can never satisfy two removals.
  Blocks that moved between files, were reordered inside one file, were split
  across destinations, or live inside renamed files are all credited through
  whole-region and fragment matching. Identical repeated blocks are counted
  soundly and flagged `pairing_ambiguous`.
- A three-valued verdict model. `pass` means every substantial removed byte is
  byte-verified against an exclusively consumed interval (with two reported
  carve-outs: bytes below the substantiality floor and formatting-only
  rewrites). `fail` means at least one substantial removed region has no
  remaining occurrence. Matching is greedy without backtracking, so rare
  overlap conflicts may `fail` where a perfect packing exists — the audit can
  under-credit but never over-credit.
- Conservative incompleteness with machine-readable reasons. Resource
  exhaustion, truncated reports, skipped inputs, strict-uniqueness ambiguity,
  and content too ubiquitous to anchor (`ambiguous_common`) all yield
  `inconclusive` with structured reasons instead of a manufactured claim. A
  whitespace-run or lockfile-like deletion is reported as "cannot decide",
  not as a pass.
- Bounded reports. The JSON report is capped at 16 MiB with deterministic
  truncation markers; listings are bounded; overflow downgrades the verdict to
  `inconclusive` rather than hiding unmatched regions. Digests (SHA-256) on
  every record let reviewers verify any claim independently.
- Two ingestion forms per side: recursive directory reads (deterministic
  sorted traversal, regular files only, symlinks rejected) and `@MANIFEST`
  files listing normalized relative paths, so large repositories can be scoped
  to a reviewable subset. Git integration stays external: materialize commits
  with ordinary `git worktree` or `git archive` plumbing outside srcmv.
- Platform posture is unchanged. The supported rows remain Linux x86_64 on
  local ext4 and macOS arm64 on local APFS; `scripts/qualify-platform.sh`
  qualifies both. Every limit — files per tree, file and total bytes, work
  units, hot-bucket suppression, report caps — degrades audits to
  `inconclusive` fail-closed rather than to hangs or unbounded output.

## Frozen contracts unaffected

The audit is an independently versioned read-only surface
(`conservation-audit-v1`) following the `selection-v1` precedent. Protocol v1,
plan-hash version 1, transaction-record version 1, the frozen edit error and
warning registries, capabilities output, and every existing command's output
are unchanged byte-for-byte. The engine performs zero syscalls by
construction: it accepts only borrowed in-memory tree slices, gains no
`std::fs` usage or new dependencies in its core crate (enforced by the source
scan in `scripts/audit-unsafe.sh`), and no request, journal, plan-hash,
identity-hash, or log type appears anywhere in it.

Completed audits exit 0 on all three verdicts — the verdict is data, not a
process failure — so automation selects with `jq -e '.verdict == "pass"'`.
Failures reuse the existing error-code table and exit codes unchanged.

## Limits

The audit's resource limits join the house table in
`docs/resource-limits.md`, with below/at/above boundary coverage in tests.
Performance baseline numbers are recorded informationally in
`docs/performance-baseline.json`; they are observations, not gates.

## Documentation

- Command guide and Git workflow: `docs/conservation-audit.md`
- Normative report schema: `docs/schema/conservation-audit-v1/`
- Golden reports: `tests/golden/conservation-audit-v1/`
- Implementation plan and acceptance criteria:
  `docs/conservation-audit-plan.md`
- Repository overview: `README.md`
