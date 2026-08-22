//! Validation, normalization, and deterministic resolution of document symbols.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use gen_lsp_types::{DocumentSymbol, Range, SymbolKind};
use serde_json::Value;
use srcmv_core::ByteRange;

use crate::position::{PositionConverter, PositionError};

/// Default maximum number of raw document-symbol nodes accepted from a server.
pub const DEFAULT_MAXIMUM_RAW_SYMBOLS: u64 = 100_000;
/// Default maximum number of normalized symbols retained for resolution.
pub const DEFAULT_MAXIMUM_FLATTENED_SYMBOLS: u64 = 100_000;
/// Default maximum hierarchy depth, counting a root symbol as depth one.
pub const DEFAULT_MAXIMUM_SYMBOL_DEPTH: u64 = 256;
/// Default maximum UTF-8 byte length of one symbol name.
pub const DEFAULT_MAXIMUM_SYMBOL_NAME_BYTES: u64 = 4 * 1024;
/// Default maximum UTF-8 byte length of one symbol detail.
pub const DEFAULT_MAXIMUM_SYMBOL_DETAIL_BYTES: u64 = 16 * 1024;
/// Default maximum cumulative UTF-8 byte length of one symbol breadcrumb.
pub const DEFAULT_MAXIMUM_SYMBOL_PATH_BYTES: u64 = 64 * 1024;
/// Default maximum cumulative owned string bytes across normalized candidates.
pub const DEFAULT_MAXIMUM_CANDIDATE_STORAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Default maximum matches representable by selection protocol v1.
pub const DEFAULT_MAXIMUM_MATCHES: usize = 1_000;
/// Default maximum number of symbols emitted by one outline response.
pub const DEFAULT_MAXIMUM_OUTLINE_SYMBOLS: usize = 10_000;
/// Default maximum number of candidates retained in an ambiguity error.
pub const DEFAULT_MAXIMUM_AMBIGUITY_CANDIDATES: usize = 50;
const MAXIMUM_LSP_UINTEGER: u32 = 2_147_483_647;

/// Resource bounds applied while normalizing and resolving document symbols.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SymbolLimits {
    /// Maximum number of hierarchy nodes visited.
    pub maximum_raw_symbols: u64,
    /// Maximum number of normalized candidates retained.
    pub maximum_flattened_symbols: u64,
    /// Maximum hierarchy depth, with roots at depth one.
    pub maximum_depth: u64,
    /// Maximum UTF-8 byte length of one name.
    pub maximum_name_bytes: u64,
    /// Maximum UTF-8 byte length of one optional detail.
    pub maximum_detail_bytes: u64,
    /// Maximum cumulative UTF-8 byte length of one breadcrumb.
    pub maximum_path_bytes: u64,
    /// Maximum cumulative owned string bytes across normalized candidates.
    pub maximum_candidate_storage_bytes: u64,
    /// Maximum matches returned by all-match discovery.
    pub maximum_matches: usize,
    /// Maximum candidates included in an ambiguity error.
    pub maximum_ambiguity_candidates: usize,
}

impl Default for SymbolLimits {
    fn default() -> Self {
        Self {
            maximum_raw_symbols: DEFAULT_MAXIMUM_RAW_SYMBOLS,
            maximum_flattened_symbols: DEFAULT_MAXIMUM_FLATTENED_SYMBOLS,
            maximum_depth: DEFAULT_MAXIMUM_SYMBOL_DEPTH,
            maximum_name_bytes: DEFAULT_MAXIMUM_SYMBOL_NAME_BYTES,
            maximum_detail_bytes: DEFAULT_MAXIMUM_SYMBOL_DETAIL_BYTES,
            maximum_path_bytes: DEFAULT_MAXIMUM_SYMBOL_PATH_BYTES,
            maximum_candidate_storage_bytes: DEFAULT_MAXIMUM_CANDIDATE_STORAGE_BYTES,
            maximum_matches: DEFAULT_MAXIMUM_MATCHES,
            maximum_ambiguity_candidates: DEFAULT_MAXIMUM_AMBIGUITY_CANDIDATES,
        }
    }
}

/// A standardized symbol kind accepted as a query filter.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum KnownSymbolKind {
    /// A file.
    File,
    /// A module.
    Module,
    /// A namespace.
    Namespace,
    /// A package.
    Package,
    /// A class.
    Class,
    /// A method.
    Method,
    /// A property.
    Property,
    /// A field.
    Field,
    /// A constructor.
    Constructor,
    /// An enumeration.
    Enum,
    /// An interface.
    Interface,
    /// A function.
    Function,
    /// A variable.
    Variable,
    /// A constant.
    Constant,
    /// A string value.
    String,
    /// A numeric value.
    Number,
    /// A boolean value.
    Boolean,
    /// An array value.
    Array,
    /// An object value.
    Object,
    /// An object key.
    Key,
    /// A null value.
    Null,
    /// An enumeration member.
    EnumMember,
    /// A structure.
    Struct,
    /// An event.
    Event,
    /// An operator.
    Operator,
    /// A type parameter.
    TypeParameter,
}

