//! Integration coverage for document-symbol normalization and resolution.

use gen_lsp_types::{DocumentSymbol, Position, Range, SymbolKind};
use serde_json::json;
use srcmv_core::{ByteRange, LineIndex};
use srcmv_lsp::capabilities::SupportedPositionEncoding;
use srcmv_lsp::position::{PositionConverter, PositionLimits};
use srcmv_lsp::symbols::{
    DEFAULT_MAXIMUM_OUTLINE_SYMBOLS, KnownSymbolKind, MatchMode, NormalizedSymbolKind,
    SelectionExtent, SymbolError, SymbolLimits, apply_extent, normalize_document_symbols,
    normalize_hierarchical_symbols, order_unique_candidates, resolve_name, resolve_position,
};

const MAXIMUM_TEST_LINES: u64 = 10_000;
const MAXIMUM_TEST_INDEX_BYTES: u64 = 1_000_000;

fn range(start_line: u32, start_character: u32, end_line: u32, end_character: u32) -> Range {
    Range::new(
        Position::new(start_line, start_character),
        Position::new(end_line, end_character),
    )
}

fn symbol(
    name: &str,
    kind: SymbolKind,
    symbol_range: Range,
    selection_range: Range,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    symbol_with_detail(name, kind, symbol_range, selection_range, None, children)
}

fn symbol_with_detail(
    name: &str,
    kind: SymbolKind,
    symbol_range: Range,
    selection_range: Range,
    detail: Option<String>,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    DocumentSymbol::new(
        name.to_owned(),
        detail,
        kind,
        None,
        None,
        symbol_range,
        selection_range,
        children,
    )
}

