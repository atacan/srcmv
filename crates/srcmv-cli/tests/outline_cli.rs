//! End-to-end outline CLI tests with fake-server process re-entry.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use srcmv_fs::Workspace;
use srcmv_test_support::fake_lsp::run_from_process_args;
use tempfile::TempDir;

const HOLD_MODE: &str = "--hold-before-success";
const MAXIMUM_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const FIXTURE_SOURCE: &[u8] =
    b"pub struct Outer;\n\nimpl Outer {\n    pub fn alpha() -> u32 {\n        1\n    }\n}\n";
const MANY_SYMBOL_LINES: usize = 150;
type TestFunction = fn() -> Result<(), String>;

fn main() -> ExitCode {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.first().is_some_and(|value| value == HOLD_MODE) {
        return run_holding_fixture(&arguments[1..]);
    }
    if arguments.first().is_some_and(|value| value == "--scenario") {
        return fake_server(arguments);
    }

    let tests: &[(&str, TestFunction)] = &[
        (
            "json_success_listing_pins_the_frozen_wire_shape",
            json_success_listing_pins_the_frozen_wire_shape,
        ),
        (
            "success_response_matches_the_golden",
            success_matches_golden,
        ),
        (
            "empty_result_succeeds_and_matches_the_golden",
            empty_matches_golden,
        ),
        (
            "kind_filters_apply_after_ordering_with_post_filter_counts",
            kind_filters_apply_after_ordering_with_post_filter_counts,
        ),
        (
            "all_filtered_out_is_an_empty_success",
            all_filtered_out_is_empty,
        ),
        (
            "unknown_kind_spelling_is_invalid_query",
            unknown_kind_spelling_is_invalid,
        ),
        ("flat_symbols_fail_closed", flat_symbols_fail_closed),
        ("malformed_range_is_rejected", malformed_range_is_rejected),
        (
            "invalid_selection_range_is_rejected",
            invalid_selection_range_is_rejected,
        ),
        (
            "duplicate_symbols_are_coalesced",
            duplicate_symbols_are_coalesced,
        ),
        (
            "deep_symbol_tree_hits_a_bounded_wire_failure",
            deep_symbol_tree_hits_a_bounded_wire_failure,
        ),
        (
            "many_symbols_are_listed_in_deterministic_order",
            many_symbols_are_listed_in_deterministic_order,
        ),
        (
            "symbol_count_limit_exceeds_the_outline_bound",
            symbol_count_limit_exceeds_the_outline_bound,
        ),
        (
            "non_utf8_source_fails_before_spawn",
            non_utf8_source_fails_before_spawn,
        ),
        (
            "source_size_boundaries_match_snapshot_limits",
            source_size_boundaries_match_snapshot_limits,
        ),
        (
            "document_symbol_timeout_reports_the_trusted_phase",
            document_symbol_timeout_reports_the_trusted_phase,
        ),
        (
            "human_output_matches_the_frozen_layout",
            human_output_layout,
        ),
        (
            "escaped_names_are_visible_in_human_output",
            escaped_names_are_visible_in_human_output,
        ),
        ("workspace_remains_unmodified", workspace_remains_unmodified),
        (
            "slow_server_does_not_hold_diagnostic_lock",
            slow_server_does_not_hold_diagnostic_lock,
        ),
    ];
    let mut failures = 0;
    for (name, test) in tests {
        match test() {
            Ok(()) => println!("test {name} ... ok"),
            Err(error) => {
                failures += 1;
                eprintln!("test {name} ... FAILED\n{error}");
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {failures} failed",
        if failures == 0 { "ok" } else { "FAILED" },
        tests.len() - failures
    );
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn json_success_listing_pins_the_frozen_wire_shape() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "success", true)
        .output()
        .map_err(display)?;
    ensure_success(&output, "outline listing")?;
    let response: Value = serde_json::from_slice(&output.stdout).map_err(display)?;

    assert_eq!(response["outline_protocol_version"], 1);
    assert_eq!(response["source"]["path"], "source.rs");
    assert_eq!(response["source"]["byte_length"], 78);
    assert_eq!(
        response["server"],
        json!({
            "configuration_id": null,
            "reported_name": "srcmv-fake-lsp",
            "reported_version": "1",
            "position_encoding": "utf-16"
        })
    );

    let symbols = response["symbols"]
        .as_array()
        .ok_or("symbols must be an array")?;
    if symbols.len() != 2 {
        return Err(format!("expected two symbols: {response}"));
    }
    let outer = &symbols[0];
    for (field, expected) in [
        ("name", json!("Outer")),
        ("symbol_kind", json!("class")),
        ("symbol_path", json!(["Outer"])),
        ("depth", json!(0)),
        ("detail", json!("fixture class")),
        ("start_line", json!(3)),
        ("start_column", json!(1)),
        ("end_line", json!(7)),
        ("end_column", json!(2)),
        (
            "lsp_range",
            json!({"start": {"line": 2, "character": 0}, "end": {"line": 6, "character": 1}}),
        ),
        (
            "lsp_selection_range",
            json!({"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 10}}),
        ),
        ("selector", json!({"kind": "bytes", "start": 19, "end": 77})),
    ] {
        if outer[field] != expected {
            return Err(format!(
                "outer[{field}] was {}, expected {expected}",
                outer[field]
            ));
        }
    }
    let alpha = &symbols[1];
    for (field, expected) in [
        ("name", json!("alpha")),
        ("symbol_kind", json!("function")),
        ("symbol_path", json!(["Outer", "alpha"])),
        ("depth", json!(1)),
        ("detail", json!("fixture function")),
        ("start_line", json!(4)),
        ("start_column", json!(5)),
        ("end_line", json!(6)),
        ("end_column", json!(6)),
        ("selector", json!({"kind": "bytes", "start": 36, "end": 75})),
    ] {
        if alpha[field] != expected {
            return Err(format!(
                "alpha[{field}] was {}, expected {expected}",
                alpha[field]
            ));
        }
    }
    Ok(())
}

fn success_matches_golden() -> Result<(), String> {
    let response = successful_outline_response()?;
    assert_matches_golden(&response, "success.json")
}

fn empty_matches_golden() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "null-symbols", true)
        .output()
        .map_err(display)?;
    ensure_success(&output, "empty outline")?;
    let response: Value = serde_json::from_slice(&output.stdout).map_err(display)?;
    if response["symbols"]
        .as_array()
        .is_none_or(|symbols| !symbols.is_empty())
    {
        return Err(format!(
            "null server result must list no symbols: {response}"
        ));
    }
    assert_matches_golden(&response, "empty.json")
}

fn kind_filters_apply_after_ordering_with_post_filter_counts() -> Result<(), String> {
    let workspace = many_symbols_workspace()?;
    let output = outline_command(
        workspace.path(),
        &["--kind", "module"],
        "many-symbols",
        false,
    )
    .output()
    .map_err(display)?;
    ensure_success(&output, "filtered human outline")?;
    let stdout = String::from_utf8(output.stdout).map_err(display)?;
    if !stdout.starts_with("source.rs: 36 document symbols\n") {
        return Err(format!("post-filter header count wrong: {stdout}"));
    }
    if !stdout.contains("  module Module7_1 lines ") {
        return Err(format!(
            "filtered modules must stay indented by depth: {stdout}"
        ));
    }
    if stdout.contains("function Fn") || stdout.contains("class Group") {
        return Err("filter did not remove other kinds".to_owned());
    }

    let json_output = outline_command(
        workspace.path(),
        &["--kind", "function"],
        "many-symbols",
        true,
    )
    .output()
    .map_err(display)?;
    ensure_success(&json_output, "filtered JSON outline")?;
    let response: Value = serde_json::from_slice(&json_output.stdout).map_err(display)?;
    let symbols = response["symbols"].as_array().ok_or("symbols array")?;
    if symbols.len() != 72 {
        return Err(format!("expected 72 functions: {}", symbols.len()));
    }
    for symbol in symbols {
        if symbol["symbol_kind"] != "function" || symbol["depth"] != 2 {
            return Err(format!("unexpected filtered record: {symbol}"));
        }
    }
    Ok(())
}

fn all_filtered_out_is_empty() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let human = outline_command(workspace.path(), &["--kind", "enum"], "success", false)
        .output()
        .map_err(display)?;
    ensure_success(&human, "all-filtered-out human outline")?;
    let stdout = String::from_utf8(human.stdout).map_err(display)?;
    if stdout != "no document symbols\n" {
        return Err(format!("all-filtered-out phrasing wrong: {stdout:?}"));
    }

    let json = outline_command(workspace.path(), &["--kind", "enum"], "success", true)
        .output()
        .map_err(display)?;
    ensure_success(&json, "all-filtered-out JSON outline")?;
    let response: Value = serde_json::from_slice(&json.stdout).map_err(display)?;
    if response["symbols"]
        .as_array()
        .is_none_or(|symbols| !symbols.is_empty())
    {
        return Err(format!("all-filtered-out must be a success: {response}"));
    }
    Ok(())
}

