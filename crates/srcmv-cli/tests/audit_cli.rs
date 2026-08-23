//! End-to-end conservation-audit CLI tests driving the real binary over
//! tempfile trees (plan §15 integration list and §12 exit-code table).

use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::LazyLock;

use jsonschema::{Registry, Validator};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const REPORT_SCHEMA_ID: &str = "https://raw.githubusercontent.com/atacan/srcmv/main/docs/schema/conservation-audit-v1/report.schema.json";

/// Compiles the frozen report schema offline exactly like the protocol-layer
/// conformance tests so emitted reports can be validated end-to-end.
static REPORT_VALIDATOR: LazyLock<Validator> = LazyLock::new(|| {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/schema/conservation-audit-v1/report.schema.json");
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
    let schema: Value = serde_json::from_slice(&bytes).expect("report schema must contain JSON");
    let registry = Registry::new()
        .add(REPORT_SCHEMA_ID, schema.clone())
        .expect("report schema ID must be a valid URI")
        .prepare()
        .expect("offline schema registry must prepare");
    jsonschema::draft202012::options()
        .with_registry(&registry)
        .build(&schema)
        .expect("report schema must compile")
});

struct Tree(TempDir);

impl Tree {
    fn new() -> Self {
        Self(TempDir::new().expect("temporary tree should be created"))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn write(&self, relative: &str, bytes: &[u8]) {
        let full = self.0.path().join(relative);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("fixture parent should be created");
        }
        fs::write(full, bytes).expect("fixture should be written");
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn run_audit(base: &Path, head: &Path, extra: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_srcmv"));
    command
        .arg("audit-conservation")
        .arg("--base")
        .arg(base)
        .arg("--head")
        .arg(head);
    command.args(extra);
    for (key, value) in environment {
        command.env(key, value);
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("srcmv should start")
}

/// Asserts a completed audit: exit 0, empty stderr, exactly one JSON value.
fn completed_report(output: &Output) -> Value {
    assert_eq!(
        output.status.code(),
        Some(0),
        "completed audits must always exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    single_json_line(output)
}

fn single_json_line(output: &Output) -> Value {
    let stdout = std::str::from_utf8(&output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.ends_with('\n'), "JSON mode must end with LF");
    assert_eq!(stdout.lines().count(), 1, "exactly one JSON value plus LF");
    serde_json::from_str(stdout).expect("stdout should contain one JSON value")
}

fn error_envelope(output: &Output, expected_exit: i32, expected_code: &str) -> Value {
    assert_eq!(output.status.code(), Some(expected_exit));
    let report = single_json_line(output);
    assert!(
        output.stderr.is_empty(),
        "json-mode errors belong on stdout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["code"], expected_code, "{report}");
    report
}

const BYSTANDER: &[u8] = b"bystander-notes-do-not-change-0123456789abcdef\n";
const MOVE_PAYLOAD: &[u8] = b"moved-block-payload-Alpha-000111222333444555666777888999";
const SPLIT_PAYLOAD: &[u8] = b"split-block-payload-Epsilon-AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH";

fn move_fixture() -> (Tree, Tree) {
    let base = Tree::new();
    let head = Tree::new();
    base.write("notes/readme.txt", BYSTANDER);
    base.write("src/moved-block.txt", MOVE_PAYLOAD);
    let mut wrapped = b"// applied:\n".to_vec();
    wrapped.extend_from_slice(MOVE_PAYLOAD);
    wrapped.extend_from_slice(b"\n// end\n");
    head.write("notes/readme.txt", BYSTANDER);
    head.write("src/applied/moved.txt", &wrapped);
    (base, head)
}

#[test]
fn move_across_files_should_pass_and_record_the_destination_span() {
    let (base, head) = move_fixture();

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "pass");
    assert_eq!(
        report["inputs"]["base"],
        json!({"kind": "directory", "files": 2, "bytes": BYSTANDER.len() + MOVE_PAYLOAD.len()})
    );
    assert_eq!(report["totals"]["removed_regions"], 1);
    assert_eq!(
        report["totals"]["conserved_exact_bytes"],
        MOVE_PAYLOAD.len()
    );
    assert_eq!(report["totals"]["insubstantial_residue_bytes"], 0);
    let regions = report["regions"].as_array().expect("regions array");
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0]["source_path"], "src/moved-block.txt");
    let destinations = regions[0]["destinations"].as_array().expect("destinations");
    assert_eq!(destinations.len(), 1);
    assert_eq!(destinations[0]["path"], "src/applied/moved.txt");
    assert_eq!(
        destinations[0]["byte_range"],
        json!([12, 12 + MOVE_PAYLOAD.len()])
    );
    assert!(REPORT_VALIDATOR.is_valid(&report));
}

#[test]
fn split_block_should_pass_with_a_multi_destination_record() {
    let base = Tree::new();
    let head = Tree::new();
    let mut holder = b"frame-top\n".to_vec();
    holder.extend_from_slice(SPLIT_PAYLOAD);
    holder.extend_from_slice(b"\nframe-bottom\n");
    base.write("split/holder.txt", &holder);
    let mut part_a = SPLIT_PAYLOAD[..32].to_vec();
    part_a.push(b'\n');
    let mut part_b = SPLIT_PAYLOAD[32..].to_vec();
    part_b.push(b'\n');
    head.write("split/part-a.txt", &part_a);
    head.write("split/part-b.txt", &part_b);
    head.write("split/holder.txt", b"frame-top\n\nframe-bottom\n");

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "pass");
    assert_eq!(report["totals"]["removed_regions"], 1);
    assert_eq!(report["totals"]["conserved_exact_bytes"], 0);
    assert_eq!(
        report["totals"]["conserved_fragments_bytes"],
        SPLIT_PAYLOAD.len()
    );
    let regions = report["regions"].as_array().expect("regions array");
    let destinations = regions[0]["destinations"].as_array().expect("destinations");
    assert_eq!(
        destinations.len(),
        2,
        "splits stay visible as separate spans"
    );
    assert_eq!(destinations[0]["path"], "split/part-a.txt");
    assert_eq!(destinations[1]["path"], "split/part-b.txt");
}

#[test]
fn reorder_only_should_pass_inside_one_file() {
    let a: &[u8] = b"reorder-block-first-Beta-012345678901234";
    let b: &[u8] = b"REORDER-BLOCK-SECOND-Gamma-01234567890123456";
    let join = |parts: &[&[u8]]| parts.concat();
    let base = Tree::new();
    let head = Tree::new();
    base.write(
        "reorder/data.txt",
        &join(&[b"header-line\n", a, b"\n", b, b"\ntrailer-line\n"]),
    );
    head.write(
        "reorder/data.txt",
        &join(&[b"header-line\n", b, b"\n", a, b"\ntrailer-line\n"]),
    );

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "pass", "{report}");
    assert_eq!(report["totals"]["unmatched_substantial_bytes"], 0);
    assert_eq!(report["totals"]["formatting_only_bytes"], 0);
}