fn with_converter<T>(text: &str, operation: impl FnOnce(&mut PositionConverter<'_>) -> T) -> T {
    let index = LineIndex::from_bytes_with_limits(
        text.as_bytes(),
        MAXIMUM_TEST_LINES,
        MAXIMUM_TEST_INDEX_BYTES,
    )
    .expect("test line index should build");
    let mut converter = PositionConverter::new(
        text,
        &index,
        SupportedPositionEncoding::Utf8,
        PositionLimits::default(),
    )
    .expect("test converter should build");
    operation(&mut converter)
}

#[test]
fn raw_null_and_empty_array_normalize_to_no_symbols() {
    for value in [json!(null), json!([])] {
        let normalized = with_converter("fn f() {}\n", |converter| {
            normalize_document_symbols(value, converter, SymbolLimits::default())
        })
        .expect("null and empty results should be accepted");
        assert!(normalized.is_empty());
    }
}

#[test]
fn nonempty_flat_and_mixed_wire_responses_are_rejected() {
    let flat = json!([{
        "name": "f",
        "kind": 12,
        "location": {
            "uri": "file:///workspace/src/lib.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
        }
    }]);
    let mixed = json!([
        {
            "name": "f",
            "kind": 12,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
        },
        {
            "name": "g",
            "kind": 12,
            "location": {
                "uri": "file:///workspace/src/lib.rs",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
            }
        }
    ]);

    let flat_error = with_converter("x", |converter| {
        normalize_document_symbols(flat, converter, SymbolLimits::default())
    })
    .expect_err("flat symbols must be rejected");
    let mixed_error = with_converter("x", |converter| {
        normalize_document_symbols(mixed, converter, SymbolLimits::default())
    })
    .expect_err("mixed symbols must be rejected");

    assert_eq!(flat_error, SymbolError::FlatSymbolsUnsupported);
    assert_eq!(mixed_error, SymbolError::MalformedDocumentSymbols);
}

#[test]
fn hierarchy_flattens_iteratively_with_complete_breadcrumbs_and_unknown_kind() {
    let leaf = symbol(
        "future",
        SymbolKind::Custom(99),
        range(2, 4, 2, 10),
        range(2, 4, 2, 10),
        None,
    );
    let inner = symbol(
        "Inner",
        SymbolKind::Class,
        range(1, 2, 3, 3),
        range(1, 2, 1, 7),
        Some(vec![leaf]),
    );
    let outer = symbol(
        "Outer",
        SymbolKind::Class,
        range(0, 0, 4, 1),
        range(0, 0, 0, 5),
        Some(vec![inner]),
    );
    let text = "Outer\n  Inner\n    future\n  }\n}\n";

    let normalized = with_converter(text, |converter| {
        normalize_hierarchical_symbols(vec![outer], converter, SymbolLimits::default())
    })
    .expect("well-formed hierarchy should normalize");

    assert_eq!(normalized.len(), 3);
    assert_eq!(normalized[2].symbol_path, ["Outer", "Inner", "future"]);
    assert_eq!(normalized[2].kind, NormalizedSymbolKind::Unknown(99));
    assert_eq!(normalized[2].kind.as_str(), "unknown");
    assert_eq!(normalized[2].kind.unknown_numeric(), Some(99));
}

#[test]
fn nesting_depth_is_bounded_without_recursive_flattening() {
    let text = "x";
    let mut root = symbol(
        "leaf",
        SymbolKind::Function,
        range(0, 0, 0, 1),
        range(0, 0, 0, 1),
        None,
    );
    for depth in (1..=300).rev() {
        root = symbol(
            &format!("n{depth}"),
            SymbolKind::Class,
            range(0, 0, 0, 1),
            range(0, 0, 0, 1),
            Some(vec![root]),
        );
    }
    let limits = SymbolLimits {
        maximum_depth: 256,
        ..SymbolLimits::default()
    };

    let error = with_converter(text, |converter| {
        normalize_hierarchical_symbols(vec![root], converter, limits)
    })
    .expect_err("depth above the limit must fail");

    assert_eq!(
        error,
        SymbolError::ResourceLimitExceeded {
            resource: "symbol_nesting_depth",
            maximum: 256,
        }
    );
}

#[test]
fn raw_and_flattened_counts_and_symbol_text_have_independent_limits() {
    let roots = vec![
        symbol(
            "a",
            SymbolKind::Function,
            range(0, 0, 0, 1),
            range(0, 0, 0, 1),
            None,
        ),
        symbol(
            "b",
            SymbolKind::Function,
            range(0, 0, 0, 1),
            range(0, 0, 0, 1),
            None,
        ),
    ];
    let raw_limits = SymbolLimits {
        maximum_raw_symbols: 1,
        ..SymbolLimits::default()
    };
    let flat_limits = SymbolLimits {
        maximum_flattened_symbols: 1,
        ..SymbolLimits::default()
    };

    let raw_error = with_converter("x", |converter| {
        normalize_hierarchical_symbols(roots.clone(), converter, raw_limits)
    })
    .expect_err("raw count should be bounded");
    let flat_error = with_converter("x", |converter| {
        normalize_hierarchical_symbols(roots, converter, flat_limits)
    })
    .expect_err("flattened count should be bounded");
    let name_error = with_converter("x", |converter| {
        normalize_hierarchical_symbols(
            vec![symbol(
                "ab",
                SymbolKind::Function,
                range(0, 0, 0, 1),
                range(0, 0, 0, 1),
                None,
            )],
            converter,
            SymbolLimits {
                maximum_name_bytes: 1,
                ..SymbolLimits::default()
            },
        )
    })
    .expect_err("name bytes should be bounded");

    assert!(matches!(
        raw_error,
        SymbolError::ResourceLimitExceeded {
            resource: "raw_document_symbols",
            ..
        }
    ));
    assert!(matches!(
        flat_error,
        SymbolError::ResourceLimitExceeded {
            resource: "flattened_document_symbols",
            ..
        }
    ));
    assert!(matches!(
        name_error,
        SymbolError::ResourceLimitExceeded {
            resource: "symbol_name_bytes",
            ..
        }
    ));
}

#[test]
fn child_fanout_is_rejected_before_growing_the_pending_stack_past_raw_limit() {
    let text = "abc";
    let children = vec![
        symbol(
            "first",
            SymbolKind::Function,
            range(0, 0, 0, 1),
            range(0, 0, 0, 1),
            None,
        ),
        symbol(
            "second",
            SymbolKind::Function,
            range(0, 1, 0, 2),
            range(0, 1, 0, 2),
            None,
        ),
    ];
    let error = with_converter(text, |converter| {
        normalize_hierarchical_symbols(
            vec![symbol(
                "root",
                SymbolKind::Class,
                range(0, 0, 0, 3),
                range(0, 0, 0, 1),
                Some(children),
            )],
            converter,
            SymbolLimits {
                maximum_raw_symbols: 2,
                ..SymbolLimits::default()
            },
        )
    })
    .expect_err("root plus two children exceeds the raw-node limit");

    assert!(matches!(
        error,
        SymbolError::ResourceLimitExceeded {
            resource: "raw_document_symbols",
            maximum: 2
        }
    ));
}

#[test]
fn candidate_storage_and_match_output_are_bounded() {
    let text = "a\nb\n";
    let roots = vec![
        symbol(
            "a",
            SymbolKind::Function,
            range(0, 0, 0, 1),
            range(0, 0, 0, 1),
            None,
        ),
        symbol(
            "b",
            SymbolKind::Function,
            range(1, 0, 1, 1),
            range(1, 0, 1, 1),
            None,
        ),
    ];
    let storage_error = with_converter(text, |converter| {
        normalize_hierarchical_symbols(
            roots.clone(),
            converter,
            SymbolLimits {
                maximum_candidate_storage_bytes: 3,
                ..SymbolLimits::default()
            },
        )
    })
    .expect_err("owned candidate strings should be bounded cumulatively");
    let normalized = with_converter(text, |converter| {
        normalize_hierarchical_symbols(roots, converter, SymbolLimits::default())
    })
    .expect("symbols should normalize");
    let matches_error = resolve_position(
        &normalized,
        text,
        0,
        None,
        SelectionExtent::Symbol,
        MatchMode::All,
        SymbolLimits {
            maximum_matches: 0,
            ..SymbolLimits::default()
        },
    )
    .expect_err("all-mode output should obey the response match cap");

    assert!(matches!(
        storage_error,
        SymbolError::ResourceLimitExceeded {
            resource: "symbol_candidate_storage_bytes",
            maximum: 3,
        }
    ));
    assert!(matches!(
        matches_error,
        SymbolError::ResourceLimitExceeded {
            resource: "selection_matches",
            maximum: 0,
        }
    ));
}

#[test]
fn standardized_kind_strings_round_trip_and_custom_values_stay_unqueryable() {
    let standardized = [
        KnownSymbolKind::File,
        KnownSymbolKind::Module,
        KnownSymbolKind::Namespace,
        KnownSymbolKind::Package,
        KnownSymbolKind::Class,
        KnownSymbolKind::Method,
        KnownSymbolKind::Property,
        KnownSymbolKind::Field,
        KnownSymbolKind::Constructor,
        KnownSymbolKind::Enum,
        KnownSymbolKind::Interface,
        KnownSymbolKind::Function,
        KnownSymbolKind::Variable,
        KnownSymbolKind::Constant,
        KnownSymbolKind::String,
        KnownSymbolKind::Number,
        KnownSymbolKind::Boolean,
        KnownSymbolKind::Array,
        KnownSymbolKind::Object,
        KnownSymbolKind::Key,
        KnownSymbolKind::Null,
        KnownSymbolKind::EnumMember,
        KnownSymbolKind::Struct,
        KnownSymbolKind::Event,
        KnownSymbolKind::Operator,
        KnownSymbolKind::TypeParameter,
    ];

    for kind in standardized {
        assert_eq!(kind.as_str().parse(), Ok(kind));
    }
    assert!("unknown".parse::<KnownSymbolKind>().is_err());
    assert!("Function".parse::<KnownSymbolKind>().is_err());
}

#[test]
fn every_range_is_validated_before_name_filtering() {
    let malformed = symbol(
        "ignored",
        SymbolKind::Function,
        range(0, 1, 0, 1),
        range(0, 1, 0, 1),
        None,
    );
    let wanted = symbol(
        "wanted",
        SymbolKind::Function,
        range(0, 0, 0, 1),
        range(0, 0, 0, 1),
        None,
    );

    let error = with_converter("x", |converter| {
        normalize_hierarchical_symbols(vec![wanted, malformed], converter, SymbolLimits::default())
    })
    .expect_err("unmatched malformed symbols must still fail normalization");

    assert!(matches!(error, SymbolError::Position(_)));
}

#[test]
fn empty_names_and_lsp_uintegers_above_the_wire_limit_are_rejected() {
    let empty_name = symbol(
        "",
        SymbolKind::Function,
        range(0, 0, 0, 1),
        range(0, 0, 0, 1),
        None,
    );
    let oversized = symbol(
        "future",
        SymbolKind::Function,
        range(0, 0, 0, u32::MAX),
        range(0, 0, 0, 1),
        None,
    );

    for invalid in [empty_name, oversized] {
        let error = with_converter("x", |converter| {
            normalize_hierarchical_symbols(vec![invalid], converter, SymbolLimits::default())
        })
        .expect_err("schema-invalid server fields must fail normalization");
        assert_eq!(error, SymbolError::MalformedDocumentSymbols);
    }
}

#[test]
fn selection_range_must_be_nonempty_and_contained() {
    let outside = symbol(
        "f",
        SymbolKind::Function,
        range(0, 1, 0, 3),
        range(0, 0, 0, 1),
        None,
    );
    let empty = symbol(
        "g",
        SymbolKind::Function,
        range(0, 0, 0, 3),
        range(0, 1, 0, 1),
        None,
    );

    let outside_error = with_converter("abcd", |converter| {
        normalize_hierarchical_symbols(vec![outside], converter, SymbolLimits::default())
    })
    .expect_err("outside selection range must fail");
    let empty_error = with_converter("abcd", |converter| {
        normalize_hierarchical_symbols(vec![empty], converter, SymbolLimits::default())
    })
    .expect_err("empty selection range must fail");

    assert_eq!(outside_error, SymbolError::SelectionRangeNotContained);
    assert!(matches!(empty_error, SymbolError::Position(_)));
}

#[test]
fn name_resolution_filters_kind_deduplicates_and_sorts_deterministically() {
    let duplicate = symbol(
        "run",
        SymbolKind::Method,
        range(1, 0, 1, 3),
        range(1, 0, 1, 3),
        None,
    );
    let other_kind = symbol(
        "run",
        SymbolKind::Function,
        range(0, 0, 0, 3),
        range(0, 0, 0, 3),
        None,
    );
    let text = "run\nrun\n";
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(
            vec![duplicate.clone(), other_kind, duplicate],
            converter,
            SymbolLimits::default(),
        )
    })
    .expect("symbols should normalize");

    let matches = resolve_name(
        &symbols,
        text,
        "run",
        Some(KnownSymbolKind::Method),
        SelectionExtent::Symbol,
        MatchMode::All,
        SymbolLimits::default(),
    )
    .expect("all mode should resolve");

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].symbol_range, ByteRange { start: 4, end: 7 });
}

