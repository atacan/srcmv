//! Golden-vector generation and verification for conservation-audit v1.
//!
//! Drives the real pipeline over the Phase 0 fixture trees:
//! `read_audit_tree`/`read_audit_tree_from_manifest` → `audit_conservation`
//! (default options/budget unless a vector deliberately lowers them) →
//! `build_audit_report` → `audit_report_json_line`, then byte-compares the
//! result against the committed vector under `tests/golden/conservation-audit-v1/`.
//! There is no bless escape hatch: regenerate goldens with the Phase 3
//! generator described in that directory's README and commit the diff.

use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Registry, Validator};
use serde_json::Value;
use srcmv_core::{
    AuditFile, ConservationBudget, ConservationInput, ConservationOptions, audit_conservation,
};
#[cfg(debug_assertions)]
use srcmv_fs::audit_tree_faults;
use srcmv_fs::{AuditTreeFile, AuditTreeLimits, SkippedInput};
use srcmv_protocol::{
    AuditRenderLimits, AuditReport, InputTreeSummary, SkipInfo, audit_report_json_line,
    build_audit_report_with_limits,
};

const REPORT_SCHEMA_ID: &str = "https://raw.githubusercontent.com/atacan/srcmv/main/docs/schema/conservation-audit-v1/report.schema.json";

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures_root() -> PathBuf {
    repository_root().join("tests/fixtures/conservation")
}

fn golden_root() -> PathBuf {
    repository_root().join("tests/golden/conservation-audit-v1")
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{} must contain JSON: {error}", path.display()))
}

fn report_validator() -> Validator {
    let schema_path =
        repository_root().join("docs/schema/conservation-audit-v1/report.schema.json");
    let schema = read_json(&schema_path);
    let registry = Registry::new()
        .add(REPORT_SCHEMA_ID, schema.clone())
        .expect("report schema ID must be a valid URI")
        .prepare()
        .expect("offline schema registry must prepare");
    jsonschema::draft202012::options()
        .with_registry(&registry)
        .build(&schema)
        .expect("report schema must compile")
}

/// One ingested tree plus its skip records, kept alive for borrowed views.
struct IngestedTree {
    files: Vec<AuditTreeFile>,
    skips: Vec<SkippedInput>,
    kind: AuditKind,
}

#[derive(Clone, Copy)]
enum AuditKind {
    Directory,
    Manifest,
}

impl AuditKind {
    fn summary_kind(self) -> srcmv_protocol::AuditInputKind {
        match self {
            Self::Directory => srcmv_protocol::AuditInputKind::Directory,
            Self::Manifest => srcmv_protocol::AuditInputKind::Manifest,
        }
    }
}

fn read_directory(root: &Path) -> IngestedTree {
    let (files, skips) = srcmv_fs::read_audit_tree(root, AuditTreeLimits::default())
        .unwrap_or_else(|error| panic!("{} must ingest: {error}", root.display()));
    IngestedTree {
        files,
        skips,
        kind: AuditKind::Directory,
    }
}

fn read_manifest(manifest: &Path) -> IngestedTree {
    let (files, skips) =
        srcmv_fs::read_audit_tree_from_manifest(manifest, AuditTreeLimits::default())
            .unwrap_or_else(|error| panic!("{} must ingest: {error}", manifest.display()));
    IngestedTree {
        files,
        skips,
        kind: AuditKind::Manifest,
    }
}

fn summary(tree: &IngestedTree) -> InputTreeSummary {
    let bytes: usize = tree.files.iter().map(|file| file.bytes.len()).sum();
    InputTreeSummary::new(
        tree.kind.summary_kind(),
        u64::try_from(tree.files.len()).expect("file count fits u64"),
        u64::try_from(bytes).expect("byte count fits u64"),
    )
}

fn projection(tree: &IngestedTree) -> Vec<AuditFile<'_>> {
    tree.files
        .iter()
        .map(|file| AuditFile {
            path: file.path.value.as_bytes(),
            bytes: &file.bytes,
            digest: &file.digest,
        })
        .collect()
}

