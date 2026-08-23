# 11 — Independent conservation audit

This example audits a committed before-tree against a committed after-tree
with the read-only `srcmv audit-conservation` command and verifies that every
substantial byte removed from the before side reappeared exactly somewhere on
the after side. The after tree demonstrates three edit shapes at once:

- a **cross-file move**: `src/moved-block.txt` disappears and its exact bytes
  reappear inside a wrapper at `src/applied/moved.txt`;
- a **split**: `src/split-block.txt` is deleted but reappears as two disjoint
  fragments in `src/parts/part-a.txt` and `src/parts/part-b.txt`;
- one **formatting-only rewrite**: `src/formatting.txt` loses four bytes of
  indentation, which the audit reports under `formatting[]` and
  `formatting_only_bytes` without failing the run.

A bystander file under `notes/` stays identical to exercise the
unchanged-pair path. Like every example this one runs only in an ignored,
disposable directory below `examples/.work/`; the committed fixtures are never
modified. Unlike examples 01–09 the command needs two input trees instead of a
workspace, so it uses its own runner, matching the layout of example 10:

```bash
examples/11-conservation-audit/run.sh
```

The script copies `before/` and `after/` into its work root, runs the real
binary twice over them — once with `--json`, once for the human summary —
prints the human summary, and then recomputes every asserted total from the
fixture files themselves with `verify-report.py`: the moved block's conserved
length and destination range, the split block's fragment total and two
destination records, the nonzero formatting-only bytes, the new-content total
(wrapper overhead plus the two newline separators), and the plan §9.2 ledger
identity. Any drift exits nonzero.

## What the output means

A `pass` verdict means every substantial removed byte was found verbatim under
one-to-one consumption accounting. Verdicts are data: the process still exits
0, so automation selects with `jq -e '.verdict == "pass"'`. A `fail` would list
the unmatched regions with paths, byte ranges, line ranges, lengths, and SHA-256
digests; an `inconclusive` verdict would carry machine-readable reasons in
`inconclusive_reasons[]`. See [the frozen contract](../../docs/conservation-audit.md)
and [the implementation plan](../../docs/conservation-audit-plan.md) for the
complete grammar, output contract, and error table.