#[test]
fn unique_name_ambiguity_is_bounded_and_deterministically_ordered() {
    let text = "run\nrun\nrun\n";
    let roots = (0..3)
        .rev()
        .map(|line| {
            symbol(
                "run",
                SymbolKind::Function,
                range(line, 0, line, 3),
                range(line, 0, line, 3),
                None,
            )
        })
        .collect();
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(roots, converter, SymbolLimits::default())
    })
    .expect("symbols should normalize");

    let error = resolve_name(
        &symbols,
        text,
        "run",
        None,
        SelectionExtent::Symbol,
        MatchMode::Unique,
        SymbolLimits {
            maximum_ambiguity_candidates: 2,
            ..SymbolLimits::default()
        },
    )
    .expect_err("unique query should report ambiguity");

    let SymbolError::Ambiguous { total, candidates } = error else {
        panic!("expected ambiguity");
    };
    assert_eq!(total, 3);
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].byte_range.start, 0);
    assert_eq!(candidates[1].byte_range.start, 4);
}

#[test]
fn position_resolution_selects_smallest_and_treats_equal_smallest_as_ambiguous() {
    let text = "abcdefghij";
    let roots = vec![
        symbol(
            "outer",
            SymbolKind::Class,
            range(0, 0, 0, 10),
            range(0, 0, 0, 1),
            None,
        ),
        symbol(
            "first",
            SymbolKind::Method,
            range(0, 2, 0, 7),
            range(0, 2, 0, 3),
            None,
        ),
        symbol(
            "second",
            SymbolKind::Method,
            range(0, 3, 0, 8),
            range(0, 3, 0, 4),
            None,
        ),
    ];
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(roots, converter, SymbolLimits::default())
    })
    .expect("symbols should normalize");

    let error = resolve_position(
        &symbols,
        text,
        4,
        Some(KnownSymbolKind::Method),
        SelectionExtent::Symbol,
        MatchMode::Unique,
        SymbolLimits::default(),
    )
    .expect_err("equal-size smallest symbols must be ambiguous");
    let outer = resolve_position(
        &symbols,
        text,
        9,
        None,
        SelectionExtent::Symbol,
        MatchMode::Unique,
        SymbolLimits::default(),
    )
    .expect("only outer symbol contains this byte");

    assert!(matches!(error, SymbolError::Ambiguous { total: 2, .. }));
    assert_eq!(outer[0].name, "outer");
}

