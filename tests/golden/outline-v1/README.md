# Read-only outline v1 golden vectors

These hand-authored files freeze representative outline-v1 wire shapes
independently from the edit protocol-v1 and selection-v1 golden vectors.

- `success.json` is a successful two-symbol listing of the standard CLI fixture
  (`source.rs`, the 78-byte `pub struct Outer; ...` document served by the fake
  language server). It pins one-based inclusive lines, always-populated
  one-based scalar columns, exclusive past-end columns, depth derivation,
  selection-v1 kind spellings, raw zero-based LSP audit ranges, and the
  validated half-open byte selector for both a root symbol and its nested child.
- `empty.json` is the successful no-symbols response (`null` or `[]` server
  result, or nothing surviving `--kind` filtering). An empty listing is success,
  never an error.

Both examples use the fake server's identity (`srcmv-fake-lsp`, version `1`,
UTF-16 negotiation) with no configuration ID, and carry the only warning
outline v1 can emit: `OBSERVATION_MAY_BE_STALE`, recorded because these
read-only observations are not coordinated by a pre-existing shared lock.
`workspace_identity_hash` is the
shared non-sensitive placeholder used because the hash covers the physical
identity of the workspace root; runtime tests substitute the observed digest
before comparing structures. `source.sha256` is the real digest of the fixture
bytes.
