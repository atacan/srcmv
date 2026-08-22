//! Deterministic integration of bounded process supervision and JSON-RPC.

use std::collections::VecDeque;
use std::fmt;
use std::process::ExitStatus;
use std::time::Instant;

use serde_json::{Map, Value};

use crate::jsonrpc::{
    ClientRequestId, EnvelopeLimits, FrameDecoder, FramingLimits, IncomingMessage, JsonRpcError,
    ResponseCorrelator, decode_body, encode_message,
};
use crate::process::{
    ManagedProcess, ProcessError, ProcessEvent, ProcessFault, ProcessFaultKind, ProcessLimits,
    ProcessSpec,
};

/// Frozen default maximum process events drained at one orchestration boundary.
///
/// This exceeds the 2,049 8-KiB chunks needed for a maximum-size default
/// inbound body plus its header, leaving room for adjacent lifecycle events.
pub const DEFAULT_MAX_READY_EVENTS: usize = 4096;
/// Frozen default maximum validated messages buffered between caller reads.
pub const DEFAULT_MAX_BUFFERED_MESSAGES: usize = 1024;
/// Frozen default cumulative framed-body bytes retained after validation.
pub const DEFAULT_MAX_BUFFERED_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Limits enforced by an integrated language-server transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportLimits {
    /// Child-process queue and diagnostic bounds.
    pub process: ProcessLimits,
    /// JSON-RPC framing bounds.
    pub framing: FramingLimits,
    /// JSON-RPC envelope bounds.
    pub envelope: EnvelopeLimits,
    /// Maximum number of requests awaiting a response.
    pub max_pending_requests: usize,
    /// Maximum already-ready process events consumed at one wake boundary.
    pub max_ready_events: usize,
    /// Maximum validated messages buffered between caller reads.
    pub max_buffered_messages: usize,
    /// Maximum cumulative framed-body bytes retained after validation.
    pub max_buffered_message_bytes: usize,
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self {
            process: ProcessLimits::default(),
            framing: FramingLimits::default(),
            envelope: EnvelopeLimits::default(),
            max_pending_requests: crate::jsonrpc::DEFAULT_MAX_PENDING_REQUESTS,
            max_ready_events: DEFAULT_MAX_READY_EVENTS,
            max_buffered_messages: DEFAULT_MAX_BUFFERED_MESSAGES,
            max_buffered_message_bytes: DEFAULT_MAX_BUFFERED_MESSAGE_BYTES,
        }
    }
}

/// A terminal transport, protocol, process, or deadline failure.
#[derive(Debug)]
pub enum TransportError {
    /// JSON-RPC framing, decoding, validation, or correlation failed.
    Protocol(JsonRpcError),
    /// The language-server process exited while a message was expected.
    Exited(ExitStatus),
    /// A process worker reported an I/O failure.
    ProcessFault(ProcessFault),
    /// Child stdout closed cleanly between frames while a message was expected.
    StdoutClosed,
    /// Child stdin closed or rejected further writes while a message was expected.
    StdinClosed,
    /// Process setup, queueing, or cleanup failed.
    Process(ProcessError),
    /// No message or higher-precedence event was ready by the deadline.
    DeadlineExceeded,
    /// An integration-level event or message buffer bound was exceeded.
    ResourceLimit {
        /// Resource whose bound was exceeded.
        resource: &'static str,
        /// Configured maximum.
        limit: usize,
    },
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(error) => write!(formatter, "language-server protocol error: {error}"),
            Self::Exited(status) => {
                write!(formatter, "language server exited unexpectedly: {status}")
            }
            Self::ProcessFault(fault) => write!(
                formatter,
                "language-server {:?} worker failed: {}",
                fault.worker, fault.kind
            ),
            Self::StdoutClosed => formatter.write_str("language-server stdout closed unexpectedly"),
            Self::StdinClosed => formatter.write_str("language-server stdin closed unexpectedly"),
            Self::Process(error) => write!(formatter, "language-server process error: {error}"),
            Self::DeadlineExceeded => {
                formatter.write_str("deadline exceeded while waiting for language server")
            }
            Self::ResourceLimit { resource, limit } => {
                write!(
                    formatter,
                    "language-server {resource} limit of {limit} exceeded"
                )
            }
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            Self::Process(error) => Some(error),
            Self::Exited(_)
            | Self::ProcessFault(_)
            | Self::StdoutClosed
            | Self::StdinClosed
            | Self::DeadlineExceeded
            | Self::ResourceLimit { .. } => None,
        }
    }
}

