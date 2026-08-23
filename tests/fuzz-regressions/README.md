# Phase 9 fuzz regressions

These seeds are permanent regression cases discovered or selected during the
bounded Phase 9 campaigns. The crate-local `hardening` tests also run generated
property cases against JSON decoding, line indexing, coordinate resolution,
event composition, deterministic CBOR validation, transaction-record decoding,
state folding, recovery classification, diff decoding, and terminal escaping.

Run the checked-in regression set with:

```bash
cargo test --workspace --all-features fuzz_regression
```

Run the bounded generated campaign with:

```bash
PROPTEST_CASES=4096 cargo test --workspace --all-features fuzz_regression
```

The campaign is deterministic enough for regression use through proptest's
checked-in source cases, has bounded input sizes, and persists any future minimal
failure in the relevant crate's `proptest-regressions` file.

## Seed surfaces

Each seed carries a `surface` key naming the consumer that replays it, plus the
encoded input:

- `json`, `line_index`, `deterministic_cbor`, `manifest_record`,
  `state_record`: raw bytes spelled as one lowercase-hex `hex` string.
- `human_escape`: a raw Unicode `text` string.
- `conservation_matcher` (plan §15): drives the conservation engine with an
  arbitrary `(region bytes, corpus bytes, option bits)` triple. The encoding
  is:
  - `hex` decodes to exactly one concatenated lowercase-hex byte string laid
    out as `u16-le region_length || region bytes || corpus bytes`. The
    two-byte little-endian length header makes the region/corpus split
    unambiguous; everything after the first `region_length` bytes is corpus.
  - `options_mask` is an integer bitmask: bit 0 = `strict_uniqueness`,
    bit 1 = `count_formatting` (the §6.1 option flags).

  The named proptest campaign
  `conservation_engine_fuzz_regression_preserves_ledger_invariant`
  (in `crates/srcmv-protocol/tests/conservation_fuzz_regressions.rs`) replays
  every `conservation_matcher` seed through the public
  `srcmv_core::audit_conservation` engine over a minimal two-tree projection
  synthesized exactly like the nightly cargo-fuzz harness
  (`crates/srcmv-core/fuzz/fuzz_targets/conservation_matcher.rs`): base tree
  holds `base/changed.txt` = `"frame\n" ++ region ++ "\ntrailer"` plus deleted
  twin `base/vanished.bin` = region; head tree holds created file
  `head/corpus.bin` = corpus. Both base paths are absent from the head tree,
  so extraction always produces at least one whole-file removed region.
  The committed seeds cover: one benign pass-shaped pair (every removed byte
  reappears verbatim), one hard-packing-flavored pair (distinct region
  contents competing for an intersecting occurrence — the conservative-fail
  direction of §4), one empty-region edge, and one hot-bucket-heavy
  whitespace-run corpus that forces §8.6 `ambiguous_common`. Future minimized
  defects found by nightly `cargo fuzz run conservation_matcher` are appended
  here in this encoding.
