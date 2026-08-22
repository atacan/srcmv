//! Outline-v1 schema, golden-vector, and structural invariant tests.

use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::Validator;
use serde_json::{Value, json};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn outline_schema() -> Value {
    read_json(
        &repository_root()
            .join("docs/schema/outline-v1")
            .join("response.schema.json"),
    )
}

fn outline_golden(name: &str) -> Value {
    read_json(&repository_root().join("tests/golden/outline-v1").join(name))
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{} must contain JSON: {error}", path.display()))
}

fn collect_json_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", directory.display()))
    {
        let path = entry.expect("directory entry must be readable").path();
        if path.is_dir() {
            collect_json_files(&path, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            files.push(path);
        }
    }
}

fn collect_references<'a>(value: &'a Value, references: &mut Vec<&'a str>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_references(value, references);
            }
        }
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                references.push(reference);
            }
            for value in object.values() {
                collect_references(value, references);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn outline_validator() -> Validator {
    jsonschema::draft202012::options()
        .build(&outline_schema())
        .expect("outline response schema must compile")
}

#[test]
fn every_outline_contract_json_file_should_parse() {
    let mut files = Vec::new();
    collect_json_files(
        &repository_root().join("docs/schema/outline-v1"),
        &mut files,
    );
    collect_json_files(
        &repository_root().join("tests/golden/outline-v1"),
        &mut files,
    );
    files.sort();

    assert!(
        !files.is_empty(),
        "outline contract must contain JSON files"
    );
    for path in files {
        read_json(&path);
    }
}

#[test]
fn outline_schema_internal_references_should_resolve() {
    let schema = outline_schema();
    let mut references = Vec::new();
    collect_references(&schema, &mut references);
    for reference in references
        .into_iter()
        .filter(|reference| reference.starts_with('#'))
    {
        let pointer = reference
            .strip_prefix('#')
            .expect("filtered reference must start with a fragment");
        assert!(
            schema.pointer(pointer).is_some(),
            "outline schema contains unresolved reference {reference}"
        );
    }
}

#[test]
fn outline_schema_should_validate_against_the_draft_2020_12_metaschema() {
    let schema = outline_schema();
    jsonschema::draft202012::meta::validate(&schema)
        .unwrap_or_else(|error| panic!("outline schema must satisfy the metaschema: {error}"));
    outline_validator();
}

#[test]
fn outline_goldens_should_validate_against_the_compiled_offline_schema() {
    let validator = outline_validator();

    for name in ["success.json", "empty.json"] {
        let instance = outline_golden(name);
        validator
            .validate(&instance)
            .unwrap_or_else(|error| panic!("{name} must satisfy outline response v1: {error}"));
    }
}

#[test]
fn outline_schema_should_freeze_keys_and_limits() {
    let schema = outline_schema();
    let required = json!([
        "outline_protocol_version",
        "workspace_identity_hash",
        "source",
        "server",
        "symbols",
        "warnings"
    ]);

    assert_eq!(schema["required"], required);
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["outline_protocol_version"]["const"], 1);
    assert_eq!(schema["properties"]["symbols"]["maxItems"], 10000);
    assert_eq!(schema["properties"]["warnings"]["maxItems"], 16);

    let symbol = &schema["$defs"]["symbol"];
    assert_eq!(symbol["additionalProperties"], false);
    let symbol_required = symbol["required"]
        .as_array()
        .expect("symbol required keys must be an array");
    for key in [
        "name",
        "symbol_kind",
        "symbol_path",
        "depth",
        "detail",
        "start_line",
        "start_column",
        "end_line",
        "end_column",
        "lsp_range",
        "lsp_selection_range",
        "selector",
    ] {
        assert!(
            symbol_required.contains(&json!(key)),
            "{key} must be required"
        );
    }

    // The v1 pipeline always populates both columns, but only `end_column`
    // keeps the nullable future-backend seam.
    assert_eq!(
        symbol["properties"]["start_column"],
        json!({"$ref": "#/$defs/one_based_scalar_column"})
    );
    assert_eq!(
        schema["$defs"]["one_based_scalar_column"]["type"],
        "integer"
    );
    assert_eq!(
        symbol["properties"]["end_column"],
        json!({"$ref": "#/$defs/nullable_end_column"})
    );
    assert_eq!(
        schema["$defs"]["nullable_end_column"]["type"],
        json!(["null", "integer"])
    );
    assert_eq!(schema["$defs"]["depth"]["maximum"], 255);
    assert_eq!(schema["$defs"]["symbol_path"]["maxItems"], 256);
    assert_eq!(
        schema["$defs"]["warning"]["properties"]["code"]["const"],
        "OBSERVATION_MAY_BE_STALE"
    );
}