fn unknown_kind_spelling_is_invalid() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &["--kind", "bogus"], "success", true)
        .output()
        .map_err(display)?;
    assert_error_with_status(&output, "INVALID_OUTLINE_QUERY", 2)
}

fn flat_symbols_fail_closed() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "flat-symbols", true)
        .output()
        .map_err(display)?;
    assert_error_with_status(&output, "LSP_FLAT_SYMBOLS_UNSUPPORTED", 4)
}

fn malformed_range_is_rejected() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "malformed-range", true)
        .output()
        .map_err(display)?;
    assert_error_with_status(&output, "LSP_PROTOCOL_ERROR", 4)
}

fn invalid_selection_range_is_rejected() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "invalid-selection-range", true)
        .output()
        .map_err(display)?;
    assert_error_with_status(&output, "LSP_PROTOCOL_ERROR", 4)
}

fn duplicate_symbols_are_coalesced() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "duplicate-symbols", true)
        .output()
        .map_err(display)?;
    ensure_success(&output, "duplicate symbols")?;
    let response: Value = serde_json::from_slice(&output.stdout).map_err(display)?;
    let names = response["symbols"]
        .as_array()
        .ok_or("symbols array")?
        .iter()
        .map(|symbol| symbol["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    // The scenario duplicates one top-level alpha record; the frozen dedup
    // key coalesces it to a single listing entry.
    if names != ["alpha"] {
        return Err(format!("duplicates were not coalesced: {names:?}"));
    }
    Ok(())
}

fn deep_symbol_tree_hits_a_bounded_wire_failure() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "deep-symbols", true)
        .output()
        .map_err(display)?;
    assert_error_with_status(&output, "LSP_PROTOCOL_ERROR", 4)
}

