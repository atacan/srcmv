# srcmv conservation-engine fuzzing

This standalone package keeps nightly-only `cargo-fuzz` tooling outside the
stable workspace build, mirroring `crates/srcmv-lsp/fuzz`. It is not a
workspace member and is never built by the repository gates.

The single target drives the production conservation engine —
`srcmv_core::audit_conservation` — with arbitrary (region bytes, corpus
bytes, option bits) triples decoded from length-prefixed input. The harness
contains no second matcher: it asserts only that the engine never panics and
that the §9.2 ledger identity (`removed_bytes = conserved_exact +
conserved_fragments + unmatched_substantial + insubstantial_residue +
formatting_only + ambiguous_common + unanalyzed`) holds for every completed
run. Early failures must map to the sanctioned pre-match resource-limit
boundary; anything else is a bug.

The dependency is pinned to `libfuzzer-sys = 0.4.13`, the same release used by
the LSP harnesses. It builds LLVM libFuzzer C++ support, so it remains
development-only and is never linked into srcmv binaries.

## Target

- `conservation_matcher` decodes an options bitmask plus two length-prefixed
  byte strings, synthesizes a two-tree input (modified pair, deleted file,
  created corpus file), runs the engine under default budgets, and asserts no
  panic plus the ledger identity. Runs that fail early are re-run with
  unlimited memory budgets so genuine panics still surface.

## Artifacts and corpora

`artifacts/` and `corpus/` directories are generated at run time and ignored;
no seed corpus is committed yet. Add every minimized defect as a regression to
`tests/fuzz-regressions/seeds.json` (Phase 5 wiring) and, where expressible,
to the property tests in `crates/srcmv-core/tests/conservation_properties.rs`.

## Running

Install cargo-fuzz and use a nightly toolchain:

```console
cargo install cargo-fuzz --locked
cargo +nightly fuzz run conservation_matcher -- -max_len=1048576
```

A stable compile-only check of the harness package is useful when the
installed compiler supports it:

```console
cargo check --manifest-path crates/srcmv-core/fuzz/Cargo.toml --locked
cargo clippy --manifest-path crates/srcmv-core/fuzz/Cargo.toml --all-targets --locked -- -D warnings
```

After a failure, minimize it:

```console
cargo +nightly fuzz tmin conservation_matcher artifacts/conservation_matcher/<artifact>
```