impl KnownSymbolKind {
    /// Returns the stable lowercase spelling used by selection protocol v1.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Module => "module",
            Self::Namespace => "namespace",
            Self::Package => "package",
            Self::Class => "class",
            Self::Method => "method",
            Self::Property => "property",
            Self::Field => "field",
            Self::Constructor => "constructor",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Function => "function",
            Self::Variable => "variable",
            Self::Constant => "constant",
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
            Self::Key => "key",
            Self::Null => "null",
            Self::EnumMember => "enum_member",
            Self::Struct => "struct",
            Self::Event => "event",
            Self::Operator => "operator",
            Self::TypeParameter => "type_parameter",
        }
    }
}

impl fmt::Display for KnownSymbolKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Error returned when a query kind is not one of the standardized spellings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownSymbolKind;

impl fmt::Display for UnknownSymbolKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unknown standardized symbol kind")
    }
}

impl std::error::Error for UnknownSymbolKind {}

impl FromStr for KnownSymbolKind {
    type Err = UnknownSymbolKind;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "file" => Ok(Self::File),
            "module" => Ok(Self::Module),
            "namespace" => Ok(Self::Namespace),
            "package" => Ok(Self::Package),
            "class" => Ok(Self::Class),
            "method" => Ok(Self::Method),
            "property" => Ok(Self::Property),
            "field" => Ok(Self::Field),
            "constructor" => Ok(Self::Constructor),
            "enum" => Ok(Self::Enum),
            "interface" => Ok(Self::Interface),
            "function" => Ok(Self::Function),
            "variable" => Ok(Self::Variable),
            "constant" => Ok(Self::Constant),
            "string" => Ok(Self::String),
            "number" => Ok(Self::Number),
            "boolean" => Ok(Self::Boolean),
            "array" => Ok(Self::Array),
            "object" => Ok(Self::Object),
            "key" => Ok(Self::Key),
            "null" => Ok(Self::Null),
            "enum_member" => Ok(Self::EnumMember),
            "struct" => Ok(Self::Struct),
            "event" => Ok(Self::Event),
            "operator" => Ok(Self::Operator),
            "type_parameter" => Ok(Self::TypeParameter),
            _ => Err(UnknownSymbolKind),
        }
    }
}

/// A normalized kind, retaining the number of a future LSP kind for diagnostics.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NormalizedSymbolKind {
    /// A kind standardized by the negotiated LSP version.
    Known(KnownSymbolKind),
    /// A future or vendor-defined numeric kind.
    Unknown(u32),
}

impl NormalizedSymbolKind {
    /// Returns the stable selection-protocol spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Known(kind) => kind.as_str(),
            Self::Unknown(_) => "unknown",
        }
    }

    /// Returns the server's numeric value when the kind is not standardized.
    #[must_use]
    pub const fn unknown_numeric(self) -> Option<u32> {
        match self {
            Self::Known(_) => None,
            Self::Unknown(value) => Some(value),
        }
    }

    const fn numeric(self) -> u32 {
        match self {
            Self::Known(kind) => known_kind_number(kind),
            Self::Unknown(value) => value,
        }
    }
}

impl fmt::Display for NormalizedSymbolKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(kind) => kind.fmt(formatter),
            Self::Unknown(value) => write!(formatter, "unknown ({value})"),
        }
    }
}

/// A fully validated hierarchical document symbol in snapshot byte coordinates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedSymbol {
    /// Unqualified name reported by the server.
    pub name: String,
    /// Optional server detail, bounded but otherwise uninterpreted.
    pub detail: Option<String>,
    /// Standardized or future numeric symbol kind.
    pub kind: NormalizedSymbolKind,
    /// Complete hierarchy breadcrumb, including this symbol.
    pub symbol_path: Vec<String>,
    /// Raw enclosing LSP range retained for audit output.
    pub lsp_range: Range,
    /// Raw LSP reveal range retained for audit output.
    pub lsp_selection_range: Range,
    /// Validated enclosing byte range used for semantic selection.
    pub byte_range: ByteRange,
    /// Validated reveal byte range, which is contained by `byte_range`.
    pub selection_byte_range: ByteRange,
}