/// Runs the complete real pipeline and returns the single wire line.
fn pipeline_line(
    base: &IngestedTree,
    head: &IngestedTree,
    options: ConservationOptions,
    budget: ConservationBudget,
    limits: AuditRenderLimits,
) -> String {
    let base_view = projection(base);
    let head_view = projection(head);
    let outcome = audit_conservation(ConservationInput {
        base: &base_view,
        head: &head_view,
        options,
        budget,
    })
    .expect("fixture pipelines must complete inside the engine");
    let skips: Vec<SkipInfo> = base
        .skips
        .iter()
        .chain(head.skips.iter())
        .map(|skipped| SkipInfo::new(skipped.path.value.clone()))
        .collect();
    let report: AuditReport = build_audit_report_with_limits(
        &outcome,
        &options,
        (summary(base), summary(head)),
        &skips,
        &head_view,
        limits,
    )
    .expect("fixture pipelines must render");
    audit_report_json_line(&report).expect("fixture reports must serialize")
}

fn default_options() -> ConservationOptions {
    ConservationOptions::default()
}

fn strict_uniqueness_options() -> ConservationOptions {
    ConservationOptions::new(32, 16, true, false).expect("strict options should validate")
}

fn count_formatting_options() -> ConservationOptions {
    ConservationOptions::new(32, 16, false, true).expect("counting options should validate")
}

fn dry_work_budget() -> ConservationBudget {
    // Extraction pool dies immediately; §10 stop-and-unanalyzed applies.
    ConservationBudget::new(1, u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64)
}

fn zero_unmatched_listing_cap() -> AuditRenderLimits {
    AuditRenderLimits::new(2_000, 0, 200, 50, 16 * 1024 * 1024)
}

fn scenario_pair(scenario: &str) -> (IngestedTree, IngestedTree) {
    let root = fixtures_root().join(scenario);
    (
        read_directory(&root.join("base")),
        read_directory(&root.join("head")),
    )
}

/// Every planned vector with its notable options and hand-derived verdict.
fn planned_vectors() -> Vec<(&'static str, String)> {
    let mut vectors = Vec::new();

    let (base, head) = scenario_pair("pass-move-across-files");
    vectors.push((
        "pass.json",
        pipeline_line(
            &base,
            &head,
            default_options(),
            ConservationBudget::default(),
            AuditRenderLimits::default(),
        ),
    ));

    let (base, head) = scenario_pair("pass-pairing-ambiguous");
    vectors.push((
        "pass-pairing-ambiguous.json",
        pipeline_line(
            &base,
            &head,
            default_options(),
            ConservationBudget::default(),
            AuditRenderLimits::default(),
        ),
    ));
    vectors.push((
        "inconclusive-strict-uniqueness.json",
        pipeline_line(
            &base,
            &head,
            strict_uniqueness_options(),
            ConservationBudget::default(),
            AuditRenderLimits::default(),
        ),
    ));

    let (base, head) = scenario_pair("pass-formatting");
    vectors.push((
        "pass-formatting.json",
        pipeline_line(
            &base,
            &head,
            default_options(),
            ConservationBudget::default(),
            AuditRenderLimits::default(),
        ),
    ));
    vectors.push((
        "fail-count-formatting.json",
        pipeline_line(
            &base,
            &head,
            count_formatting_options(),
            ConservationBudget::default(),
            AuditRenderLimits::default(),
        ),
    ));

    let (base, head) = scenario_pair("fail-deleted-block");
    vectors.push((
        "fail.json",
        pipeline_line(
            &base,
            &head,
            default_options(),
            ConservationBudget::default(),
            AuditRenderLimits::default(),
        ),
    ));
    vectors.push((
        "inconclusive-work-budget.json",
        pipeline_line(
            &base,
            &head,
            default_options(),
            dry_work_budget(),
            AuditRenderLimits::default(),
        ),
    ));
    vectors.push((
        "inconclusive-report-truncated.json",
        pipeline_line(
            &base,
            &head,
            default_options(),
            ConservationBudget::default(),
            zero_unmatched_listing_cap(),
        ),
    ));

    let (base, head) = scenario_pair("inconclusive-ambiguous-common");
    vectors.push((
        "inconclusive-ambiguous-common.json",
        pipeline_line(
            &base,
            &head,
            default_options(),
            ConservationBudget::default(),
            AuditRenderLimits::default(),
        ),
    ));

    let manifest_base =
        read_manifest(&fixtures_root().join("manifest-mode/base/base-manifest.lst"));
    let manifest_head =
        read_manifest(&fixtures_root().join("manifest-mode/head/head-manifest.lst"));
    vectors.push((
        "manifest-mode.json",
        pipeline_line(
            &manifest_base,
            &manifest_head,
            default_options(),
            ConservationBudget::default(),
            AuditRenderLimits::default(),
        ),
    ));

    // The vanished-file fault seam is compiled out of release builds, where
    // installation is a no-op; this vector therefore exists only in debug
    // test builds, exactly like the srcmv-fs seam tests it mirrors.
    #[cfg(debug_assertions)]
    {
        audit_tree_faults::install(audit_tree_faults::Faults {
            vanish: vec!["docs/note.txt".to_owned()],
            unstable: Vec::new(),
            type_flip: Vec::new(),
        });
        let (base, head) = scenario_pair("pass-move-across-files");
        audit_tree_faults::clear();
        vectors.push((
            "inconclusive-skipped-input.json",
            pipeline_line(
                &base,
                &head,
                default_options(),
                ConservationBudget::default(),
                AuditRenderLimits::default(),
            ),
        ));
    }

    vectors
}

