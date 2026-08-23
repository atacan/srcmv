# Conservation audit fixtures

This directory freezes the hand-built two-tree fixtures used to qualify the
`audit-conservation` engine and its golden vectors (plan §15). Each child
directory realizes one documented scenario as a `base/` and a `head/` subtree
that `srcmv audit-conservation --base <dir>/base --head <dir>/head` consumes
directly; the `manifest-mode` directory additionally carries two manifest
files consumed in `@FILE` form. The fixtures are test inputs, not goldens:
golden reports land with Phase 3 under `tests/golden/conservation-audit-v1/`.

Every expected-matching span is 32 bytes or more (at or above both the 16-byte
index window `K` and the default substantiality floor), so outcomes are
verdict-visible. Expected outcomes below are hand-derived by tracing plan §8
per scenario and are labeled as such; §15 pins them normatively only for
`hard-packing`.

## Scenarios

| Directory | Scenario | Expected outcome (hand-derived) | Consumed by |
|---|---|---|---|
| `pass-move-across-files/` | deleted `src/feature.txt` reappears inside created `dst/feature-applied.txt`; unchanged bystander on both sides | verdict `pass`; removed_regions 1; conserved_exact_bytes 48; new_bytes 20 (the wrapper lines) | audit_cli.rs move-across-files case; golden `pass.json` |
| `pass-rename-as-delete-create/` | whole-file delete + byte-identical create at a new path, content distinct from the move fixture | verdict `pass`; removed_regions 1; conserved_exact_bytes 57; new_bytes 0 | audit_cli.rs rename-as-delete+create case; no dedicated golden (audit_cli.rs case) |
| `pass-reorder/` | same file, unique blocks A and B swapped | verdict `pass`; removed_regions 1; conserved_exact_bytes 41; new_bytes 1. Trace: §8.1 step 3 anchors B↔B (lexicographically smallest after-position), so the gap before it trims to exactly `A ++ "\n"`; gap 2's after-side remainder `"\n" ++ A` feeds the insertion-span set, consumed span `data.txt[57..98)` clips inside it, leaving the separator byte at `[56..57)` as the only new byte (§8.8 requires `consumed ∪ new_spans` disjoint, forcing interval-subtraction semantics) | audit_cli.rs reorder-only case; no dedicated golden (audit_cli.rs case) |
| `pass-split-block/` | modified pair loses a 64-B unique block (post-trim after-middle is empty ⇒ zero-anchor fallback yields exactly that block); it reappears as two 32-B halves in `part-a.txt` / `part-b.txt` | verdict `pass`; removed_regions 1 matched fragmentarily: conserved_exact_bytes 0, conserved_fragments_bytes 64 (two fragments, two files); new_bytes 2 (each half file keeps its trailing newline) | audit_cli.rs split-block case; no dedicated golden (audit_cli.rs case) |
| `pass-pairing-ambiguous/` | two identical blocks removed (`copy-one.txt`, `copy-two.txt`); two identical survivors (`retained-a.txt`, `retained-b.txt`) | verdict `pass`; pairing_ambiguous_regions 2; conserved_exact_bytes 106; new_bytes 0 | audit_cli.rs duplicate-boilerplate case; golden `pass-pairing-ambiguous.json`; `--strict-uniqueness` run feeds golden `inconclusive-strict-uniqueness.json` |
| `pass-formatting/` | pure CRLF→LF conversion of a 3-line file (bytes pinned below) | verdict `pass` by default; `fail` under `--count-formatting`. Hand trace per §8.7: prefix trim consumes `alpha` and stops at the CR/LF divergence, suffix trim leaves one `\n`; the middles anchor on `""`, `bravo`, `charlie`, and three gap regions of exactly one CR byte each qualify `formatting_only` (W(`"\r"`) = W("") = []) ⇒ formatting_only_bytes 3, removed_regions 3 | audit_cli.rs formatting-only cases (default and `--count-formatting`); goldens `pass-formatting.json` and `fail-count-formatting.json` |
| `fail-deleted-block/` | unique multi-line `unique-notes.md` deleted, no copy anywhere; `keeper.txt` unchanged on both sides | verdict `fail`; unmatched_substantial_bytes 74; one entry in `unmatched[]` | audit_cli.rs deleted-block case; golden `fail.json` |
| `inconclusive-ambiguous-common/` | whitespace-heavy file (200 spaces) deleted while head contains 4000 spaces — the region's single 16-B window key (' '×16) has 3985 corpus positions > hot-bucket limit 64 ⇒ no cold window anywhere | verdict `inconclusive` with reason `ambiguous_common`; ambiguous_common_bytes 200; ambiguous_common_regions 1 | golden `inconclusive-ambiguous-common.json` |
| `manifest-mode/` | manifests (`base/base-manifest.lst`, `head/head-manifest.lst`) with comments and blank lines list normalized paths resolved against the manifest's parent directory; `alpha.txt`'s bytes move into created `gamma.txt`, `beta.txt` unchanged | run with `--base @base/base-manifest.lst --head @head/head-manifest.lst`: verdict `pass`; inputs.kind `manifest` on both sides; conserved_exact_bytes 49; new_bytes 11 (`relocated:` line) | audit_cli.rs manifest happy path; golden `manifest-mode.json` |
| `hard-packing/` | canonical intersecting-regions fixture; byte-exact recipe pinned below | plan-pinned (§15): verdict `fail`; conserved_exact_bytes 96 (r1 at `a.txt[0..96)`); unmatched_substantial_bytes 32 (r2's residue). Recipe-implied remainder of the ledger: removed_regions 2, removed_bytes 128, conserved_fragments_bytes 0, new_bytes 15 (a.txt's unconsumed `y` tail) | Phase 1 labeled-fixture engine tests pinning the documented outcome |