fn many_symbols_are_listed_in_deterministic_order() -> Result<(), String> {
    let workspace = many_symbols_workspace()?;
    let output = outline_command(workspace.path(), &[], "many-symbols", true)
        .output()
        .map_err(display)?;
    ensure_success(&output, "many-symbol outline")?;
    let response: Value = serde_json::from_slice(&output.stdout).map_err(display)?;
    let symbols = response["symbols"].as_array().ok_or("symbols array")?;
    if symbols.len() != 120 {
        return Err(format!("expected 120 symbols: {}", symbols.len()));
    }

    let mut expected_names = Vec::new();
    for group in 0..12 {
        let base_line = 12 * group;
        expected_names.push(json!({
            "name": format!("Group{group}"),
            "kind": "class",
            "depth": 0,
            "start_line": base_line + 1
        }));
        for module in 0..3 {
            let module_line = base_line + 1 + 3 * module;
            expected_names.push(json!({
                "name": format!("Module{group}_{module}"),
                "kind": "module",
                "depth": 1,
                "start_line": module_line + 1
            }));
            for index in 0..2 {
                expected_names.push(json!({
                    "name": format!("Fn{group}_{module}_{index}"),
                    "kind": "function",
                    "depth": 2,
                    "start_line": module_line + index + 1
                }));
            }
        }
    }

    for (position, expected) in expected_names.iter().enumerate() {
        let actual = &symbols[position];
        for field in ["name", "kind", "depth"] {
            let key = match field {
                "kind" => "symbol_kind",
                other => other,
            };
            if actual[key] != expected[field] {
                return Err(format!(
                    "position {position} {key}: {} != {}",
                    actual[key], expected[field]
                ));
            }
        }
        if actual["start_line"] != expected["start_line"] {
            return Err(format!(
                "position {position} start_line mismatch: {}",
                actual["start_line"]
            ));
        }
    }
    Ok(())
}