#[test]
fn all_position_matches_use_the_frozen_start_end_candidate_order() {
    let text = "abcdefghij";
    let roots = vec![
        symbol(
            "outer",
            SymbolKind::Class,
            range(0, 0, 0, 10),
            range(0, 0, 0, 1),
            None,
        ),
        symbol(
            "inner",
            SymbolKind::Method,
            range(0, 2, 0, 7),
            range(0, 2, 0, 3),
            None,
        ),
    ];
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(roots, converter, SymbolLimits::default())
    })
    .expect("symbols should normalize");

    let matches = resolve_position(
        &symbols,
        text,
        4,
        None,
        SelectionExtent::Symbol,
        MatchMode::All,
        SymbolLimits::default(),
    )
    .expect("all position matches should resolve");

    assert_eq!(
        matches
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        ["outer", "inner"]
    );
}

#[test]
fn eof_position_matches_only_nonempty_symbols_ending_at_eof() {
    let text = "abc";
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(
            vec![symbol(
                "f",
                SymbolKind::Function,
                range(0, 0, 0, 3),
                range(0, 0, 0, 1),
                None,
            )],
            converter,
            SymbolLimits::default(),
        )
    })
    .expect("symbol should normalize");

    let matches = resolve_position(
        &symbols,
        text,
        3,
        None,
        SelectionExtent::Symbol,
        MatchMode::Unique,
        SymbolLimits::default(),
    )
    .expect("EOF exception should select an ending symbol");

    assert_eq!(matches[0].selected_range, ByteRange { start: 0, end: 3 });
}

