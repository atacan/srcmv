//! Deterministic regression tests for bounded LSP framing and JSON-RPC envelopes.

use std::io::{self, Cursor, Read};

use serde_json::{Value, json};
use srcmv_lsp::jsonrpc::{
    ClientRequestId, EnvelopeLimits, EnvelopeViolation, FrameDecoder, FrameSection, FramingLimits,
    IncomingMessage, JsonRpcError, ResponseCorrelator, ResponsePayload, ServerRequestId,
    classify_message, decode_body, encode_message, read_body, read_message,
};

fn framing_limits(header: usize, inbound: usize, outbound: usize) -> FramingLimits {
    FramingLimits {
        max_header_bytes: header,
        max_inbound_body_bytes: inbound,
        max_outbound_body_bytes: outbound,
    }
}

fn frame(value: &Value) -> Vec<u8> {
    encode_message(value, FramingLimits::default()).expect("test frame should encode")
}

#[test]
fn defaults_freeze_phase_zero_limits() {
    let framing = FramingLimits::default();
    assert_eq!(framing.max_header_bytes, 16 * 1024);
    assert_eq!(framing.max_inbound_body_bytes, 16 * 1024 * 1024);
    assert_eq!(framing.max_outbound_body_bytes, 64 * 1024 * 1024);

    let envelope = EnvelopeLimits::default();
    assert_eq!(envelope.max_json_depth, 64);
    assert_eq!(envelope.max_id_bytes, 256);
    assert_eq!(envelope.max_method_bytes, 256);
    assert_eq!(envelope.max_params_bytes, 1024 * 1024);
}

#[test]
fn reads_split_header_and_body_and_clean_eof() {
    let bytes = frame(&json!({"jsonrpc":"2.0","method":"ready","params":{}}));
    let mut reader = OneByteReader::new(bytes);
    let message = read_message(
        &mut reader,
        FramingLimits::default(),
        EnvelopeLimits::default(),
    )
    .expect("split frame should parse")
    .expect("one message should exist");
    assert!(matches!(message, IncomingMessage::Notification(_)));
    assert!(
        read_message(
            &mut reader,
            FramingLimits::default(),
            EnvelopeLimits::default()
        )
        .expect("between-frame EOF is clean")
        .is_none()
    );
}

#[test]
fn header_read_retries_interrupted_io() {
    let bytes = frame(&json!({"jsonrpc":"2.0","method":"ready"}));
    let mut reader = InterruptOnceReader::new(bytes);
    let message = read_message(
        &mut reader,
        FramingLimits::default(),
        EnvelopeLimits::default(),
    )
    .expect("interrupted read should be retried")
    .expect("fixture contains a message");
    assert!(matches!(message, IncomingMessage::Notification(_)));
}

#[test]
fn incremental_decoder_handles_every_split_and_concatenated_frames() {
    let first_body = serde_json::to_vec(&json!({"jsonrpc":"2.0","method":"first"}))
        .expect("fixture should serialize");
    let second_body = serde_json::to_vec(&json!({"jsonrpc":"2.0","method":"second"}))
        .expect("fixture should serialize");
    let mut bytes = frame(&json!({"jsonrpc":"2.0","method":"first"}));
    bytes.extend_from_slice(&frame(&json!({"jsonrpc":"2.0","method":"second"})));

    for split in 0..=bytes.len() {
        let mut decoder = FrameDecoder::new(FramingLimits::default());
        let mut bodies = decoder.push(&bytes[..split]).expect("prefix should decode");
        bodies.extend(decoder.push(&bytes[split..]).expect("suffix should decode"));
        decoder.finish().expect("both frames are complete");
        assert_eq!(bodies, [first_body.clone(), second_body.clone()]);
    }
}

#[test]
fn incremental_decoder_enforces_header_limit_before_terminator() {
    let mut decoder = FrameDecoder::new(framing_limits(8, 8, 8));
    let error = decoder
        .push(b"12345678")
        .expect_err("a full header bound without a terminator cannot become valid");
    assert!(matches!(error, JsonRpcError::HeaderTooLarge { limit: 8 }));
}

