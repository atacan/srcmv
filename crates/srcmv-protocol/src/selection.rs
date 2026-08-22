use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use srcmv_core::Sha256Digest;

use crate::{MAX_RESPONSE_BYTES, WarningDto};

/// The only semantic-selection protocol version supported by this release.
pub const SELECTION_PROTOCOL_VERSION: u64 = 1;

/// A byte selector ready for insertion into an edit protocol-v1 source.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionByteSelectorDto {
    kind: &'static str,
    start: u64,
    end: u64,
}

impl SelectionByteSelectorDto {
    /// Creates a half-open byte selector from already validated coordinates.
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Self {
        Self {
            kind: "bytes",
            start,
            end,
        }
    }

    /// Returns the inclusive start byte offset.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end byte offset.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }
}

/// One raw zero-based LSP position retained for audit output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionLspPositionDto {
    line: u32,
    character: u32,
}

impl SelectionLspPositionDto {
    /// Creates a raw LSP position from server-provided coordinates.
    #[must_use]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    /// Returns the raw zero-based line.
    #[must_use]
    pub const fn line(self) -> u32 {
        self.line
    }

    /// Returns the raw zero-based character offset.
    #[must_use]
    pub const fn character(self) -> u32 {
        self.character
    }
}

/// One raw zero-based LSP range retained for audit output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionLspRangeDto {
    start: SelectionLspPositionDto,
    end: SelectionLspPositionDto,
}

impl SelectionLspRangeDto {
    /// Creates an LSP range from its raw endpoints.
    #[must_use]
    pub const fn new(start: SelectionLspPositionDto, end: SelectionLspPositionDto) -> Self {
        Self { start, end }
    }

    /// Returns the raw range start.
    #[must_use]
    pub const fn start(self) -> SelectionLspPositionDto {
        self.start
    }

    /// Returns the raw range end.
    #[must_use]
    pub const fn end(self) -> SelectionLspPositionDto {
        self.end
    }
}

/// A standardized symbol kind accepted as a selection query filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionKnownSymbolKindDto {
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

impl SelectionKnownSymbolKindDto {
    /// Every standardized symbol kind representable by selection protocol v1.
    pub const ALL: [Self; 26] = [
        Self::File,
        Self::Module,
        Self::Namespace,
        Self::Package,
        Self::Class,
        Self::Method,
        Self::Property,
        Self::Field,
        Self::Constructor,
        Self::Enum,
        Self::Interface,
        Self::Function,
        Self::Variable,
        Self::Constant,
        Self::String,
        Self::Number,
        Self::Boolean,
        Self::Array,
        Self::Object,
        Self::Key,
        Self::Null,
        Self::EnumMember,
        Self::Struct,
        Self::Event,
        Self::Operator,
        Self::TypeParameter,
    ];

    /// Returns the frozen lowercase wire spelling.
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

impl Serialize for SelectionKnownSymbolKindDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// A standardized or unknown symbol kind in a selection result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionSymbolKindDto {
    /// A symbol kind standardized by LSP.
    Known(SelectionKnownSymbolKindDto),
    /// A future or vendor-defined symbol kind.
    Unknown,
}

impl SelectionSymbolKindDto {
    /// Returns the frozen lowercase wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Known(kind) => kind.as_str(),
            Self::Unknown => "unknown",
        }
    }
}

impl From<SelectionKnownSymbolKindDto> for SelectionSymbolKindDto {
    fn from(kind: SelectionKnownSymbolKindDto) -> Self {
        Self::Known(kind)
    }
}

impl Serialize for SelectionSymbolKindDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// The normalized query recorded in a selection response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionQueryDto {
    /// An exact symbol-name query.
    Name {
        /// Exact unqualified symbol name.
        name: String,
        /// Optional standardized symbol-kind filter.
        symbol_kind: Option<SelectionKnownSymbolKindDto>,
    },
    /// A query for the smallest symbol containing one snapshot byte.
    Position {
        /// Validated zero-based snapshot byte offset.
        byte_offset: u64,
        /// Optional standardized symbol-kind filter.
        symbol_kind: Option<SelectionKnownSymbolKindDto>,
    },
}