#[test]
fn position_resolution_rejects_offsets_splitting_utf8_scalars() {
    let text = "é";
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(
            vec![symbol(
                "value",
                SymbolKind::String,
                range(0, 0, 0, 2),
                range(0, 0, 0, 2),
                None,
            )],
            converter,
            SymbolLimits::default(),
        )
    })
    .expect("UTF-8 symbol should normalize");

    let error = resolve_position(
        &symbols,
        text,
        1,
        None,
        SelectionExtent::Symbol,
        MatchMode::Unique,
        SymbolLimits::default(),
    )
    .expect_err("an interior UTF-8 byte is not a valid insertion position");

    assert_eq!(error, SymbolError::QueryPositionOutOfBounds);
}

#[test]
fn declaration_lines_expands_whitespace_and_preserves_all_terminator_forms() {
    let cases = [
        ("\t  abc  \nnext", ByteRange { start: 3, end: 6 }, 0..9),
        ("  abc\t\r\nnext", ByteRange { start: 2, end: 5 }, 0..8),
        ("  abc \rnext", ByteRange { start: 2, end: 5 }, 0..7),
        ("  abc  ", ByteRange { start: 2, end: 5 }, 0..7),
    ];

    for (text, input, expected) in cases {
        let actual = apply_extent(text, input, SelectionExtent::DeclarationLines)
            .expect("extent should expand");
        assert_eq!(
            actual,
            ByteRange {
                start: expected.start,
                end: expected.end,
            }
        );
    }
}

