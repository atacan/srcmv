//! Deterministic, dependency-light fake language-server support.
//!
//! The fake server deliberately does not depend on `srcmv-lsp`. It speaks
//! enough JSON-RPC over stdio to exercise the production client's framing,
//! lifecycle, capability, synchronization, and document-symbol behavior.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;

use serde_json::{Value, json};

const MAX_FAKE_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_FAKE_HEADER_BYTES: usize = 64 * 1024;
/// Tiny distinct symbols served by the outline-count-limit scenario.
const OUTLINE_LIMIT_SYMBOL_COUNT: usize = 10_001;

/// Named behavior exposed by the fake language-server executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FakeLspScenario {
    /// Completes a normal full-synchronization, hierarchical-symbol session.
    Success,
    /// Requires a configuration notification before `didOpen`.
    SuccessWithConfiguration,
    /// Returns a JSON-RPC error for `initialize`.
    InitializeError,
    /// Exits immediately after returning a successful initialize response.
    ExitAfterInitialize,
    /// Reads `initialize` and then never responds.
    HangInitialize,
    /// Reads `textDocument/documentSymbol` and then never responds.
    HangDocumentSymbols,
    /// Reads `shutdown` but never responds.
    IgnoreShutdown,
    /// Spawns a same-process-group sleeper and then ignores `shutdown`.
    IgnoreShutdownWithChild,
    /// Emits a malformed header instead of an initialize response.
    MalformedHeader,
    /// Emits a framed invalid JSON body instead of an initialize response.
    InvalidJson,
    /// Returns an initialize response with an unrequested ID.
    UnknownResponseId,
    /// Returns a response containing both `result` and `error`.
    ResponseAndError,
    /// Fills stderr before completing the successful lifecycle.
    StderrFlood,
    /// Sends supported and unsupported server requests during symbol lookup.
    ServerRequests,
    /// Sends more server requests than the production per-selection limit.
    ServerRequestFlood,
    /// Omits document-symbol support from the initialize result.
    NoDocumentSymbols,
    /// Omits text-document synchronization from the initialize result.
    NoDocumentSync,
    /// Reports incremental synchronization through synchronization options.
    IncrementalSync,
    /// Reports legacy numeric full synchronization.
    LegacyFullSync,
    /// Reports legacy numeric incremental synchronization.
    LegacyIncrementalSync,
    /// Reports synchronization options with `openClose` disabled.
    OpenCloseFalse,
    /// Negotiates UTF-8 positions.
    Utf8Encoding,
    /// Negotiates UTF-32 positions.
    Utf32Encoding,
    /// Omits position encoding so clients must use the UTF-16 default.
    DefaultEncoding,
    /// Reports an unsupported future position encoding.
    UnsupportedEncoding,
    /// Returns legacy flat `SymbolInformation` records.
    FlatSymbols,
    /// Returns `null` for document symbols.
    NullSymbols,
    /// Returns a `selectionRange` outside the enclosing symbol range.
    InvalidSelectionRange,
    /// Returns a symbol whose enclosing range has its endpoints reversed.
    MalformedRange,
    /// Returns a hierarchical symbol with a future numeric kind.
    UnknownSymbolKind,
    /// Returns a deeply nested hierarchical symbol tree.
    DeepSymbols,
    /// Returns duplicate hierarchical symbols.
    DuplicateSymbols,
    /// Returns equal-range symbols with distinct hierarchical paths.
    AmbiguousSymbols,
    /// Sends a bounded burst of notifications during symbol lookup.
    NotificationFlood,
    /// Sends more notifications than the production per-selection limit.
    NotificationLimitExceeded,
    /// Returns ~120 well-formed nested symbols spanning several depths and kinds.
    ManySymbols,
    /// Returns more tiny distinct symbols than the outline emission bound.
    SymbolCountLimitExceeded,
    /// Returns a symbol whose name contains terminal-control characters.
    EscapedNameSymbols,
}

impl FakeLspScenario {
    /// All public scenario names accepted by the fixture executable.
    pub const ALL: [Self; 38] = [
        Self::Success,
        Self::SuccessWithConfiguration,
        Self::InitializeError,
        Self::ExitAfterInitialize,
        Self::HangInitialize,
        Self::HangDocumentSymbols,
        Self::IgnoreShutdown,
        Self::IgnoreShutdownWithChild,
        Self::MalformedHeader,
        Self::InvalidJson,
        Self::UnknownResponseId,
        Self::ResponseAndError,
        Self::StderrFlood,
        Self::ServerRequests,
        Self::ServerRequestFlood,
        Self::NoDocumentSymbols,
        Self::NoDocumentSync,
        Self::IncrementalSync,
        Self::LegacyFullSync,
        Self::LegacyIncrementalSync,
        Self::OpenCloseFalse,
        Self::Utf8Encoding,
        Self::Utf32Encoding,
        Self::DefaultEncoding,
        Self::UnsupportedEncoding,
        Self::FlatSymbols,
        Self::NullSymbols,
        Self::InvalidSelectionRange,
        Self::MalformedRange,
        Self::UnknownSymbolKind,
        Self::DeepSymbols,
        Self::DuplicateSymbols,
        Self::AmbiguousSymbols,
        Self::NotificationFlood,
        Self::NotificationLimitExceeded,
        Self::ManySymbols,
        Self::SymbolCountLimitExceeded,
        Self::EscapedNameSymbols,
    ];