#[test]
fn incremental_decoder_checks_eof_state() {
    let mut header_decoder = FrameDecoder::new(FramingLimits::default());
    header_decoder
        .push(b"Content-Length:")
        .expect("partial header should remain buffered");
    assert!(matches!(
        header_decoder.finish(),
        Err(JsonRpcError::UnexpectedEof {
            section: FrameSection::Header
        })
    ));

    let mut body_decoder = FrameDecoder::new(FramingLimits::default());
    body_decoder
        .push(b"Content-Length: 2\r\n\r\n{")
        .expect("partial body should remain buffered");
    assert!(matches!(
        body_decoder.finish(),
        Err(JsonRpcError::UnexpectedEof {
            section: FrameSection::Body
        })
    ));

    FrameDecoder::new(FramingLimits::default())
        .finish()
        .expect("EOF between frames is clean");
}

#[test]
fn incremental_decoder_emits_empty_body_then_continues_same_chunk() {
    let mut decoder = FrameDecoder::new(framing_limits(64, 2, 2));
    let bodies = decoder
        .push(b"Content-Length: 0\r\n\r\nContent-Length: 2\r\n\r\n{}")
        .expect("concatenated frames should decode");
    assert_eq!(bodies, [Vec::new(), b"{}".to_vec()]);
    decoder.finish().expect("decoder should end between frames");
}

#[test]
fn header_limit_accepts_at_boundary_and_rejects_above_without_terminator() {
    let at = b"Content-Length: 0\r\n\r\n";
    assert_eq!(
        read_body(&mut Cursor::new(at), framing_limits(at.len() + 1, 0, 1))
            .expect("header below limit should pass"),
        Some(Vec::new())
    );
    let limits = framing_limits(at.len(), 0, 1);
    assert_eq!(
        read_body(&mut Cursor::new(at), limits).expect("exact limit should pass"),
        Some(Vec::new())
    );

    let mut overlong = vec![b'X'; 9];
    let error = read_body(&mut Cursor::new(&mut overlong), framing_limits(8, 1, 1))
        .expect_err("unterminated header must stop at the bound");
    assert!(matches!(error, JsonRpcError::HeaderTooLarge { limit: 8 }));
}

#[test]
fn rejects_incomplete_header_and_body() {
    let header_error = read_body(
        &mut Cursor::new(b"Content-Length: 1\r\n"),
        FramingLimits::default(),
    )
    .expect_err("partial header should fail");
    assert!(matches!(
        header_error,
        JsonRpcError::UnexpectedEof {
            section: FrameSection::Header
        }
    ));

    let body_error = read_body(
        &mut Cursor::new(b"Content-Length: 2\r\n\r\n{"),
        FramingLimits::default(),
    )
    .expect_err("partial body should fail");
    assert!(matches!(
        body_error,
        JsonRpcError::UnexpectedEof {
            section: FrameSection::Body
        }
    ));
}

#[test]
fn rejects_missing_duplicate_invalid_and_overflowing_lengths() {
    type ErrorPredicate = fn(&JsonRpcError) -> bool;
    let cases: &[(&[u8], ErrorPredicate)] = &[
        (b"Other: 0\r\n\r\n", |error| {
            matches!(error, JsonRpcError::MissingContentLength)
        }),
        (b"Content-Length: 0\r\nContent-Length: 0\r\n\r\n", |error| {
            matches!(error, JsonRpcError::DuplicateContentLength)
        }),
        (b"Content-Length: +1\r\n\r\n", |error| {
            matches!(error, JsonRpcError::InvalidContentLength)
        }),
        (b"Content-Length: -1\r\n\r\n", |error| {
            matches!(error, JsonRpcError::InvalidContentLength)
        }),
        (b"Content-Length: 1x\r\n\r\n", |error| {
            matches!(error, JsonRpcError::InvalidContentLength)
        }),
        (
            b"Content-Length: 999999999999999999999999999999999999\r\n\r\n",
            |error| matches!(error, JsonRpcError::ContentLengthOverflow),
        ),
    ];

    for (input, predicate) in cases {
        let error = read_body(&mut Cursor::new(input), FramingLimits::default())
            .expect_err("invalid length must fail");
        assert!(predicate(&error), "unexpected error: {error}");
    }
}

#[test]
fn rejects_malformed_and_non_ascii_headers() {
    for input in [
        b"Content-Length 0\r\n\r\n".as_slice(),
        b"Content-Length: 0\n\n".as_slice(),
        b" Content-Length: 0\r\n\r\n".as_slice(),
        b"Bad Name: x\r\nContent-Length: 0\r\n\r\n".as_slice(),
        b"X-Name: \0\r\nContent-Length: 0\r\n\r\n".as_slice(),
    ] {
        let error = read_body(&mut Cursor::new(input), FramingLimits::default())
            .expect_err("malformed header must fail");
        assert!(matches!(
            error,
            JsonRpcError::MalformedHeader | JsonRpcError::UnexpectedEof { .. }
        ));
    }

    let error = read_body(
        &mut Cursor::new(b"X-Name: \xff\r\nContent-Length: 0\r\n\r\n"),
        FramingLimits::default(),
    )
    .expect_err("non-ASCII header must fail");
    assert!(matches!(error, JsonRpcError::NonAsciiHeader));
}

