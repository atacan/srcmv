//! Bounded JSON-RPC framing and envelope validation for LSP streams.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Read};

use serde_json::{Map, Number, Value};

/// The default maximum number of bytes in an LSP header, including its terminator.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 16 * 1024;
/// The default maximum number of bytes in an inbound JSON-RPC body.
pub const DEFAULT_MAX_INBOUND_BODY_BYTES: usize = 16 * 1024 * 1024;
/// The default maximum number of bytes in an outbound JSON-RPC body.
pub const DEFAULT_MAX_OUTBOUND_BODY_BYTES: usize = 64 * 1024 * 1024;
/// The default maximum encoded byte length of a server request ID.
pub const DEFAULT_MAX_ID_BYTES: usize = 256;
/// The default maximum byte length of a JSON-RPC method name.
pub const DEFAULT_MAX_METHOD_BYTES: usize = 256;
/// The default maximum serialized byte length of request or notification parameters.
pub const DEFAULT_MAX_PARAMS_BYTES: usize = 1024 * 1024;
/// The default maximum nesting depth of arrays and objects in an inbound JSON body.
pub const DEFAULT_MAX_JSON_DEPTH: usize = 64;
/// The default maximum number of simultaneously pending client requests.
pub const DEFAULT_MAX_PENDING_REQUESTS: usize = 8;

/// Independent bounds for LSP framing in each direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FramingLimits {
    /// Maximum bytes in an inbound header, including `\r\n\r\n`.
    pub max_header_bytes: usize,
    /// Maximum bytes in an inbound message body.
    pub max_inbound_body_bytes: usize,
    /// Maximum bytes in an outbound message body.
    pub max_outbound_body_bytes: usize,
}

impl Default for FramingLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_inbound_body_bytes: DEFAULT_MAX_INBOUND_BODY_BYTES,
            max_outbound_body_bytes: DEFAULT_MAX_OUTBOUND_BODY_BYTES,
        }
    }
}

/// Bounds applied while classifying an already framed JSON-RPC value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnvelopeLimits {
    /// Maximum nesting depth of arrays and objects in a JSON body.
    pub max_json_depth: usize,
    /// Maximum encoded byte length of a server request ID.
    pub max_id_bytes: usize,
    /// Maximum UTF-8 byte length of a method name.
    pub max_method_bytes: usize,
    /// Maximum serialized byte length of request or notification parameters.
    pub max_params_bytes: usize,
}

impl Default for EnvelopeLimits {
    fn default() -> Self {
        Self {
            max_json_depth: DEFAULT_MAX_JSON_DEPTH,
            max_id_bytes: DEFAULT_MAX_ID_BYTES,
            max_method_bytes: DEFAULT_MAX_METHOD_BYTES,
            max_params_bytes: DEFAULT_MAX_PARAMS_BYTES,
        }
    }
}

/// The section of an LSP frame that ended prematurely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameSection {
    /// The HTTP-like header section.
    Header,
    /// The JSON-RPC body.
    Body,
}

/// A deterministic category for an invalid JSON-RPC envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeViolation {
    /// The top-level JSON value was neither a request, notification, nor response object.
    InvalidShape,
    /// The `jsonrpc` member was absent or was not exactly `"2.0"`.
    InvalidVersion,
    /// A request or notification contained response-only members.
    RequestContainsResponseMembers,
    /// A response contained request-only members.
    ResponseContainsRequestMembers,
    /// A response contained both or neither of `result` and `error`.
    InvalidResponsePayload,
    /// The optional `params` member was not null, an object, or an array.
    InvalidParams,
    /// A JSON-RPC error object had an invalid shape.
    InvalidErrorObject,
}