    /// Returns the stable command-line spelling of this scenario.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::SuccessWithConfiguration => "success-with-configuration",
            Self::InitializeError => "initialize-error",
            Self::ExitAfterInitialize => "exit-after-initialize",
            Self::HangInitialize => "hang-initialize",
            Self::HangDocumentSymbols => "hang-document-symbols",
            Self::IgnoreShutdown => "ignore-shutdown",
            Self::IgnoreShutdownWithChild => "ignore-shutdown-with-child",
            Self::MalformedHeader => "malformed-header",
            Self::InvalidJson => "invalid-json",
            Self::UnknownResponseId => "unknown-response-id",
            Self::ResponseAndError => "response-and-error",
            Self::StderrFlood => "stderr-flood",
            Self::ServerRequests => "server-requests",
            Self::ServerRequestFlood => "server-request-flood",
            Self::NoDocumentSymbols => "no-document-symbols",
            Self::NoDocumentSync => "no-document-sync",
            Self::IncrementalSync => "incremental-sync",
            Self::LegacyFullSync => "legacy-full-sync",
            Self::LegacyIncrementalSync => "legacy-incremental-sync",
            Self::OpenCloseFalse => "open-close-false",
            Self::Utf8Encoding => "utf-8-encoding",
            Self::Utf32Encoding => "utf-32-encoding",
            Self::DefaultEncoding => "default-encoding",
            Self::UnsupportedEncoding => "unsupported-encoding",
            Self::FlatSymbols => "flat-symbols",
            Self::NullSymbols => "null-symbols",
            Self::InvalidSelectionRange => "invalid-selection-range",
            Self::MalformedRange => "malformed-range",
            Self::UnknownSymbolKind => "unknown-symbol-kind",
            Self::DeepSymbols => "deep-symbols",
            Self::DuplicateSymbols => "duplicate-symbols",
            Self::AmbiguousSymbols => "ambiguous-symbols",
            Self::NotificationFlood => "notification-flood",
            Self::NotificationLimitExceeded => "notification-limit-exceeded",
            Self::ManySymbols => "many-symbols",
            Self::SymbolCountLimitExceeded => "symbol-count-limit-exceeded",
            Self::EscapedNameSymbols => "escaped-name-symbols",
        }
    }
}

impl fmt::Display for FakeLspScenario {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for FakeLspScenario {
    type Err = FakeLspError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|scenario| scenario.as_str() == value)
            .ok_or_else(|| FakeLspError::new(format!("unknown fake LSP scenario `{value}`")))
    }
}

/// Optional exact expectations applied to the `didOpen` notification.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FakeLspExpectations {
    /// Expected canonical document URI, when set.
    pub document_uri: Option<String>,
    /// Expected language identifier, when set.
    pub language_id: Option<String>,
    /// Expected immutable document text, when set.
    pub document_text: Option<String>,
}

/// Error returned by fake-server setup or deterministic transcript validation.
#[derive(Debug, Eq, PartialEq)]
pub struct FakeLspError {
    message: String,
}

impl FakeLspError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for FakeLspError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FakeLspError {}

impl From<io::Error> for FakeLspError {
    fn from(error: io::Error) -> Self {
        Self::new(format!("fake LSP I/O failed: {error}"))
    }
}

impl From<serde_json::Error> for FakeLspError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(format!("fake LSP JSON failed: {error}"))
    }
}

