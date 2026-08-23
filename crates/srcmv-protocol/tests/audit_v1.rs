//! Conservation-audit v1 schema conformance and frozen-contract tests.
//!
//! Mirrors the `selection_v1` precedent: Draft 2020-12 validation through an
//! OFFLINE registry that registers the schema under its absolute `$id`, plus
//! key-set comparisons that read the expected property names directly from
//! `docs/schema/conservation-audit-v1/report.schema.json` so DTO drift fails
//! loudly.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use jsonschema::{Registry, Validator};
use serde_json::{Value, json};

const REPORT_SCHEMA_ID: &str = "https://raw.githubusercontent.com/atacan/srcmv/main/docs/schema/conservation-audit-v1/report.schema.json";

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn schema_json() -> Value {
    let path = repository_root().join("docs/schema/conservation-audit-v1/report.schema.json");
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("report schema must contain JSON: {error}"))
}

fn collect_golden_files(files: &mut Vec<PathBuf>) {
    let directory = repository_root().join("tests/golden/conservation-audit-v1");
    for entry in fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", directory.display()))
    {
        let path = entry.expect("directory entry must be readable").path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            files.push(path);
        }
    }
}

fn golden(name: &str) -> Value {
    let path = repository_root()
        .join("tests/golden/conservation-audit-v1")
        .join(name);
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{} must contain JSON: {error}", path.display()))
}

fn report_validator() -> Validator {
    let schema = schema_json();
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

fn property_names(definition: &Value) -> BTreeSet<String> {
    definition["properties"]
        .as_object()
        .expect("definition must declare properties")
        .keys()
        .cloned()
        .collect()
}

fn required_names(definition: &Value) -> BTreeSet<String> {
    definition["required"]
        .as_array()
        .expect("definition must declare required keys")
        .iter()
        .map(|key| key.as_str().expect("required key string").to_owned())
        .collect()
}

fn assert_keys_match(
    label: &str,
    instance_keys: &BTreeSet<String>,
    definition: &Value,
    allow_optional_absence: bool,
) {
    let properties = property_names(definition);
    assert!(
        instance_keys.is_subset(&properties),
        "{label} emits unknown keys: {:?}",
        instance_keys.difference(&properties).collect::<Vec<_>>()
    );
    let missing = required_names(definition)
        .difference(instance_keys)
        .cloned()
        .collect::<Vec<_>>();
    if allow_optional_absence {
        assert!(
            missing.is_empty(),
            "{label} misses required keys: {missing:?}"
        );
    } else {
        assert_eq!(
            instance_keys, &properties,
            "{label} keys must match the frozen property set exactly"
        );
    }
}

#[test]
fn report_schema_should_satisfy_the_draft_2020_12_metaschema() {
    jsonschema::draft202012::meta::validate(&schema_json())
        .unwrap_or_else(|error| panic!("report schema must satisfy the metaschema: {error}"));
}

#[test]
fn report_schema_internal_references_should_resolve() {
    let schema = schema_json();
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
            "report schema contains unresolved reference {reference}"
        );
    }
}

fn collect_references<'a>(value: &'a Value, references: &mut Vec<&'a str>) {
    match value {
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_references(value, references)),
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                references.push(reference);
            }
            object
                .values()
                .for_each(|value| collect_references(value, references));
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[test]
fn every_golden_should_validate_against_the_compiled_offline_schema() {
    let validator = report_validator();
    let mut paths = Vec::new();
    collect_golden_files(&mut paths);
    paths.sort();
    assert_eq!(
        paths.len(),
        11,
        "the documented §15 matrix pins exactly eleven vectors"
    );
    for path in &paths {
        let instance = golden(&path.file_name().expect("file name").to_string_lossy());
        validator.validate(&instance).unwrap_or_else(|error| {
            panic!(
                "{} must satisfy conservation-audit v1: {error}",
                path.display()
            )
        });
    }
}

#[test]
fn serialized_keys_should_match_the_schema_property_sets_exactly() {
    let schema = schema_json();
    let defs = &schema["$defs"];
    let mut paths = Vec::new();
    collect_golden_files(&mut paths);
    for path in &paths {
        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let instance = golden(&name);

        let keys: BTreeSet<String> = instance
            .as_object()
            .expect("report object")
            .keys()
            .cloned()
            .collect();
        assert_keys_match(&name, &keys, &schema, true);

        assert_keys_match(
            &format!("{name}.options"),
            &instance["options"]
                .as_object()
                .expect("options")
                .keys()
                .cloned()
                .collect(),
            &defs["options"],
            false,
        );
        assert_keys_match(
            &format!("{name}.inputs"),
            &instance["inputs"]
                .as_object()
                .expect("inputs")
                .keys()
                .cloned()
                .collect(),
            &schema["properties"]["inputs"],
            false,
        );
        for side in ["base", "head"] {
            assert_keys_match(
                &format!("{name}.inputs.{side}"),
                &instance["inputs"][side]
                    .as_object()
                    .expect("input tree")
                    .keys()
                    .cloned()
                    .collect(),
                &defs["input_tree"],
                false,
            );
        }
        assert_keys_match(
            &format!("{name}.totals"),
            &instance["totals"]
                .as_object()
                .expect("totals")
                .keys()
                .cloned()
                .collect(),
            &defs["totals"],
            false,
        );
        for region in instance["regions"].as_array().expect("regions") {
            assert_keys_match(
                &format!("{name}.regions[{}]", region["id"]),
                &region
                    .as_object()
                    .expect("region")
                    .keys()
                    .cloned()
                    .collect(),
                &defs["removed_region"],
                true,
            );
            for destination in region["destinations"].as_array().expect("destinations") {
                assert_keys_match(
                    &format!("{name}.destinations"),
                    &destination
                        .as_object()
                        .expect("destination")
                        .keys()
                        .cloned()
                        .collect(),
                    &defs["destination"],
                    false,
                );
            }
        }
        for entry in instance["unmatched"].as_array().expect("unmatched") {
            assert_keys_match(
                &format!("{name}.unmatched[{}]", entry["id"]),
                &entry.as_object().expect("entry").keys().cloned().collect(),
                &defs["unmatched_region"],
                false,
            );
        }
        for entry in instance["formatting"].as_array().expect("formatting") {
            assert_keys_match(
                &format!("{name}.formatting[{}]", entry["id"]),
                &entry.as_object().expect("entry").keys().cloned().collect(),
                &defs["formatting_region"],
                true,
            );
            if let Some(paired) = entry.get("paired_destination") {
                assert_keys_match(
                    &format!("{name}.paired_destination"),
                    &paired
                        .as_object()
                        .expect("paired")
                        .keys()
                        .cloned()
                        .collect(),
                    &defs["paired_destination"],
                    false,
                );
            }
        }
        for span in instance["new_content"].as_array().expect("new_content") {
            assert_keys_match(
                &format!("{name}.new_content"),
                &span.as_object().expect("span").keys().cloned().collect(),
                &defs["new_span"],
                false,
            );
        }
    }
}