## Test-time-only scenarios

Three golden scenarios are deliberately realized at test time rather than as
static trees:

- `inconclusive-work-budget.json` and `inconclusive-report-truncated.json`
  require lowered work budgets and a shrunken serialized-report cap,
  which exist only as trusted test constructors (Phases 1–4); static trees
  cannot force deterministic exhaustion.
- `inconclusive-skipped-input.json` requires a file that vanishes between
  walk and read, forced through the `read_audit_tree` skip seam.

Likewise the empty-tree and identical-tree scenarios are built in-test via
`tempfile` because git cannot track empty directories.

## Byte-exact recipes

All bytes are UTF-8/ASCII except where noted. `fit(prefix, n)` means the
literal `prefix` followed by the decimal digits `0123456789` repeated from
`0`, truncated to total length `n`. Files not listed are literal text as
named (e.g. `unchanged bystander\n`, `keeper line stays\n`,
`beta file stays put\n`, `relocated:\n`, `// applied:\n`, `\n// end\n`,
`frame-top\n`, `frame-bottom\n`, `header-line\n`, `trailer-line\n`,
`alpha\r\nbravo\r\ncharlie\r\n`, `alpha\nbravo\ncharlie\n`,
`vanishing-block-alpha\nsecond-line-beta-goes-too\nthird-line-gamma-finishes\n`).

- `pass-move-across-files/base/src/feature.txt` =
  `fit("move-across-files-payload-", ...)` → exact bytes
  `move-across-files-payload-Alpha-0123456789012345` (48 B). The head copy
  wraps these same 48 bytes: `"// applied:\n"` + payload + `"\n// end\n"`
  (68 B).
- `pass-rename-as-delete-create/{base/legacy/old-module.txt,
  head/modern/new-module.txt}` =
  `rename-scenario-content-Omega-01234567890123456789012345\n` (57 B).
