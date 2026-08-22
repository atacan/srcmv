# Conservation audit v1 golden vectors

Placeholder — the golden vectors land with Phase 3, when the report DTOs and
serialization budget exist to generate them from the fixture trees in
`tests/fixtures/conservation/`. CI will compare full serialized reports
byte-for-byte; human output is not pinned because it embeds absolute temp
paths by design.

The planned vector set covers the documented §15 scenario matrix:

- `pass.json`
- `pass-pairing-ambiguous.json`
- `pass-formatting.json`
- `fail.json`
- `fail-count-formatting.json`
- `manifest-mode.json`
- `inconclusive-work-budget.json`
- `inconclusive-report-truncated.json`
- `inconclusive-ambiguous-common.json`
- `inconclusive-strict-uniqueness.json`
- `inconclusive-skipped-input.json`

Every report must validate against
`docs/schema/conservation-audit-v1/report.schema.json`.
