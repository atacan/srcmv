use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use srcmv_core::Sha256Digest;

use crate::selection::{
    BoundedJsonLineFailure, SelectionByteSelectorDto, SelectionLspRangeDto, SelectionServerDto,
    SelectionSourceDto, SelectionSymbolKindDto, write_bounded_json_line,
};
use crate::{MAX_RESPONSE_BYTES, WarningDto};

/// The only read-only outline protocol version supported by this release.
pub const OUTLINE_PROTOCOL_VERSION: u64 = 1;

/// One flattened document symbol in a successful outline response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutlineSymbolDto {
    name: String,
    symbol_kind: SelectionSymbolKindDto,
    symbol_path: Vec<String>,
    depth: u64,
    detail: Option<String>,
    start_line: u64,
    start_column: Option<u64>,
    end_line: u64,
    end_column: Option<u64>,
    lsp_range: SelectionLspRangeDto,
    lsp_selection_range: SelectionLspRangeDto,
    selector: SelectionByteSelectorDto,
}

impl OutlineSymbolDto {
    /// Creates one outline record from already validated coordinates.
    ///
    /// `start_column` and `end_column` are one-based Unicode-scalar columns;
    /// the frozen v1 LSP pipeline always populates both, but a future backend
    /// seam may emit `None` for offsets without a scalar-column position.
    #[must_use]
    #[expect(clippy::too_many_arguments, reason = "wire record fields are explicit")]
    pub fn new(
        name: impl Into<String>,
        symbol_kind: SelectionSymbolKindDto,
        symbol_path: Vec<String>,
        depth: u64,
        detail: Option<String>,
        start_line: u64,
        start_column: Option<u64>,
        end_line: u64,
        end_column: Option<u64>,
        lsp_range: SelectionLspRangeDto,
        lsp_selection_range: SelectionLspRangeDto,
        selector: SelectionByteSelectorDto,
    ) -> Self {
        Self {
            name: name.into(),
            symbol_kind,
            symbol_path,
            depth,
            detail,
            start_line,
            start_column,
            end_line,
            end_column,
            lsp_range,
            lsp_selection_range,
            selector,
        }
    }

    /// Returns the unqualified symbol name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the normalized symbol kind.
    #[must_use]
    pub const fn symbol_kind(&self) -> SelectionSymbolKindDto {
        self.symbol_kind
    }

    /// Returns the derived hierarchy depth (`symbol_path.len() - 1`).
    #[must_use]
    pub const fn depth(&self) -> u64 {
        self.depth
    }

    /// Returns the one-based physical line of the symbol's start.
    #[must_use]
    pub const fn start_line(&self) -> u64 {
        self.start_line
    }

    /// Returns the one-based physical line containing the symbol's end.
    #[must_use]
    pub const fn end_line(&self) -> u64 {
        self.end_line
    }

    /// Returns the raw zero-based LSP enclosing range retained for audit.
    #[must_use]
    pub const fn lsp_range(&self) -> SelectionLspRangeDto {
        self.lsp_range
    }

    /// Returns the validated half-open byte selector of the enclosing range.
    #[must_use]
    pub const fn selector(&self) -> SelectionByteSelectorDto {
        self.selector
    }
}

/// A successful read-only outline protocol-v1 response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutlineResponse {
    outline_protocol_version: u64,
    workspace_identity_hash: String,
    source: SelectionSourceDto,
    server: SelectionServerDto,
    symbols: Vec<OutlineSymbolDto>,
    warnings: Vec<WarningDto>,
}

impl OutlineResponse {
    /// Creates an outline protocol-v1 response.
    #[must_use]
    pub fn new(
        workspace_identity_hash: Sha256Digest,
        source: SelectionSourceDto,
        server: SelectionServerDto,
        symbols: Vec<OutlineSymbolDto>,
        warnings: Vec<WarningDto>,
    ) -> Self {
        Self {
            outline_protocol_version: OUTLINE_PROTOCOL_VERSION,
            workspace_identity_hash: workspace_identity_hash.to_prefixed_hex(),
            source,
            server,
            symbols,
            warnings,
        }
    }
}

/// Stable outline error categories and their process exit status.
///
/// Outline performs no query matching, so no conflict category exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutlineErrorCategory {
    /// Invalid outline input.
    Request,
    /// The configured server or platform cannot satisfy the request.
    Support,
    /// An implementation invariant failed.
    Internal,
}

impl OutlineErrorCategory {
    /// Returns the documented command-line exit status for this category.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Request => 2,
            Self::Support => 4,
            Self::Internal => 8,
        }
    }

    /// Returns the stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Support => "support",
            Self::Internal => "internal",
        }
    }
}

