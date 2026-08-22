# Resource limits

Version `0.1.0` uses the following maximum defaults. Trusted local configuration
may lower them but may not raise them. Every layer charges the limit when it first
knows the value and uses checked arithmetic before allocation or I/O.

| Resource | Limit |
|---|---:|
| JSON request bytes | 4 MiB |
| JSON nesting depth | 64 |
| Serialized JSON response | 16 MiB |
| Operations per batch | 1,000 |
| Distinct operation paths | 1,000 |
| UTF-8 relative path | 4,096 bytes |
| Individual snapshot file | 256 MiB |
| Total snapshot bytes | 1 GiB |
| Total line count | 5,000,000 |
| Line-index memory | 256 MiB |
| Total charged planning memory | 2 GiB |
| Resulting bytes per output | 512 MiB |
| Total planned output bytes | 1 GiB |
| Segments per output | 100,000 |
| Total segments | 250,000 |
| Changed transaction targets | 100 |
| Projected transaction disk use | 3 GiB |
| Manifest or state record | 16 MiB |
| State records per transaction | 512 |
| Cumulative state-record bytes | 128 MiB |
| Transaction directories scanned | 100 |
| Recovery bytes read per command | 256 MiB |
| Human or JSON diff bytes | 4 MiB |

## Semantic-selection limits

Selection protocol v1 is separately bounded. These defaults apply to one
`srcmv select` invocation. Lowerable limits are used in boundary tests;
trusted user configuration controls only the documented lifecycle deadlines and
cannot make the versioned response exceed its schema maxima.

| Resource | Limit |
|---|---:|
| UTF-8 workspace-relative source path | 4,096 bytes |
| Source snapshot / `didOpen` text | 8 MiB |
| Source files captured | 1 |
| Source line count | 5,000,000 |
| Source line-index memory | 256 MiB |
| Selection query name | 4,096 bytes |
| Position-conversion Unicode scalars examined | 16,777,216 |
| Raw document-symbol nodes | 100,000 |
| Flattened document-symbol candidates | 100,000 |
| Symbol hierarchy / breadcrumb elements | 256 |
| One symbol name | 4 KiB |
| One symbol detail | 16 KiB |
| One symbol breadcrumb | 64 KiB |
| Candidate-owned string storage | 64 MiB |
| Successful `--all` matches | 1,000 |
| Ambiguity candidates reported | 50 |
| Observation warnings reported | 16 |
| Serialized selection JSON or human output | 16 MiB |

Position work is cumulative across conversion of the selected snapshot and all
server ranges. UTF-16, UTF-32, and user scalar-column scans charge one unit per
Unicode scalar examined. UTF-8 boundary checks that can be performed in
constant time do not consume that work budget.

## Outline limits

One `srcmv outline` invocation shares every selection bound above: the same
snapshot, normalization, position-conversion, session, and transport defaults.
The frozen 1,000-match selection limit does not apply; one additional emission
cap does:

| Resource | Limit |
|---|---:|
| Successful outline symbols | 10,000 |
| Serialized outline JSON or human output | 16 MiB |

The symbol count is checked after ordering, deduplication, and `--kind`
filtering, before any serialization; exceeding it fails closed with
`LSP_RESOURCE_LIMIT_EXCEEDED` (resource `outline_symbols`) and no partial
output.

The language-server configuration and session have independent bounds:

| Resource | Limit |
|---|---:|
| User TOML configuration | 1 MiB |
| TOML or configured JSON nesting depth | 32 |
| Server descriptors | bounded by the 1 MiB document |
| Server ID or language ID / extension | 255 bytes |
| Program value | 16 KiB |
| Literal server arguments | 128 |
| One literal argument | 16 KiB |
| All literal arguments | 256 KiB |
| Initialization options | 1 MiB |
| Settings | 1 MiB |
| `workspace/configuration` items | 256 |
| Serialized configuration response | 1 MiB |
| Server-initiated requests | 64 per selection |
| Server notifications | 1,024 per selection |

Default initialization and document-symbol deadlines are 10 and 30 seconds.
Graceful shutdown and forced cleanup each receive 5 seconds. The total deadline
adds those four phase budgets plus a 10-second scheduling allowance, yielding a
60-second default. A trusted descriptor may configure the initialization and
request timeouts from 100 through 300000 milliseconds; the session total is
recomputed from those values, the two cleanup intervals, and the same allowance.