#[test]
fn rename_as_delete_create_should_pass_without_new_bytes() {
    let content: &[u8] = b"rename-scenario-content-Omega-01234567890123456789012345\n";
    let base = Tree::new();
    let head = Tree::new();
    base.write("legacy/old-module.txt", content);
    head.write("modern/new-module.txt", content);

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "pass");
    assert_eq!(report["totals"]["removed_regions"], 1);
    assert_eq!(report["totals"]["conserved_exact_bytes"], content.len());
    assert_eq!(report["totals"]["new_bytes"], 0);
}

#[test]
fn deleted_block_should_fail_with_the_full_unmatched_record_fields() {
    let payload: &[u8] = b"deleted-block-payload-Theta-012345678901234567890123456789";
    let base = Tree::new();
    let head = Tree::new();
    base.write("gone/payload.txt", payload);
    head.write(
        "other/unrelated.txt",
        b"surviving-unrelated-content-Quiet-9876543210abcdef",
    );

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "fail");
    assert!(
        report["inconclusive_reasons"]
            .as_array()
            .expect("reasons array")
            .is_empty()
    );
    let unmatched = report["unmatched"].as_array().expect("unmatched array");
    assert_eq!(unmatched.len(), 1);
    assert_eq!(unmatched[0]["source_path"], "gone/payload.txt");
    assert_eq!(unmatched[0]["source_byte_range"], json!([0, payload.len()]));
    assert_eq!(unmatched[0]["length"], payload.len());
    assert_eq!(unmatched[0]["sha256"], json!(digest(payload)));
    assert_eq!(unmatched[0]["status"], "unmatched_substantial");
    assert_eq!(unmatched[0]["source_line_range"], json!([1, 1]));
    assert_eq!(
        report["totals"]["unmatched_substantial_bytes"],
        payload.len()
    );
    assert!(REPORT_VALIDATOR.is_valid(&report));
}