impl SelectionQueryDto {
    /// Creates an exact name query.
    #[must_use]
    pub fn name(name: impl Into<String>, symbol_kind: Option<SelectionKnownSymbolKindDto>) -> Self {
        Self::Name {
            name: name.into(),
            symbol_kind,
        }
    }

    /// Creates a normalized position query.
    #[must_use]
    pub const fn position(
        byte_offset: u64,
        symbol_kind: Option<SelectionKnownSymbolKindDto>,
    ) -> Self {
        Self::Position {
            byte_offset,
            symbol_kind,
        }
    }
}

/// The immutable source snapshot described by a selection response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionSourceDto {
    path: String,
    sha256: String,
    byte_length: u64,
}

impl SelectionSourceDto {
    /// Creates source metadata from validated workspace-relative data.
    #[must_use]
    pub fn new(path: impl Into<String>, sha256: Sha256Digest, byte_length: u64) -> Self {
        Self {
            path: path.into(),
            sha256: sha256.to_prefixed_hex(),
            byte_length,
        }
    }
}

/// The negotiated LSP position encoding recorded in a response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionPositionEncodingDto {
    /// UTF-8 code units.
    Utf8,
    /// UTF-16 code units.
    Utf16,
    /// UTF-32 code units.
    Utf32,
}

impl SelectionPositionEncodingDto {
    /// Returns the LSP wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }
}

impl Serialize for SelectionPositionEncodingDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Non-sensitive server metadata recorded in a selection response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionServerDto {
    configuration_id: Option<String>,
    reported_name: Option<String>,
    reported_version: Option<String>,
    position_encoding: SelectionPositionEncodingDto,
}

impl SelectionServerDto {
    /// Creates server metadata without retaining invocation or configuration details.
    #[must_use]
    pub fn new(
        configuration_id: Option<String>,
        reported_name: Option<String>,
        reported_version: Option<String>,
        position_encoding: SelectionPositionEncodingDto,
    ) -> Self {
        Self {
            configuration_id,
            reported_name,
            reported_version,
            position_encoding,
        }
    }
}

/// The source SHA-256 precondition embedded in a copy-ready source fragment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SelectionSourcePreconditionDto {
    kind: &'static str,
    value: String,
}

/// A copy-ready edit protocol-v1 source fragment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionRequestSourceDto {
    path: String,
    selector: SelectionByteSelectorDto,
    precondition: SelectionSourcePreconditionDto,
}

impl SelectionRequestSourceDto {
    /// Creates an edit source with an exact snapshot digest precondition.
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        selector: SelectionByteSelectorDto,
        source_sha256: Sha256Digest,
    ) -> Self {
        Self {
            path: path.into(),
            selector,
            precondition: SelectionSourcePreconditionDto {
                kind: "sha256",
                value: source_sha256.to_prefixed_hex(),
            },
        }
    }
}

/// The extent applied to an authoritative result byte selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionExtentDto {
    /// Use the raw enclosing symbol range.
    Symbol,
    /// Include whitespace-only declaration line edges.
    DeclarationLines,
}

impl SelectionExtentDto {
    /// Returns the stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::DeclarationLines => "declaration_lines",
        }
    }
}

impl Serialize for SelectionExtentDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// One deterministic semantic-selection match.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionMatchDto {
    name: String,
    symbol_kind: SelectionSymbolKindDto,
    symbol_path: Vec<String>,
    detail: Option<String>,
    lsp_range: SelectionLspRangeDto,
    lsp_selection_range: SelectionLspRangeDto,
    extent: SelectionExtentDto,
    selector: SelectionByteSelectorDto,
    selected_payload_sha256: String,
    selected_byte_length: u64,
    request_source: SelectionRequestSourceDto,
}