fn symbol_count_limit_exceeds_the_outline_bound() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let started = Instant::now();
    let output = outline_command(workspace.path(), &[], "symbol-count-limit-exceeded", true)
        .output()
        .map_err(display)?;
    assert_error_with_status(&output, "LSP_RESOURCE_LIMIT_EXCEEDED", 4)?;
    let response: Value = serde_json::from_slice(&output.stdout).map_err(display)?;
    if response["context"]["resource"] != "outline_symbols" || response["context"]["limit"] != 10000
    {
        return Err(format!("unexpected bound context: {response}"));
    }
    if started.elapsed() > Duration::from_secs(30) {
        return Err("count-bound scenario took too long".to_owned());
    }
    Ok(())
}

fn non_utf8_source_fails_before_spawn() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    fs::write(workspace.path().join("source.rs"), [0xff, 0xfe]).map_err(display)?;
    let output = outline_command(workspace.path(), &[], "success", true)
        .output()
        .map_err(display)?;
    assert_error_with_status(&output, "UNSUPPORTED_TEXT_ENCODING", 4)
}

fn source_size_boundaries_match_snapshot_limits() -> Result<(), String> {
    for size in [MAXIMUM_SOURCE_BYTES - 1, MAXIMUM_SOURCE_BYTES] {
        let workspace = fixture_workspace()?;
        resize_source(workspace.path(), size)?;
        let output = outline_command(workspace.path(), &[], "success", true)
            .output()
            .map_err(display)?;
        ensure_success(&output, &format!("{size}-byte outline"))?;
    }

    let workspace = fixture_workspace()?;
    resize_source(workspace.path(), MAXIMUM_SOURCE_BYTES + 1)?;
    let output = outline_command(workspace.path(), &[], "success", true)
        .output()
        .map_err(display)?;
    assert_error_with_status(&output, "LSP_RESOURCE_LIMIT_EXCEEDED", 4)
}

fn document_symbol_timeout_reports_the_trusted_phase() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let config = write_trusted_config(workspace.path(), "hang-document-symbols", 100, "")?;
    let started = Instant::now();
    let output = trusted_outline_command(workspace.path(), &config)
        .output()
        .map_err(display)?;
    let elapsed = started.elapsed();
    assert_error(&output, "LSP_TIMEOUT")?;
    let response: Value = serde_json::from_slice(&output.stdout).map_err(display)?;
    if response["context"]["phase"] != "document_symbol" {
        return Err(format!("unexpected timeout phase: {response}"));
    }
    if elapsed >= Duration::from_secs(5) {
        return Err(format!("timeout scenario took too long: {elapsed:?}"));
    }
    Ok(())
}

fn human_output_layout() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "success", false)
        .output()
        .map_err(display)?;
    ensure_success(&output, "human outline")?;
    let stdout = String::from_utf8(output.stdout).map_err(display)?;
    let expected = concat!(
        "source.rs: 2 document symbols\n",
        "class Outer lines 3..7 lsp=2:0..6:1 bytes 19..77\n",
        "  function alpha lines 4..6 lsp=3:4..5:5 bytes 36..75\n",
    );
    if stdout != expected {
        return Err(format!("human layout mismatch:\n{stdout:?}\n{expected:?}"));
    }
    Ok(())
}