/// Whether a selected declaration is expanded to whitespace-only line edges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionExtent {
    /// Use the server's enclosing symbol range exactly.
    Symbol,
    /// Include line indentation and a terminator when only spaces or tabs intervene.
    DeclarationLines,
}

/// Whether resolution requires one match or returns all matches for discovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchMode {
    /// Require exactly one result after query-specific resolution.
    Unique,
    /// Return every bounded result, including an empty result.
    All,
}

/// One selected symbol with both raw LSP and authoritative byte coordinates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolMatch {
    /// Unqualified symbol name.
    pub name: String,
    /// Optional bounded detail reported by the server.
    pub detail: Option<String>,
    /// Normalized symbol kind.
    pub kind: NormalizedSymbolKind,
    /// Complete hierarchy breadcrumb.
    pub symbol_path: Vec<String>,
    /// Raw enclosing LSP range.
    pub lsp_range: Range,
    /// Raw LSP reveal range.
    pub lsp_selection_range: Range,
    /// Validated enclosing byte range before extent expansion.
    pub symbol_range: ByteRange,
    /// Authoritative final byte selector.
    pub selected_range: ByteRange,
    /// Extent applied to produce `selected_range`.
    pub extent: SelectionExtent,
}

/// A bounded candidate suitable for an ambiguity diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbiguityCandidate {
    /// Candidate name.
    pub name: String,
    /// Candidate kind.
    pub kind: NormalizedSymbolKind,
    /// Complete hierarchy breadcrumb.
    pub symbol_path: Vec<String>,
    /// Validated, unexpanded symbol range.
    pub byte_range: ByteRange,
}

/// Fail-closed document-symbol validation or resolution failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SymbolError {
    /// The server returned legacy flat `SymbolInformation[]` records.
    FlatSymbolsUnsupported,
    /// The document-symbol result was neither `null` nor one homogeneous array.
    MalformedDocumentSymbols,
    /// A configured symbol resource bound was exceeded.
    ResourceLimitExceeded {
        /// Stable resource name.
        resource: &'static str,
        /// Configured maximum.
        maximum: u64,
    },
    /// Position or range conversion failed.
    Position(PositionError),
    /// A selection range was not contained by its enclosing symbol range.
    SelectionRangeNotContained,
    /// A position query was outside the immutable snapshot.
    QueryPositionOutOfBounds,
    /// No symbol matched a unique query.
    NotFound,
    /// Multiple distinct symbols remained where a unique result was required.
    Ambiguous {
        /// Total number of ambiguous candidates.
        total: u64,
        /// Deterministically ordered, bounded diagnostic candidates.
        candidates: Vec<AmbiguityCandidate>,
    },
    /// Extent expansion produced an invalid selector.
    InvalidExtent,
}

impl fmt::Display for SymbolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FlatSymbolsUnsupported => {
                formatter.write_str("language server returned unsupported flat symbols")
            }
            Self::MalformedDocumentSymbols => {
                formatter.write_str("language server returned malformed document symbols")
            }
            Self::ResourceLimitExceeded { resource, maximum } => {
                write!(formatter, "{resource} exceeds configured maximum {maximum}")
            }
            Self::Position(error) => write!(formatter, "invalid language-server range: {error}"),
            Self::SelectionRangeNotContained => formatter
                .write_str("language-server selection range is outside its enclosing range"),
            Self::QueryPositionOutOfBounds => {
                formatter.write_str("selection position is outside the immutable snapshot")
            }
            Self::NotFound => formatter.write_str("no document symbol matched the query"),
            Self::Ambiguous { total, .. } => {
                write!(formatter, "document-symbol query has {total} matches")
            }
            Self::InvalidExtent => formatter.write_str("declaration extent is invalid"),
        }
    }
}

impl std::error::Error for SymbolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Position(error) => Some(error),
            Self::FlatSymbolsUnsupported
            | Self::MalformedDocumentSymbols
            | Self::ResourceLimitExceeded { .. }
            | Self::SelectionRangeNotContained
            | Self::QueryPositionOutOfBounds
            | Self::NotFound
            | Self::Ambiguous { .. }
            | Self::InvalidExtent => None,
        }
    }
}

impl From<PositionError> for SymbolError {
    fn from(error: PositionError) -> Self {
        Self::Position(error)
    }
}