impl fmt::Display for EnvelopeViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidShape => "invalid JSON-RPC message shape",
            Self::InvalidVersion => "JSON-RPC version must be exactly 2.0",
            Self::RequestContainsResponseMembers => {
                "JSON-RPC request contains response-only members"
            }
            Self::ResponseContainsRequestMembers => {
                "JSON-RPC response contains request-only members"
            }
            Self::InvalidResponsePayload => {
                "JSON-RPC response must contain exactly one of result or error"
            }
            Self::InvalidParams => "JSON-RPC params must be null, an object, or an array",
            Self::InvalidErrorObject => "invalid JSON-RPC error object",
        };
        formatter.write_str(message)
    }
}

/// A framing, serialization, envelope, or response-correlation failure.
#[derive(Debug)]
pub enum JsonRpcError {
    /// An I/O operation failed.
    Io {
        /// The operation being performed.
        operation: &'static str,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// EOF occurred after a frame had begun but before it was complete.
    UnexpectedEof {
        /// The incomplete portion of the frame.
        section: FrameSection,
    },
    /// An unterminated or terminated header exceeded its configured bound.
    HeaderTooLarge {
        /// The maximum accepted header size.
        limit: usize,
    },
    /// A header contained a non-ASCII byte.
    NonAsciiHeader,
    /// A header line was malformed.
    MalformedHeader,
    /// The required `Content-Length` header was absent.
    MissingContentLength,
    /// More than one `Content-Length` header was present.
    DuplicateContentLength,
    /// A `Content-Length` value was not an unsigned decimal integer.
    InvalidContentLength,
    /// A `Content-Length` value overflowed the platform's `usize`.
    ContentLengthOverflow,
    /// More than one `Content-Type` header was present.
    DuplicateContentType,
    /// A `Content-Type` media type, parameter, or charset was unsupported.
    UnsupportedContentType,
    /// An inbound JSON-RPC body exceeded its configured bound.
    InboundBodyTooLarge {
        /// The declared inbound body length.
        length: usize,
        /// The maximum accepted body length.
        limit: usize,
    },
    /// The exactly serialized outbound JSON-RPC body exceeded its configured bound.
    OutboundBodyTooLarge {
        /// The serialized outbound body length.
        length: usize,
        /// The maximum accepted body length.
        limit: usize,
    },
    /// An inbound body was not valid JSON.
    MalformedJson {
        /// The parser error, which contains location but not source text.
        source: serde_json::Error,
    },
    /// An inbound JSON body exceeded its configured array/object nesting bound.
    JsonNestingTooDeep {
        /// The first observed depth above the configured limit.
        depth: usize,
        /// The maximum accepted array/object nesting depth.
        limit: usize,
    },
    /// JSON-RPC batch messages are unsupported.
    BatchUnsupported,
    /// A JSON-RPC envelope violated the protocol shape.
    InvalidEnvelope {
        /// The deterministic violation category.
        violation: EnvelopeViolation,
    },
    /// An ID was not an accepted integer or string in its message context.
    InvalidId,
    /// An encoded server request ID exceeded its configured bound.
    IdTooLarge {
        /// The encoded ID length.
        length: usize,
        /// The maximum accepted encoded length.
        limit: usize,
    },
    /// A method name exceeded its configured bound.
    MethodTooLong {
        /// The UTF-8 method-name length.
        length: usize,
        /// The maximum accepted length.
        limit: usize,
    },
    /// Serialized request or notification parameters exceeded their configured bound.
    ParamsTooLarge {
        /// The serialized parameter length.
        length: usize,
        /// The maximum accepted length.
        limit: usize,
    },
    /// The pending-client-request limit was reached.
    PendingRequestLimit {
        /// The maximum pending request count.
        limit: usize,
    },
    /// No additional monotonically increasing client request ID was available.
    ClientRequestIdsExhausted,
    /// A response repeated an already completed client request ID.
    DuplicateResponseId {
        /// The repeated client request ID.
        id: ClientRequestId,
    },
    /// A response did not correspond to an issued client request ID.
    UnknownResponseId {
        /// The unrecognized client request ID.
        id: ClientRequestId,
    },
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::UnexpectedEof { section } => {
                write!(formatter, "unexpected EOF while reading {section:?}")
            }
            Self::HeaderTooLarge { limit } => {
                write!(formatter, "LSP header exceeds {limit} bytes")
            }
            Self::NonAsciiHeader => formatter.write_str("LSP header is not ASCII"),
            Self::MalformedHeader => formatter.write_str("malformed LSP header"),
            Self::MissingContentLength => formatter.write_str("missing Content-Length header"),
            Self::DuplicateContentLength => formatter.write_str("duplicate Content-Length header"),
            Self::InvalidContentLength => {
                formatter.write_str("Content-Length is not an unsigned decimal integer")
            }
            Self::ContentLengthOverflow => formatter.write_str("Content-Length overflow"),
            Self::DuplicateContentType => formatter.write_str("duplicate Content-Type header"),
            Self::UnsupportedContentType => formatter.write_str("unsupported Content-Type"),
            Self::InboundBodyTooLarge { length, limit } => {
                write!(
                    formatter,
                    "inbound body is {length} bytes; limit is {limit}"
                )
            }
            Self::OutboundBodyTooLarge { length, limit } => {
                write!(
                    formatter,
                    "outbound body is {length} bytes; limit is {limit}"
                )
            }
            Self::MalformedJson { source } => write!(formatter, "malformed JSON: {source}"),
            Self::JsonNestingTooDeep { depth, limit } => {
                write!(formatter, "JSON nesting depth is {depth}; limit is {limit}")
            }
            Self::BatchUnsupported => formatter.write_str("JSON-RPC batches are unsupported"),
            Self::InvalidEnvelope { violation } => violation.fmt(formatter),
            Self::InvalidId => formatter.write_str("invalid JSON-RPC ID"),
            Self::IdTooLarge { length, limit } => {
                write!(formatter, "JSON-RPC ID is {length} bytes; limit is {limit}")
            }
            Self::MethodTooLong { length, limit } => {
                write!(
                    formatter,
                    "JSON-RPC method is {length} bytes; limit is {limit}"
                )
            }
            Self::ParamsTooLarge { length, limit } => {
                write!(
                    formatter,
                    "JSON-RPC params are {length} bytes; limit is {limit}"
                )
            }
            Self::PendingRequestLimit { limit } => {
                write!(formatter, "pending client request limit of {limit} reached")
            }
            Self::ClientRequestIdsExhausted => formatter.write_str("client request IDs exhausted"),
            Self::DuplicateResponseId { id } => {
                write!(formatter, "duplicate response ID {}", id.get())
            }
            Self::UnknownResponseId { id } => {
                write!(formatter, "unknown response ID {}", id.get())
            }
        }
    }
}