/// Runs one deterministic fake-server session over the supplied streams.
///
/// The successful scenarios require the standard lifecycle order from
/// `initialize` through `exit`. Failure scenarios deviate only at the point
/// named by the scenario.
///
/// # Errors
///
/// Returns an error when framing is invalid, lifecycle messages arrive out of
/// order, an exact `didOpen` expectation is not met, or stream I/O fails.
pub fn serve_fake_lsp(
    scenario: FakeLspScenario,
    expectations: &FakeLspExpectations,
    mut input: impl Read,
    mut output: impl Write,
    mut error_output: impl Write,
) -> Result<(), FakeLspError> {
    let initialize = read_required_message(&mut input)?;
    let initialize_id = expect_request(&initialize, "initialize")?;
    validate_initialize(&initialize)?;
    let workspace_folder = initialize
        .pointer("/params/workspaceFolders/0")
        .cloned()
        .ok_or_else(|| FakeLspError::new("initialize must contain one workspace folder"))?;

    match scenario {
        FakeLspScenario::HangInitialize => hang_forever(),
        FakeLspScenario::MalformedHeader => {
            output.write_all(b"Content-Length: nope\r\n\r\n")?;
            output.flush()?;
            hang_forever();
        }
        FakeLspScenario::InvalidJson => {
            output.write_all(b"Content-Length: 1\r\n\r\n{")?;
            output.flush()?;
            hang_forever();
        }
        FakeLspScenario::InitializeError => {
            write_message(
                &mut output,
                &json!({
                    "jsonrpc": "2.0",
                    "id": initialize_id,
                    "error": {"code": -32603, "message": "fixture initialize failure"}
                }),
            )?;
            // Keep the process alive so the client can deterministically
            // observe the JSON-RPC error before its documented higher-priority
            // unexpected-exit condition. The client aborts this fixture after
            // receiving the error.
            hang_forever();
        }
        FakeLspScenario::UnknownResponseId => {
            write_message(
                &mut output,
                &json!({"jsonrpc": "2.0", "id": 999999, "result": initialize_result(scenario)}),
            )?;
            hang_forever();
        }
        FakeLspScenario::ResponseAndError => {
            write_message(
                &mut output,
                &json!({
                    "jsonrpc": "2.0",
                    "id": initialize_id,
                    "result": initialize_result(scenario),
                    "error": {"code": -32603, "message": "invalid dual response"}
                }),
            )?;
            hang_forever();
        }
        _ => {}
    }

    if scenario == FakeLspScenario::StderrFlood {
        let chunk = [b'!'; 4096];
        for _ in 0..256 {
            error_output.write_all(&chunk)?;
        }
        error_output.flush()?;
    }

    write_message(
        &mut output,
        &json!({
            "jsonrpc": "2.0",
            "id": initialize_id,
            "result": initialize_result(scenario)
        }),
    )?;

    if scenario == FakeLspScenario::ExitAfterInitialize {
        return Ok(());
    }

    let initialized = read_required_message(&mut input)?;
    expect_notification(&initialized, "initialized")?;

    if matches!(
        scenario,
        FakeLspScenario::SuccessWithConfiguration | FakeLspScenario::ServerRequests
    ) {
        let configuration = read_required_message(&mut input)?;
        expect_notification(&configuration, "workspace/didChangeConfiguration")?;
        if configuration.pointer("/params/settings").is_none() {
            return Err(FakeLspError::new(
                "configuration notification must contain params.settings",
            ));
        }
    }

    let did_open = read_required_message(&mut input)?;
    expect_notification(&did_open, "textDocument/didOpen")?;
    validate_did_open(&did_open, expectations)?;
    let opened_uri = string_at(&did_open, "/params/textDocument/uri")?;

    let document_symbol = read_required_message(&mut input)?;
    let document_symbol_id = expect_request(&document_symbol, "textDocument/documentSymbol")?;
    if string_at(&document_symbol, "/params/textDocument/uri")? != opened_uri {
        return Err(FakeLspError::new(
            "documentSymbol URI must match the didOpen URI",
        ));
    }

    match scenario {
        FakeLspScenario::HangDocumentSymbols => hang_forever(),
        FakeLspScenario::ServerRequests => {
            exercise_server_requests(&mut input, &mut output, &workspace_folder)?;
        }
        FakeLspScenario::ServerRequestFlood => {
            exercise_server_request_flood(&mut input, &mut output)?;
        }
        FakeLspScenario::NotificationFlood => {
            for sequence in 0..64 {
                write_message(
                    &mut output,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "window/logMessage",
                        "params": {"type": 3, "message": format!("fixture notification {sequence}")}
                    }),
                )?;
            }
        }
        FakeLspScenario::NotificationLimitExceeded => {
            for sequence in 0..=1024 {
                write_message(
                    &mut output,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "window/logMessage",
                        "params": {"type": 3, "message": format!("fixture notification {sequence}")}
                    }),
                )?;
            }
        }
        _ => {}
    }

    write_message(
        &mut output,
        &json!({
            "jsonrpc": "2.0",
            "id": document_symbol_id,
            "result": symbol_result(scenario, opened_uri)
        }),
    )?;

    let did_close = read_required_message(&mut input)?;
    expect_notification(&did_close, "textDocument/didClose")?;
    if string_at(&did_close, "/params/textDocument/uri")? != opened_uri {
        return Err(FakeLspError::new("didClose URI must match the didOpen URI"));
    }

    let shutdown = read_required_message(&mut input)?;
    let shutdown_id = expect_request(&shutdown, "shutdown")?;

    match scenario {
        FakeLspScenario::IgnoreShutdown => hang_forever(),
        FakeLspScenario::IgnoreShutdownWithChild => {
            spawn_sleeper_child()?;
            hang_forever();
        }
        _ => {}
    }

    write_message(
        &mut output,
        &json!({"jsonrpc": "2.0", "id": shutdown_id, "result": null}),
    )?;

    let exit = read_required_message(&mut input)?;
    expect_notification(&exit, "exit")
}