/// Decodes, validates, and iteratively flattens a raw document-symbol result.
///
/// Every enclosing and selection range is converted before any later query
/// filter can hide malformed server output. `null` and the structurally
/// ambiguous empty array both normalize to no symbols. Nonempty arrays are
/// classified by their wire fields before generated LSP types are used, so the
/// generated untagged enum cannot misclassify `[]` as flat symbols.
///
/// # Errors
///
/// Returns [`SymbolError::FlatSymbolsUnsupported`] for legacy flat responses,
/// [`SymbolError::MalformedDocumentSymbols`] for mixed or invalid shapes, a
/// resource error for oversized hierarchies or strings, and a range error for
/// invalid or non-contained positions.
pub fn normalize_document_symbols(
    response: Value,
    converter: &mut PositionConverter<'_>,
    limits: SymbolLimits,
) -> Result<Vec<NormalizedSymbol>, SymbolError> {
    let roots = decode_hierarchical_response(response)?;
    normalize_hierarchical_symbols(roots, converter, limits)
}

/// Validates and iteratively flattens already-decoded hierarchical symbols.
///
/// This entry point is useful when the caller has a structurally unambiguous
/// typed hierarchy. Wire-facing callers should use [`normalize_document_symbols`].
/// Every symbol and selection range is converted before resolution.
///
/// # Errors
///
/// Returns a resource error for oversized hierarchies or strings, and a range
/// error for invalid or non-contained positions.
pub fn normalize_hierarchical_symbols(
    roots: Vec<DocumentSymbol>,
    converter: &mut PositionConverter<'_>,
    limits: SymbolLimits,
) -> Result<Vec<NormalizedSymbol>, SymbolError> {
    let root_count = u64::try_from(roots.len())
        .map_err(|_| resource_error("raw_document_symbols", limits.maximum_raw_symbols))?;
    enforce_limit(
        root_count,
        limits.maximum_raw_symbols,
        "raw_document_symbols",
    )?;
    let mut stack = Vec::with_capacity(roots.len());
    for root in roots.into_iter().rev() {
        stack.push(PendingSymbol {
            symbol: root,
            parent_path: None,
            parent_path_bytes: 0,
            depth: 1,
        });
    }

    let mut raw_count = 0_u64;
    let mut flattened_count = 0_u64;
    let mut candidate_storage_bytes = 0_u64;
    let mut normalized = Vec::new();
    while let Some(pending) = stack.pop() {
        raw_count = raw_count
            .checked_add(1)
            .ok_or_else(|| resource_error("raw_document_symbols", limits.maximum_raw_symbols))?;
        enforce_limit(
            raw_count,
            limits.maximum_raw_symbols,
            "raw_document_symbols",
        )?;
        enforce_limit(pending.depth, limits.maximum_depth, "symbol_nesting_depth")?;

        let DocumentSymbol {
            name,
            detail,
            kind,
            range,
            selection_range,
            children,
            ..
        } = pending.symbol;
        if name.is_empty() || !wire_range_is_valid(range) || !wire_range_is_valid(selection_range) {
            return Err(SymbolError::MalformedDocumentSymbols);
        }
        enforce_byte_length(&name, limits.maximum_name_bytes, "symbol_name_bytes")?;
        if let Some(detail) = detail.as_deref() {
            enforce_byte_length(detail, limits.maximum_detail_bytes, "symbol_detail_bytes")?;
        }
        let name_bytes = u64::try_from(name.len())
            .map_err(|_| resource_error("symbol_path_bytes", limits.maximum_path_bytes))?;
        let path_bytes = pending
            .parent_path_bytes
            .checked_add(name_bytes)
            .ok_or_else(|| resource_error("symbol_path_bytes", limits.maximum_path_bytes))?;
        enforce_limit(path_bytes, limits.maximum_path_bytes, "symbol_path_bytes")?;

        let byte_range = converter.lsp_range_to_byte_range(range)?;
        let selection_byte_range = converter.lsp_range_to_byte_range(selection_range)?;
        if !range_contains(byte_range, selection_byte_range) {
            return Err(SymbolError::SelectionRangeNotContained);
        }

        let path_node = Arc::new(PathNode {
            name: name.clone(),
            parent: pending.parent_path,
        });
        let detail_bytes = detail.as_ref().map_or(0, String::len);
        let owned_bytes = name
            .len()
            .checked_add(name.len())
            .and_then(|value| value.checked_add(detail_bytes))
            .and_then(|value| value.checked_add(usize::try_from(path_bytes).ok()?))
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| {
                resource_error(
                    "symbol_candidate_storage_bytes",
                    limits.maximum_candidate_storage_bytes,
                )
            })?;
        candidate_storage_bytes = candidate_storage_bytes
            .checked_add(owned_bytes)
            .ok_or_else(|| {
                resource_error(
                    "symbol_candidate_storage_bytes",
                    limits.maximum_candidate_storage_bytes,
                )
            })?;
        enforce_limit(
            candidate_storage_bytes,
            limits.maximum_candidate_storage_bytes,
            "symbol_candidate_storage_bytes",
        )?;
        let symbol_path = materialize_path(&path_node, pending.depth)?;

        if let Some(children) = children {
            let pending_count = u64::try_from(stack.len())
                .ok()
                .and_then(|pending| pending.checked_add(raw_count))
                .and_then(|pending| {
                    u64::try_from(children.len())
                        .ok()
                        .and_then(|children| pending.checked_add(children))
                })
                .ok_or_else(|| {
                    resource_error("raw_document_symbols", limits.maximum_raw_symbols)
                })?;
            enforce_limit(
                pending_count,
                limits.maximum_raw_symbols,
                "raw_document_symbols",
            )?;
            let child_depth = pending
                .depth
                .checked_add(1)
                .ok_or_else(|| resource_error("symbol_nesting_depth", limits.maximum_depth))?;
            for child in children.into_iter().rev() {
                stack.push(PendingSymbol {
                    symbol: child,
                    parent_path: Some(Arc::clone(&path_node)),
                    parent_path_bytes: path_bytes,
                    depth: child_depth,
                });
            }
        }

        flattened_count = flattened_count.checked_add(1).ok_or_else(|| {
            resource_error(
                "flattened_document_symbols",
                limits.maximum_flattened_symbols,
            )
        })?;
        enforce_limit(
            flattened_count,
            limits.maximum_flattened_symbols,
            "flattened_document_symbols",
        )?;
        normalized.push(NormalizedSymbol {
            name,
            detail,
            kind: normalize_kind(kind),
            symbol_path,
            lsp_range: range,
            lsp_selection_range: selection_range,
            byte_range,
            selection_byte_range,
        });
    }

    Ok(normalized)
}