#[test]
fn declaration_lines_does_not_cross_nonwhitespace_or_consume_line_after_start_end() {
    let text = "prefix abc suffix\nnext\n\n";
    let unchanged = apply_extent(
        text,
        ByteRange { start: 7, end: 10 },
        SelectionExtent::DeclarationLines,
    )
    .expect("nonwhitespace sides should preserve boundaries");
    let ends_at_line_start = apply_extent(
        text,
        ByteRange { start: 0, end: 18 },
        SelectionExtent::DeclarationLines,
    )
    .expect("range ending after a terminator should not consume the next line");

    assert_eq!(unchanged, ByteRange { start: 7, end: 10 });
    assert_eq!(ends_at_line_start, ByteRange { start: 0, end: 18 });
}

#[test]
fn all_mode_returns_empty_while_unique_mode_reports_not_found() {
    let all = resolve_name(
        &[],
        "",
        "missing",
        None,
        SelectionExtent::Symbol,
        MatchMode::All,
        SymbolLimits::default(),
    )
    .expect("all mode should permit no matches");
    let unique = resolve_name(
        &[],
        "",
        "missing",
        None,
        SelectionExtent::Symbol,
        MatchMode::Unique,
        SymbolLimits::default(),
    )
    .expect_err("unique mode should report no match");

    assert!(all.is_empty());
    assert_eq!(unique, SymbolError::NotFound);
}

#[test]
fn outline_candidates_follow_the_frozen_comparator_and_coalesce_duplicates() {
    assert_eq!(DEFAULT_MAXIMUM_OUTLINE_SYMBOLS, 10_000);

    let text = "aaaa\nbbbb\ncccc\ndddd\neeee\n";
    let roots = vec![
        symbol(
            "late",
            SymbolKind::Function,
            range(4, 0, 4, 4),
            range(4, 0, 4, 1),
            None,
        ),
        symbol(
            "mid",
            SymbolKind::Class,
            range(2, 0, 3, 4),
            range(2, 0, 2, 1),
            Some(vec![symbol(
                "inner",
                SymbolKind::Method,
                range(3, 0, 3, 4),
                range(3, 0, 3, 1),
                None,
            )]),
        ),
        symbol(
            "early",
            SymbolKind::Function,
            range(0, 0, 0, 4),
            range(0, 0, 0, 1),
            None,
        ),
        // Exact duplicate of the first root: same lsp_range, kind, path, name.
        symbol(
            "late",
            SymbolKind::Function,
            range(4, 0, 4, 4),
            range(4, 0, 4, 1),
            None,
        ),
        // Same enclosing range and name as `early`, but an earlier kind spelling.
        symbol(
            "same",
            SymbolKind::Class,
            range(0, 0, 0, 4),
            range(0, 0, 0, 1),
            None,
        ),
        symbol(
            "same",
            SymbolKind::Function,
            range(0, 0, 0, 4),
            range(0, 0, 0, 1),
            None,
        ),
    ];
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(roots, converter, SymbolLimits::default())
    })
    .expect("symbols should normalize");

    let ordered = order_unique_candidates(&symbols);

    let observed = ordered
        .iter()
        .map(|candidate| (candidate.name.as_str(), candidate.kind.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        [
            ("same", "class"),
            ("early", "function"),
            ("same", "function"),
            ("mid", "class"),
            ("inner", "method"),
            ("late", "function"),
        ]
    );
}