#[test]
fn content_type_accepts_utf8_spellings_and_rejects_others() {
    for charset in ["utf-8", "UTF-8", "utf8", "UTF8"] {
        let input = format!(
            "Content-Length: 0\r\nContent-Type: application/vscode-jsonrpc; charset={charset}\r\n\r\n"
        );
        assert_eq!(
            read_body(&mut Cursor::new(input), FramingLimits::default())
                .expect("compatible charset should pass"),
            Some(Vec::new())
        );
    }
    assert_eq!(
        read_body(
            &mut Cursor::new(
                b"Content-Length: 0\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n"
            ),
            FramingLimits::default()
        )
        .expect("omitted charset defaults to UTF-8"),
        Some(Vec::new())
    );

    for content_type in [
        "application/vscode-jsonrpc; charset=utf-16",
        "application/json; charset=utf-8",
        "application/vscode-jsonrpc; boundary=x",
        "application/vscode-jsonrpc; charset=utf-8; charset=utf8",
    ] {
        let input = format!("Content-Length: 0\r\nContent-Type: {content_type}\r\n\r\n");
        let error = read_body(&mut Cursor::new(input), FramingLimits::default())
            .expect_err("unsupported content type must fail");
        assert!(matches!(error, JsonRpcError::UnsupportedContentType));
    }

    let duplicate = b"Content-Length: 0\r\nContent-Type: application/vscode-jsonrpc\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n";
    let error = read_body(&mut Cursor::new(duplicate), FramingLimits::default())
        .expect_err("duplicate content type must fail");
    assert!(matches!(error, JsonRpcError::DuplicateContentType));
}

#[test]
fn inbound_body_limit_is_checked_before_body_allocation_or_read() {
    let below = b"Content-Length: 2\r\n\r\n{}";
    assert_eq!(
        read_body(&mut Cursor::new(below), framing_limits(64, 3, 2))
            .expect("below inbound limit should pass"),
        Some(b"{}".to_vec())
    );
    assert_eq!(
        read_body(&mut Cursor::new(below), framing_limits(64, 2, 2))
            .expect("at inbound limit should pass"),
        Some(b"{}".to_vec())
    );

    let error = read_body(
        &mut Cursor::new(b"Content-Length: 3\r\n\r\n"),
        framing_limits(64, 2, 2),
    )
    .expect_err("declared body above limit must fail before reading it");
    assert!(matches!(
        error,
        JsonRpcError::InboundBodyTooLarge {
            length: 3,
            limit: 2
        }
    ));
}

#[test]
fn outbound_preflight_uses_exact_serialized_bytes_at_boundary() {
    let value = json!({"text":"\n\""});
    let exact = serde_json::to_vec(&value)
        .expect("value should serialize")
        .len();
    assert!(encode_message(&value, framing_limits(64, 64, exact + 1)).is_ok());
    assert!(encode_message(&value, framing_limits(64, 64, exact)).is_ok());
    let error = encode_message(&value, framing_limits(64, 64, exact - 1))
        .expect_err("escaped serialized bytes above limit must fail");
    assert!(matches!(
        error,
        JsonRpcError::OutboundBodyTooLarge { length, limit }
            if length == exact && limit == exact - 1
    ));
}

#[test]
fn malformed_json_and_batches_are_rejected() {
    let malformed = b"Content-Length: 1\r\n\r\n{";
    let error = read_message(
        &mut Cursor::new(malformed),
        FramingLimits::default(),
        EnvelopeLimits::default(),
    )
    .expect_err("malformed JSON must fail");
    assert!(matches!(error, JsonRpcError::MalformedJson { .. }));

    let error =
        classify_message(json!([]), EnvelopeLimits::default()).expect_err("batch must fail");
    assert!(matches!(error, JsonRpcError::BatchUnsupported));
}