#[test]
fn outline_schema_should_reject_contract_mutations() {
    let validator = outline_validator();

    let mut unknown_field = outline_golden("success.json");
    unknown_field["unknown"] = json!(true);
    assert!(!validator.is_valid(&unknown_field));

    let mut missing_source = outline_golden("success.json");
    missing_source
        .as_object_mut()
        .expect("golden must be an object")
        .remove("source");
    assert!(!validator.is_valid(&missing_source));

    let mut bad_encoding = outline_golden("empty.json");
    bad_encoding["server"]["position_encoding"] = json!("utf-7");
    assert!(!validator.is_valid(&bad_encoding));

    let mut bad_kind = outline_golden("success.json");
    bad_kind["symbols"][0]["symbol_kind"] = json!("Function");
    assert!(!validator.is_valid(&bad_kind));

    let mut oversized_depth = outline_golden("success.json");
    oversized_depth["symbols"][0]["depth"] = json!(256);
    assert!(!validator.is_valid(&oversized_depth));

    let mut zero_column = outline_golden("success.json");
    zero_column["symbols"][0]["start_column"] = json!(0);
    assert!(!validator.is_valid(&zero_column));

    let mut empty_selector = outline_golden("empty.json");
    empty_selector["selector"] = json!({"kind": "bytes", "start": 5, "end": 5});
    assert!(!validator.is_valid(&empty_selector));
}

#[test]
fn outline_schema_keeps_only_the_documented_end_column_seam_nullable() {
    let validator = outline_validator();

    let mut nullable_end = outline_golden("empty.json");
    nullable_end["symbols"] = json!([{
        "name": "Outer",
        "symbol_kind": "class",
        "symbol_path": ["Outer"],
        "depth": 0,
        "detail": null,
        "start_line": 3,
        "start_column": 1,
        "end_line": 7,
        "end_column": null,
        "lsp_range": {"start": {"line": 2, "character": 0}, "end": {"line": 6, "character": 1}},
        "lsp_selection_range": {
            "start": {"line": 2, "character": 0},
            "end": {"line": 2, "character": 5}
        },
        "selector": {"kind": "bytes", "start": 19, "end": 77}
    }]);
    validator
        .validate(&nullable_end)
        .expect("the documented end-column seam must remain schema-reachable");

    let mut nullable_start = nullable_end.clone();
    nullable_start["symbols"][0]["start_column"] = json!(null);
    assert!(!validator.is_valid(&nullable_start));
}

#[test]
fn success_example_should_preserve_structural_invariants() {
    let response = outline_golden("success.json");
    let source_byte_length = response["source"]["byte_length"]
        .as_u64()
        .expect("byte_length must be unsigned");
    let symbols = response["symbols"]
        .as_array()
        .expect("symbols must be an array");
    assert_eq!(symbols.len(), 2);

    let mut previous_starts = Vec::new();
    for symbol in symbols {
        let path = symbol["symbol_path"].as_array().expect("path array");
        assert_eq!(path.last(), Some(&symbol["name"]));
        assert_eq!(
            symbol["depth"].as_u64().expect("depth"),
            u64::try_from(path.len().saturating_sub(1)).expect("path fits"),
        );

        let start_line = symbol["start_line"].as_u64().expect("start line");
        let end_line = symbol["end_line"].as_u64().expect("end line");
        assert!(start_line >= 1 && end_line >= start_line);

        let start_column = symbol["start_column"].as_u64().expect("start column");
        let end_column = symbol["end_column"]
            .as_u64()
            .expect("v1 columns are concrete");
        assert!(start_column >= 1 && end_column >= 1);

        let selector_start = symbol["selector"]["start"].as_u64().expect("selector");
        let selector_end = symbol["selector"]["end"].as_u64().expect("selector");
        assert!(selector_start < selector_end);
        assert!(selector_end <= source_byte_length);
        previous_starts.push(selector_start);

        let lsp_range = &symbol["lsp_range"];
        let lsp_selection = &symbol["lsp_selection_range"];
        assert!(lsp_range["start"]["line"].as_u64() <= Some(start_line.saturating_sub(1)));
        assert!(
            lsp_selection["start"]["line"]
                .as_u64()
                .expect("selection line")
                >= lsp_range["start"]["line"].as_u64().expect("range line"),
            "selection start must stay inside the enclosing range"
        );
        assert!(
            lsp_selection["end"]["line"]
                .as_u64()
                .expect("selection line")
                <= lsp_range["end"]["line"].as_u64().expect("range line"),
            "selection end must stay inside the enclosing range"
        );
    }
    let mut sorted_starts = previous_starts.clone();
    sorted_starts.sort_unstable();
    assert_eq!(previous_starts, sorted_starts, "records must be ordered");

    assert_eq!(response["outline_protocol_version"], 1);
    let warnings = response["warnings"].as_array().expect("warnings array");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["code"], "OBSERVATION_MAY_BE_STALE");
}