- `pass-reorder/base/reorder/data.txt` =
  `"header-line\n"` + A + `"\n"` + B + `"\n"` + `"trailer-line\n"` with
  A = `reorder-block-first-Beta-012345678901234` (40 B) and
  B = `REORDER-BLOCK-SECOND-Gamma-01234567890123456` (44 B);
  `head/reorder/data.txt` swaps A and B.
- `pass-split-block/base/split/holder.txt` =
  `"frame-top\n"` + X + `"\nframe-bottom\n"` with X =
  `split-block-payload-Epsilon-012345678901234567890123456789012345` (64 B);
  head `holder.txt` = `"frame-top\n\nframe-bottom\n"`;
  `part-a.txt` = X[0..32] + `"\n"`; `part-b.txt` = X[32..64] + `"\n"`.
- `pass-pairing-ambiguous/{...}` — all four files equal
  `duplicate-boilerplate-Zeta-0123456789012345678901234\n` (53 B).
- `inconclusive-ambiguous-common/base/ws/heavy-spaces.txt` = 0x20 × 200;
  `head/ws/bulk-spaces.txt` = 0x20 × 4000.
- `manifest-mode/base/tree/alpha.txt` =
  `manifest-mode-payload-Delta-01234567890123456789\n` (49 B);
  `head/tree/gamma.txt` = `"relocated:\n"` + those 49 B.
- `hard-packing` recipe (pinned): `U` is the ascending bytes 0x21..0x7E
  excluding 0x79 (`y`) — 93 pairwise-distinct, terminator-free,
  whitespace-free bytes — followed by the three UTF-8 bytes E2 80 A6, for 96
  pairwise-distinct bytes total. `r1 = U` (96 B);
  `r2 = U[79..96] ++ y×15` (32 B); `P = "P"`, `Q = "Q"`, `R = "R"`.
  `base/f1.txt = "P\n" ++ r1 ++ "\nQ\n" ++ r2 ++ "\nR\n"` (136 B);
  `head/f1.txt = "P\n\nQ\n\nR\n"` (8 B);
  `head/a.txt = U ++ y×15` (111 B). Extraction, matching, and the pinned
  fail outcome are derived step-by-step in plan §15.

## Checksums

Regenerate with
`(cd tests/fixtures/conservation && find . -type f ! -name README.md -print0 | sort -z | xargs -0 shasum -a 256)`
(keeps fixture-relative paths, excludes the README itself) and compare
against this table; any drift means the fixtures (and therefore future golden
expectations) changed.