/// Parses executable arguments and serves one stdio session.
///
/// Supported arguments are `--scenario NAME`, `--expected-document-uri URI`,
/// `--expected-language-id ID`, and `--expected-document-text-file PATH`.
/// The hidden `--sleep-forever` mode is reserved for descendant-cleanup tests.
///
/// # Errors
///
/// Returns an error for malformed arguments, an unreadable expected-text file,
/// or a fake-server protocol failure.
pub fn run_from_process_args(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<(), FakeLspError> {
    let parsed = parse_process_args(arguments)?;
    if parsed.sleep_forever {
        hang_forever();
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    serve_fake_lsp(
        parsed.scenario,
        &parsed.expectations,
        stdin.lock(),
        stdout.lock(),
        stderr.lock(),
    )
}

struct ParsedProcessArgs {
    scenario: FakeLspScenario,
    expectations: FakeLspExpectations,
    sleep_forever: bool,
}

fn parse_process_args(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<ParsedProcessArgs, FakeLspError> {
    let mut arguments = arguments.into_iter();
    let mut scenario = FakeLspScenario::Success;
    let mut expectations = FakeLspExpectations::default();
    let mut sleep_forever = false;

    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| FakeLspError::new("fake LSP arguments must be UTF-8"))?;
        match argument.as_str() {
            "--scenario" => {
                scenario = next_utf8_argument(&mut arguments, "--scenario")?.parse()?;
            }
            "--expected-document-uri" => {
                expectations.document_uri = Some(next_utf8_argument(
                    &mut arguments,
                    "--expected-document-uri",
                )?);
            }
            "--expected-language-id" => {
                expectations.language_id = Some(next_utf8_argument(
                    &mut arguments,
                    "--expected-language-id",
                )?);
            }
            "--expected-document-text-file" => {
                let path = PathBuf::from(next_utf8_argument(
                    &mut arguments,
                    "--expected-document-text-file",
                )?);
                expectations.document_text = Some(fs::read_to_string(path).map_err(|error| {
                    FakeLspError::new(format!("failed to read expected document text: {error}"))
                })?);
            }
            "--sleep-forever" => sleep_forever = true,
            _ => {
                return Err(FakeLspError::new(format!(
                    "unknown fake LSP argument `{argument}`"
                )));
            }
        }
    }

    Ok(ParsedProcessArgs {
        scenario,
        expectations,
        sleep_forever,
    })
}

fn next_utf8_argument(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<String, FakeLspError> {
    arguments
        .next()
        .ok_or_else(|| FakeLspError::new(format!("missing value for `{option}`")))?
        .into_string()
        .map_err(|_| FakeLspError::new(format!("value for `{option}` must be UTF-8")))
}

fn initialize_result(scenario: FakeLspScenario) -> Value {
    let mut capabilities = serde_json::Map::new();
    match scenario {
        FakeLspScenario::DefaultEncoding => {}
        FakeLspScenario::Utf8Encoding => {
            capabilities.insert("positionEncoding".to_owned(), json!("utf-8"));
        }
        FakeLspScenario::Utf32Encoding => {
            capabilities.insert("positionEncoding".to_owned(), json!("utf-32"));
        }
        FakeLspScenario::UnsupportedEncoding => {
            capabilities.insert("positionEncoding".to_owned(), json!("fixture-encoding"));
        }
        _ => {
            capabilities.insert("positionEncoding".to_owned(), json!("utf-16"));
        }
    }
    match scenario {
        FakeLspScenario::NoDocumentSync => {}
        FakeLspScenario::IncrementalSync => {
            capabilities.insert(
                "textDocumentSync".to_owned(),
                json!({"openClose": true, "change": 2}),
            );
        }
        FakeLspScenario::LegacyFullSync => {
            capabilities.insert("textDocumentSync".to_owned(), json!(1));
        }
        FakeLspScenario::LegacyIncrementalSync => {
            capabilities.insert("textDocumentSync".to_owned(), json!(2));
        }
        FakeLspScenario::OpenCloseFalse => {
            capabilities.insert(
                "textDocumentSync".to_owned(),
                json!({"openClose": false, "change": 1}),
            );
        }
        _ => {
            capabilities.insert(
                "textDocumentSync".to_owned(),
                json!({"openClose": true, "change": 1}),
            );
        }
    }
    if scenario != FakeLspScenario::NoDocumentSymbols {
        capabilities.insert("documentSymbolProvider".to_owned(), json!(true));
    }

    json!({
        "capabilities": capabilities,
        "serverInfo": {"name": "srcmv-fake-lsp", "version": "1"}
    })
}

fn symbol_result(scenario: FakeLspScenario, document_uri: &str) -> Value {
    match scenario {
        FakeLspScenario::FlatSymbols => json!([{
            "name": "alpha",
            "kind": 12,
            "location": {
                "uri": document_uri,
                "range": range(3, 4, 5, 5)
            }
        }]),
        FakeLspScenario::NullSymbols => Value::Null,
        FakeLspScenario::InvalidSelectionRange => json!([{
            "name": "alpha",
            "kind": 12,
            "range": range(3, 4, 5, 5),
            "selectionRange": range(2, 0, 2, 4)
        }]),
        FakeLspScenario::MalformedRange => json!([{
            "name": "alpha",
            "kind": 12,
            "range": range(5, 5, 3, 4),
            "selectionRange": range(3, 11, 3, 16)
        }]),
        FakeLspScenario::UnknownSymbolKind => json!([{
            "name": "alpha",
            "kind": 999,
            "range": range(3, 4, 5, 5),
            "selectionRange": range(3, 11, 3, 16)
        }]),
        FakeLspScenario::DeepSymbols => deep_symbols(64),
        FakeLspScenario::DuplicateSymbols => {
            let symbol = alpha_symbol();
            json!([symbol, alpha_symbol()])
        }
        FakeLspScenario::ManySymbols => many_symbols(),
        FakeLspScenario::SymbolCountLimitExceeded => symbol_count_limit_exceeded(),
        FakeLspScenario::EscapedNameSymbols => json!([{
            "name": "bad\u{1b}name",
            "kind": 5,
            "range": range(2, 0, 6, 1),
            "selectionRange": range(2, 0, 2, 3)
        }]),
        FakeLspScenario::AmbiguousSymbols => json!([
            {
                "name": "First",
                "kind": 3,
                "range": range(2, 0, 6, 1),
                "selectionRange": range(2, 0, 2, 5),
                "children": [alpha_symbol()]
            },
            {
                "name": "Second",
                "kind": 3,
                "range": range(2, 0, 6, 1),
                "selectionRange": range(2, 0, 2, 6),
                "children": [alpha_symbol()]
            }
        ]),
        _ => json!([{
            "name": "Outer",
            "detail": "fixture class",
            "kind": 5,
            "range": range(2, 0, 6, 1),
            "selectionRange": range(2, 5, 2, 10),
            "children": [alpha_symbol()]
        }]),
    }
}

fn alpha_symbol() -> Value {
    json!({
        "name": "alpha",
        "detail": "fixture function",
        "kind": 12,
        "range": range(3, 4, 5, 5),
        "selectionRange": range(3, 11, 3, 16)
    })
}

fn deep_symbols(depth: usize) -> Value {
    let mut symbol = alpha_symbol();
    for level in (0..depth).rev() {
        symbol = json!({
            "name": format!("Level{level}"),
            "kind": 3,
            "range": range(2, 0, 6, 1),
            "selectionRange": range(2, 0, 2, 1),
            "children": [symbol]
        });
    }
    json!([symbol])
}

/// Builds 120 well-formed symbols spanning three depths: twelve classes, each
/// holding three modules of two functions, all with distinct contained ranges.
fn many_symbols() -> Value {
    let groups = (0..12)
        .map(|group| {
            let base_line = 12 * group;
            let modules = (0..3)
                .map(|module| {
                    let module_line = base_line + 1 + 3 * module;
                    let functions = (0..2)
                        .map(|index| {
                            json!({
                                "name": format!("Fn{group}_{module}_{index}"),
                                "kind": 12,
                                "range": range(module_line + index, 4, module_line + index, 20),
                                "selectionRange": range(
                                    module_line + index,
                                    8,
                                    module_line + index,
                                    12
                                )
                            })
                        })
                        .collect::<Vec<_>>();
                    json!({
                        "name": format!("Module{group}_{module}"),
                        "kind": 2,
                        "range": range(module_line, 2, module_line + 2, 3),
                        "selectionRange": range(module_line, 8, module_line, 10),
                        "children": functions
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "name": format!("Group{group}"),
                "kind": 5,
                "range": range(base_line, 0, base_line + 11, 1),
                "selectionRange": range(base_line, 6, base_line, 11),
                "children": modules
            })
        })
        .collect::<Vec<_>>();
    json!(groups)
}

/// Builds more distinct tiny symbols than the outline emission bound accepts.
fn symbol_count_limit_exceeded() -> Value {
    Value::Array(
        (0..OUTLINE_LIMIT_SYMBOL_COUNT)
            .map(|index| {
                json!({
                    "name": format!("s{index}"),
                    "kind": 12,
                    "range": range(0, 0, 0, 1),
                    "selectionRange": range(0, 0, 0, 1)
                })
            })
            .collect(),
    )
}

fn range(start_line: u32, start_character: u32, end_line: u32, end_character: u32) -> Value {
    json!({
        "start": {"line": start_line, "character": start_character},
        "end": {"line": end_line, "character": end_character}
    })
}

fn validate_initialize(message: &Value) -> Result<(), FakeLspError> {
    let root_uri = string_at(message, "/params/rootUri")?;
    let workspace_uri = string_at(message, "/params/workspaceFolders/0/uri")?;
    if root_uri != workspace_uri {
        return Err(FakeLspError::new(
            "initialize rootUri must match the single workspace folder URI",
        ));
    }
    let folders = message
        .pointer("/params/workspaceFolders")
        .and_then(Value::as_array)
        .ok_or_else(|| FakeLspError::new("initialize must contain workspaceFolders"))?;
    if folders.len() != 1 {
        return Err(FakeLspError::new(
            "initialize must contain exactly one workspace folder",
        ));
    }
    let expected_encodings = json!(["utf-8", "utf-16", "utf-32"]);
    if message.pointer("/params/capabilities/general/positionEncodings")
        != Some(&expected_encodings)
    {
        return Err(FakeLspError::new(
            "initialize must advertise utf-8, utf-16, and utf-32 in preference order",
        ));
    }
    if message.pointer(
        "/params/capabilities/textDocument/documentSymbol/hierarchicalDocumentSymbolSupport",
    ) != Some(&Value::Bool(true))
    {
        return Err(FakeLspError::new(
            "initialize must advertise hierarchical document symbols",
        ));
    }
    if message.pointer("/params/capabilities/textDocument/documentSymbol/dynamicRegistration")
        != Some(&Value::Bool(false))
    {
        return Err(FakeLspError::new(
            "initialize must disable document-symbol dynamic registration",
        ));
    }
    if message.pointer("/params/capabilities/window/workDoneProgress") != Some(&Value::Bool(false))
    {
        return Err(FakeLspError::new(
            "initialize must disable work-done progress",
        ));
    }
    if message.pointer("/params/capabilities/workspace/applyEdit") != Some(&Value::Bool(false)) {
        return Err(FakeLspError::new(
            "initialize must disable workspace applyEdit",
        ));
    }
    Ok(())
}

fn validate_did_open(
    message: &Value,
    expectations: &FakeLspExpectations,
) -> Result<(), FakeLspError> {
    if message
        .pointer("/params/textDocument/version")
        .and_then(Value::as_i64)
        .is_none()
    {
        return Err(FakeLspError::new(
            "didOpen textDocument.version must be an integer",
        ));
    }

    validate_optional_string(
        message,
        "/params/textDocument/uri",
        expectations.document_uri.as_deref(),
        "didOpen document URI",
    )?;
    validate_optional_string(
        message,
        "/params/textDocument/languageId",
        expectations.language_id.as_deref(),
        "didOpen language ID",
    )?;
    validate_optional_string(
        message,
        "/params/textDocument/text",
        expectations.document_text.as_deref(),
        "didOpen document text",
    )
}

fn validate_optional_string(
    message: &Value,
    pointer: &str,
    expected: Option<&str>,
    field_name: &str,
) -> Result<(), FakeLspError> {
    let actual = string_at(message, pointer)?;
    if let Some(expected) = expected
        && actual != expected
    {
        return Err(FakeLspError::new(format!(
            "{field_name} did not match its exact expectation"
        )));
    }
    Ok(())
}

fn exercise_server_requests(
    input: &mut impl Read,
    output: &mut impl Write,
    workspace_folder: &Value,
) -> Result<(), FakeLspError> {
    let requests = [
        (
            json!("folders-request"),
            "workspace/workspaceFolders",
            Value::Null,
            json!({
                "jsonrpc": "2.0",
                "id": "folders-request",
                "result": [workspace_folder]
            }),
        ),
        (
            json!(7001),
            "workspace/configuration",
            json!({"items": [{"section": "fixture"}]}),
            json!({"jsonrpc": "2.0", "id": 7001, "result": [{"enabled": true}]}),
        ),
        (
            json!("show-message-request"),
            "window/showMessageRequest",
            json!({"type": 3, "message": "fixture question", "actions": []}),
            json!({"jsonrpc": "2.0", "id": "show-message-request", "result": null}),
        ),
        (
            json!(7002),
            "workspace/applyEdit",
            json!({"edit": {"changes": {}}}),
            json!({
                "jsonrpc": "2.0",
                "id": 7002,
                "result": {
                    "applied": false,
                    "failureReason": "srcmv selection is read-only"
                }
            }),
        ),
        (
            json!("unknown-request"),
            "fixture/unknownRequest",
            json!({}),
            json!({
                "jsonrpc": "2.0",
                "id": "unknown-request",
                "error": {"code": -32601, "message": "Method not found"}
            }),
        ),
    ];

    for (id, method, params, expected_response) in requests {
        write_message(
            output,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        let response = read_required_message(input)?;
        expect_exact_response(&response, &expected_response, method)?;
    }
    Ok(())
}

fn exercise_server_request_flood(
    input: &mut impl Read,
    output: &mut impl Write,
) -> Result<(), FakeLspError> {
    for sequence in 0..=64 {
        let id = 8_000_u64 + sequence;
        write_message(
            output,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "fixture/unknownRequest",
                "params": {}
            }),
        )?;
        let expected = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "Method not found"}
        });
        let response = read_required_message(input)?;
        expect_exact_response(&response, &expected, "fixture/unknownRequest")?;
    }
    Ok(())
}

