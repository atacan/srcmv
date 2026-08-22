# Conservation audit v1 report

`report.schema.json` is the normative JSON Schema Draft 2020-12 contract for
the read-only `srcmv audit-conservation` surface (`docs/conservation-audit-plan.md`).
It does not modify edit protocol version 1, plan-hash version 1, transaction-record
version 1, capability output, or the frozen edit error and warning registries.
Reports serialize as exactly one UTF-8 JSON value plus LF on stdout.

As in every srcmv schema, schema `maxLength` counts Unicode characters, not
encoded bytes; the 4096-character path bound mirrors the 4096-byte path limit
of §10. Serialized reports are additionally bounded at runtime by the global
16 MiB report cap, with unknown fields and duplicate JSON keys rejected by the
protocol layer — overflow behavior follows plan §11.3 up to and including the
`RESOURCE_LIMIT_EXCEEDED` error envelope when even the reserved skeleton
cannot fit.

## Verdict contract

`verdict` is a closed vocabulary: `pass`, `fail`, or `inconclusive`. A `pass`
means every substantial removed byte was found verbatim under one-to-one
consumption accounting; a `fail` means at least one substantial removed region
has no remaining exact occurrence; an `inconclusive` means no conservation
claim is made either way. The derivation order is normative (§9.1): fatal
conditions force `inconclusive`; otherwise any unmatched-substantial byte
forces `fail` (and formatting bytes too when the run carried
`count_formatting`); only then may the verdict be `pass`.

## Closed vocabularies

- Region `status`: exactly `conserved`, `unmatched_substantial`,
  `formatting_only`. Records exist only for these three classifications;
  `insubstantial_residue` and `ambiguous_common` bytes are reported through
  totals and counts alone.
- `inconclusive_reasons[]`: exactly `work_budget_exceeded`,
  `memory_budget_exceeded`, `unreadable_input_skipped`,
  `strict_uniqueness_ambiguity`, `ambiguous_common`, `report_truncated`
  (§9.3).
- `warnings` is pinned to the empty array in v1 because v1 defines no warning
  codes (§12); per the §13 evolution rule any future warning is a
  wire-visible change requiring a `conservation_audit_version` bump.

## Totals and the ledger identity

The `totals` object carries exactly thirteen keys: the removed-region and
removed-byte counters, the seven §9.2 ledger byte terms
(`conserved_exact_bytes`, `conserved_fragments_bytes`,
`unmatched_substantial_bytes`, `insubstantial_residue_bytes`,
`formatting_only_bytes`, `ambiguous_common_bytes`, `unanalyzed_bytes`), the
convenience aggregate `conserved_bytes`
(`conserved_exact_bytes + conserved_fragments_bytes`), `new_bytes`, and the
two ambiguity counts. Each §9.2 ledger term maps to its JSON field by
appending `_bytes`, and `removed_bytes` equals the sum of all seven ledger
terms. The identity

```text
removed_bytes = conserved_exact_bytes + conserved_fragments_bytes
              + formatting_only_bytes + insubstantial_residue_bytes
              + unmatched_substantial_bytes + ambiguous_common_bytes
              + unanalyzed_bytes
```

holds on every completed run (§9.2) but is a runtime-only guarantee: JSON
Schema cannot express it, so implementations must recompute it when building
the report and conformance tests must recheck it from serialized reports.

## Records, coordinates, and ordering

Region listings share one record shape (`id`, `source_path`,
`source_byte_range`, `source_line_range`, `length`, `sha256`, `status`),
partitioned by that status: `regions[]` lists `conserved` records only, each
carrying `pairing_ambiguous` and a non-empty `destinations` array (nine
required fields) — a rule the schema itself encodes via
`"status": {"const": "conserved"}` plus `required` — while `unmatched[]`
lists `unmatched_substantial` records and `formatting[]` lists
`formatting_only` records (both omitting `pairing_ambiguous`). Destination
entries appear as separate spans ordered by `(path, offset)`, which is how
splits stay visible. `formatting[]` entries may additionally annotate a
`paired_destination` carrying exactly `{path, byte_range}` — it deliberately
omits `line_range`.

Runtime code additionally guarantees the following relationships, which JSON
Schema cannot express:

- byte ranges are zero-based half-open `[start, end)` pairs with
  `start < end`; each source range lies within its base file bounds and each
  destination/new-content range within its head file bounds;
- line ranges are one-based inclusive `[first_line, last_line]` under existing
  terminator semantics and are exactly `null` for binary files (`is_binary`
  reused from preview: contains NUL or invalid UTF-8, §9.5);
- `sha256(head_file[dest_range]) == sha256(base_file[src_range])` for every
  destination span, so reviewers can verify any claim independently;
- destinations within one region never overlap consumed or other credited
  intervals (one-to-one accounting, §8.6);
- `min_substantial_bytes >= min_fragment_bytes >= 16` holds for accepted runs;
  this pairwise option relation is enforced at CLI parse time (§6.1) and
  cannot be expressed pairwise by JSON Schema — the individual `>= 16` floors
  on both options ARE encoded in the schema.

## Bounding and truncation

Release caps mirror §10 and are encoded as array `maxItems`: 2000 listed
regions, 200 unmatched entries, 200 formatting entries, 50 new-content spans,
and six inconclusive reasons. When the `regions[]` entry cap forces drops, the
smallest conserved regions go first and the report gains a top-level
`omitted_region_count` sibling (absent otherwise) without changing the
verdict; dropping any unmatched/formatting listing entry instead sets reason
`report_truncated`, which is fatal and downgrades the verdict to
`inconclusive` (§11.3). Truncation can therefore never manufacture a `pass`.
Lower configured limits are permitted, but implementations must not emit a
report that exceeds these versioned maxima.

## Golden vectors

The golden vectors in `tests/golden/conservation-audit-v1/` pin full
serialized reports for the documented scenario matrix; they land with Phase 3
and CI compares them byte-for-byte against real-binary output over the
fixtures in `tests/fixtures/conservation/`. The illustrative JSON in plan
§11.1 elides digests (`sha256:9f2c…`) and is therefore not itself a valid
instance of this schema; real reports always carry digests of exactly 64
lowercase hex digits.