#[derive(Debug)]
enum IoCondition {
    Fault(ProcessFault),
    Process(ProcessError),
}

#[derive(Clone, Copy, Debug)]
enum TerminalCondition {
    StdinClosed,
    StdoutClosed,
}

#[derive(Debug, Default)]
struct ReadyBatch {
    protocol_error: Option<JsonRpcError>,
    integration_resource: Option<(&'static str, usize)>,
    resource_violation: Option<ProcessFault>,
    exit: Option<ExitStatus>,
    terminal_condition: Option<TerminalCondition>,
    io_condition: Option<IoCondition>,
}

impl ReadyBatch {
    fn is_empty(&self) -> bool {
        self.protocol_error.is_none()
            && self.integration_resource.is_none()
            && self.resource_violation.is_none()
            && self.exit.is_none()
            && self.terminal_condition.is_none()
            && self.io_condition.is_none()
    }
}

/// A bounded synchronous JSON-RPC transport for one supervised process.
///
/// This type deliberately stops below LSP initialization and shutdown
/// semantics. A higher-level client builds the corresponding JSON values and
/// uses this transport for deadline-aware delivery and receipt.
pub struct Transport {
    process: ManagedProcess,
    framing_limits: FramingLimits,
    envelope_limits: EnvelopeLimits,
    decoder: Option<FrameDecoder>,
    correlator: ResponseCorrelator,
    incoming: VecDeque<BufferedIncoming>,
    buffered_message_bytes: usize,
    max_ready_events: usize,
    max_buffered_messages: usize,
    max_buffered_message_bytes: usize,
}

struct BufferedIncoming {
    message: IncomingMessage,
    body_bytes: usize,
}

impl Transport {
    /// Spawns a language server with bounded process and JSON-RPC state.
    ///
    /// # Errors
    ///
    /// Returns an error when process limits are invalid or the configured
    /// executable and its supervised workers cannot be started.
    pub fn spawn(
        specification: &ProcessSpec,
        limits: TransportLimits,
    ) -> Result<Self, TransportError> {
        if limits.max_ready_events == 0
            || limits.max_buffered_messages == 0
            || limits.max_buffered_message_bytes == 0
        {
            return Err(TransportError::ResourceLimit {
                resource: "configured zero capacity",
                limit: 0,
            });
        }
        let process =
            ManagedProcess::spawn(specification, limits.process).map_err(map_process_error)?;
        Ok(Self {
            process,
            framing_limits: limits.framing,
            envelope_limits: limits.envelope,
            decoder: Some(FrameDecoder::new(limits.framing)),
            correlator: ResponseCorrelator::new(limits.max_pending_requests),
            incoming: VecDeque::new(),
            buffered_message_bytes: 0,
            max_ready_events: limits.max_ready_events,
            max_buffered_messages: limits.max_buffered_messages,
            max_buffered_message_bytes: limits.max_buffered_message_bytes,
        })
    }

    /// Returns the operating-system process identifier.
    #[must_use]
    pub const fn process_id(&self) -> u32 {
        self.process.process_id()
    }

    /// Serializes and queues one complete JSON-RPC value by a fixed deadline.
    ///
    /// This low-level method does not register client request IDs. Use
    /// [`Self::send_request`] for requests that expect correlated responses.
    ///
    /// # Errors
    ///
    /// Returns an error for exact outbound-body overflow or bounded process
    /// queue, deadline, and worker failures.
    pub fn send_value(&self, value: &Value, deadline: Instant) -> Result<(), TransportError> {
        let frame = encode_message(value, self.framing_limits).map_err(TransportError::Protocol)?;
        self.process
            .send_frame(frame, deadline)
            .map_err(map_process_error)
    }