impl SelectionMatchDto {
    /// Creates one match from already validated LSP and byte ranges.
    ///
    /// The selected byte length is derived from the selector. Callers must pass
    /// the same source path and snapshot digest as the response source.
    #[must_use]
    #[expect(clippy::too_many_arguments, reason = "wire record fields are explicit")]
    pub fn new(
        name: impl Into<String>,
        symbol_kind: SelectionSymbolKindDto,
        symbol_path: Vec<String>,
        detail: Option<String>,
        lsp_range: SelectionLspRangeDto,
        lsp_selection_range: SelectionLspRangeDto,
        extent: SelectionExtentDto,
        selector: SelectionByteSelectorDto,
        selected_payload_sha256: Sha256Digest,
        source_path: impl Into<String>,
        source_sha256: Sha256Digest,
    ) -> Self {
        let selected_byte_length = selector.end().saturating_sub(selector.start());
        Self {
            name: name.into(),
            symbol_kind,
            symbol_path,
            detail,
            lsp_range,
            lsp_selection_range,
            extent,
            selector,
            selected_payload_sha256: selected_payload_sha256.to_prefixed_hex(),
            selected_byte_length,
            request_source: SelectionRequestSourceDto::new(source_path, selector, source_sha256),
        }
    }
}

/// A successful semantic-selection protocol-v1 response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionResponse {
    selection_protocol_version: u64,
    workspace_identity_hash: String,
    source: SelectionSourceDto,
    query: SelectionQueryDto,
    server: SelectionServerDto,
    matches: Vec<SelectionMatchDto>,
    warnings: Vec<WarningDto>,
}

impl SelectionResponse {
    /// Creates a semantic-selection protocol-v1 response.
    #[must_use]
    pub fn new(
        workspace_identity_hash: Sha256Digest,
        source: SelectionSourceDto,
        query: SelectionQueryDto,
        server: SelectionServerDto,
        matches: Vec<SelectionMatchDto>,
        warnings: Vec<WarningDto>,
    ) -> Self {
        Self {
            selection_protocol_version: SELECTION_PROTOCOL_VERSION,
            workspace_identity_hash: workspace_identity_hash.to_prefixed_hex(),
            source,
            query,
            server,
            matches,
            warnings,
        }
    }
}

/// Stable semantic-selection error categories and their process exit status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionErrorCategory {
    /// Invalid semantic-selection input.
    Request,
    /// A valid query conflicts with the observed symbol set.
    Conflict,
    /// The configured server or platform cannot satisfy the query.
    Support,
    /// An implementation invariant failed.
    Internal,
}

impl SelectionErrorCategory {
    /// Returns the documented command-line exit status for this category.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Request => 2,
            Self::Conflict => 3,
            Self::Support => 4,
            Self::Internal => 8,
        }
    }

    /// Returns the stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Conflict => "conflict",
            Self::Support => "support",
            Self::Internal => "internal",
        }
    }
}

impl Serialize for SelectionErrorCategory {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Complete semantic-selection protocol-v1 error-code registry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SelectionErrorCode {
    /// The selection query is invalid.
    InvalidSelectionQuery,
    /// No language server is configured for the source.
    LspServerNotConfigured,
    /// No symbol matched a unique query.
    SelectionNotFound,
    /// Multiple distinct symbols remained for a unique query.
    SelectionAmbiguous,
    /// The source snapshot is not valid UTF-8.
    UnsupportedTextEncoding,
    /// The server lacks a required semantic-selection capability.
    LspCapabilityUnavailable,
    /// The server returned unsupported flat document symbols.
    LspFlatSymbolsUnsupported,
    /// The server cannot synchronize an opened snapshot.
    LspDocumentSyncUnavailable,
    /// A language-server or selection resource bound was exceeded.
    LspResourceLimitExceeded,
    /// A language-server lifecycle deadline expired.
    LspTimeout,
    /// The configured language server could not be started.
    LspStartFailed,
    /// The language server exited before completing the session.
    LspExited,
    /// The server violated JSON-RPC, LSP, or range semantics.
    LspProtocolError,
    /// The server returned a JSON-RPC request error.
    LspRequestFailed,
    /// An internal semantic-selection invariant failed.
    SelectionInternalError,
}

impl SelectionErrorCode {
    /// Every error code registered by semantic-selection protocol version 1.
    pub const ALL: [Self; 15] = [
        Self::InvalidSelectionQuery,
        Self::LspServerNotConfigured,
        Self::SelectionNotFound,
        Self::SelectionAmbiguous,
        Self::UnsupportedTextEncoding,
        Self::LspCapabilityUnavailable,
        Self::LspFlatSymbolsUnsupported,
        Self::LspDocumentSyncUnavailable,
        Self::LspResourceLimitExceeded,
        Self::LspTimeout,
        Self::LspStartFailed,
        Self::LspExited,
        Self::LspProtocolError,
        Self::LspRequestFailed,
        Self::SelectionInternalError,
    ];