impl Serialize for OutlineErrorCategory {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Complete read-only outline protocol-v1 error-code registry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OutlineErrorCode {
    /// The outline request is invalid.
    InvalidOutlineQuery,
    /// No language server is configured for the source.
    LspServerNotConfigured,
    /// The source snapshot is not valid UTF-8.
    UnsupportedTextEncoding,
    /// The server lacks a required capability.
    LspCapabilityUnavailable,
    /// The server returned unsupported flat document symbols.
    LspFlatSymbolsUnsupported,
    /// The server cannot synchronize an opened snapshot.
    LspDocumentSyncUnavailable,
    /// A language-server or outline resource bound was exceeded.
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
    /// An internal outline invariant failed.
    OutlineInternalError,
}

impl OutlineErrorCode {
    /// Every error code registered by outline protocol version 1.
    pub const ALL: [Self; 13] = [
        Self::InvalidOutlineQuery,
        Self::LspServerNotConfigured,
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
        Self::OutlineInternalError,
    ];

    /// Returns the stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidOutlineQuery => "INVALID_OUTLINE_QUERY",
            Self::LspServerNotConfigured => "LSP_SERVER_NOT_CONFIGURED",
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
            Self::OutlineInternalError => "OUTLINE_INTERNAL_ERROR",
        }
    }

    /// Returns the documented category for this code.
    #[must_use]
    pub const fn category(self) -> OutlineErrorCategory {
        match self {
            Self::InvalidOutlineQuery => OutlineErrorCategory::Request,
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
            | Self::LspRequestFailed => OutlineErrorCategory::Support,
            Self::OutlineInternalError => OutlineErrorCategory::Internal,
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

impl Serialize for OutlineErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// A stable structured outline protocol-v1 error response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutlineErrorDto {
    outline_protocol_version: u64,
    code: OutlineErrorCode,
    category: OutlineErrorCategory,
    retryable: bool,
    message: String,
    context: BTreeMap<String, Value>,
}

impl OutlineErrorDto {
    /// Creates an error response using registry-owned category and retry policy.
    #[must_use]
    pub fn new(
        code: OutlineErrorCode,
        message: impl Into<String>,
        context: BTreeMap<String, Value>,
    ) -> Self {
        Self {
            outline_protocol_version: OUTLINE_PROTOCOL_VERSION,
            code,
            category: code.category(),
            retryable: code.retryable(),
            message: message.into(),
            context,
        }
    }

    /// Returns the stable outline error code.
    #[must_use]
    pub const fn code(&self) -> OutlineErrorCode {
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

/// A typed outline response serialization or size failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutlineProtocolError {
    report: OutlineErrorDto,
}

impl OutlineProtocolError {
    /// Returns the stable outline error response.
    #[must_use]
    pub const fn report(&self) -> &OutlineErrorDto {
        &self.report
    }

    /// Consumes the failure and returns its stable outline error response.
    #[must_use]
    pub fn into_report(self) -> OutlineErrorDto {
        self.report
    }
}

impl fmt::Display for OutlineProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}",
            self.report.code.as_str(),
            self.report.message
        )
    }
}

impl Error for OutlineProtocolError {}

/// Serializes one outline JSON value followed by exactly one LF and checks
/// the frozen 16 MiB response limit before the caller writes any bytes.
///
/// # Errors
///
/// Returns `OUTLINE_INTERNAL_ERROR` if JSON serialization fails, or
/// `LSP_RESOURCE_LIMIT_EXCEEDED` if the exact serialized line is too large.
pub fn to_outline_json_line<T: Serialize>(value: &T) -> Result<String, OutlineProtocolError> {
    match write_bounded_json_line(value, MAX_RESPONSE_BYTES) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| outline_serialization_error()),
        Err(BoundedJsonLineFailure::ResponseLimit(actual)) => {
            Err(outline_response_limit_error(actual))
        }
        Err(BoundedJsonLineFailure::Serialization) => Err(outline_serialization_error()),
    }
}

fn outline_serialization_error() -> OutlineProtocolError {
    OutlineProtocolError {
        report: OutlineErrorDto::new(
            OutlineErrorCode::OutlineInternalError,
            "failed to serialize the outline response",
            BTreeMap::new(),
        ),
    }
}

fn outline_response_limit_error(actual: u64) -> OutlineProtocolError {
    OutlineProtocolError {
        report: OutlineErrorDto::new(
            OutlineErrorCode::LspResourceLimitExceeded,
            "the outline response exceeds the serialized response limit",
            outline_context([
                ("actual", json!(actual)),
                ("limit", json!(MAX_RESPONSE_BYTES)),
                ("resource", json!("serialized_json_response")),
            ]),
        ),
    }
}