impl std::error::Error for JsonRpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::MalformedJson { source } => Some(source),
            _ => None,
        }
    }
}

/// An integer or string ID on a request initiated by the server.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ServerRequestId {
    /// A signed JSON integer.
    Integer(i64),
    /// A nonnegative JSON integer outside the signed range.
    Unsigned(u64),
    /// A JSON string.
    String(String),
}

impl ServerRequestId {
    /// Returns this ID in a form suitable for a JSON-RPC response.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Integer(value) => Value::Number(Number::from(*value)),
            Self::Unsigned(value) => Value::Number(Number::from(*value)),
            Self::String(value) => Value::String(value.clone()),
        }
    }
}

/// A monotonically allocated request ID owned by the srcmv client.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ClientRequestId(u64);

impl ClientRequestId {
    /// Returns the integer sent on the JSON-RPC wire.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns this ID in a form suitable for an outbound JSON-RPC request.
    #[must_use]
    pub fn to_json(self) -> Value {
        Value::Number(Number::from(self.0))
    }
}

/// A request initiated by the language server.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerRequest {
    /// The server-owned request ID.
    pub id: ServerRequestId,
    /// The request method.
    pub method: String,
    /// Optional structured parameters.
    pub params: Option<Value>,
}

/// A notification emitted by the language server.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerNotification {
    /// The notification method.
    pub method: String,
    /// Optional structured parameters.
    pub params: Option<Value>,
}