    /// Returns the stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidSelectionQuery => "INVALID_SELECTION_QUERY",
            Self::LspServerNotConfigured => "LSP_SERVER_NOT_CONFIGURED",
            Self::SelectionNotFound => "SELECTION_NOT_FOUND",
            Self::SelectionAmbiguous => "SELECTION_AMBIGUOUS",
            Self::UnsupportedTextEncoding => "UNSUPPORTED_TEXT_ENCODING",
            Self::LspCapabilityUnavailable => "LSP_CAPABILITY_UNAVAILABLE",
            Self::LspFlatSymbolsUnsupported => "LSP_FLAT_SYMBOLS_UNSUPPORTED",
            Self::LspDocumentSyncUnavailable => "LSP_DOCUMENT_SYNC_UNAVAILABLE",
            Self::LspResourceLimitExceeded => "LSP_RESOURCE_LIMIT_EXCEEDED",
            Self::LspTimeout => "LSP_TIMEOUT",
            Self::LspStartFailed => "LSP_START_FAILED",
            Self::LspExited => "LSP_EXITED",
            Self::LspProtocolError => "LSP_PROTOCOL_ERROR",
            Self::LspRequestFailed => "LSP_REQUEST_FAILED",
            Self::SelectionInternalError => "SELECTION_INTERNAL_ERROR",
        }
    }

    /// Returns the documented category for this code.
    #[must_use]
    pub const fn category(self) -> SelectionErrorCategory {
        match self {
            Self::InvalidSelectionQuery => SelectionErrorCategory::Request,
            Self::SelectionNotFound | Self::SelectionAmbiguous => SelectionErrorCategory::Conflict,
            Self::LspServerNotConfigured
            | Self::UnsupportedTextEncoding
            | Self::LspCapabilityUnavailable
            | Self::LspFlatSymbolsUnsupported
            | Self::LspDocumentSyncUnavailable
            | Self::LspResourceLimitExceeded
            | Self::LspTimeout
            | Self::LspStartFailed
            | Self::LspExited
            | Self::LspProtocolError
            | Self::LspRequestFailed => SelectionErrorCategory::Support,
            Self::SelectionInternalError => SelectionErrorCategory::Internal,
        }
    }

    /// Returns whether retrying after an external-state refresh may resolve the error.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::LspTimeout | Self::LspStartFailed | Self::LspExited | Self::LspRequestFailed
        )
    }
}

impl Serialize for SelectionErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// A stable structured semantic-selection protocol-v1 error response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionErrorDto {
    selection_protocol_version: u64,
    code: SelectionErrorCode,
    category: SelectionErrorCategory,
    retryable: bool,
    message: String,
    context: BTreeMap<String, Value>,
}

impl SelectionErrorDto {
    /// Creates an error response using registry-owned category and retry policy.
    #[must_use]
    pub fn new(
        code: SelectionErrorCode,
        message: impl Into<String>,
        context: BTreeMap<String, Value>,
    ) -> Self {
        Self {
            selection_protocol_version: SELECTION_PROTOCOL_VERSION,
            code,
            category: code.category(),
            retryable: code.retryable(),
            message: message.into(),
            context,
        }
    }

    /// Returns the stable selection error code.
    #[must_use]
    pub const fn code(&self) -> SelectionErrorCode {
        self.code
    }

    /// Returns the process exit status associated with this error.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        self.category.exit_code()
    }

    /// Returns whether this failure may be transient.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// Returns the human-readable message before terminal escaping.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns structured non-sensitive error context.
    #[must_use]
    pub fn context(&self) -> &BTreeMap<String, Value> {
        &self.context
    }
}

/// A typed selection response serialization or size failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionProtocolError {
    report: SelectionErrorDto,
}