const DUPLICATE_CONTENT: &[u8] = b"duplicate-boilerplate-Zeta-0123456789012345678901234\n";

fn duplicate_trees(survivors: usize) -> (Tree, Tree) {
    let base = Tree::new();
    let head = Tree::new();
    base.write("dup/copy-one.txt", DUPLICATE_CONTENT);
    base.write("dup/copy-two.txt", DUPLICATE_CONTENT);
    if survivors >= 1 {
        head.write("keep/retained-a.txt", DUPLICATE_CONTENT);
    }
    if survivors >= 2 {
        head.write("keep/retained-b.txt", DUPLICATE_CONTENT);
    }
    (base, head)
}

#[test]
fn duplicate_boilerplate_two_vs_two_should_pass_with_pairing_ambiguity_flags() {
    let (base, head) = duplicate_trees(2);

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "pass");
    assert_eq!(report["totals"]["pairing_ambiguous_regions"], 2);
    assert_eq!(
        report["totals"]["conserved_exact_bytes"],
        2 * DUPLICATE_CONTENT.len()
    );
    for region in report["regions"].as_array().expect("regions") {
        assert_eq!(region["pairing_ambiguous"], true);
    }
}

#[test]
fn duplicate_boilerplate_one_survivor_should_fail() {
    let (base, head) = duplicate_trees(1);

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "fail");
    assert_eq!(report["totals"]["pairing_ambiguous_regions"], 1);
    assert_eq!(
        report["totals"]["unmatched_substantial_bytes"],
        DUPLICATE_CONTENT.len()
    );
}

#[test]
fn strict_uniqueness_should_downgrade_duplicates_to_inconclusive() {
    let (base, head) = duplicate_trees(2);

    let lenient = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));
    assert_eq!(lenient["verdict"], "pass");

    let strict = completed_report(&run_audit(
        base.path(),
        head.path(),
        &["--json", "--strict-uniqueness"],
        &[],
    ));
    assert_eq!(strict["verdict"], "inconclusive");
    assert_eq!(
        strict["inconclusive_reasons"],
        json!(["strict_uniqueness_ambiguity"])
    );
}

fn formatting_trees() -> (Tree, Tree) {
    let base = Tree::new();
    let head = Tree::new();
    base.write("fmt/file.txt", b"alpha-one\n    beta-two\ngamma-three\n");
    head.write("fmt/file.txt", b"alpha-one\nbeta-two\ngamma-three\n");
    (base, head)
}

#[test]
fn formatting_only_delta_should_pass_with_a_report_by_default_and_fail_under_count_formatting() {
    let (base, head) = formatting_trees();

    let default_run = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));
    assert_eq!(default_run["verdict"], "pass");
    let formatting = default_run["formatting"]
        .as_array()
        .expect("formatting array");
    assert!(
        !formatting.is_empty(),
        "rewrites must stay visible: {default_run}"
    );
    assert_eq!(formatting[0]["status"], "formatting_only");
    assert!(
        default_run["totals"]["formatting_only_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );

    let counting = completed_report(&run_audit(
        base.path(),
        head.path(),
        &["--json", "--count-formatting"],
        &[],
    ));
    assert_eq!(counting["verdict"], "fail");
}

#[test]
fn new_file_should_surface_in_new_content_and_deleted_file_in_removal_totals() {
    let stay: &[u8] = b"staying-file-content-Iota-0123456789abcdef\n";
    let fresh: &[u8] = b"brand-new-content-Kappa-9876543210fedcba\n";

    let base = Tree::new();
    let head = Tree::new();
    base.write("stay.txt", stay);
    head.write("stay.txt", stay);
    head.write("added/new.txt", fresh);

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));
    assert_eq!(report["verdict"], "pass");
    assert_eq!(report["totals"]["new_bytes"], fresh.len());
    let new_content = report["new_content"].as_array().expect("new_content array");
    assert_eq!(new_content.len(), 1);
    assert_eq!(new_content[0]["path"], "added/new.txt");
    assert_eq!(new_content[0]["length"], fresh.len());
    assert_eq!(new_content[0]["byte_range"], json!([0, fresh.len()]));

    // Deleted-only accounting: everything removed, nothing surviving.
    let emptied_base = Tree::new();
    let emptied_head = Tree::new();
    emptied_base.write("old/gone.txt", fresh);
    let deletion = completed_report(&run_audit(
        emptied_base.path(),
        emptied_head.path(),
        &["--json"],
        &[],
    ));
    assert_eq!(deletion["verdict"], "fail");
    assert_eq!(deletion["totals"]["removed_bytes"], fresh.len());
    assert_eq!(
        deletion["totals"]["unmatched_substantial_bytes"],
        fresh.len()
    );
    assert_eq!(deletion["totals"]["new_bytes"], 0);
}