#[test]
fn duplicate_keys_coalesce_regardless_of_detail_and_keep_the_sorted_survivor() {
    let text = "run\nrun\n";
    let roots = vec![
        symbol_with_detail(
            "run",
            SymbolKind::Method,
            range(1, 0, 1, 3),
            range(1, 0, 1, 3),
            Some("zzz detail".to_owned()),
            None,
        ),
        symbol_with_detail(
            "run",
            SymbolKind::Method,
            range(1, 0, 1, 3),
            range(1, 0, 1, 3),
            Some("aaa detail".to_owned()),
            None,
        ),
    ];
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(roots, converter, SymbolLimits::default())
    })
    .expect("symbols should normalize");

    let ordered = order_unique_candidates(&symbols);

    assert_eq!(ordered.len(), 1);
    assert_eq!(ordered[0].detail.as_deref(), Some("aaa detail"));
}

#[test]
fn outline_ordering_matches_all_mode_resolution_for_every_name() {
    let text = "aaaa\nbbbb\ncccc\ndddd\neeee\n";
    let roots = vec![
        symbol(
            "late",
            SymbolKind::Function,
            range(4, 0, 4, 4),
            range(4, 0, 4, 1),
            None,
        ),
        symbol(
            "mid",
            SymbolKind::Class,
            range(2, 0, 3, 4),
            range(2, 0, 2, 1),
            Some(vec![
                symbol(
                    "shared",
                    SymbolKind::Method,
                    range(3, 0, 3, 4),
                    range(3, 0, 3, 1),
                    None,
                ),
                symbol(
                    "shared",
                    SymbolKind::Function,
                    range(3, 0, 3, 4),
                    range(3, 0, 3, 2),
                    None,
                ),
            ]),
        ),
        symbol(
            "early",
            SymbolKind::Function,
            range(0, 0, 0, 4),
            range(0, 0, 0, 1),
            None,
        ),
        symbol(
            "late",
            SymbolKind::Function,
            range(4, 0, 4, 4),
            range(4, 0, 4, 1),
            None,
        ),
    ];
    let symbols = with_converter(text, |converter| {
        normalize_hierarchical_symbols(roots, converter, SymbolLimits::default())
    })
    .expect("symbols should normalize");
    let ordered = order_unique_candidates(&symbols);

    let mut names = ordered
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let resolved = resolve_name(
            &symbols,
            text,
            name,
            None,
            SelectionExtent::Symbol,
            MatchMode::All,
            SymbolLimits::default(),
        )
        .expect("all-mode resolution should succeed");
        let outlined = ordered
            .iter()
            .filter(|item| item.name == name)
            .map(|item| (item.name.as_str(), item.kind.as_str(), item.byte_range))
            .collect::<Vec<_>>();
        let resolved = resolved
            .iter()
            .map(|item: &srcmv_lsp::symbols::SymbolMatch| {
                (item.name.as_str(), item.kind.as_str(), item.symbol_range)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            outlined, resolved,
            "outline order must match all-mode resolution for `{name}`"
        );
    }
}