#[test]
fn raw_json_array_depth_is_bounded_below_at_and_above_limit() {
    let limits = EnvelopeLimits::default();
    for array_depth in [62, 63] {
        let body = nested_params(array_depth, '[', ']');
        assert!(
            decode_body(body.as_bytes(), limits).is_ok(),
            "top-level object plus {array_depth} arrays should fit"
        );
    }

    let body = nested_params(64, '[', ']');
    let error = decode_body(body.as_bytes(), limits)
        .expect_err("top-level object plus 64 arrays exceeds depth 64");
    assert!(matches!(
        error,
        JsonRpcError::JsonNestingTooDeep {
            depth: 65,
            limit: 64
        }
    ));
}

#[test]
fn raw_json_object_depth_is_bounded_below_at_and_above_limit() {
    let limits = EnvelopeLimits::default();
    for object_depth in [62, 63] {
        let body = nested_params(object_depth, '{', '}');
        assert!(
            decode_body(body.as_bytes(), limits).is_ok(),
            "top-level object plus {object_depth} objects should fit"
        );
    }

    let body = nested_params(64, '{', '}');
    let error = decode_body(body.as_bytes(), limits)
        .expect_err("top-level object plus 64 objects exceeds depth 64");
    assert!(matches!(
        error,
        JsonRpcError::JsonNestingTooDeep {
            depth: 65,
            limit: 64
        }
    ));
}

#[test]
fn depth_scanner_ignores_delimiters_and_escaped_quotes_inside_strings() {
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "log",
        "params": {"text": "[{\\\"nested-looking\\\": [{{{]}}}]}]"}
    }))
    .expect("fixture should serialize");
    let limits = EnvelopeLimits {
        max_json_depth: 2,
        ..EnvelopeLimits::default()
    };
    assert!(decode_body(&body, limits).is_ok());
}

#[test]
fn malformed_json_still_reports_the_serde_error() {
    let body = br#"{"jsonrpc":"2.0","method":"log","params":[}"#;
    let error = decode_body(body, EnvelopeLimits::default())
        .expect_err("mismatched delimiters are malformed JSON");
    assert!(matches!(error, JsonRpcError::MalformedJson { .. }));
}

#[test]
fn deterministic_envelope_regression_corpus_stays_rejected() {
    let corpus = [
        include_bytes!("../fuzz/corpus/jsonrpc-envelope/batch.json").as_slice(),
        include_bytes!("../fuzz/corpus/jsonrpc-envelope/invalid-version.json").as_slice(),
        include_bytes!("../fuzz/corpus/jsonrpc-envelope/both-result-error.json").as_slice(),
        include_bytes!("../fuzz/corpus/jsonrpc-envelope/invalid-request-id.json").as_slice(),
    ];
    for body in corpus {
        assert!(
            decode_body(body, EnvelopeLimits::default()).is_err(),
            "regression corpus entry unexpectedly became valid"
        );
    }
}

#[test]
fn invalid_version_and_non_object_messages_are_rejected() {
    for value in [
        json!(null),
        json!({"method":"x"}),
        json!({"jsonrpc":"1.0","method":"x"}),
        json!({"jsonrpc":2.0,"method":"x"}),
    ] {
        let error = classify_message(value, EnvelopeLimits::default())
            .expect_err("invalid envelope must fail");
        assert!(matches!(error, JsonRpcError::InvalidEnvelope { .. }));
    }
}

#[test]
fn classifies_integer_and_string_server_requests_and_notifications() {
    let integer = classify_message(
        json!({"jsonrpc":"2.0","id":-7,"method":"workspace/configuration","params":{}}),
        EnvelopeLimits::default(),
    )
    .expect("integer request should pass");
    assert!(matches!(
        integer,
        IncomingMessage::Request(ref request)
            if request.id == ServerRequestId::Integer(-7)
                && request.id.to_json() == json!(-7)
    ));

    let string = classify_message(
        json!({"jsonrpc":"2.0","id":"request-1","method":"window/showMessageRequest"}),
        EnvelopeLimits::default(),
    )
    .expect("string request should pass");
    assert!(matches!(
        string,
        IncomingMessage::Request(ref request)
            if request.id == ServerRequestId::String("request-1".to_owned())
    ));

    let notification = classify_message(
        json!({"jsonrpc":"2.0","method":"window/logMessage","params":[]}),
        EnvelopeLimits::default(),
    )
    .expect("notification should pass");
    assert!(matches!(notification, IncomingMessage::Notification(_)));
}