/// Hand-derived expectations pinned by tests/fixtures/conservation/README.md.
fn expected_outcome(name: &str) -> (&'static str, &'static [&'static str]) {
    match name {
        "pass.json" => ("pass", &[]),
        "pass-pairing-ambiguous.json" => ("pass", &[]),
        "pass-formatting.json" => ("pass", &[]),
        "fail.json" => ("fail", &[]),
        "fail-count-formatting.json" => ("fail", &[]),
        "manifest-mode.json" => ("pass", &[]),
        "inconclusive-work-budget.json" => ("inconclusive", &["work_budget_exceeded"]),
        "inconclusive-report-truncated.json" => ("inconclusive", &["report_truncated"]),
        "inconclusive-ambiguous-common.json" => ("inconclusive", &["ambiguous_common"]),
        "inconclusive-strict-uniqueness.json" => ("inconclusive", &["strict_uniqueness_ambiguity"]),
        "inconclusive-skipped-input.json" => ("inconclusive", &["unreadable_input_skipped"]),
        other => panic!("unplanned golden vector {other}"),
    }
}

fn is_digest_spelling(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn intervals_disjoint(intervals: &[(u64, u64)]) -> bool {
    let mut sorted = intervals.to_vec();
    sorted.sort();
    sorted.windows(2).all(|pair| pair[0].1 <= pair[1].0)
}

/// Recomputes every runtime-only guarantee from the serialized report alone:
/// the §9.2 ledger identity, consumed/new-span disjointness, range widths,
/// digest spellings, and the deterministic §11.1 orderings.
fn assert_serialized_contract(line: &str, name: &str) {
    let value: Value = serde_json::from_str(line.trim_end())
        .unwrap_or_else(|error| panic!("{name} must be one JSON value: {error}"));

    let verdict = value["verdict"].as_str().expect("verdict string");
    assert_eq!(verdict, expected_outcome(name).0, "{name} verdict");
    let reasons: Vec<&str> = value["inconclusive_reasons"]
        .as_array()
        .expect("reasons array")
        .iter()
        .map(|reason| reason.as_str().expect("reason string"))
        .collect();
    assert_eq!(reasons, expected_outcome(name).1, "{name} reasons");
    assert_eq!(
        verdict == "inconclusive",
        !reasons.is_empty(),
        "{name} verdict/reasons coherence"
    );
    assert_eq!(value["conservation_audit_version"], 1, "{name}");
    assert_eq!(value["warnings"], serde_json::json!([]), "{name}");

    let totals = &value["totals"];
    let term = |key: &str| totals[key].as_u64().expect(key);
    let ledger_sum = term("conserved_exact_bytes")
        + term("conserved_fragments_bytes")
        + term("insubstantial_residue_bytes")
        + term("unmatched_substantial_bytes")
        + term("formatting_only_bytes")
        + term("ambiguous_common_bytes")
        + term("unanalyzed_bytes");
    assert_eq!(ledger_sum, term("removed_bytes"), "{name} ledger identity");
    assert_eq!(
        term("conserved_exact_bytes") + term("conserved_fragments_bytes"),
        term("conserved_bytes"),
        "{name} conserved aggregate"
    );

    let mut consumed: std::collections::BTreeMap<String, Vec<(u64, u64)>> =
        std::collections::BTreeMap::new();
    for region in value["regions"].as_array().expect("regions array") {
        assert_eq!(region["status"], "conserved", "{name}");
        let (start, end) = (
            region["source_byte_range"][0].as_u64().expect("start"),
            region["source_byte_range"][1].as_u64().expect("end"),
        );
        assert_eq!(
            end - start,
            region["length"].as_u64().expect("length"),
            "{name}"
        );
        assert!(
            is_digest_spelling(region["sha256"].as_str().expect("digest")),
            "{name}"
        );
        let rows = region["destinations"].as_array().expect("destinations");
        assert!(!rows.is_empty(), "{name} conserved destinations non-empty");
        let mut previous: Option<(String, u64)> = None;
        for destination in rows {
            let path = destination["path"].as_str().expect("path").to_owned();
            let span = (
                destination["byte_range"][0].as_u64().expect("start"),
                destination["byte_range"][1].as_u64().expect("end"),
            );
            assert!(span.0 < span.1, "{name} destination width");
            if let Some(previous) = &previous {
                assert!(
                    previous.0 < path || (previous.0 == path && previous.1 < span.0),
                    "{name} destinations ordered by (path, offset)"
                );
            }
            previous = Some((path.clone(), span.0));
            consumed.entry(path).or_default().push(span);
        }
    }

    // New-content spans may legitimately contain consumed intervals; the
    // §8.8 guarantee is that each surviving length equals its width minus
    // the consumed bytes inside it, and that spans stay disjoint from each
    // other per path.
    let mut fresh: std::collections::BTreeMap<String, Vec<(u64, u64)>> =
        std::collections::BTreeMap::new();
    for span in value["new_content"].as_array().expect("new_content array") {
        let path = span["path"].as_str().expect("path").to_owned();
        let range = (
            span["byte_range"][0].as_u64().expect("start"),
            span["byte_range"][1].as_u64().expect("end"),
        );
        let length = span["length"].as_u64().expect("length");
        assert!(range.0 < range.1, "{name} new-span width");
        assert!(
            length > 0 && length <= range.1 - range.0,
            "{name} surviving length"
        );
        assert!(
            is_digest_spelling(span["sha256"].as_str().expect("digest")),
            "{name}"
        );
        let covered_inside: u64 = consumed
            .get(&path)
            .map(|intervals| {
                intervals
                    .iter()
                    .map(|&(low, high)| high.min(range.1).saturating_sub(low.max(range.0)))
                    .sum()
            })
            .unwrap_or(0);
        assert_eq!(
            covered_inside,
            range.1 - range.0 - length,
            "{name} new-span survivor accounting ({path})"
        );
        fresh.entry(path).or_default().push(range);
    }
    for (path, intervals) in &consumed {
        assert!(
            intervals_disjoint(intervals),
            "{name} consumed intervals must stay disjoint per path ({path})"
        );
    }
    for (path, intervals) in &fresh {
        assert!(
            intervals_disjoint(intervals),
            "{name} new-content spans must stay disjoint per path ({path})"
        );
    }

    let mut previous_length = u64::MAX;
    let mut previous_id = 0_u64;
    for region in value["regions"].as_array().expect("regions array") {
        let length = region["length"].as_u64().expect("length");
        let id = region["id"].as_u64().expect("id");
        assert!(
            length < previous_length || (length == previous_length && id > previous_id),
            "{name} regions ordered largest-first then id"
        );
        previous_length = length;
        previous_id = id;
    }
    for listing in ["unmatched", "formatting"] {
        let status = match listing {
            "unmatched" => "unmatched_substantial",
            _ => "formatting_only",
        };
        let mut previous_length = u64::MAX;
        let mut previous_id = 0_u64;
        for entry in value[listing].as_array().expect(listing) {
            assert_eq!(entry["status"], status, "{name} {listing} status");
            let length = entry["length"].as_u64().expect("length");
            let id = entry["id"].as_u64().expect("id");
            assert!(
                length < previous_length || (length == previous_length && id > previous_id),
                "{name} {listing} ordered largest-first then id"
            );
            previous_length = length;
            previous_id = id;
        }
    }
}

#[test]
fn every_committed_golden_should_match_the_real_pipeline_byte_for_byte() {
    let validator = report_validator();
    let vectors = planned_vectors();

    let mut names: Vec<&str> = vectors.iter().map(|(name, _)| *name).collect();
    names.sort();
    assert_eq!(names.len(), vectors.len(), "vector names are unique");

    for (name, line) in &vectors {
        let committed = golden_root().join(name);
        let expected = fs::read(&committed)
            .unwrap_or_else(|error| panic!("{} must be readable: {error}", committed.display()));
        assert_eq!(
            line.as_bytes(),
            expected.as_slice(),
            "{name} drifted from the committed vector; regenerate per the golden README"
        );

        let instance = read_json(&committed);
        validator
            .validate(&instance)
            .unwrap_or_else(|error| panic!("{name} must satisfy the frozen schema: {error}"));
        assert_serialized_contract(line, name);
    }
}

#[test]
fn the_planned_vector_set_should_cover_the_documented_matrix() {
    let names: Vec<&str> = planned_vectors().iter().map(|(name, _)| *name).collect();
    for name in [
        "pass.json",
        "pass-pairing-ambiguous.json",
        "pass-formatting.json",
        "fail.json",
        "fail-count-formatting.json",
        "manifest-mode.json",
        "inconclusive-work-budget.json",
        "inconclusive-report-truncated.json",
        "inconclusive-ambiguous-common.json",
        "inconclusive-strict-uniqueness.json",
    ] {
        assert!(names.contains(&name), "{name} must be produced");
    }
    // The vanished-file vector relies on the debug-only fault seam.
    #[cfg(debug_assertions)]
    assert!(
        names.contains(&"inconclusive-skipped-input.json"),
        "inconclusive-skipped-input.json must be produced"
    );
}

#[test]
fn identical_pipeline_runs_should_be_byte_identical() {
    let expected_count = if cfg!(debug_assertions) { 11 } else { 10 };
    for repeat in 0..2 {
        let vectors = planned_vectors();
        assert_eq!(
            vectors.len(),
            expected_count,
            "run {repeat}: eleven debug-build vectors, ten without the fault seam"
        );
        for (name, line) in &vectors {
            let committed = fs::read(golden_root().join(name)).expect("committed golden");
            assert_eq!(line.as_bytes(), committed.as_slice(), "{name} run {repeat}");
        }
    }
}

#[test]
fn manifest_mode_should_record_manifest_kinds_in_the_inputs_object() {
    let manifest_base =
        read_manifest(&fixtures_root().join("manifest-mode/base/base-manifest.lst"));
    let manifest_head =
        read_manifest(&fixtures_root().join("manifest-mode/head/head-manifest.lst"));
    let line = pipeline_line(
        &manifest_base,
        &manifest_head,
        default_options(),
        ConservationBudget::default(),
        AuditRenderLimits::default(),
    );
    let value: Value = serde_json::from_str(line.trim_end()).expect("one JSON value");
    assert_eq!(value["inputs"]["base"]["kind"], "manifest");
    assert_eq!(value["inputs"]["head"]["kind"], "manifest");
}