fn expect_request(message: &Value, expected_method: &str) -> Result<Value, FakeLspError> {
    expect_method(message, expected_method)?;
    message
        .get("id")
        .filter(|id| id.is_i64() || id.is_u64() || id.is_string())
        .cloned()
        .ok_or_else(|| {
            FakeLspError::new(format!(
                "request `{expected_method}` must have an integer or string ID"
            ))
        })
}

fn expect_notification(message: &Value, expected_method: &str) -> Result<(), FakeLspError> {
    expect_method(message, expected_method)?;
    if message.get("id").is_some() {
        return Err(FakeLspError::new(format!(
            "notification `{expected_method}` must not contain an ID"
        )));
    }
    Ok(())
}

fn expect_method(message: &Value, expected_method: &str) -> Result<(), FakeLspError> {
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(FakeLspError::new("message must declare JSON-RPC 2.0"));
    }
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| FakeLspError::new(format!("expected `{expected_method}` message")))?;
    if method != expected_method {
        return Err(FakeLspError::new(format!(
            "expected `{expected_method}`, received `{method}`"
        )));
    }
    Ok(())
}

fn expect_exact_response(
    message: &Value,
    expected: &Value,
    method: &str,
) -> Result<(), FakeLspError> {
    if message != expected {
        return Err(FakeLspError::new(format!(
            "response to `{method}` did not match the frozen expected JSON-RPC envelope"
        )));
    }
    Ok(())
}