fn wire_range_is_valid(range: Range) -> bool {
    [range.start, range.end].into_iter().all(|position| {
        position.line <= MAXIMUM_LSP_UINTEGER && position.character <= MAXIMUM_LSP_UINTEGER
    })
}

/// Resolves an exact, case-sensitive name query.
///
/// Duplicate records are removed by `(range, kind, symbol_path, name)` before
/// unique-or-all behavior is applied. Results use deterministic candidate order.
///
/// # Errors
///
/// Returns [`SymbolError::NotFound`] or [`SymbolError::Ambiguous`] in unique
/// mode, or [`SymbolError::InvalidExtent`] if line expansion is invalid.
pub fn resolve_name(
    symbols: &[NormalizedSymbol],
    snapshot: &str,
    name: &str,
    kind: Option<KnownSymbolKind>,
    extent: SelectionExtent,
    mode: MatchMode,
    limits: SymbolLimits,
) -> Result<Vec<SymbolMatch>, SymbolError> {
    let candidates = prepare_candidates(
        symbols
            .iter()
            .filter(|symbol| symbol.name == name && kind_matches(symbol.kind, kind)),
    );
    finish_resolution(candidates, snapshot, extent, mode, limits)
}

/// Resolves symbols containing a validated snapshot byte insertion offset.
///
/// Unique mode retains only the shortest containing range and reports equally
/// short distinct symbols as ambiguous. All mode returns every containing
/// symbol in the frozen start/end/kind/path/name candidate order.
///
/// # Errors
///
/// Returns [`SymbolError::QueryPositionOutOfBounds`] for an offset past EOF,
/// [`SymbolError::NotFound`] or [`SymbolError::Ambiguous`] in unique mode, or
/// [`SymbolError::InvalidExtent`] if line expansion is invalid.
pub fn resolve_position(
    symbols: &[NormalizedSymbol],
    snapshot: &str,
    byte_offset: u64,
    kind: Option<KnownSymbolKind>,
    extent: SelectionExtent,
    mode: MatchMode,
    limits: SymbolLimits,
) -> Result<Vec<SymbolMatch>, SymbolError> {
    let file_length =
        u64::try_from(snapshot.len()).map_err(|_| SymbolError::QueryPositionOutOfBounds)?;
    let byte_offset_usize =
        usize::try_from(byte_offset).map_err(|_| SymbolError::QueryPositionOutOfBounds)?;
    if byte_offset > file_length || !snapshot.is_char_boundary(byte_offset_usize) {
        return Err(SymbolError::QueryPositionOutOfBounds);
    }

    let mut candidates = prepare_candidates(symbols.iter().filter(|symbol| {
        kind_matches(symbol.kind, kind)
            && contains_position(symbol.byte_range, byte_offset, file_length)
    }));
    if mode == MatchMode::Unique && !candidates.is_empty() {
        candidates.sort_by(|left, right| {
            range_length(left.byte_range)
                .cmp(&range_length(right.byte_range))
                .then_with(|| compare_candidates(left, right))
        });
        let shortest = range_length(candidates[0].byte_range);
        candidates.truncate(
            candidates.partition_point(|candidate| range_length(candidate.byte_range) == shortest),
        );
    }
    finish_resolution(candidates, snapshot, extent, mode, limits)
}