fn escaped_names_are_visible_in_human_output() -> Result<(), String> {
    let workspace = fixture_workspace()?;
    let human = outline_command(workspace.path(), &[], "escaped-name-symbols", false)
        .output()
        .map_err(display)?;
    ensure_success(&human, "escaped human outline")?;
    let stdout = String::from_utf8(human.stdout).map_err(display)?;
    if !stdout.contains("class bad\\u{1b}name lines 3..7") {
        return Err(format!(
            "control characters were not escaped visibly: {stdout:?}"
        ));
    }

    let json = outline_command(workspace.path(), &[], "escaped-name-symbols", true)
        .output()
        .map_err(display)?;
    ensure_success(&json, "escaped JSON outline")?;
    let response: Value = serde_json::from_slice(&json.stdout).map_err(display)?;
    if response["symbols"][0]["name"] != "bad\u{1b}name" {
        return Err("JSON name must retain the raw control character".to_owned());
    }
    Ok(())
}

fn workspace_remains_unmodified() -> Result<(), String> {
    let workspace_directory = fixture_workspace()?;
    let config = write_trusted_config(
        workspace_directory.path(),
        "server-requests",
        1_000,
        "settings = { fixture = { enabled = true } }\n",
    )?;
    let before = snapshot_workspace(workspace_directory.path())?;
    let output = trusted_outline_command(workspace_directory.path(), &config)
        .output()
        .map_err(display)?;
    ensure_success(&output, "read-only outline")?;
    let after = snapshot_workspace(workspace_directory.path())?;
    if before != after {
        return Err("outline changed workspace source or control state".to_owned());
    }
    Ok(())
}

fn slow_server_does_not_hold_diagnostic_lock() -> Result<(), String> {
    let workspace_directory = fixture_workspace()?;
    let sentinel = workspace_directory.path().join("server-started");
    let expected_source = workspace_directory.path().join("captured-source.rs");
    fs::copy(
        workspace_directory.path().join("source.rs"),
        &expected_source,
    )
    .map_err(display)?;
    fs::write(workspace_directory.path().join("commit.txt"), b"commit\n").map_err(display)?;
    let mut command = outline_holding_command(
        workspace_directory.path(),
        sentinel.to_str().ok_or("sentinel path is not UTF-8")?,
        &expected_source,
    );
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().map_err(display)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sentinel.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !sentinel.exists() {
        return Err("fake server did not start".to_owned());
    }
    fs::write(
        workspace_directory.path().join("source.rs"),
        b"changed after immutable capture\n",
    )
    .map_err(display)?;
    let workspace = Workspace::open(workspace_directory.path()).map_err(display)?;
    let lock = workspace.mutation_lock().map_err(display)?;
    drop(lock);
    run_unrelated_commit(workspace_directory.path())?;
    let output = child.wait_with_output().map_err(display)?;
    ensure_success(&output, "slow outline")?;
    if fs::read(workspace_directory.path().join("committed.txt")).map_err(display)? != b"c" {
        return Err("the unrelated srcmv commit did not complete".to_owned());
    }
    Ok(())
}

fn run_unrelated_commit(workspace: &Path) -> Result<(), String> {
    let source = b"commit\n";
    let digest = format!("sha256:{:x}", Sha256::digest(source));
    let request = json!({
        "protocol_version": 1,
        "operations": [{
            "kind": "copy",
            "source": {
                "path": "commit.txt",
                "selector": {"kind": "bytes", "start": 0, "end": 1},
                "precondition": {"kind": "sha256", "value": digest}
            },
            "destination": {
                "path": "committed.txt",
                "anchor": {"kind": "file_start"},
                "precondition": {"kind": "must_not_exist"}
            }
        }]
    });
    let request_path = workspace.join("commit-request.json");
    fs::write(
        &request_path,
        serde_json::to_vec(&request).map_err(display)?,
    )
    .map_err(display)?;
    let output = Command::new(srcmv_binary())
        .args(["--workspace"])
        .arg(workspace)
        .args(["apply", "--request"])
        .arg(request_path)
        .args(["--commit", "--accept-current-plan", "--json"])
        .output()
        .map_err(display)?;
    ensure_success(&output, "concurrent commit")
}