    /// Allocates an ID, constructs, and queues one client request.
    ///
    /// A queue failure leaves the ID pending because the transport outcome is
    /// indeterminate; callers must enter cleanup rather than reuse the session.
    ///
    /// # Errors
    ///
    /// Returns an error when the pending-request limit or request-ID space is
    /// exhausted, serialization exceeds a bound, or queueing fails.
    pub fn send_request(
        &mut self,
        method: &str,
        params: Option<Value>,
        deadline: Instant,
    ) -> Result<ClientRequestId, TransportError> {
        let id = self
            .correlator
            .begin_request()
            .map_err(TransportError::Protocol)?;
        let mut request = Map::new();
        request.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
        request.insert("id".to_owned(), id.to_json());
        request.insert("method".to_owned(), Value::String(method.to_owned()));
        if let Some(params) = params {
            request.insert("params".to_owned(), params);
        }
        self.send_value(&Value::Object(request), deadline)?;
        Ok(id)
    }

    /// Returns the next validated incoming message by a fixed deadline.
    ///
    /// At every wake boundary this method drains all events that are already
    /// ready, then applies the frozen precedence: protocol/resource violation,
    /// unexpected exit, terminal pipe closure, other I/O fault, validated
    /// message, deadline. Thus an event observed in the final deadline drain
    /// wins over the deadline. Multiple valid messages retain byte-stream order
    /// across calls.
    ///
    /// # Errors
    ///
    /// Returns the highest-precedence failure ready at the orchestration
    /// boundary, or [`TransportError::DeadlineExceeded`] if none was ready.
    ///
    /// A partially decoded inbound frame keeps waiting for its remaining
    /// chunks within the active deadline; only a genuinely expired deadline
    /// reports [`TransportError::DeadlineExceeded`].
    pub fn next_incoming(&mut self, deadline: Instant) -> Result<IncomingMessage, TransportError> {
        let ready = self.drain_ready(None, deadline);
        if !ready.is_empty() {
            return self.choose(ready);
        }
        if let Some(message) = self.pop_incoming() {
            return Ok(message);
        }

        let mut observed = match self.process.next_event(deadline) {
            Ok(event) => Some(event),
            Err(ProcessError::DeadlineExceeded(_)) => None,
            Err(error) => {
                let mut ready = self.drain_ready(None, deadline);
                if ready.io_condition.is_none() {
                    ready.io_condition = Some(IoCondition::Process(error));
                }
                return self.choose_or_deadline(ready);
            }
        };

        loop {
            let ready = self.drain_ready(observed.take(), deadline);
            if !ready.is_empty() {
                return self.choose(ready);
            }
            if let Some(message) = self.pop_incoming() {
                return Ok(message);
            }

            // The observed chunk can be one fragment of a larger inbound
            // frame whose remainder is still in flight. While the frame
            // decoder holds partial input and time remains, keep blocking on
            // the next event instead of reporting a premature deadline.
            let mid_frame = self
                .decoder
                .as_ref()
                .is_some_and(crate::jsonrpc::FrameDecoder::is_mid_frame);
            if !mid_frame || Instant::now() >= deadline {
                break;
            }
            observed = match self.process.next_event(deadline) {
                Ok(event) => Some(event),
                Err(ProcessError::DeadlineExceeded(_)) => break,
                Err(error) => {
                    let mut ready = self.drain_ready(None, deadline);
                    if ready.io_condition.is_none() {
                        ready.io_condition = Some(IoCondition::Process(error));
                    }
                    return self.choose_or_deadline(ready);
                }
            };
        }
        let ready = self.drain_ready(None, deadline);
        self.choose_or_deadline(ready)
    }

    /// Returns a snapshot of the bounded recent stderr tail.
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<u8> {
        self.process.stderr_tail()
    }