#[test]
fn warnings_should_be_frozen_to_the_constant_empty_array_both_ways() {
    assert_eq!(schema_json()["properties"]["warnings"]["maxItems"], 0);

    let validator = report_validator();
    let mut instance = golden("pass.json");
    assert_eq!(instance["warnings"], json!([]));
    assert!(validator.is_valid(&instance));

    instance["warnings"] = json!([{"code": "OBSERVATION_MAY_BE_STALE"}]);
    assert!(
        !validator.is_valid(&instance),
        "any warning element must fail the frozen v1 contract"
    );

    instance["warnings"] = json!([]);
    assert!(
        validator.is_valid(&instance),
        "removing it restores validity"
    );
}

#[test]
fn report_schema_should_reject_unknown_fields_and_missing_required_fields() {
    let validator = report_validator();

    let mut unknown = golden("pass.json");
    unknown["unknown_extension"] = json!(true);
    assert!(!validator.is_valid(&unknown));

    let mut missing = golden("pass.json");
    missing
        .as_object_mut()
        .expect("success golden must be an object")
        .remove("totals");
    assert!(!validator.is_valid(&missing));

    let mut bad_verdict = golden("pass.json");
    bad_verdict["verdict"] = json!("passed");
    assert!(!validator.is_valid(&bad_verdict));

    let mut zero_omitted = golden("fail.json");
    zero_omitted["omitted_region_count"] = json!(0);
    assert!(
        !validator.is_valid(&zero_omitted),
        "omitted_region_count must be absent or positive"
    );
}

#[test]
fn closed_vocabularies_should_match_the_schema_enums() {
    let schema = schema_json();

    let verdicts: Vec<&str> = schema["properties"]["verdict"]["enum"]
        .as_array()
        .expect("verdict enum")
        .iter()
        .map(|value| value.as_str().expect("verdict string"))
        .collect();
    assert_eq!(verdicts, vec!["pass", "fail", "inconclusive"]);

    let reasons: BTreeSet<&str> = schema["properties"]["inconclusive_reasons"]["items"]["enum"]
        .as_array()
        .expect("reason enum")
        .iter()
        .map(|value| value.as_str().expect("reason string"))
        .collect();
    assert_eq!(
        reasons,
        BTreeSet::from([
            "work_budget_exceeded",
            "memory_budget_exceeded",
            "unreadable_input_skipped",
            "strict_uniqueness_ambiguity",
            "ambiguous_common",
            "report_truncated",
        ])
    );

    let defs = &schema["$defs"];
    assert_eq!(
        defs["removed_region"]["properties"]["status"]["const"],
        "conserved"
    );
    assert_eq!(
        defs["unmatched_region"]["properties"]["status"]["const"],
        "unmatched_substantial"
    );
    assert_eq!(
        defs["formatting_region"]["properties"]["status"]["const"],
        "formatting_only"
    );

    // Listing caps mirror §10 exactly.
    assert_eq!(schema["properties"]["regions"]["maxItems"], 2_000);
    assert_eq!(schema["properties"]["unmatched"]["maxItems"], 200);
    assert_eq!(schema["properties"]["formatting"]["maxItems"], 200);
    assert_eq!(schema["properties"]["new_content"]["maxItems"], 50);
    assert_eq!(
        defs["removed_region"]["properties"]["destinations"]["minItems"],
        1
    );
}

#[test]
fn every_golden_file_should_be_exactly_one_lf_terminated_json_value() {
    let mut paths = Vec::new();
    collect_golden_files(&mut paths);
    for path in &paths {
        let bytes = fs::read(path)
            .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
        assert_eq!(
            bytes.last(),
            Some(&b'\n'),
            "{} must end with exactly one LF",
            path.display()
        );
        assert!(
            !bytes[..bytes.len() - 1].contains(&b'\n'),
            "{} must not embed raw newlines",
            path.display()
        );
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            panic!("{} must parse as one JSON value: {error}", path.display())
        });
        assert_eq!(parsed["conservation_audit_version"], 1);
    }
}