```text
3c04f91a197b6f5a0a2691836ec15843bf3d86b35617f78342ec11f3f389885e  ./fail-deleted-block/base/vanished/keeper.txt
6f544c7f032eba5025c50fba51b15eff8ec47f9fc293cfc052c4b8363db413cc  ./fail-deleted-block/base/vanished/unique-notes.md
3c04f91a197b6f5a0a2691836ec15843bf3d86b35617f78342ec11f3f389885e  ./fail-deleted-block/head/vanished/keeper.txt
c72df8980df6ad81d6e53ecb5f46840e8aa8a95b6014c04cdb1337e37a0c3c00  ./hard-packing/base/f1.txt
e42b3999f0930a96221156381b3e1ce3ac0fafe2e93d1ad356d33aec724d463f  ./hard-packing/head/a.txt
738e8dce2fa46ab24802179b5d0a1de7280f826991cfff8a9a0d94165ec82cac  ./hard-packing/head/f1.txt
2b4115a029ab615990dbb0f27184074bc3e46a08e7e65fe4098f3faba6f56187  ./inconclusive-ambiguous-common/base/ws/heavy-spaces.txt
8666bde2be3ff7923ac93bb743525c770b9f8e7bc703cbd2551351d9dc63fec7  ./inconclusive-ambiguous-common/head/ws/bulk-spaces.txt
3f5514dbf74ec2719f2458c79a31da79cb4b81e22c493354d5a5ef0ca326f2b9  ./manifest-mode/base/base-manifest.lst
830a8bcbd3d1eefeb8196a26e6fb6f44ac359df5ed58a8efc61f510d9221b8b7  ./manifest-mode/base/tree/alpha.txt
1481d57a80b9c6a167633373155e8ff3334ae1d616f0d4fbcc21f89d486cbb84  ./manifest-mode/base/tree/beta.txt
17d465fa02b4f192ea2c61945a407a156279abe24f81d89bcb0481d0f9502b2e  ./manifest-mode/head/head-manifest.lst
1481d57a80b9c6a167633373155e8ff3334ae1d616f0d4fbcc21f89d486cbb84  ./manifest-mode/head/tree/beta.txt
45505696aa1e97caf58a88c7ff74a8868a7b3e82ea0af41d26d59e9df1d6b2a3  ./manifest-mode/head/tree/gamma.txt
36aa37df409fcb3d525376eb132b448bcc52a5eb6aa8e00cc345b4edd6ccdaa1  ./pass-formatting/base/eol/poem.txt
3eca7ea48b0da0ad30bee679c92c7b68d487547068b6914d10a64e8cedb03f51  ./pass-formatting/head/eol/poem.txt
0ace365bc0200cd455985c501776791f944e933e5990eb3147bfdc1ffb800b96  ./pass-move-across-files/base/docs/note.txt
41f2783505f46a21b2b9a5d9116d8fee6f5571c22cf0f2b3b3f5cfeb3e659a8d  ./pass-move-across-files/base/src/feature.txt
0ace365bc0200cd455985c501776791f944e933e5990eb3147bfdc1ffb800b96  ./pass-move-across-files/head/docs/note.txt
c064ae1e2f70a543ce15de38238df5d9a2c870e1bf19a1bafb29adf42d40867a  ./pass-move-across-files/head/dst/feature-applied.txt
4a9537b50cb97a67ac8571c5ed88f1d9362db347e995d03ca0f3239e974c360a  ./pass-pairing-ambiguous/base/dup/copy-one.txt
4a9537b50cb97a67ac8571c5ed88f1d9362db347e995d03ca0f3239e974c360a  ./pass-pairing-ambiguous/base/dup/copy-two.txt
4a9537b50cb97a67ac8571c5ed88f1d9362db347e995d03ca0f3239e974c360a  ./pass-pairing-ambiguous/head/keep/retained-a.txt
4a9537b50cb97a67ac8571c5ed88f1d9362db347e995d03ca0f3239e974c360a  ./pass-pairing-ambiguous/head/keep/retained-b.txt
d5d784894fda4a26786726a46b3d4418035cd99fbe3a07b57e82824a2337bfdc  ./pass-rename-as-delete-create/base/legacy/old-module.txt
d5d784894fda4a26786726a46b3d4418035cd99fbe3a07b57e82824a2337bfdc  ./pass-rename-as-delete-create/head/modern/new-module.txt
b7afad2353a0b3d22afa9e9cdd7371771820457ca3d4c8f690c7f83990580be5  ./pass-reorder/base/reorder/data.txt
69833ae195e3956eb35034f9902c89344dfb1a0895fc56878f755701d6b7f1e9  ./pass-reorder/head/reorder/data.txt
541c92c5760724c05d9dc961ea56974e2a9529d13b30f73297d66264ca0375a0  ./pass-split-block/base/split/holder.txt
1fbb9d63845903a22ccd4c14e4a44dc21ffb4e7a03580a2f778adee9b4128640  ./pass-split-block/head/split/holder.txt
715be39b8dfa434a0e6ef1fcf0b600015b640f2c6e2fed848f4f79342812d90d  ./pass-split-block/head/split/part-a.txt
4cabbf9007b33200a7acb268e29520445b754e5d3a914a0897ce2c785ef9f6bc  ./pass-split-block/head/split/part-b.txt
```