#[test]
fn validates_request_id_method_and_params_bounds() {
    let limits = EnvelopeLimits {
        max_json_depth: 64,
        max_id_bytes: 3,
        max_method_bytes: 3,
        max_params_bytes: 2,
    };
    assert!(
        classify_message(
            json!({"jsonrpc":"2.0","id":"x","method":"abc","params":{}}),
            limits
        )
        .is_ok()
    );

    let id_error = classify_message(json!({"jsonrpc":"2.0","id":"xx","method":"abc"}), limits)
        .expect_err("encoded ID includes quotes and is above limit");
    assert!(matches!(id_error, JsonRpcError::IdTooLarge { .. }));

    let method_error = classify_message(json!({"jsonrpc":"2.0","id":1,"method":"abcd"}), limits)
        .expect_err("method above limit must fail");
    assert!(matches!(method_error, JsonRpcError::MethodTooLong { .. }));

    let params_error = classify_message(
        json!({"jsonrpc":"2.0","id":1,"method":"abc","params":{"x":1}}),
        limits,
    )
    .expect_err("params above limit must fail");
    assert!(matches!(params_error, JsonRpcError::ParamsTooLarge { .. }));

    let scalar_error = classify_message(
        json!({"jsonrpc":"2.0","id":1,"method":"abc","params":1}),
        EnvelopeLimits::default(),
    )
    .expect_err("scalar params must fail");
    assert!(matches!(
        scalar_error,
        JsonRpcError::InvalidEnvelope {
            violation: EnvelopeViolation::InvalidParams
        }
    ));

    let null_params = classify_message(
        json!({
            "jsonrpc":"2.0",
            "id":"folders-request",
            "method":"workspace/workspaceFolders",
            "params":null
        }),
        EnvelopeLimits::default(),
    )
    .expect("parameterless LSP requests may use null params");
    assert!(matches!(null_params, IncomingMessage::Request(_)));
}

#[test]
fn null_float_and_boolean_server_request_ids_are_invalid() {
    for id in [json!(null), json!(1.5), json!(true)] {
        let error = classify_message(
            json!({"jsonrpc":"2.0","id":id,"method":"x"}),
            EnvelopeLimits::default(),
        )
        .expect_err("invalid request ID must fail");
        assert!(matches!(error, JsonRpcError::InvalidId));
    }
}

#[test]
fn responses_require_numeric_client_id_and_exactly_one_payload() {
    let success = classify_message(
        json!({"jsonrpc":"2.0","id":1,"result":null}),
        EnvelopeLimits::default(),
    )
    .expect("result response should pass");
    assert!(matches!(
        success,
        IncomingMessage::Response(ref response)
            if response.id.get() == 1 && response.payload == ResponsePayload::Result(Value::Null)
    ));

    let failure = classify_message(
        json!({"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"missing","data":{"method":"x"}}}),
        EnvelopeLimits::default(),
    )
    .expect("error response should pass");
    assert!(matches!(
        failure,
        IncomingMessage::Response(ref response)
            if matches!(&response.payload, ResponsePayload::Error(error) if error.code == -32601)
    ));

    for value in [
        json!({"jsonrpc":"2.0","id":1}),
        json!({"jsonrpc":"2.0","id":1,"result":null,"error":null}),
    ] {
        let error = classify_message(value, EnvelopeLimits::default())
            .expect_err("response payload shape must fail");
        assert!(matches!(
            error,
            JsonRpcError::InvalidEnvelope {
                violation: EnvelopeViolation::InvalidResponsePayload
            }
        ));
    }

    for id in [json!("1"), json!(-1), json!(null), json!(1.5)] {
        let error = classify_message(
            json!({"jsonrpc":"2.0","id":id,"result":null}),
            EnvelopeLimits::default(),
        )
        .expect_err("client response ID must be a nonnegative integer");
        assert!(matches!(error, JsonRpcError::InvalidId));
    }
}

#[test]
fn request_response_member_mix_and_invalid_error_object_are_rejected() {
    let request_error = classify_message(
        json!({"jsonrpc":"2.0","id":"x","method":"m","result":null}),
        EnvelopeLimits::default(),
    )
    .expect_err("request with response member must fail");
    assert!(matches!(
        request_error,
        JsonRpcError::InvalidEnvelope {
            violation: EnvelopeViolation::RequestContainsResponseMembers
        }
    ));

    let response_error = classify_message(
        json!({"jsonrpc":"2.0","id":1,"params":{},"result":null}),
        EnvelopeLimits::default(),
    )
    .expect_err("response with params must fail");
    assert!(matches!(
        response_error,
        JsonRpcError::InvalidEnvelope {
            violation: EnvelopeViolation::ResponseContainsRequestMembers
        }
    ));

    for error_value in [
        json!(null),
        json!({"code":"-1","message":"bad"}),
        json!({"code":-1}),
    ] {
        let error = classify_message(
            json!({"jsonrpc":"2.0","id":1,"error":error_value}),
            EnvelopeLimits::default(),
        )
        .expect_err("invalid error object must fail");
        assert!(matches!(
            error,
            JsonRpcError::InvalidEnvelope {
                violation: EnvelopeViolation::InvalidErrorObject
            }
        ));
    }
}

#[test]
fn response_correlator_bounds_pending_and_classifies_completed_ids() {
    let mut correlator = ResponseCorrelator::new(1);
    let first = correlator
        .begin_request()
        .expect("first ID should allocate");
    assert_eq!(first.get(), 1);
    assert_eq!(first.to_json(), json!(1));
    assert_eq!(correlator.pending_count(), 1);
    assert!(matches!(
        correlator.begin_request(),
        Err(JsonRpcError::PendingRequestLimit { limit: 1 })
    ));

    correlator
        .complete(first)
        .expect("pending ID should complete");
    assert_eq!(correlator.pending_count(), 0);
    assert!(matches!(
        correlator.complete(first),
        Err(JsonRpcError::DuplicateResponseId { id }) if id == first
    ));

    let unknown = ClientRequestId::from_test_value(99);
    assert!(matches!(
        correlator.complete(unknown),
        Err(JsonRpcError::UnknownResponseId { id }) if id == unknown
    ));
}

struct OneByteReader {
    inner: Cursor<Vec<u8>>,
}

struct InterruptOnceReader {
    inner: Cursor<Vec<u8>>,
    interrupted: bool,
}

impl InterruptOnceReader {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            inner: Cursor::new(bytes),
            interrupted: false,
        }
    }
}