/// A structured JSON-RPC error returned for a client request.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponseError {
    /// The JSON-RPC error code.
    pub code: i64,
    /// The bounded-by-frame error message.
    pub message: String,
    /// Optional error data.
    pub data: Option<Value>,
}

/// The mutually exclusive payload of a client response.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponsePayload {
    /// A successful result, including JSON `null`.
    Result(Value),
    /// A JSON-RPC error object.
    Error(ResponseError),
}

/// A response to a request initiated by the srcmv client.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientResponse {
    /// The client-owned response ID.
    pub id: ClientRequestId,
    /// The exactly one result or error payload.
    pub payload: ResponsePayload,
}

/// A validated inbound JSON-RPC message.
#[derive(Clone, Debug, PartialEq)]
pub enum IncomingMessage {
    /// A request initiated by the language server.
    Request(ServerRequest),
    /// A notification emitted by the language server.
    Notification(ServerNotification),
    /// A response to a srcmv client request.
    Response(ClientResponse),
}

#[derive(Debug)]
enum DecoderState {
    Header(Vec<u8>),
    Body { bytes: Vec<u8>, expected: usize },
}

/// Incrementally decodes bounded LSP frames from arbitrary byte chunks.
///
/// The decoder never waits for additional input. Callers can therefore read
/// stdout using their own poll, channel, or deadline mechanism, pass each
/// available chunk to [`Self::push`], and call [`Self::finish`] at EOF.
#[derive(Debug)]
pub struct FrameDecoder {
    limits: FramingLimits,
    state: DecoderState,
}

impl FrameDecoder {
    /// Creates an empty decoder with the supplied directional limits.
    #[must_use]
    pub fn new(limits: FramingLimits) -> Self {
        Self {
            limits,
            state: DecoderState::Header(Vec::with_capacity(limits.max_header_bytes.min(1024))),
        }
    }

    /// Reports whether a frame has been partially decoded and awaits more input.
    ///
    /// This is `false` between frames: a fresh decoder and a decoder that just
    /// completed a body both report `false`.
    #[must_use]
    pub fn is_mid_frame(&self) -> bool {
        match &self.state {
            DecoderState::Header(header) => !header.is_empty(),
            DecoderState::Body { .. } => true,
        }
    }

    /// Consumes an available input chunk and returns every completed body.
    ///
    /// An empty body is a completed frame. Empty input is a no-op. After any
    /// returned error, the caller must discard the decoder and enter cleanup.
    pub fn push(&mut self, mut chunk: &[u8]) -> Result<Vec<Vec<u8>>, JsonRpcError> {
        const TERMINATOR: &[u8; 4] = b"\r\n\r\n";
        let mut completed = Vec::new();

        while !chunk.is_empty() {
            match &mut self.state {
                DecoderState::Header(header) => {
                    let byte = chunk[0];
                    chunk = &chunk[1..];
                    if header.len() == self.limits.max_header_bytes {
                        return Err(JsonRpcError::HeaderTooLarge {
                            limit: self.limits.max_header_bytes,
                        });
                    }
                    if !byte.is_ascii() {
                        return Err(JsonRpcError::NonAsciiHeader);
                    }
                    header.push(byte);
                    if header.ends_with(TERMINATOR) {
                        let content_length = parse_header(header)?;
                        if content_length > self.limits.max_inbound_body_bytes {
                            return Err(JsonRpcError::InboundBodyTooLarge {
                                length: content_length,
                                limit: self.limits.max_inbound_body_bytes,
                            });
                        }
                        if content_length == 0 {
                            completed.push(Vec::new());
                            header.clear();
                        } else {
                            self.state = DecoderState::Body {
                                bytes: Vec::with_capacity(content_length),
                                expected: content_length,
                            };
                        }
                    } else if header.len() == self.limits.max_header_bytes {
                        return Err(JsonRpcError::HeaderTooLarge {
                            limit: self.limits.max_header_bytes,
                        });
                    }
                }
                DecoderState::Body { bytes, expected } => {
                    let remaining = expected.saturating_sub(bytes.len());
                    let take = remaining.min(chunk.len());
                    bytes.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];
                    if bytes.len() == *expected {
                        let body = std::mem::take(bytes);
                        completed.push(body);
                        self.state = DecoderState::Header(Vec::with_capacity(
                            self.limits.max_header_bytes.min(1024),
                        ));
                    }
                }
            }
        }
        Ok(completed)
    }

    /// Validates that EOF occurred between complete frames.
    pub fn finish(self) -> Result<(), JsonRpcError> {
        match self.state {
            DecoderState::Header(header) if header.is_empty() => Ok(()),
            DecoderState::Header(_) => Err(JsonRpcError::UnexpectedEof {
                section: FrameSection::Header,
            }),
            DecoderState::Body { .. } => Err(JsonRpcError::UnexpectedEof {
                section: FrameSection::Body,
            }),
        }
    }
}