    /// Closes stdin, waits for graceful child exit, and escalates cleanup by
    /// the supplied deadlines.
    ///
    /// Protocol-level `shutdown` and `exit` messages are the higher-level
    /// client's responsibility and must be queued before this call.
    ///
    /// # Errors
    ///
    /// Returns an error if process cleanup or worker joining fails.
    pub fn finish(
        &mut self,
        graceful_deadline: Instant,
        cleanup_deadline: Instant,
    ) -> Result<ExitStatus, TransportError> {
        self.process
            .finish(graceful_deadline, cleanup_deadline)
            .map_err(map_process_error)
    }

    /// Immediately terminates the process group, reaps the direct child, and
    /// joins all process workers.
    ///
    /// # Errors
    ///
    /// Returns an error if forced cleanup cannot complete by the deadline.
    pub fn abort(&mut self, cleanup_deadline: Instant) -> Result<ExitStatus, TransportError> {
        self.process
            .abort(cleanup_deadline)
            .map_err(map_process_error)
    }

    fn drain_ready(&mut self, first: Option<ProcessEvent>, deadline: Instant) -> ReadyBatch {
        let mut ready = ReadyBatch::default();
        let mut observed = 0_usize;
        if let Some(event) = first {
            self.observe(event, &mut ready);
            observed = 1;
        }
        loop {
            if should_probe_ready_event_overflow(observed, self.max_ready_events)
                && self.probe_ready_event_overflow(&mut ready)
            {
                break;
            }
            if Instant::now() >= deadline && (!ready.is_empty() || !self.incoming.is_empty()) {
                break;
            }
            match self.process.try_next_event() {
                Ok(Some(event)) => {
                    observed += 1;
                    self.observe(event, &mut ready);
                }
                Ok(None) => break,
                Err(error) => {
                    if ready.io_condition.is_none() {
                        ready.io_condition = Some(IoCondition::Process(error));
                    }
                    break;
                }
            }
        }
        ready
    }

    fn probe_ready_event_overflow(&mut self, ready: &mut ReadyBatch) -> bool {
        match self.process.try_next_event() {
            Ok(Some(_)) => {
                ready
                    .integration_resource
                    .get_or_insert(("ready process events", self.max_ready_events));
            }
            Ok(None) => {}
            Err(error) => {
                if ready.io_condition.is_none() {
                    ready.io_condition = Some(IoCondition::Process(error));
                }
            }
        }
        true
    }

    fn observe(&mut self, event: ProcessEvent, ready: &mut ReadyBatch) {
        match event {
            ProcessEvent::Exited(status) => ready.exit = Some(status),
            ProcessEvent::Fault(fault) => {
                if matches!(fault.kind, ProcessFaultKind::ResourceLimit { .. }) {
                    if ready.resource_violation.is_none() {
                        ready.resource_violation = Some(fault);
                    }
                } else if ready.io_condition.is_none() {
                    ready.io_condition = Some(IoCondition::Fault(fault));
                }
            }
            ProcessEvent::Stdout(bytes) => self.decode_chunk(&bytes, ready),
            ProcessEvent::StdoutClosed => self.observe_stdout_closed(ready),
            ProcessEvent::StdinClosed => {
                ready
                    .terminal_condition
                    .get_or_insert(TerminalCondition::StdinClosed);
            }
        }
    }

