//! Integration tests for deterministic process and JSON-RPC arbitration.

#![cfg(unix)]

use std::ffi::OsString;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use srcmv_lsp::jsonrpc::{IncomingMessage, JsonRpcError};
use srcmv_lsp::process::{ProcessFaultKind, ProcessSpec};
use srcmv_lsp::transport::{Transport, TransportError, TransportLimits};

const OPERATION: Duration = Duration::from_secs(2);
const CLEANUP: Duration = Duration::from_secs(3);

fn deadline_after(duration: Duration) -> Instant {
    Instant::now() + duration
}

fn frame(value: &Value) -> String {
    let body = serde_json::to_string(value).expect("fixture should serialize");
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

fn output_then_wait(output: impl Into<OsString>) -> ProcessSpec {
    ProcessSpec::new("/bin/sh")
        .args(["-c", "printf '%s' \"$1\"; sleep 60", "transport-test"])
        .arg(output)
}

fn output_then_exit(output: impl Into<OsString>) -> ProcessSpec {
    ProcessSpec::new("/bin/sh")
        .args(["-c", "printf '%s' \"$1\"", "transport-test"])
        .arg(output)
}

fn abort_and_assert_reaped(transport: &mut Transport) {
    let process_id = transport.process_id();
    transport
        .abort(deadline_after(CLEANUP))
        .expect("transport failure cleanup should succeed");
    assert_process_reaped(process_id);
}

#[cfg(unix)]
fn assert_process_reaped(process_id: u32) {
    use rustix::process::{Pid, test_kill_process};

    let raw_pid = i32::try_from(process_id).expect("test PID should fit i32");
    let pid = Pid::from_raw(raw_pid).expect("spawned PID should be positive");
    assert!(
        test_kill_process(pid).is_err(),
        "direct language-server child should be reaped"
    );
}

#[cfg(not(unix))]
fn assert_process_reaped(_process_id: u32) {}

#[test]
fn outbound_frame_is_consumed_before_successful_inbound_message() {
    let outgoing = json!({"jsonrpc":"2.0","method":"client/ready","params":{}});
    let outgoing_body = serde_json::to_vec(&outgoing).expect("fixture should serialize");
    let incoming = json!({"jsonrpc":"2.0","method":"server/ready","params":{}});
    let specification = ProcessSpec::new("/bin/sh")
        .args([
            "-c",
            "IFS= read -r _header; IFS= read -r _blank; dd bs=1 count=\"$2\" >/dev/null 2>/dev/null; printf '%s' \"$1\"; sleep 60",
            "transport-test",
        ])
        .arg(frame(&incoming))
        .arg(outgoing_body.len().to_string());
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");

    transport
        .send_value(&outgoing, deadline_after(OPERATION))
        .expect("outbound value should be framed and queued");
    let message = transport
        .next_incoming(deadline_after(OPERATION))
        .expect("server notification should arrive after request bytes");
    assert!(matches!(
        message,
        IncomingMessage::Notification(notification) if notification.method == "server/ready"
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn live_child_closing_stdin_is_a_terminal_transport_condition() {
    let ready = json!({"jsonrpc":"2.0","method":"server/ready"});
    let outgoing = json!({
        "jsonrpc":"2.0",
        "method":"client/ready",
        "params":{"payload":"x".repeat(1024 * 1024 + 1)}
    });
    let specification = ProcessSpec::new("/bin/sh")
        .args([
            "-c",
            "exec 0<&-; printf '%s' \"$1\"; sleep 60",
            "transport-test",
        ])
        .arg(frame(&ready));
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");

    transport
        .next_incoming(deadline_after(OPERATION))
        .expect("server readiness notification should arrive");
    transport
        .send_value(&outgoing, deadline_after(OPERATION))
        .expect("large outbound value should reach the asynchronous writer");

    let event_error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("closed stdin should terminate the transport");
    assert!(
        matches!(event_error, TransportError::StdinClosed),
        "unexpected queued terminal transport condition: {event_error:?}"
    );

    let synchronous_error = transport
        .send_value(
            &json!({"jsonrpc":"2.0","method":"client/after-close"}),
            deadline_after(OPERATION),
        )
        .expect_err("closed stdin worker should reject a synchronous transport send");
    assert!(
        matches!(synchronous_error, TransportError::StdinClosed),
        "unexpected synchronous transport condition: {synchronous_error:?}"
    );

    abort_and_assert_reaped(&mut transport);
}

#[test]
fn concatenated_valid_messages_preserve_stream_order_across_calls() {
    let first = json!({"jsonrpc":"2.0","method":"first"});
    let second = json!({"jsonrpc":"2.0","method":"second"});
    let mut output = frame(&first);
    output.push_str(&frame(&second));
    let specification = output_then_wait(output);
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");

    let first_message = transport
        .next_incoming(deadline_after(OPERATION))
        .expect("first notification should decode");
    let second_message = transport
        .next_incoming(deadline_after(OPERATION))
        .expect("buffered second notification should decode");
    assert!(matches!(
        first_message,
        IncomingMessage::Notification(notification) if notification.method == "first"
    ));
    assert!(matches!(
        second_message,
        IncomingMessage::Notification(notification) if notification.method == "second"
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn response_for_id_allocated_by_send_request_is_accepted() {
    let response = json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
    let specification = output_then_wait(frame(&response));
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    let request_id = transport
        .send_request("test/request", Some(json!({})), deadline_after(OPERATION))
        .expect("request should be registered and queued");
    let message = transport
        .next_incoming(deadline_after(OPERATION))
        .expect("matching response should be accepted");
    assert!(matches!(
        message,
        IncomingMessage::Response(response) if response.id == request_id
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn malformed_header_wins_and_cleanup_reaps_child() {
    let specification = output_then_wait("Content-Length nope\r\n\r\n{}");
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("malformed header should fail");
    assert!(matches!(
        error,
        TransportError::Protocol(JsonRpcError::MalformedHeader)
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn malformed_json_wins_and_cleanup_reaps_child() {
    let specification = output_then_wait("Content-Length: 1\r\n\r\n{");
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("malformed JSON should fail");
    assert!(matches!(
        error,
        TransportError::Protocol(JsonRpcError::MalformedJson { .. })
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn oversized_inbound_frame_is_rejected_before_body_arrives() {
    let specification = output_then_wait("Content-Length: 33\r\n\r\n");
    let mut limits = TransportLimits::default();
    limits.framing.max_inbound_body_bytes = 32;
    let mut transport = Transport::spawn(&specification, limits).expect("child should spawn");
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("oversized frame should fail");
    assert!(matches!(
        error,
        TransportError::Protocol(JsonRpcError::InboundBodyTooLarge {
            length: 33,
            limit: 32
        })
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn unknown_response_id_is_a_protocol_failure() {
    let response = json!({"jsonrpc":"2.0","id":1,"result":null});
    let specification = output_then_wait(frame(&response));
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("unissued response ID should fail");
    assert!(matches!(
        error,
        TransportError::Protocol(JsonRpcError::UnknownResponseId { .. })
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn duplicate_response_in_same_ready_batch_wins_over_first_valid_response() {
    let response = json!({"jsonrpc":"2.0","id":1,"result":null});
    let mut responses = frame(&response);
    responses.push_str(&frame(&response));
    let specification = output_then_wait(responses);
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    transport
        .send_request("test/request", None, deadline_after(OPERATION))
        .expect("request should be registered and queued");
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("duplicate response should outrank buffered valid response");
    assert!(matches!(
        error,
        TransportError::Protocol(JsonRpcError::DuplicateResponseId { .. })
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn clean_early_exit_precedes_stdout_eof() {
    let specification = ProcessSpec::new("/bin/sh").args(["-c", "exit 7"]);
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    thread::sleep(Duration::from_millis(100));
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("early exit should fail");
    assert!(
        matches!(
            error,
            TransportError::Exited(status) if status.code() == Some(7)
        ),
        "unexpected early-exit error: {error:?}"
    );
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn deadline_is_returned_when_no_process_event_is_ready() {
    let specification = ProcessSpec::new("/bin/sh").args(["-c", "sleep 60"]);
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    let error = transport
        .next_incoming(deadline_after(Duration::from_millis(40)))
        .expect_err("silent child should time out");
    assert!(matches!(error, TransportError::DeadlineExceeded));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn malformed_frame_precedes_simultaneously_ready_child_exit() {
    let specification = output_then_exit("Content-Length nope\r\n\r\n{}");
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    thread::sleep(Duration::from_millis(100));
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("framing violation should outrank ready exit");
    assert!(matches!(
        error,
        TransportError::Protocol(JsonRpcError::MalformedHeader)
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn inbound_resource_violation_precedes_simultaneously_ready_child_exit() {
    let specification = output_then_exit("xx");
    let mut limits = TransportLimits::default();
    limits.process.inbound_bytes = 1;
    let mut transport = Transport::spawn(&specification, limits).expect("child should spawn");
    thread::sleep(Duration::from_millis(100));
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("resource violation should outrank ready exit");
    assert!(matches!(
        error,
        TransportError::ProcessFault(ref fault)
            if matches!(
                fault.kind,
                ProcessFaultKind::ResourceLimit {
                    queue: "inbound",
                    item_bytes: 2,
                    capacity_bytes: 1
                }
            )
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn continuous_valid_notifications_hit_bounded_message_buffer_promptly() {
    let notification = frame(&json!({"jsonrpc":"2.0","method":"tick"}));
    let specification = ProcessSpec::new("/bin/sh")
        .args([
            "-c",
            "while :; do printf '%s' \"$1\"; done",
            "transport-test",
        ])
        .arg(notification);
    let limits = TransportLimits {
        max_buffered_messages: 4,
        max_ready_events: 16,
        ..TransportLimits::default()
    };
    let mut transport = Transport::spawn(&specification, limits).expect("child should spawn");

    let started = Instant::now();
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("continuous producer must hit a bounded integration limit");
    assert!(matches!(
        error,
        TransportError::ResourceLimit {
            resource: "buffered incoming messages",
            limit: 4
        } | TransportError::ResourceLimit {
            resource: "ready process events",
            limit: 16
        }
    ));
    assert!(started.elapsed() < OPERATION);
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn transport_rejects_zero_integration_capacities_before_spawn() {
    let defaults = TransportLimits::default();
    let cases = [
        TransportLimits {
            max_ready_events: 0,
            ..defaults
        },
        TransportLimits {
            max_buffered_messages: 0,
            ..defaults
        },
        TransportLimits {
            max_buffered_message_bytes: 0,
            ..defaults
        },
    ];

    for limits in cases {
        let result = Transport::spawn(&ProcessSpec::new("/absent"), limits);
        assert!(matches!(
            result,
            Err(TransportError::ResourceLimit {
                resource: "configured zero capacity",
                limit: 0
            })
        ));
    }
}

#[test]
fn buffered_message_count_accepts_below_and_at_then_rejects_above() {
    let values = [
        json!({"jsonrpc":"2.0","method":"one"}),
        json!({"jsonrpc":"2.0","method":"two"}),
        json!({"jsonrpc":"2.0","method":"three"}),
    ];
    let two_messages = format!("{}{}", frame(&values[0]), frame(&values[1]));
    for limit in [3, 2] {
        let limits = TransportLimits {
            max_buffered_messages: limit,
            ..TransportLimits::default()
        };
        let mut transport = Transport::spawn(&output_then_wait(two_messages.clone()), limits)
            .expect("bounded transport should spawn");
        transport
            .next_incoming(deadline_after(OPERATION))
            .expect("first notification should fit");
        transport
            .next_incoming(deadline_after(OPERATION))
            .expect("second notification should fit");
        abort_and_assert_reaped(&mut transport);
    }

    let three_messages = format!(
        "{}{}{}",
        frame(&values[0]),
        frame(&values[1]),
        frame(&values[2])
    );
    let limits = TransportLimits {
        max_buffered_messages: 2,
        ..TransportLimits::default()
    };
    let mut transport = Transport::spawn(&output_then_wait(three_messages), limits)
        .expect("bounded transport should spawn");
    let error = transport
        .next_incoming(deadline_after(OPERATION))
        .expect_err("third ready notification must exceed the two-message limit");
    assert!(matches!(
        error,
        TransportError::ResourceLimit {
            resource: "buffered incoming messages",
            limit: 2
        }
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn buffered_message_byte_limit_accepts_at_boundary_and_rejects_above() {
    let first = json!({"jsonrpc":"2.0","method":"one"});
    let second = json!({"jsonrpc":"2.0","method":"two"});
    let first_body = serde_json::to_vec(&first).expect("fixture should serialize");
    let second_body = serde_json::to_vec(&second).expect("fixture should serialize");
    let exact = first_body.len() + second_body.len();
    let mut output = frame(&first);
    output.push_str(&frame(&second));

    let at_limits = TransportLimits {
        max_buffered_message_bytes: exact,
        ..TransportLimits::default()
    };
    let mut at = Transport::spawn(&output_then_wait(output.clone()), at_limits)
        .expect("boundary transport should spawn");
    assert!(at.next_incoming(deadline_after(OPERATION)).is_ok());
    assert!(at.next_incoming(deadline_after(OPERATION)).is_ok());
    abort_and_assert_reaped(&mut at);

    let below_limits = TransportLimits {
        max_buffered_message_bytes: exact - 1,
        ..TransportLimits::default()
    };
    let mut below = Transport::spawn(&output_then_wait(output), below_limits)
        .expect("over-boundary transport should spawn");
    let error = below
        .next_incoming(deadline_after(OPERATION))
        .expect_err("one byte above cumulative bound must fail");
    assert!(matches!(
        error,
        TransportError::ResourceLimit {
            resource: "buffered incoming message bytes",
            limit
        } if limit == exact - 1
    ));
    abort_and_assert_reaped(&mut below);
}

#[test]
fn stderr_tail_remains_bounded_and_finish_reaps_child() {
    let specification = ProcessSpec::new("/bin/sh").args([
        "-c",
        "printf 'abcdefghijkl' >&2; sleep 0.05",
        "transport-test",
    ]);
    let mut limits = TransportLimits::default();
    limits.process.stderr_tail_bytes = 8;
    let mut transport = Transport::spawn(&specification, limits).expect("child should spawn");
    let process_id = transport.process_id();
    let status = transport
        .finish(deadline_after(OPERATION), deadline_after(CLEANUP))
        .expect("natural finish should succeed");
    assert!(status.success());
    assert_eq!(transport.stderr_tail(), b"efghijkl");
    assert_process_reaped(process_id);
}

fn large_notification() -> Value {
    json!({"jsonrpc":"2.0","method":"server/ready","params":{"payload":"x".repeat(20 * 1024)}})
}

fn split_frame(value: &Value, split: usize) -> (String, String) {
    let framed = frame(value);
    (framed[..split].to_owned(), framed[split..].to_owned())
}

fn chunked_output(first_part: &str, second_part: &str, delay_seconds: &str) -> ProcessSpec {
    ProcessSpec::new("/bin/sh")
        .args([
            "-c",
            "printf '%s' \"$1\"; sleep \"$3\"; printf '%s' \"$2\"; sleep 60",
            "transport-test",
        ])
        .arg(first_part)
        .arg(second_part)
        .arg(delay_seconds)
}

#[test]
fn multi_chunk_frame_is_delivered_within_the_active_deadline() {
    // The first write is smaller than the client's 8 KiB read chunk and the
    // tail is delayed, so the decoder holds partial input across wakes.
    let (first_part, second_part) = split_frame(&large_notification(), 4096);
    let specification = chunked_output(&first_part, &second_part, "1");
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");

    let message = transport
        .next_incoming(deadline_after(OPERATION))
        .expect("the delayed frame remainder should arrive within the deadline");
    assert!(matches!(
        &message,
        IncomingMessage::Notification(notification)
            if notification.method == "server/ready"
                && notification.params.as_ref().is_some_and(|params| {
                    params["payload"].as_str().is_some_and(|payload| payload.len() == 20 * 1024)
                })
    ));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn incomplete_frame_waits_until_the_deadline_before_expiring() {
    let (first_part, second_part) = split_frame(&large_notification(), 4096);
    let specification = chunked_output(&first_part, &second_part, "30");
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");

    let started = Instant::now();
    let result = transport.next_incoming(deadline_after(Duration::from_secs(1)));
    let elapsed = started.elapsed();

    assert!(matches!(result, Err(TransportError::DeadlineExceeded)));
    assert!(
        elapsed >= Duration::from_millis(900),
        "a mid-frame wait must not report an early expiry: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(5));
    abort_and_assert_reaped(&mut transport);
}

#[test]
fn deadline_passing_mid_frame_reports_expiry_without_waiting() {
    let (first_part, second_part) = split_frame(&large_notification(), 4096);
    let specification = chunked_output(&first_part, &second_part, "30");
    let mut transport =
        Transport::spawn(&specification, TransportLimits::default()).expect("child should spawn");
    thread::sleep(Duration::from_millis(300));

    let started = Instant::now();
    let result = transport.next_incoming(deadline_after(Duration::from_millis(50)));
    let elapsed = started.elapsed();

    assert!(matches!(result, Err(TransportError::DeadlineExceeded)));
    assert!(
        elapsed < Duration::from_secs(1),
        "an expired deadline must not keep waiting for the frame tail: {elapsed:?}"
    );
    abort_and_assert_reaped(&mut transport);
}