/// Reads and validates one framed inbound JSON-RPC message.
///
/// `Ok(None)` means EOF occurred between frames. EOF after any bytes of a frame
/// have been received is an error.
pub fn read_message<R: Read>(
    reader: &mut R,
    framing_limits: FramingLimits,
    envelope_limits: EnvelopeLimits,
) -> Result<Option<IncomingMessage>, JsonRpcError> {
    let Some(body) = read_body(reader, framing_limits)? else {
        return Ok(None);
    };
    decode_body(&body, envelope_limits).map(Some)
}

/// Parses and classifies one already framed JSON-RPC body.
///
/// This byte-oriented entry point is suitable for deterministic corpus tests
/// and fuzz harnesses without constructing an I/O adapter.
pub fn decode_body(
    body: &[u8],
    envelope_limits: EnvelopeLimits,
) -> Result<IncomingMessage, JsonRpcError> {
    validate_raw_json_depth(body, envelope_limits.max_json_depth)?;
    let value =
        serde_json::from_slice(body).map_err(|source| JsonRpcError::MalformedJson { source })?;
    classify_message(value, envelope_limits)
}

fn validate_raw_json_depth(body: &[u8], limit: usize) -> Result<(), JsonRpcError> {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for byte in body {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match *byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                if depth > limit {
                    return Err(JsonRpcError::JsonNestingTooDeep { depth, limit });
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// Reads one bounded LSP body without interpreting its JSON contents.
pub fn read_body<R: Read>(
    reader: &mut R,
    limits: FramingLimits,
) -> Result<Option<Vec<u8>>, JsonRpcError> {
    let header = read_header(reader, limits.max_header_bytes)?;
    let Some(header) = header else {
        return Ok(None);
    };
    let content_length = parse_header(&header)?;
    if content_length > limits.max_inbound_body_bytes {
        return Err(JsonRpcError::InboundBodyTooLarge {
            length: content_length,
            limit: limits.max_inbound_body_bytes,
        });
    }

    let mut body = vec![0_u8; content_length];
    read_exact_section(reader, &mut body, FrameSection::Body)?;
    Ok(Some(body))
}

/// Classifies and validates one parsed inbound JSON-RPC value.
pub fn classify_message(
    value: Value,
    limits: EnvelopeLimits,
) -> Result<IncomingMessage, JsonRpcError> {
    if value.is_array() {
        return Err(JsonRpcError::BatchUnsupported);
    }
    let Value::Object(mut object) = value else {
        return invalid_envelope(EnvelopeViolation::InvalidShape);
    };

    match object.remove("jsonrpc") {
        Some(Value::String(version)) if version == "2.0" => {}
        _ => return invalid_envelope(EnvelopeViolation::InvalidVersion),
    }

    if object.contains_key("method") {
        classify_request_or_notification(object, limits)
    } else {
        classify_response(object)
    }
}

/// Serializes and frames one outbound JSON value after exact byte-size preflight.
pub fn encode_message(value: &Value, limits: FramingLimits) -> Result<Vec<u8>, JsonRpcError> {
    let body =
        serde_json::to_vec(value).map_err(|source| JsonRpcError::MalformedJson { source })?;
    if body.len() > limits.max_outbound_body_bytes {
        return Err(JsonRpcError::OutboundBodyTooLarge {
            length: body.len(),
            limit: limits.max_outbound_body_bytes,
        });
    }

    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let capacity =
        header
            .len()
            .checked_add(body.len())
            .ok_or(JsonRpcError::OutboundBodyTooLarge {
                length: body.len(),
                limit: limits.max_outbound_body_bytes,
            })?;
    let mut frame = Vec::with_capacity(capacity);
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Allocates client request IDs and rejects duplicate or unknown responses.
#[derive(Debug)]
pub struct ResponseCorrelator {
    next_id: u64,
    pending: BTreeSet<ClientRequestId>,
    max_pending: usize,
}

impl ResponseCorrelator {
    /// Creates a correlator whose first allocated ID is one.
    #[must_use]
    pub fn new(max_pending: usize) -> Self {
        Self {
            next_id: 1,
            pending: BTreeSet::new(),
            max_pending,
        }
    }

    /// Allocates and records a new pending client request ID.
    pub fn begin_request(&mut self) -> Result<ClientRequestId, JsonRpcError> {
        if self.pending.len() >= self.max_pending {
            return Err(JsonRpcError::PendingRequestLimit {
                limit: self.max_pending,
            });
        }
        let following = self
            .next_id
            .checked_add(1)
            .ok_or(JsonRpcError::ClientRequestIdsExhausted)?;
        let id = ClientRequestId(self.next_id);
        self.next_id = following;
        let inserted = self.pending.insert(id);
        debug_assert!(inserted, "monotonic request ID must be new");
        Ok(id)
    }

    /// Completes a pending response ID or classifies it as duplicate or unknown.
    pub fn complete(&mut self, id: ClientRequestId) -> Result<(), JsonRpcError> {
        if self.pending.remove(&id) {
            return Ok(());
        }
        if id.get() > 0 && id.get() < self.next_id {
            Err(JsonRpcError::DuplicateResponseId { id })
        } else {
            Err(JsonRpcError::UnknownResponseId { id })
        }
    }

    /// Returns the current number of requests awaiting a response.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Default for ResponseCorrelator {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PENDING_REQUESTS)
    }
}

fn read_header<R: Read>(reader: &mut R, limit: usize) -> Result<Option<Vec<u8>>, JsonRpcError> {
    const TERMINATOR: &[u8; 4] = b"\r\n\r\n";
    let mut header = Vec::with_capacity(limit.min(1024));
    let mut byte = [0_u8; 1];

    loop {
        let read = match reader.read(&mut byte) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(JsonRpcError::Io {
                    operation: "read LSP header",
                    source,
                });
            }
        };
        if read == 0 {
            return if header.is_empty() {
                Ok(None)
            } else {
                Err(JsonRpcError::UnexpectedEof {
                    section: FrameSection::Header,
                })
            };
        }
        if header.len() == limit {
            return Err(JsonRpcError::HeaderTooLarge { limit });
        }
        if !byte[0].is_ascii() {
            return Err(JsonRpcError::NonAsciiHeader);
        }
        header.push(byte[0]);
        if header.ends_with(TERMINATOR) {
            return Ok(Some(header));
        }
        if header.len() == limit {
            return Err(JsonRpcError::HeaderTooLarge { limit });
        }
    }
}

fn parse_header(header: &[u8]) -> Result<usize, JsonRpcError> {
    let payload = header
        .strip_suffix(b"\r\n\r\n")
        .ok_or(JsonRpcError::MalformedHeader)?;
    let payload = std::str::from_utf8(payload).map_err(|_| JsonRpcError::NonAsciiHeader)?;
    let mut content_length = None;
    let mut saw_content_type = false;

    for line in payload.split("\r\n") {
        if line.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
            return Err(JsonRpcError::MalformedHeader);
        }
        let line = line.as_bytes();
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(JsonRpcError::MalformedHeader)?;
        let name = &line[..colon];
        let value = trim_ascii_whitespace(&line[colon + 1..]);
        if name.is_empty()
            || !name.iter().copied().all(is_header_name_byte)
            || !value
                .iter()
                .all(|byte| *byte == b'\t' || matches!(*byte, b' '..=b'~'))
        {
            return Err(JsonRpcError::MalformedHeader);
        }

        if name.eq_ignore_ascii_case(b"Content-Length") {
            if content_length.is_some() {
                return Err(JsonRpcError::DuplicateContentLength);
            }
            content_length = Some(parse_content_length(value)?);
        } else if name.eq_ignore_ascii_case(b"Content-Type") {
            if saw_content_type {
                return Err(JsonRpcError::DuplicateContentType);
            }
            saw_content_type = true;
            validate_content_type(value)?;
        }
    }

    content_length.ok_or(JsonRpcError::MissingContentLength)
}

fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn parse_content_length(value: &[u8]) -> Result<usize, JsonRpcError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(JsonRpcError::InvalidContentLength);
    }
    value.iter().try_fold(0_usize, |length, digit| {
        length
            .checked_mul(10)
            .and_then(|length| length.checked_add(usize::from(*digit - b'0')))
            .ok_or(JsonRpcError::ContentLengthOverflow)
    })
}

fn validate_content_type(value: &[u8]) -> Result<(), JsonRpcError> {
    let mut components = value.split(|byte| *byte == b';');
    let media_type = trim_ascii_whitespace(components.next().unwrap_or_default());
    if !media_type.eq_ignore_ascii_case(b"application/vscode-jsonrpc") {
        return Err(JsonRpcError::UnsupportedContentType);
    }

    let mut saw_charset = false;
    for component in components {
        let component = trim_ascii_whitespace(component);
        let equals = component
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or(JsonRpcError::UnsupportedContentType)?;
        let name = trim_ascii_whitespace(&component[..equals]);
        let parameter_value = trim_ascii_whitespace(&component[equals + 1..]);
        if !name.eq_ignore_ascii_case(b"charset") || saw_charset {
            return Err(JsonRpcError::UnsupportedContentType);
        }
        saw_charset = true;
        if !parameter_value.eq_ignore_ascii_case(b"utf-8")
            && !parameter_value.eq_ignore_ascii_case(b"utf8")
        {
            return Err(JsonRpcError::UnsupportedContentType);
        }
    }
    Ok(())
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        bytes = &bytes[1..];
    }
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn read_exact_section<R: Read>(
    reader: &mut R,
    mut buffer: &mut [u8],
    section: FrameSection,
) -> Result<(), JsonRpcError> {
    while !buffer.is_empty() {
        match reader.read(buffer) {
            Ok(0) => return Err(JsonRpcError::UnexpectedEof { section }),
            Ok(read) => buffer = &mut buffer[read..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => {
                return Err(JsonRpcError::Io {
                    operation: "read LSP body",
                    source,
                });
            }
        }
    }
    Ok(())
}

fn classify_request_or_notification(
    mut object: Map<String, Value>,
    limits: EnvelopeLimits,
) -> Result<IncomingMessage, JsonRpcError> {
    if object.contains_key("result") || object.contains_key("error") {
        return invalid_envelope(EnvelopeViolation::RequestContainsResponseMembers);
    }
    let method = match object.remove("method") {
        Some(Value::String(method)) => method,
        _ => return invalid_envelope(EnvelopeViolation::InvalidShape),
    };
    if method.len() > limits.max_method_bytes {
        return Err(JsonRpcError::MethodTooLong {
            length: method.len(),
            limit: limits.max_method_bytes,
        });
    }
    let params = object.remove("params");
    validate_params(params.as_ref(), limits.max_params_bytes)?;

    if let Some(id) = object.remove("id") {
        let id = parse_server_request_id(id, limits.max_id_bytes)?;
        Ok(IncomingMessage::Request(ServerRequest {
            id,
            method,
            params,
        }))
    } else {
        Ok(IncomingMessage::Notification(ServerNotification {
            method,
            params,
        }))
    }
}

fn classify_response(mut object: Map<String, Value>) -> Result<IncomingMessage, JsonRpcError> {
    if object.contains_key("params") {
        return invalid_envelope(EnvelopeViolation::ResponseContainsRequestMembers);
    }
    let id = match object.remove("id") {
        Some(Value::Number(number)) => number
            .as_u64()
            .map(ClientRequestId)
            .ok_or(JsonRpcError::InvalidId)?,
        _ => return Err(JsonRpcError::InvalidId),
    };

    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_result == has_error {
        return invalid_envelope(EnvelopeViolation::InvalidResponsePayload);
    }
    let payload = if let Some(result) = object.remove("result") {
        ResponsePayload::Result(result)
    } else {
        ResponsePayload::Error(parse_response_error(object.remove("error").ok_or(
            JsonRpcError::InvalidEnvelope {
                violation: EnvelopeViolation::InvalidResponsePayload,
            },
        )?)?)
    };

    Ok(IncomingMessage::Response(ClientResponse { id, payload }))
}

fn validate_params(params: Option<&Value>, limit: usize) -> Result<(), JsonRpcError> {
    let Some(params) = params else {
        return Ok(());
    };
    if !params.is_null() && !params.is_array() && !params.is_object() {
        return invalid_envelope(EnvelopeViolation::InvalidParams);
    }
    let length = serde_json::to_vec(params)
        .map_err(|source| JsonRpcError::MalformedJson { source })?
        .len();
    if length > limit {
        return Err(JsonRpcError::ParamsTooLarge { length, limit });
    }
    Ok(())
}

fn parse_server_request_id(value: Value, limit: usize) -> Result<ServerRequestId, JsonRpcError> {
    let length = serde_json::to_vec(&value)
        .map_err(|source| JsonRpcError::MalformedJson { source })?
        .len();
    if length > limit {
        return Err(JsonRpcError::IdTooLarge { length, limit });
    }
    match value {
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Ok(ServerRequestId::Integer(integer))
            } else if let Some(integer) = number.as_u64() {
                Ok(ServerRequestId::Unsigned(integer))
            } else {
                Err(JsonRpcError::InvalidId)
            }
        }
        Value::String(string) => Ok(ServerRequestId::String(string)),
        _ => Err(JsonRpcError::InvalidId),
    }
}

fn parse_response_error(value: Value) -> Result<ResponseError, JsonRpcError> {
    let Value::Object(mut object) = value else {
        return invalid_envelope(EnvelopeViolation::InvalidErrorObject);
    };
    let code = match object.remove("code") {
        Some(Value::Number(number)) => number.as_i64(),
        _ => None,
    }
    .ok_or(JsonRpcError::InvalidEnvelope {
        violation: EnvelopeViolation::InvalidErrorObject,
    })?;
    let message = match object.remove("message") {
        Some(Value::String(message)) => message,
        _ => return invalid_envelope(EnvelopeViolation::InvalidErrorObject),
    };
    let data = object.remove("data");
    Ok(ResponseError {
        code,
        message,
        data,
    })
}

fn invalid_envelope<T>(violation: EnvelopeViolation) -> Result<T, JsonRpcError> {
    Err(JsonRpcError::InvalidEnvelope { violation })
}