#[test]
fn empty_trees_should_complete_with_a_zero_total_pass() {
    let base = Tree::new();
    let head = Tree::new();

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "pass");
    assert_eq!(report["inputs"]["base"]["files"], 0);
    assert_eq!(report["inputs"]["head"]["files"], 0);
    assert_eq!(report["totals"]["removed_bytes"], 0);
    assert_eq!(report["totals"]["new_bytes"], 0);
}

#[test]
fn identical_trees_should_pass_without_removed_regions() {
    let base = Tree::new();
    let head = Tree::new();
    base.write("same/content.txt", MOVE_PAYLOAD);
    head.write("same/content.txt", MOVE_PAYLOAD);

    let report = completed_report(&run_audit(base.path(), head.path(), &["--json"], &[]));

    assert_eq!(report["verdict"], "pass");
    assert_eq!(report["totals"]["removed_regions"], 0);
}

#[test]
fn symlink_in_tree_should_fail_closed_with_exit_four() {
    let base = Tree::new();
    let head = Tree::new();
    base.write("real/target.txt", MOVE_PAYLOAD);
    symlink(
        base.path().join("real/target.txt"),
        base.path().join("link.txt"),
    )
    .expect("symlink fixture should be created");
    head.write("other.txt", BYSTANDER);

    let output = run_audit(base.path(), head.path(), &["--json"], &[]);

    error_envelope(&output, 4, "SYMLINK_NOT_ALLOWED");
}

#[test]
fn oversized_file_should_fail_closed_with_exit_four_before_reading() {
    let base = Tree::new();
    let head = Tree::new();
    File::create(base.path().join("huge.bin"))
        .and_then(|file| {
            file.set_len(256 * 1024 * 1024 + 1)?;
            file.sync_all()
        })
        .expect("sparse oversized fixture should be created");

    let output = run_audit(base.path(), head.path(), &["--json"], &[]);

    // The per-file byte cap is charged from metadata before any read, so the
    // sparse file costs nothing to reject (§10).
    let report = error_envelope(&output, 4, "RESOURCE_LIMIT_EXCEEDED");
    assert_eq!(report["context"]["resource"], "audit_tree_file_bytes");
}

/// Pins plan §12 against drift: a missing root exits 8 via IO_ERROR.
#[test]
fn missing_root_should_exit_eight_via_io_error_per_plan_section_twelve() {
    let head = Tree::new();
    let missing = head.path().join("definitely/not/here");

    let output = run_audit(&missing, head.path(), &["--json"], &[]);

    error_envelope(&output, 8, "IO_ERROR");
}

#[test]
fn root_that_is_not_a_directory_should_be_invalid_request_exit_two() {
    let base = Tree::new();
    let head = Tree::new();
    base.write("plain-file.txt", BYSTANDER);

    let output = run_audit(
        &base.path().join("plain-file.txt"),
        head.path(),
        &["--json"],
        &[],
    );

    error_envelope(&output, 2, "INVALID_REQUEST");
}

#[test]
fn manifest_mode_happy_path_should_resolve_entries_against_the_manifest_parent() {
    let base = Tree::new();
    let head = Tree::new();
    base.write("tree/alpha.txt", MOVE_PAYLOAD);
    base.write("tree/beta.txt", BYSTANDER);
    base.write(
        "base-manifest.lst",
        b"# touched base files\ntree/alpha.txt\n\ntree/beta.txt\n",
    );
    let mut wrapped = b"// applied:\n".to_vec();
    wrapped.extend_from_slice(MOVE_PAYLOAD);
    wrapped.extend_from_slice(b"\n// end\n");
    head.write("tree/beta.txt", BYSTANDER);
    head.write("tree/gamma.txt", &wrapped);
    head.write("head-manifest.lst", b"tree/beta.txt\ntree/gamma.txt\n");

    // TREE arguments are spelled with the '@' prefix; entries resolve against
    // each manifest's own parent directory (§6.3).
    let output = run_audit_spelled(
        &format!("@{}", base.path().join("base-manifest.lst").display()),
        &format!("@{}", head.path().join("head-manifest.lst").display()),
        &["--json"],
        &[],
    );

    let report = completed_report(&output);
    assert_eq!(report["inputs"]["base"]["kind"], "manifest");
    assert_eq!(report["inputs"]["head"]["kind"], "manifest");
    assert_eq!(report["inputs"]["base"]["files"], 2);
    assert_eq!(report["verdict"], "pass");
    assert!(REPORT_VALIDATOR.is_valid(&report));
}