fn string_at<'a>(message: &'a Value, pointer: &str) -> Result<&'a str, FakeLspError> {
    message
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| FakeLspError::new(format!("expected string at JSON pointer `{pointer}`")))
}

fn read_required_message(input: &mut impl Read) -> Result<Value, FakeLspError> {
    read_message(input)?.ok_or_else(|| FakeLspError::new("unexpected EOF from fake LSP client"))
}

fn read_message(input: &mut impl Read) -> Result<Option<Value>, FakeLspError> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte)? {
            0 if header.is_empty() => return Ok(None),
            0 => return Err(FakeLspError::new("EOF inside fake LSP header")),
            _ => header.push(byte[0]),
        }
        if header.len() > MAX_FAKE_HEADER_BYTES {
            return Err(FakeLspError::new("fake LSP header exceeded its limit"));
        }
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let header = std::str::from_utf8(&header)
        .map_err(|_| FakeLspError::new("fake LSP header must be ASCII"))?;
    if !header.is_ascii() {
        return Err(FakeLspError::new("fake LSP header must be ASCII"));
    }

    let mut content_length = None;
    for line in header.trim_end_matches("\r\n\r\n").split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            return Err(FakeLspError::new("malformed fake LSP header line"));
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(FakeLspError::new(
                    "duplicate Content-Length in fake LSP input",
                ));
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| FakeLspError::new("invalid fake LSP Content-Length"))?,
            );
        }
    }

    let content_length = content_length
        .ok_or_else(|| FakeLspError::new("missing Content-Length in fake LSP input"))?;
    if content_length > MAX_FAKE_FRAME_BYTES {
        return Err(FakeLspError::new("fake LSP input frame exceeded its limit"));
    }
    let mut body = vec![0_u8; content_length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn write_message(output: &mut impl Write, message: &Value) -> Result<(), FakeLspError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_FAKE_FRAME_BYTES {
        return Err(FakeLspError::new(
            "fake LSP output frame exceeded its limit",
        ));
    }
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

fn spawn_sleeper_child() -> Result<(), FakeLspError> {
    let executable = std::env::current_exe().map_err(|error| {
        FakeLspError::new(format!("failed to resolve fake LSP binary: {error}"))
    })?;
    Command::new(executable)
        .arg("--sleep-forever")
        .spawn()
        .map_err(|error| FakeLspError::new(format!("failed to spawn sleeper child: {error}")))?;
    Ok(())
}

fn hang_forever() -> ! {
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        FakeLspExpectations, FakeLspScenario, expect_exact_response, read_message, serve_fake_lsp,
        symbol_result, write_message,
    };
    use serde_json::{Value, json};

    const DOCUMENT_URI: &str = "file:///fixture/workspace/source.rs";
    const DOCUMENT_TEXT: &str =
        "pub struct Outer;\n\nimpl Outer {\n    pub fn alpha() -> u32 {\n        1\n    }\n}\n";

    #[test]
    fn scenario_names_should_round_trip_through_the_command_line_spelling() {
        for scenario in FakeLspScenario::ALL {
            assert_eq!(scenario.as_str().parse(), Ok(scenario));
        }
    }

    #[test]
    fn success_should_replay_the_complete_lifecycle() {
        let input = successful_client_input(false);
        let expectations = exact_expectations();
        let mut output = Vec::new();

        serve_fake_lsp(
            FakeLspScenario::Success,
            &expectations,
            Cursor::new(input),
            &mut output,
            Vec::new(),
        )
        .expect("successful fixture session should complete");

        assert_eq!(
            read_all_messages(&output),
            vec![
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "capabilities": {
                            "positionEncoding": "utf-16",
                            "textDocumentSync": {"openClose": true, "change": 1},
                            "documentSymbolProvider": true
                        },
                        "serverInfo": {"name": "srcmv-fake-lsp", "version": "1"}
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": [{
                        "name": "Outer",
                        "detail": "fixture class",
                        "kind": 5,
                        "range": {
                            "start": {"line": 2, "character": 0},
                            "end": {"line": 6, "character": 1}
                        },
                        "selectionRange": {
                            "start": {"line": 2, "character": 5},
                            "end": {"line": 2, "character": 10}
                        },
                        "children": [{
                            "name": "alpha",
                            "detail": "fixture function",
                            "kind": 12,
                            "range": {
                                "start": {"line": 3, "character": 4},
                                "end": {"line": 5, "character": 5}
                            },
                            "selectionRange": {
                                "start": {"line": 3, "character": 11},
                                "end": {"line": 3, "character": 16}
                            }
                        }]
                    }]
                }),
                json!({"jsonrpc": "2.0", "id": 3, "result": null})
            ]
        );
    }

    #[test]
    fn success_with_configuration_should_require_configuration_before_open() {
        let input = successful_client_input(true);

        let result = serve_fake_lsp(
            FakeLspScenario::SuccessWithConfiguration,
            &exact_expectations(),
            Cursor::new(input),
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn did_open_should_reject_text_that_differs_from_the_snapshot_expectation() {
        let input = successful_client_input(false);
        let mut expectations = exact_expectations();
        expectations.document_text = Some("different snapshot".to_owned());

        let error = serve_fake_lsp(
            FakeLspScenario::Success,
            &expectations,
            Cursor::new(input),
            Vec::new(),
            Vec::new(),
        )
        .expect_err("mismatched snapshot must fail");

        assert_eq!(
            error.to_string(),
            "didOpen document text did not match its exact expectation"
        );
    }

    #[test]
    fn server_request_response_should_reject_both_result_and_error() {
        let expected = json!({"jsonrpc": "2.0", "id": 7001, "result": [{"enabled": true}]});
        let actual = json!({
            "jsonrpc": "2.0",
            "id": 7001,
            "result": [{"enabled": true}],
            "error": {"code": -32603, "message": "invalid"}
        });

        let error = expect_exact_response(&actual, &expected, "workspace/configuration")
            .expect_err("dual response must fail");

        assert_eq!(
            error.to_string(),
            "response to `workspace/configuration` did not match the frozen expected JSON-RPC envelope"
        );
    }

    #[test]
    fn server_request_response_should_reject_neither_result_nor_error() {
        let expected = json!({"jsonrpc": "2.0", "id": 7001, "result": [{"enabled": true}]});
        let actual = json!({"jsonrpc": "2.0", "id": 7001});

        let error = expect_exact_response(&actual, &expected, "workspace/configuration")
            .expect_err("empty response must fail");

        assert_eq!(
            error.to_string(),
            "response to `workspace/configuration` did not match the frozen expected JSON-RPC envelope"
        );
    }

    #[test]
    fn workspace_folders_response_should_reject_the_wrong_folder() {
        let expected = json!({
            "jsonrpc": "2.0",
            "id": "folders-request",
            "result": [{"uri": "file:///fixture/workspace/", "name": "workspace"}]
        });
        let actual = json!({
            "jsonrpc": "2.0",
            "id": "folders-request",
            "result": [{"uri": "file:///wrong/", "name": "wrong"}]
        });

        let result = expect_exact_response(&actual, &expected, "workspace/workspaceFolders");

        assert!(result.is_err(), "wrong workspace folder was accepted");
    }

    #[test]
    fn configuration_response_should_reject_the_wrong_section_value() {
        let expected = json!({"jsonrpc": "2.0", "id": 7001, "result": [{"enabled": true}]});
        let actual = json!({"jsonrpc": "2.0", "id": 7001, "result": [null]});

        let result = expect_exact_response(&actual, &expected, "workspace/configuration");

        assert!(result.is_err(), "wrong configuration value was accepted");
    }

    #[test]
    fn show_message_response_should_reject_a_non_null_result() {
        let expected = json!({"jsonrpc": "2.0", "id": "show-message-request", "result": null});
        let actual = json!({
            "jsonrpc": "2.0",
            "id": "show-message-request",
            "result": {"title": "Apply"}
        });

        let result = expect_exact_response(&actual, &expected, "window/showMessageRequest");

        assert!(result.is_err(), "non-null show-message result was accepted");
    }

    #[test]
    fn apply_edit_response_should_reject_an_applied_result() {
        let expected = json!({
            "jsonrpc": "2.0",
            "id": 7002,
            "result": {
                "applied": false,
                "failureReason": "srcmv selection is read-only"
            }
        });
        let actual = json!({"jsonrpc": "2.0", "id": 7002, "result": {"applied": true}});

        let result = expect_exact_response(&actual, &expected, "workspace/applyEdit");

        assert!(result.is_err(), "successful apply-edit result was accepted");
    }

    #[test]
    fn unknown_request_response_should_reject_a_non_method_not_found_error() {
        let expected = json!({
            "jsonrpc": "2.0",
            "id": "unknown-request",
            "error": {"code": -32601, "message": "Method not found"}
        });
        let actual = json!({
            "jsonrpc": "2.0",
            "id": "unknown-request",
            "error": {"code": -32603, "message": "Internal error"}
        });

        let result = expect_exact_response(&actual, &expected, "fixture/unknownRequest");

        assert!(result.is_err(), "non-MethodNotFound error was accepted");
    }

    #[test]
    fn deep_symbols_should_form_a_valid_contained_hierarchy() {
        let symbols = symbol_result(FakeLspScenario::DeepSymbols, DOCUMENT_URI);
        let mut symbol = &symbols[0];
        let mut depth = 0;
        let mut all_ranges_are_contained = true;

        loop {
            let range_start = position(symbol, "/range/start");
            let range_end = position(symbol, "/range/end");
            let selection_start = position(symbol, "/selectionRange/start");
            let selection_end = position(symbol, "/selectionRange/end");
            all_ranges_are_contained &= range_start <= selection_start
                && selection_start <= selection_end
                && selection_end <= range_end;
            depth += 1;

            let Some(child) = symbol.pointer("/children/0") else {
                break;
            };
            symbol = child;
        }

        assert_eq!((depth, all_ranges_are_contained), (65, true));
    }

    fn position(value: &Value, pointer: &str) -> (u64, u64) {
        let position = value
            .pointer(pointer)
            .unwrap_or_else(|| panic!("missing position at `{pointer}`"));
        (
            position["line"]
                .as_u64()
                .unwrap_or_else(|| panic!("missing line at `{pointer}`")),
            position["character"]
                .as_u64()
                .unwrap_or_else(|| panic!("missing character at `{pointer}`")),
        )
    }

    fn exact_expectations() -> FakeLspExpectations {
        FakeLspExpectations {
            document_uri: Some(DOCUMENT_URI.to_owned()),
            language_id: Some("fixture-rust".to_owned()),
            document_text: Some(DOCUMENT_TEXT.to_owned()),
        }
    }

    fn successful_client_input(with_configuration: bool) -> Vec<u8> {
        let mut input = Vec::new();
        let messages = [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "rootUri": "file:///fixture/workspace/",
                    "workspaceFolders": [{
                        "uri": "file:///fixture/workspace/",
                        "name": "workspace"
                    }],
                    "capabilities": {
                        "general": {"positionEncodings": ["utf-8", "utf-16", "utf-32"]},
                        "textDocument": {"documentSymbol": {
                            "dynamicRegistration": false,
                            "hierarchicalDocumentSymbolSupport": true
                        }},
                        "window": {"workDoneProgress": false},
                        "workspace": {"applyEdit": false}
                    }
                }
            }),
            json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        ];
        for message in messages {
            write_message(&mut input, &message).expect("input frame should serialize");
        }
        if with_configuration {
            write_message(
                &mut input,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "workspace/didChangeConfiguration",
                    "params": {"settings": {"fixture": {"enabled": true}}}
                }),
            )
            .expect("configuration frame should serialize");
        }
        let messages = [
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {"textDocument": {
                    "uri": DOCUMENT_URI,
                    "languageId": "fixture-rust",
                    "version": 1,
                    "text": DOCUMENT_TEXT
                }}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "textDocument/documentSymbol",
                "params": {"textDocument": {"uri": DOCUMENT_URI}}
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didClose",
                "params": {"textDocument": {"uri": DOCUMENT_URI}}
            }),
            json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": null}),
            json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
        ];
        for message in messages {
            write_message(&mut input, &message).expect("input frame should serialize");
        }
        input
    }

    fn read_all_messages(bytes: &[u8]) -> Vec<Value> {
        let mut input = Cursor::new(bytes);
        let mut messages = Vec::new();
        while let Some(message) = read_message(&mut input).expect("output frame should parse") {
            messages.push(message);
        }
        messages
    }
}