/// Applies the selected declaration extent to a validated nonempty byte range.
///
/// `DeclarationLines` expands through spaces and tabs only. It preserves LF,
/// CRLF, lone CR, and an absent final terminator byte-for-byte.
///
/// # Errors
///
/// Returns [`SymbolError::InvalidExtent`] when the input or result is empty,
/// reversed, outside the snapshot, or not on UTF-8 boundaries.
pub fn apply_extent(
    snapshot: &str,
    range: ByteRange,
    extent: SelectionExtent,
) -> Result<ByteRange, SymbolError> {
    let length = u64::try_from(snapshot.len()).map_err(|_| SymbolError::InvalidExtent)?;
    if range.start >= range.end || range.end > length {
        return Err(SymbolError::InvalidExtent);
    }
    let start = usize::try_from(range.start).map_err(|_| SymbolError::InvalidExtent)?;
    let end = usize::try_from(range.end).map_err(|_| SymbolError::InvalidExtent)?;
    if !snapshot.is_char_boundary(start) || !snapshot.is_char_boundary(end) {
        return Err(SymbolError::InvalidExtent);
    }
    if extent == SelectionExtent::Symbol {
        return Ok(range);
    }

    let bytes = snapshot.as_bytes();
    let physical_line_start = bytes[..start]
        .iter()
        .rposition(|byte| matches!(byte, b'\r' | b'\n'))
        .map_or(0, |terminator| terminator + 1);
    let expanded_start = if bytes[physical_line_start..start]
        .iter()
        .all(|byte| matches!(byte, b' ' | b'\t'))
    {
        physical_line_start
    } else {
        start
    };

    let expanded_end = if end > 0 && matches!(bytes[end - 1], b'\r' | b'\n') {
        end
    } else {
        declaration_line_end(bytes, end)
    };
    let result = ByteRange {
        start: u64::try_from(expanded_start).map_err(|_| SymbolError::InvalidExtent)?,
        end: u64::try_from(expanded_end).map_err(|_| SymbolError::InvalidExtent)?,
    };
    if result.start >= result.end || result.end > length {
        return Err(SymbolError::InvalidExtent);
    }
    Ok(result)
}

struct PendingSymbol {
    symbol: DocumentSymbol,
    parent_path: Option<Arc<PathNode>>,
    parent_path_bytes: u64,
    depth: u64,
}

struct PathNode {
    name: String,
    parent: Option<Arc<Self>>,
}

fn decode_hierarchical_response(response: Value) -> Result<Vec<DocumentSymbol>, SymbolError> {
    let Value::Array(items) = response else {
        return if response.is_null() {
            Ok(Vec::new())
        } else {
            Err(SymbolError::MalformedDocumentSymbols)
        };
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let hierarchical = items
        .iter()
        .all(|item| item.get("range").is_some() && item.get("selectionRange").is_some());
    let flat = items.iter().all(|item| item.get("location").is_some());
    if flat {
        return Err(SymbolError::FlatSymbolsUnsupported);
    }
    if !hierarchical {
        return Err(SymbolError::MalformedDocumentSymbols);
    }

    serde_json::from_value(Value::Array(items)).map_err(|_| SymbolError::MalformedDocumentSymbols)
}

fn materialize_path(node: &Arc<PathNode>, depth: u64) -> Result<Vec<String>, SymbolError> {
    let capacity =
        usize::try_from(depth).map_err(|_| resource_error("symbol_nesting_depth", depth))?;
    let mut path = Vec::with_capacity(capacity);
    let mut current = Some(node);
    while let Some(node) = current {
        path.push(node.name.clone());
        current = node.parent.as_ref();
    }
    path.reverse();
    Ok(path)
}

fn prepare_candidates<'a>(
    symbols: impl Iterator<Item = &'a NormalizedSymbol>,
) -> Vec<&'a NormalizedSymbol> {
    let mut candidates: Vec<_> = symbols.collect();
    candidates.sort_by(|left, right| {
        compare_candidates(left, right)
            .then_with(|| compare_ranges(left.selection_byte_range, right.selection_byte_range))
            .then_with(|| left.detail.cmp(&right.detail))
    });
    candidates.dedup_by(|left, right| duplicate_key_equal(left, right));
    candidates
}