fn run_audit_spelled(
    base: &str,
    head: &str,
    extra: &[&str],
    environment: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_srcmv"));
    command
        .arg("audit-conservation")
        .arg("--base")
        .arg(base)
        .arg("--head")
        .arg(head);
    command.args(extra);
    for (key, value) in environment {
        command.env(key, value);
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("srcmv should start")
}

#[test]
fn missing_manifest_entry_should_fail_fast_as_invalid_request_exit_two() {
    let base = Tree::new();
    let head = Tree::new();
    base.write("present.txt", MOVE_PAYLOAD);
    base.write("ghost.lst", b"ghost.txt\n");
    head.write("present.txt", MOVE_PAYLOAD);

    let output = run_audit_spelled(
        &format!("@{}", base.path().join("ghost.lst").display()),
        head.path().to_str().expect("utf8 temp path"),
        &["--json"],
        &[],
    );

    error_envelope(&output, 2, "INVALID_REQUEST");
}

#[test]
fn malformed_manifest_syntax_should_fail_as_invalid_request_exit_two() {
    let base = Tree::new();
    let head = Tree::new();
    base.write("escape.lst", b"../escape.txt\n");

    let output = run_audit_spelled(
        &format!("@{}", base.path().join("escape.lst").display()),
        head.path().to_str().expect("utf8 temp path"),
        &["--json"],
        &[],
    );

    error_envelope(&output, 2, "INVALID_REQUEST");
}

/// Forces a deterministic vanished-mid-walk skip through the Phase 2 debug
/// fault seam's environment activation arm (test-only, debug builds only).
#[test]
fn skipped_input_should_complete_with_exit_zero_inconclusive_and_unreadable_reason() {
    if !cfg!(debug_assertions) {
        return;
    }
    let base = Tree::new();
    let head = Tree::new();
    base.write("keep/a.txt", MOVE_PAYLOAD);
    base.write("flaky/doomed.txt", b"vanishing-fixture-bytes-Zeta-1234");
    let mut wrapped = b"// applied:\n".to_vec();
    wrapped.extend_from_slice(MOVE_PAYLOAD);
    wrapped.extend_from_slice(b"\n// end\n");
    head.write("out/a-applied.txt", &wrapped);

    let output = run_audit(
        base.path(),
        head.path(),
        &["--json"],
        &[
            ("SRCMV_TEST_CHILD", "1"),
            ("SRCMV_TEST_AUDIT_TREE_FAULT", "vanish:flaky/doomed.txt"),
        ],
    );

    let report = completed_report(&output);
    assert_eq!(report["verdict"], "inconclusive");
    let reasons = report["inconclusive_reasons"]
        .as_array()
        .expect("reasons array");
    assert!(
        reasons
            .iter()
            .any(|reason| reason == "unreadable_input_skipped"),
        "{report}"
    );
}

#[test]
fn flagless_invocation_should_print_the_human_summary_to_stdout_and_nothing_to_stderr() {
    let base = Tree::new();
    let head = Tree::new();
    base.write("same/content.txt", MOVE_PAYLOAD);
    head.write("same/content.txt", MOVE_PAYLOAD);

    let output = run_audit(base.path(), head.path(), &[], &[]);

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = std::str::from_utf8(&output.stdout).expect("UTF-8 summary");
    assert!(stdout.starts_with("conservation audit v1\n"), "{stdout}");
    assert!(stdout.contains("verdict: PASS"), "{stdout}");
    assert!(stdout.contains("warnings: (none)"), "{stdout}");
}

#[test]
fn error_rendering_should_follow_the_raw_arguments_json_pre_scan_in_both_modes() {
    let head = Tree::new();

    // Clap-level grammar violation WITH --json: JSON envelope on stdout.
    let mut command = Command::new(env!("CARGO_BIN_EXE_srcmv"));
    command
        .args(["audit-conservation", "--base", "somewhere", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let with_flag = command.output().expect("srcmv should start");
    error_envelope(&with_flag, 2, "INVALID_CLI");

    // Same violation without --json: human text on stderr, empty stdout.
    let mut command = Command::new(env!("CARGO_BIN_EXE_srcmv"));
    command
        .args(["audit-conservation", "--base", "somewhere"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let without_flag = command.output().expect("srcmv should start");
    assert_eq!(without_flag.status.code(), Some(2));
    assert!(without_flag.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&without_flag.stderr);
    assert!(stderr.starts_with("srcmv: INVALID_CLI:"), "{stderr}");

    // Option-constraint violation WITH --json renders INVALID_REQUEST.
    let base = Tree::new();
    base.write("a.txt", MOVE_PAYLOAD);
    head.write("a.txt", MOVE_PAYLOAD);
    let constrained = run_audit(
        base.path(),
        head.path(),
        &["--json", "--min-fragment-bytes", "15"],
        &[],
    );
    error_envelope(&constrained, 2, "INVALID_REQUEST");

    // --workspace is rejected through the target-independent helper.
    let mut command = Command::new(env!("CARGO_BIN_EXE_srcmv"));
    command
        .arg("--workspace")
        .arg(head.path())
        .arg("audit-conservation")
        .arg("--base")
        .arg(base.path())
        .arg("--head")
        .arg(head.path())
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let workspace_rejected = command.output().expect("srcmv should start");
    error_envelope(&workspace_rejected, 2, "INVALID_CLI");
}

#[test]
fn json_reports_should_conform_to_the_frozen_schema_across_scenarios() {
    // pass with a conserved destination record
    let (move_base, move_head) = move_fixture();

    // fail carrying an unmatched record
    let fail_base = Tree::new();
    let fail_head = Tree::new();
    fail_base.write("gone/payload.txt", MOVE_PAYLOAD);
    fail_head.write("other.txt", BYSTANDER);

    // formatting-only listing
    let (fmt_base, fmt_head) = formatting_trees();

    // strict-uniqueness inconclusive
    let (dup_base, dup_head) = duplicate_trees(2);
    let scenarios: Vec<(&Tree, &Tree, Vec<&str>)> = vec![
        (&move_base, &move_head, vec![]),
        (&fail_base, &fail_head, vec![]),
        (&fmt_base, &fmt_head, vec![]),
        (&dup_base, &dup_head, vec!["--strict-uniqueness"]),
    ];

    for (base, head, extra) in scenarios {
        let mut arguments = vec!["--json"];
        arguments.extend(extra);
        let report = completed_report(&run_audit(base.path(), head.path(), &arguments, &[]));
        assert!(
            REPORT_VALIDATOR.is_valid(&report),
            "scenario report must conform: {report}"
        );
    }
}

#[test]
fn human_output_should_escape_hostile_filenames_visibly() {
    let base = Tree::new();
    let head = Tree::new();
    base.write("evil\u{202e}\u{7}.txt", MOVE_PAYLOAD);

    let output = run_audit(base.path(), head.path(), &[], &[]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 summary");
    assert!(stdout.contains("\\u{202e}"), "{stdout}");
    assert!(stdout.contains("\\u{7}"), "{stdout}");
    assert!(
        !stdout.contains('\u{7}'),
        "raw control bytes must be escaped"
    );
    assert!(
        !stdout.contains('\u{202e}'),
        "raw bidi characters must be escaped"
    );
    assert!(stdout.contains("verdict: FAIL"), "{stdout}");
}

#[test]
fn all_three_verdicts_should_complete_with_exit_code_zero() {
    // pass
    let base = Tree::new();
    let head = Tree::new();
    base.write("same.txt", MOVE_PAYLOAD);
    head.write("same.txt", MOVE_PAYLOAD);
    let passed = run_audit(base.path(), head.path(), &[], &[]);
    assert_eq!(passed.status.code(), Some(0));

    // fail
    let failing_base = Tree::new();
    let failing_head = Tree::new();
    failing_base.write("gone.txt", MOVE_PAYLOAD);
    let failed = run_audit(failing_base.path(), failing_head.path(), &[], &[]);
    assert_eq!(failed.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&failed.stdout).contains("verdict: FAIL"));

    // inconclusive (strict-uniqueness downgrade keeps this hermetic)
    let (dup_base, dup_head) = duplicate_trees(2);
    let inconclusive = run_audit(
        dup_base.path(),
        dup_head.path(),
        &["--strict-uniqueness"],
        &[],
    );
    assert_eq!(inconclusive.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&inconclusive.stdout).contains("verdict: INCONCLUSIVE"));
}