The stdio JSON-RPC transport enforces these defaults before retaining untrusted
server data:

| Resource | Limit |
|---|---:|
| LSP header including terminator | 16 KiB |
| Inbound JSON-RPC body | 16 MiB |
| Outbound JSON-RPC body | 64 MiB |
| JSON nesting depth | 64 |
| Request ID / method name | 256 bytes each |
| Request or notification parameters | 1 MiB |
| Pending client requests | 8 |
| Buffered validated messages | 1,024 / 32 MiB total body bytes |
| Ready process events drained per wake | 4,096 |
| Queued stdout | 32 chunks / 32 MiB |
| Queued stdin | 16 frames / 64 MiB |
| Completion events | 16 |
| Retained stderr tail | 64 KiB |

The response never includes source text, the server command line, initialization
options, settings, absolute workspace paths, or stderr. Exceeding a selection or
LSP bound fails closed with `LSP_RESOURCE_LIMIT_EXCEEDED`; output is not
partially emitted.

Detailed diff input is limited to 8 MiB per side and 10,000,000 explicitly
counted input-and-render work units. The linear-memory text comparison removes a
common exact prefix and suffix and reports the changed middle with original
terminator labels; it never allocates a quadratic matrix. Both human text and the
JSON-encoded diff string are capped at 4 MiB. Diff truncation never changes the
plan digest or commit.

An opt-in review summary is complete or preview fails; its rows are never
truncated. Indexed selected-range metrics and composable output-segment metrics
avoid rescanning or materializing output bytes solely for line counts. The
planner's conservative response projection charges 1,024 bytes per operation,
512 bytes per output, and 512 bytes per output segment, covering the
nonduplicative summary supplements and insertion groups. Summary metadata is
reserved within the 16 MiB serialized-response limit before the remaining budget
is assigned to detailed diff text. A response-driven reduction uses the existing
`DIFF_TRUNCATED` warning with reason `response_budget`.

Phase 4 charges every resulting output byte and retained segment. Changed-target
count uses byte classification, so an effectful but byte-identical output is not a
target. Planning memory excludes immutable snapshot storage and charges retained
operation/output records, segment enums, and owned path bytes. The projected
response charge is a conservative structural bound of 1,024 base bytes, 1,024
bytes plus paths per resolved operation, 512 bytes plus path per output, and 512
bytes per segment; Phase 6 additionally enforces the exact serialized response
limit.

Phase 5 bounds the complete binary envelope for each manifest or state record at
16 MiB, validates no more than 100 targets and 512 contiguous state snapshots,
and caps cumulative state records at 128 MiB. A diagnostic scan visits at most 100
active-plus-completed transaction directories and reads at most 256 MiB across
records and authorized artifacts. Checked arithmetic precedes every cumulative
charge. Persisted data beyond a structural bound is rejected rather than partially
interpreted or guessed through.

## Boundary verification

Every row above has below/at/above coverage using lowerable accounting limits or
numeric accounting test doubles, so the suite does not allocate the release
maximum merely to prove comparison behavior:

| Layer | Covered resources | Regression test |
|---|---|---|
| Protocol | request bytes, depth, operations, paths, path bytes | `request_resource_boundaries_should_fail_at_the_first_exceeded_limit` |
| CLI | serialized response bytes | `phase9_serialized_response_limit_covers_below_at_and_above_boundaries` |
| Snapshot | file/total bytes, identities, lines, index memory, path bytes | `phase9_snapshot_limits_cover_below_at_and_above_every_boundary` |
| Planner | output/total bytes, per-output/total segments, targets, response projection, planning memory | `planner_enforces_every_phase_four_resource_boundary` |
| Transaction | record bytes, targets, state count/bytes, directories, recovery bytes, projected disk | `journal_limits_should_pass_at_and_reject_below_each_known_schema_boundary` and `journal_scan_limits_should_reject_below_directory_recovery_and_state_byte_usage` |
| Diff | detailed input, work units, human/JSON output bytes | `phase9_diff_limits_cover_below_at_and_above_boundaries` |