fn outline_context<const N: usize>(entries: [(&str, Value); N]) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{
        SelectionKnownSymbolKindDto, SelectionLspPositionDto, SelectionPositionEncodingDto,
    };

    fn sample_symbol() -> OutlineSymbolDto {
        OutlineSymbolDto::new(
            "Outer",
            SelectionSymbolKindDto::Known(SelectionKnownSymbolKindDto::Class),
            vec!["Outer".to_owned()],
            0,
            Some("fixture class".to_owned()),
            3,
            Some(1),
            7,
            Some(2),
            SelectionLspRangeDto::new(
                SelectionLspPositionDto::new(2, 0),
                SelectionLspPositionDto::new(6, 1),
            ),
            SelectionLspRangeDto::new(
                SelectionLspPositionDto::new(2, 5),
                SelectionLspPositionDto::new(2, 10),
            ),
            SelectionByteSelectorDto::new(19, 77),
        )
    }

    fn sample_response() -> OutlineResponse {
        OutlineResponse::new(
            Sha256Digest([0xee; 32]),
            SelectionSourceDto::new("source.rs", Sha256Digest([0xfc; 32]), 78),
            SelectionServerDto::new(
                None,
                Some("srcmv-fake-lsp".to_owned()),
                Some("1".to_owned()),
                SelectionPositionEncodingDto::Utf16,
            ),
            vec![sample_symbol()],
            Vec::new(),
        )
    }

    #[test]
    fn outline_response_serializes_the_frozen_wire_shape() {
        let value = serde_json::to_value(sample_response()).expect("response should serialize");

        assert_eq!(value["outline_protocol_version"], 1);
        assert_eq!(
            value["workspace_identity_hash"],
            "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        );
        assert_eq!(
            value["source"],
            json!({
                "path": "source.rs",
                "sha256": "sha256:fcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfc",
                "byte_length": 78
            })
        );
        assert_eq!(
            value["server"],
            json!({
                "configuration_id": null,
                "reported_name": "srcmv-fake-lsp",
                "reported_version": "1",
                "position_encoding": "utf-16"
            })
        );
        assert_eq!(value["warnings"], json!([]));
        assert_eq!(value["symbols"].as_array().map(Vec::len), Some(1));

        let symbol = &value["symbols"][0];
        let keys = symbol.as_object().expect("symbol must be an object");
        let mut expected_keys = [
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
        ];
        expected_keys.sort_unstable();
        let mut observed_keys = keys.keys().cloned().collect::<Vec<_>>();
        observed_keys.sort_unstable();
        assert_eq!(observed_keys, expected_keys);
        assert_eq!(symbol["name"], "Outer");
        assert_eq!(symbol["symbol_kind"], "class");
        assert_eq!(symbol["symbol_path"], json!(["Outer"]));
        assert_eq!(symbol["depth"], 0);
        assert_eq!(symbol["detail"], "fixture class");
        assert_eq!(symbol["start_line"], 3);
        assert_eq!(symbol["start_column"], 1);
        assert_eq!(symbol["end_line"], 7);
        assert_eq!(symbol["end_column"], 2);
        assert_eq!(
            symbol["lsp_range"],
            json!({"start": {"line": 2, "character": 0}, "end": {"line": 6, "character": 1}})
        );
        assert_eq!(
            symbol["lsp_selection_range"],
            json!({"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 10}})
        );
        assert_eq!(
            symbol["selector"],
            json!({"kind": "bytes", "start": 19, "end": 77})
        );
    }

    #[test]
    fn to_outline_json_line_emits_exactly_one_trailing_lf() {
        let line = to_outline_json_line(&sample_response()).expect("small response should fit");

        assert!(line.ends_with('\n'));
        assert!(!line[..line.len() - 1].contains('\n'));
        let parsed: Value =
            serde_json::from_str(line.trim_end()).expect("serialized line must parse");
        assert_eq!(parsed["outline_protocol_version"], 1);
    }

    #[test]
    fn bounded_serialization_accepts_below_and_at_the_exact_limit() {
        let value = sample_response();
        let exact_bytes = serde_json::to_vec(&value)
            .expect("value should serialize")
            .len()
            + 1;

        let below = write_bounded_json_line(&value, (exact_bytes + 1) as u64)
            .expect("below-limit response should serialize");
        assert_eq!(below.len(), exact_bytes);

        let at = write_bounded_json_line(&value, exact_bytes as u64)
            .expect("at-limit response should serialize");
        assert_eq!(at.len(), exact_bytes);
    }

    #[test]
    fn bounded_serialization_rejects_above_the_exact_limit_without_partial_output() {
        let value = sample_response();
        let exact_bytes = (serde_json::to_vec(&value)
            .expect("value should serialize")
            .len()
            + 1) as u64;

        let error =
            write_bounded_json_line(&value, exact_bytes - 1).expect_err("above-limit must fail");

        let BoundedJsonLineFailure::ResponseLimit(actual) = error else {
            panic!("an oversized value must report the response limit");
        };
        assert!(actual > exact_bytes - 1);
    }

    #[test]
    fn to_outline_json_line_maps_oversized_responses_to_the_resource_registry() {
        let value = sample_response();
        let exact_bytes = serde_json::to_vec(&value)
            .expect("value should serialize")
            .len()
            + 1;
        // Exercise the shared writer with a shrunken bound so the same small
        // fixture exceeds it without mutating the frozen constant.
        let failure = write_bounded_json_line(&value, (exact_bytes - 1) as u64)
            .expect_err("oversized response must fail");
        let BoundedJsonLineFailure::ResponseLimit(actual) = failure else {
            panic!("expected a response-limit failure");
        };

        let report = outline_response_limit_error(actual).into_report();
        assert_eq!(report.code(), OutlineErrorCode::LspResourceLimitExceeded);
        assert_eq!(report.exit_code(), 4);
        assert!(!report.retryable());
        assert_eq!(
            report.context()["resource"],
            json!("serialized_json_response")
        );
        assert_eq!(report.context()["limit"], json!(MAX_RESPONSE_BYTES));
    }

    #[test]
    fn every_outline_error_code_has_a_frozen_category_exit_and_retry_flag() {
        let expected = [
            (
                OutlineErrorCode::InvalidOutlineQuery,
                "request",
                2_u8,
                false,
            ),
            (
                OutlineErrorCode::LspServerNotConfigured,
                "support",
                4,
                false,
            ),
            (
                OutlineErrorCode::UnsupportedTextEncoding,
                "support",
                4,
                false,
            ),
            (
                OutlineErrorCode::LspCapabilityUnavailable,
                "support",
                4,
                false,
            ),
            (
                OutlineErrorCode::LspFlatSymbolsUnsupported,
                "support",
                4,
                false,
            ),
            (
                OutlineErrorCode::LspDocumentSyncUnavailable,
                "support",
                4,
                false,
            ),
            (
                OutlineErrorCode::LspResourceLimitExceeded,
                "support",
                4,
                false,
            ),
            (OutlineErrorCode::LspTimeout, "support", 4, true),
            (OutlineErrorCode::LspStartFailed, "support", 4, true),
            (OutlineErrorCode::LspExited, "support", 4, true),
            (OutlineErrorCode::LspProtocolError, "support", 4, false),
            (OutlineErrorCode::LspRequestFailed, "support", 4, true),
            (OutlineErrorCode::OutlineInternalError, "internal", 8, false),
        ];
        assert_eq!(OutlineErrorCode::ALL.len(), expected.len());

        for (code, category, exit, retryable) in expected {
            assert_eq!(code.category().as_str(), category, "{}", code.as_str());
            assert_eq!(code.category().exit_code(), exit, "{}", code.as_str());
            assert_eq!(code.retryable(), retryable, "{}", code.as_str());
            let report = OutlineErrorDto::new(code, "fixture message", BTreeMap::new());
            assert_eq!(report.code(), code);
            assert_eq!(report.exit_code(), exit);
            assert_eq!(report.retryable(), retryable);
            assert_eq!(
                serde_json::to_value(&report).expect("error should serialize")["outline_protocol_version"],
                1
            );
        }
    }

    #[test]
    fn outline_error_spellings_are_unique_and_stable() {
        let mut spellings = OutlineErrorCode::ALL
            .iter()
            .map(|code| code.as_str())
            .collect::<Vec<_>>();
        spellings.sort_unstable();
        let unique_count = spellings.len();
        spellings.dedup();
        assert_eq!(spellings.len(), unique_count);

        for spelling in [
            "INVALID_OUTLINE_QUERY",
            "OUTLINE_INTERNAL_ERROR",
            "LSP_SERVER_NOT_CONFIGURED",
            "UNSUPPORTED_TEXT_ENCODING",
            "LSP_CAPABILITY_UNAVAILABLE",
            "LSP_FLAT_SYMBOLS_UNSUPPORTED",
            "LSP_DOCUMENT_SYNC_UNAVAILABLE",
            "LSP_RESOURCE_LIMIT_EXCEEDED",
            "LSP_TIMEOUT",
            "LSP_START_FAILED",
            "LSP_EXITED",
            "LSP_PROTOCOL_ERROR",
            "LSP_REQUEST_FAILED",
        ] {
            assert!(
                OutlineErrorCode::ALL
                    .iter()
                    .any(|code| code.as_str() == spelling),
                "{spelling} must stay registered"
            );
        }
    }

    #[test]
    fn outline_protocol_error_displays_code_and_message() {
        let error = outline_serialization_error();

        assert_eq!(
            error.to_string(),
            "OUTLINE_INTERNAL_ERROR: failed to serialize the outline response"
        );
        assert_eq!(
            error.report().code(),
            OutlineErrorCode::OutlineInternalError
        );
    }
}