    fn decode_chunk(&mut self, bytes: &[u8], ready: &mut ReadyBatch) {
        if ready.protocol_error.is_some() {
            return;
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        let bodies = match decoder.push(bytes) {
            Ok(bodies) => bodies,
            Err(error) => {
                ready.protocol_error = Some(error);
                return;
            }
        };
        for body in bodies {
            let message = match decode_body(&body, self.envelope_limits) {
                Ok(message) => message,
                Err(error) => {
                    ready.protocol_error = Some(error);
                    break;
                }
            };
            if let IncomingMessage::Response(response) = &message
                && let Err(error) = self.correlator.complete(response.id)
            {
                ready.protocol_error = Some(error);
                break;
            }
            let body_bytes = body.len();
            let Some(next_bytes) = self.buffered_message_bytes.checked_add(body_bytes) else {
                self.fail_buffered_bytes(ready);
                break;
            };
            if next_bytes > self.max_buffered_message_bytes {
                self.fail_buffered_bytes(ready);
                break;
            }
            self.buffered_message_bytes = next_bytes;
            self.incoming.push_back(BufferedIncoming {
                message,
                body_bytes,
            });
            if self.incoming.len() > self.max_buffered_messages {
                self.clear_incoming();
                ready.integration_resource =
                    Some(("buffered incoming messages", self.max_buffered_messages));
                break;
            }
        }
    }

    fn observe_stdout_closed(&mut self, ready: &mut ReadyBatch) {
        if ready.protocol_error.is_some() {
            self.decoder.take();
            return;
        }
        if let Some(decoder) = self.decoder.take()
            && let Err(error) = decoder.finish()
        {
            ready.protocol_error = Some(error);
            return;
        }
        ready
            .terminal_condition
            .get_or_insert(TerminalCondition::StdoutClosed);
    }

    fn choose(&mut self, ready: ReadyBatch) -> Result<IncomingMessage, TransportError> {
        if let Some(error) = ready.protocol_error {
            self.clear_incoming();
            return Err(TransportError::Protocol(error));
        }
        if let Some((resource, limit)) = ready.integration_resource {
            self.clear_incoming();
            return Err(TransportError::ResourceLimit { resource, limit });
        }
        if let Some(fault) = ready.resource_violation {
            self.clear_incoming();
            return Err(TransportError::ProcessFault(fault));
        }
        if let Some(status) = ready.exit {
            self.clear_incoming();
            return Err(TransportError::Exited(status));
        }
        if let Some(condition) = ready.terminal_condition {
            self.clear_incoming();
            return Err(match condition {
                TerminalCondition::StdinClosed => TransportError::StdinClosed,
                TerminalCondition::StdoutClosed => TransportError::StdoutClosed,
            });
        }
        if let Some(condition) = ready.io_condition {
            self.clear_incoming();
            return Err(match condition {
                IoCondition::Fault(fault) => TransportError::ProcessFault(fault),
                IoCondition::Process(error) => map_process_error(error),
            });
        }
        self.pop_incoming().ok_or(TransportError::DeadlineExceeded)
    }

    fn choose_or_deadline(&mut self, ready: ReadyBatch) -> Result<IncomingMessage, TransportError> {
        if ready.is_empty()
            && let Some(message) = self.pop_incoming()
        {
            return Ok(message);
        }
        self.choose(ready)
    }

    fn fail_buffered_bytes(&mut self, ready: &mut ReadyBatch) {
        self.clear_incoming();
        ready.integration_resource = Some((
            "buffered incoming message bytes",
            self.max_buffered_message_bytes,
        ));
    }

    fn pop_incoming(&mut self) -> Option<IncomingMessage> {
        let buffered = self.incoming.pop_front()?;
        self.buffered_message_bytes -= buffered.body_bytes;
        Some(buffered.message)
    }

    fn clear_incoming(&mut self) {
        self.incoming.clear();
        self.buffered_message_bytes = 0;
    }
}

fn map_process_error(error: ProcessError) -> TransportError {
    match error {
        ProcessError::Exited(status) => TransportError::Exited(status),
        ProcessError::StdinClosed => TransportError::StdinClosed,
        error => TransportError::Process(error),
    }
}

fn should_probe_ready_event_overflow(observed: usize, limit: usize) -> bool {
    observed == limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_event_bound_probes_only_after_exact_limit_is_consumed() {
        assert!(!should_probe_ready_event_overflow(2, 3));
        assert!(should_probe_ready_event_overflow(3, 3));
        assert!(!should_probe_ready_event_overflow(4, 3));
    }

    #[test]
    fn default_ready_event_bound_covers_maximum_inbound_body_chunks() {
        const PROCESS_READ_CHUNK_BYTES: usize = 8 * 1024;
        let limits = TransportLimits::default();
        let body_chunks = limits
            .framing
            .max_inbound_body_bytes
            .div_ceil(PROCESS_READ_CHUNK_BYTES);
        let header_chunk = 1;
        assert!(limits.max_ready_events > body_chunks + header_chunk);
    }
}