/// Orders every normalized symbol with the frozen candidate comparator and
/// coalesces exact duplicates.
///
/// This is the same treatment all-match resolution applies, extracted so the
/// outline surface shares one deterministic order and dedup key. Results are
/// ordered by enclosing-range start/end bytes, kind spelling then numeric
/// value, symbol path, name, reveal-range start/end, then detail; duplicates
/// coalesce by `(lsp_range, kind, symbol_path, name)`.
#[must_use]
pub fn order_unique_candidates(symbols: &[NormalizedSymbol]) -> Vec<&NormalizedSymbol> {
    prepare_candidates(symbols.iter())
}

fn finish_resolution(
    candidates: Vec<&NormalizedSymbol>,
    snapshot: &str,
    extent: SelectionExtent,
    mode: MatchMode,
    limits: SymbolLimits,
) -> Result<Vec<SymbolMatch>, SymbolError> {
    if mode == MatchMode::All && candidates.len() > limits.maximum_matches {
        return Err(resource_error(
            "selection_matches",
            u64::try_from(limits.maximum_matches).unwrap_or(u64::MAX),
        ));
    }
    if mode == MatchMode::Unique {
        if candidates.is_empty() {
            return Err(SymbolError::NotFound);
        }
        if candidates.len() > 1 {
            let total = u64::try_from(candidates.len()).unwrap_or(u64::MAX);
            let diagnostic_candidates = candidates
                .iter()
                .take(limits.maximum_ambiguity_candidates)
                .map(|symbol| AmbiguityCandidate {
                    name: symbol.name.clone(),
                    kind: symbol.kind,
                    symbol_path: symbol.symbol_path.clone(),
                    byte_range: symbol.byte_range,
                })
                .collect();
            return Err(SymbolError::Ambiguous {
                total,
                candidates: diagnostic_candidates,
            });
        }
    }

    candidates
        .into_iter()
        .map(|symbol| {
            Ok(SymbolMatch {
                name: symbol.name.clone(),
                detail: symbol.detail.clone(),
                kind: symbol.kind,
                symbol_path: symbol.symbol_path.clone(),
                lsp_range: symbol.lsp_range,
                lsp_selection_range: symbol.lsp_selection_range,
                symbol_range: symbol.byte_range,
                selected_range: apply_extent(snapshot, symbol.byte_range, extent)?,
                extent,
            })
        })
        .collect()
}

fn declaration_line_end(bytes: &[u8], end: usize) -> usize {
    let mut cursor = end;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b' ' | b'\t' => cursor += 1,
            b'\r' => {
                return if bytes.get(cursor + 1) == Some(&b'\n') {
                    cursor + 2
                } else {
                    cursor + 1
                };
            }
            b'\n' => return cursor + 1,
            _ => return end,
        }
    }
    cursor
}

fn compare_candidates(left: &NormalizedSymbol, right: &NormalizedSymbol) -> Ordering {
    compare_ranges(left.byte_range, right.byte_range)
        .then_with(|| compare_kinds(left.kind, right.kind))
        .then_with(|| left.symbol_path.cmp(&right.symbol_path))
        .then_with(|| left.name.cmp(&right.name))
}

fn compare_ranges(left: ByteRange, right: ByteRange) -> Ordering {
    left.start
        .cmp(&right.start)
        .then_with(|| left.end.cmp(&right.end))
}

fn compare_kinds(left: NormalizedSymbolKind, right: NormalizedSymbolKind) -> Ordering {
    left.as_str()
        .cmp(right.as_str())
        .then_with(|| left.numeric().cmp(&right.numeric()))
}

fn duplicate_key_equal(left: &NormalizedSymbol, right: &NormalizedSymbol) -> bool {
    left.lsp_range == right.lsp_range
        && left.kind == right.kind
        && left.symbol_path == right.symbol_path
        && left.name == right.name
}

fn kind_matches(actual: NormalizedSymbolKind, requested: Option<KnownSymbolKind>) -> bool {
    requested.is_none_or(|requested| actual == NormalizedSymbolKind::Known(requested))
}

fn contains_position(range: ByteRange, position: u64, file_length: u64) -> bool {
    (range.start <= position && position < range.end)
        || (position == file_length && range.end == file_length && range.start < range.end)
}

fn range_contains(outer: ByteRange, inner: ByteRange) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

fn range_length(range: ByteRange) -> u64 {
    range.end - range.start
}

fn enforce_byte_length(
    value: &str,
    maximum: u64,
    resource: &'static str,
) -> Result<(), SymbolError> {
    let length = u64::try_from(value.len()).map_err(|_| resource_error(resource, maximum))?;
    enforce_limit(length, maximum, resource)
}