fn successful_outline_response() -> Result<Value, String> {
    let workspace = fixture_workspace()?;
    let output = outline_command(workspace.path(), &[], "success", true)
        .output()
        .map_err(display)?;
    ensure_success(&output, "successful outline")?;
    serde_json::from_slice(&output.stdout).map_err(display)
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn assert_matches_golden(actual: &Value, golden_name: &str) -> Result<(), String> {
    let golden_bytes = fs::read(
        repository_root()
            .join("tests/golden/outline-v1")
            .join(golden_name),
    )
    .map_err(display)?;
    let mut expected: Value = serde_json::from_slice(&golden_bytes).map_err(display)?;

    // The fixture digest is frozen in the golden; the identity hash covers the
    // physical workspace root and differs per machine.
    let fixture_digest = format!("sha256:{:x}", Sha256::digest(FIXTURE_SOURCE));
    if expected["source"]["sha256"] != json!(fixture_digest.clone()) {
        return Err(format!(
            "golden {golden_name} does not describe the standard fixture"
        ));
    }
    if actual["source"]["sha256"] != json!(fixture_digest) {
        return Err("outline observed an unexpected snapshot digest".to_owned());
    }
    expected["workspace_identity_hash"] = actual["workspace_identity_hash"].clone();

    if *actual != expected {
        return Err(format!(
            "response did not match golden {golden_name}:\nactual   {actual}\nexpected {expected}"
        ));
    }
    Ok(())
}

fn run_holding_fixture(arguments: &[std::ffi::OsString]) -> ExitCode {
    let Some(sentinel) = arguments.first() else {
        return ExitCode::FAILURE;
    };
    if fs::File::create(sentinel)
        .and_then(|mut file| file.write_all(b"started"))
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    thread::sleep(Duration::from_secs(2));
    fake_server(arguments[1..].to_vec())
}

fn fake_server(arguments: Vec<std::ffi::OsString>) -> ExitCode {
    match run_from_process_args(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fake LSP failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn fixture_workspace() -> Result<TempDir, String> {
    let directory = TempDir::new().map_err(display)?;
    fs::write(directory.path().join("source.rs"), FIXTURE_SOURCE).map_err(display)?;
    Ok(directory)
}

fn many_symbols_workspace() -> Result<TempDir, String> {
    let directory = TempDir::new().map_err(display)?;
    let line = "fn placeholder_for_outline_fixture() {}\n";
    let mut source = String::with_capacity(MANY_SYMBOL_LINES * line.len());
    for _ in 0..MANY_SYMBOL_LINES {
        source.push_str(line);
    }
    fs::write(directory.path().join("source.rs"), source).map_err(display)?;
    Ok(directory)
}

fn resize_source(workspace: &Path, size: usize) -> Result<(), String> {
    let source_path = workspace.join("source.rs");
    let mut source = fs::read(&source_path).map_err(display)?;
    if size < source.len() {
        return Err("requested source size is smaller than the fixture".to_owned());
    }
    source.resize(size, b' ');
    fs::write(source_path, source).map_err(display)
}

fn assert_error(output: &Output, expected_code: &str) -> Result<(), String> {
    assert_error_with_status(output, expected_code, 4)
}

fn assert_error_with_status(
    output: &Output,
    expected_code: &str,
    expected_status: i32,
) -> Result<(), String> {
    let response: Value = serde_json::from_slice(&output.stdout).map_err(display)?;
    if output.status.code() != Some(expected_status) || response["code"] != expected_code {
        return Err(format!(
            "expected {expected_code}, got status {} and response {response}",
            output.status
        ));
    }
    Ok(())
}

fn write_trusted_config(
    workspace: &Path,
    scenario: &str,
    timeout_ms: u64,
    settings: &str,
) -> Result<PathBuf, String> {
    let source = workspace.join("source.rs");
    let canonical_source = source.canonicalize().map_err(display)?;
    let uri = url::Url::from_file_path(&canonical_source)
        .map_err(|()| "failed to build fixture source URI".to_owned())?;
    let arguments = vec![
        "--scenario".to_owned(),
        scenario.to_owned(),
        "--expected-document-uri".to_owned(),
        uri.to_string(),
        "--expected-language-id".to_owned(),
        "fixture-rust".to_owned(),
        "--expected-document-text-file".to_owned(),
        canonical_source.display().to_string(),
    ];
    let program = env::current_exe().map_err(display)?.display().to_string();
    let document = format!(
        "version = 1\n\n[[servers]]\nid = \"fixture\"\nextensions = [\"rs\"]\nlanguage_id = \"fixture-rust\"\nprogram = {}\nargs = {}\n{settings}startup_timeout_ms = {timeout_ms}\nrequest_timeout_ms = {timeout_ms}\n",
        serde_json::to_string(&program).map_err(display)?,
        serde_json::to_string(&arguments).map_err(display)?,
    );
    let path = workspace.join("lsp-config.toml");
    fs::write(&path, document).map_err(display)?;
    Ok(path)
}

fn trusted_outline_command(workspace: &Path, config: &Path) -> Command {
    let mut command = Command::new(srcmv_binary());
    command
        .env("SRCMV_CONFIG", config)
        .args(["--workspace"])
        .arg(workspace)
        .args([
            "outline",
            "--path",
            "source.rs",
            "--server-id",
            "fixture",
            "--json",
        ]);
    command
}

fn outline_holding_command(workspace: &Path, sentinel: &str, expected_source: &Path) -> Command {
    // The holding fixture consumes the sentinel argument first, so these
    // arguments must precede the fake-server scenario arguments.
    let canonical_source = workspace
        .join("source.rs")
        .canonicalize()
        .expect("canonical fixture source");
    let uri = url::Url::from_file_path(&canonical_source).expect("absolute fixture URI");
    let mut command = Command::new(srcmv_binary());
    command
        .args(["--workspace"])
        .arg(workspace)
        .args(["outline", "--path", "source.rs"])
        .arg("--server-program")
        .arg(env::current_exe().expect("current test executable"))
        .args(["--language-id", "fixture-rust"]);
    command
        .arg(format!("--server-arg={HOLD_MODE}"))
        .arg(format!("--server-arg={sentinel}"))
        .arg("--server-arg=--scenario")
        .arg("--server-arg=success")
        .arg("--server-arg=--expected-document-uri")
        .arg(format!("--server-arg={uri}"))
        .arg("--server-arg=--expected-language-id")
        .arg("--server-arg=fixture-rust")
        .arg("--server-arg=--expected-document-text-file")
        .arg(format!("--server-arg={}", expected_source.display()));
    command
}

fn outline_command(
    workspace: &Path,
    extra_arguments: &[&str],
    scenario: &str,
    json_output: bool,
) -> Command {
    let source = workspace.join("source.rs");
    let canonical_source = source.canonicalize().expect("canonical fixture source");
    let uri = url::Url::from_file_path(&canonical_source).expect("absolute fixture URI");
    let mut command = Command::new(srcmv_binary());
    command
        .args(["--workspace"])
        .arg(workspace)
        .args(["outline", "--path", "source.rs"])
        .args(extra_arguments)
        .arg("--server-program")
        .arg(env::current_exe().expect("current test executable"))
        .args(["--language-id", "fixture-rust"]);
    if json_output {
        command.arg("--json");
    }
    command
        .arg("--server-arg=--scenario")
        .arg(format!("--server-arg={scenario}"))
        .arg("--server-arg=--expected-document-uri")
        .arg(format!("--server-arg={uri}"))
        .arg("--server-arg=--expected-language-id")
        .arg("--server-arg=fixture-rust")
        .arg("--server-arg=--expected-document-text-file")
        .arg(format!("--server-arg={}", source.display()));
    command
}

fn snapshot_workspace(root: &Path) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>, String> {
    let mut snapshot = BTreeMap::new();
    snapshot_directory(root, root, &mut snapshot)?;
    Ok(snapshot)
}

fn snapshot_directory(
    root: &Path,
    directory: &Path,
    snapshot: &mut BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(display)? {
        let entry = entry.map_err(display)?;
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(display)?.to_path_buf();
        let file_type = entry.file_type().map_err(display)?;
        if file_type.is_dir() {
            snapshot.insert(relative, None);
            snapshot_directory(root, &path, snapshot)?;
        } else if file_type.is_file() {
            snapshot.insert(relative, Some(fs::read(path).map_err(display)?));
        }
    }
    Ok(())
}

fn srcmv_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_srcmv"))
}

fn ensure_success(output: &Output, label: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{label} failed with {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