impl Read for InterruptOnceReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        self.inner.read(buffer)
    }
}

impl OneByteReader {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            inner: Cursor::new(bytes),
        }
    }
}

impl Read for OneByteReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let Some(first) = buffer.first_mut() else {
            return Ok(0);
        };
        self.inner.read(std::slice::from_mut(first))
    }
}

trait ClientRequestIdTestExt {
    fn from_test_value(value: u64) -> Self;
}

impl ClientRequestIdTestExt for ClientRequestId {
    fn from_test_value(value: u64) -> Self {
        let response = classify_message(
            json!({"jsonrpc":"2.0","id":value,"result":null}),
            EnvelopeLimits::default(),
        )
        .expect("test response should classify");
        let IncomingMessage::Response(response) = response else {
            unreachable!("test fixture is a response")
        };
        response.id
    }
}

fn nested_params(depth: usize, opener: char, closer: char) -> String {
    let mut body = String::from(r#"{"jsonrpc":"2.0","method":"nested","params":"#);
    for _ in 0..depth {
        body.push(opener);
        if opener == '{' {
            body.push_str(r#""value":"#);
        }
    }
    body.push_str("null");
    for _ in 0..depth {
        body.push(closer);
    }
    body.push('}');
    body
}

#[test]
fn frame_decoder_reports_mid_frame_exactly_while_input_is_partial() {
    let mut decoder = FrameDecoder::new(FramingLimits::default());
    assert!(!decoder.is_mid_frame(), "a fresh decoder is between frames");

    let mut partial_header = Vec::new();
    partial_header.extend_from_slice(b"Content-Length: 5\r\n\r");
    let pushed = decoder
        .push(&partial_header)
        .expect("partial header should be retained");
    assert!(pushed.is_empty());
    assert!(decoder.is_mid_frame(), "a partial header is mid-frame");

    let pushed = decoder
        .push(b"\nhello")
        .expect("terminator completion should emit the body");
    assert_eq!(pushed, [b"hello".to_vec()]);
    assert!(!decoder.is_mid_frame(), "a completed body ends the frame");

    let complete = encode_message(
        &json!({"jsonrpc":"2.0","method":"server/ready"}),
        FramingLimits::default(),
    )
    .expect("fixture should encode");
    let (head, tail) = complete.split_at(3);
    let pushed = decoder.push(head).expect("frame head should be retained");
    assert!(pushed.is_empty());
    assert!(decoder.is_mid_frame(), "a partial body header is mid-frame");

    let pushed = decoder
        .push(tail)
        .expect("frame tail should complete the next body");
    assert_eq!(pushed.len(), 1);
    assert!(!decoder.is_mid_frame(), "the next frame boundary is clean");
}