fn enforce_limit(actual: u64, maximum: u64, resource: &'static str) -> Result<(), SymbolError> {
    if actual > maximum {
        return Err(resource_error(resource, maximum));
    }
    Ok(())
}

fn resource_error(resource: &'static str, maximum: u64) -> SymbolError {
    SymbolError::ResourceLimitExceeded { resource, maximum }
}

fn normalize_kind(kind: SymbolKind) -> NormalizedSymbolKind {
    match kind {
        SymbolKind::File => NormalizedSymbolKind::Known(KnownSymbolKind::File),
        SymbolKind::Module => NormalizedSymbolKind::Known(KnownSymbolKind::Module),
        SymbolKind::Namespace => NormalizedSymbolKind::Known(KnownSymbolKind::Namespace),
        SymbolKind::Package => NormalizedSymbolKind::Known(KnownSymbolKind::Package),
        SymbolKind::Class => NormalizedSymbolKind::Known(KnownSymbolKind::Class),
        SymbolKind::Method => NormalizedSymbolKind::Known(KnownSymbolKind::Method),
        SymbolKind::Property => NormalizedSymbolKind::Known(KnownSymbolKind::Property),
        SymbolKind::Field => NormalizedSymbolKind::Known(KnownSymbolKind::Field),
        SymbolKind::Constructor => NormalizedSymbolKind::Known(KnownSymbolKind::Constructor),
        SymbolKind::Enum => NormalizedSymbolKind::Known(KnownSymbolKind::Enum),
        SymbolKind::Interface => NormalizedSymbolKind::Known(KnownSymbolKind::Interface),
        SymbolKind::Function => NormalizedSymbolKind::Known(KnownSymbolKind::Function),
        SymbolKind::Variable => NormalizedSymbolKind::Known(KnownSymbolKind::Variable),
        SymbolKind::Constant => NormalizedSymbolKind::Known(KnownSymbolKind::Constant),
        SymbolKind::String => NormalizedSymbolKind::Known(KnownSymbolKind::String),
        SymbolKind::Number => NormalizedSymbolKind::Known(KnownSymbolKind::Number),
        SymbolKind::Boolean => NormalizedSymbolKind::Known(KnownSymbolKind::Boolean),
        SymbolKind::Array => NormalizedSymbolKind::Known(KnownSymbolKind::Array),
        SymbolKind::Object => NormalizedSymbolKind::Known(KnownSymbolKind::Object),
        SymbolKind::Key => NormalizedSymbolKind::Known(KnownSymbolKind::Key),
        SymbolKind::Null => NormalizedSymbolKind::Known(KnownSymbolKind::Null),
        SymbolKind::EnumMember => NormalizedSymbolKind::Known(KnownSymbolKind::EnumMember),
        SymbolKind::Struct => NormalizedSymbolKind::Known(KnownSymbolKind::Struct),
        SymbolKind::Event => NormalizedSymbolKind::Known(KnownSymbolKind::Event),
        SymbolKind::Operator => NormalizedSymbolKind::Known(KnownSymbolKind::Operator),
        SymbolKind::TypeParameter => NormalizedSymbolKind::Known(KnownSymbolKind::TypeParameter),
        SymbolKind::Custom(value) => NormalizedSymbolKind::Unknown(value),
    }
}

const fn known_kind_number(kind: KnownSymbolKind) -> u32 {
    match kind {
        KnownSymbolKind::File => 1,
        KnownSymbolKind::Module => 2,
        KnownSymbolKind::Namespace => 3,
        KnownSymbolKind::Package => 4,
        KnownSymbolKind::Class => 5,
        KnownSymbolKind::Method => 6,
        KnownSymbolKind::Property => 7,
        KnownSymbolKind::Field => 8,
        KnownSymbolKind::Constructor => 9,
        KnownSymbolKind::Enum => 10,
        KnownSymbolKind::Interface => 11,
        KnownSymbolKind::Function => 12,
        KnownSymbolKind::Variable => 13,
        KnownSymbolKind::Constant => 14,
        KnownSymbolKind::String => 15,
        KnownSymbolKind::Number => 16,
        KnownSymbolKind::Boolean => 17,
        KnownSymbolKind::Array => 18,
        KnownSymbolKind::Object => 19,
        KnownSymbolKind::Key => 20,
        KnownSymbolKind::Null => 21,
        KnownSymbolKind::EnumMember => 22,
        KnownSymbolKind::Struct => 23,
        KnownSymbolKind::Event => 24,
        KnownSymbolKind::Operator => 25,
        KnownSymbolKind::TypeParameter => 26,
    }
}