impl SelectionProtocolError {
    /// Returns the stable selection error response.
    #[must_use]
    pub const fn report(&self) -> &SelectionErrorDto {
        &self.report
    }

    /// Consumes the failure and returns its stable selection error response.
    #[must_use]
    pub fn into_report(self) -> SelectionErrorDto {
        self.report
    }
}

impl fmt::Display for SelectionProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}",
            self.report.code.as_str(),
            self.report.message
        )
    }
}

impl Error for SelectionProtocolError {}

/// Serializes one selection JSON value followed by exactly one LF and checks
/// the frozen 16 MiB response limit before the caller writes any bytes.
///
/// # Errors
///
/// Returns `SELECTION_INTERNAL_ERROR` if JSON serialization fails, or
/// `LSP_RESOURCE_LIMIT_EXCEEDED` if the exact serialized line is too large.
pub fn to_selection_json_line<T: Serialize>(value: &T) -> Result<String, SelectionProtocolError> {
    match write_bounded_json_line(value, MAX_RESPONSE_BYTES) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| selection_serialization_error()),
        Err(BoundedJsonLineFailure::ResponseLimit(actual)) => {
            Err(selection_response_limit_error(actual))
        }
        Err(BoundedJsonLineFailure::Serialization) => Err(selection_serialization_error()),
    }
}

/// Why a bounded single-line JSON serialization stopped early.
#[derive(Debug)]
pub(crate) enum BoundedJsonLineFailure {
    /// The value could not be serialized as UTF-8 JSON.
    Serialization,
    /// The exact serialized line would exceed the response limit; carries the
    /// attempted byte count for error context.
    ResponseLimit(u64),
}

/// Serializes one JSON value plus exactly one trailing LF under an exact
/// total-byte bound, shared by every versioned command surface.
pub(crate) fn write_bounded_json_line<T: Serialize>(
    value: &T,
    maximum_response_bytes: u64,
) -> Result<Vec<u8>, BoundedJsonLineFailure> {
    let maximum_payload_bytes = usize::try_from(maximum_response_bytes.saturating_sub(1))
        .map_err(|_| BoundedJsonLineFailure::Serialization)?;
    let mut output = BoundedJsonWriter::new(maximum_payload_bytes);
    if serde_json::to_writer(&mut output, value).is_err() {
        return Err(if output.exceeded {
            BoundedJsonLineFailure::ResponseLimit(output.attempted_bytes)
        } else {
            BoundedJsonLineFailure::Serialization
        });
    }
    output.bytes.push(b'\n');
    Ok(output.bytes)
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    maximum_payload_bytes: usize,
    attempted_bytes: u64,
    exceeded: bool,
}

impl BoundedJsonWriter {
    fn new(maximum_payload_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum_payload_bytes,
            attempted_bytes: 0,
            exceeded: false,
        }
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let attempted_payload = self.bytes.len().checked_add(buffer.len());
        let Some(attempted_payload) = attempted_payload else {
            self.exceeded = true;
            self.attempted_bytes = u64::MAX;
            return Err(io::Error::other("response limit exceeded"));
        };
        if attempted_payload > self.maximum_payload_bytes {
            self.exceeded = true;
            self.attempted_bytes = u64::try_from(attempted_payload)
                .unwrap_or(u64::MAX)
                .saturating_add(1);
            return Err(io::Error::other("response limit exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn selection_serialization_error() -> SelectionProtocolError {
    SelectionProtocolError {
        report: SelectionErrorDto::new(
            SelectionErrorCode::SelectionInternalError,
            "failed to serialize the semantic-selection response",
            BTreeMap::new(),
        ),
    }
}

fn selection_response_limit_error(actual: u64) -> SelectionProtocolError {
    SelectionProtocolError {
        report: SelectionErrorDto::new(
            SelectionErrorCode::LspResourceLimitExceeded,
            "the semantic-selection response exceeds the serialized response limit",
            selection_context([
                ("actual", json!(actual)),
                ("limit", json!(MAX_RESPONSE_BYTES)),
                ("resource", json!("serialized_json_response")),
            ]),
        ),
    }
}

fn selection_context<const N: usize>(entries: [(&str, Value); N]) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}
