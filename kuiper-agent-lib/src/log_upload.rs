//! Ships the agent's own log lines (not runner logs) to the coordinator in batches.
//!
//! [`LogCapture::layer`] is a tracing layer that copies events into a bounded in-memory buffer. The runtime
//! drains it over the `UploadLogs` rpc while a session is up, see [`crate::runtime::connect_with_logs`]. Lines
//! logged before the first connect are kept (up to the buffer size) and sent once connected.
//!
//! Events that carry a `local_only` field are never captured, see [`LOCAL_ONLY`].

use kuiper_agent_proto::{AgentServiceClient, LogBatch, LogRecord};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tonic::transport::Channel;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber, debug};
use tracing_subscriber::layer::{Context, Layer};

/// Lines kept in memory while disconnected or between flushes. Oldest are dropped past this.
const BUFFER_CAPACITY: usize = 5_000;

/// Max lines per `UploadLogs` call. Hitting this also triggers a flush before the timer.
const MAX_BATCH: usize = 500;

/// Also cap batches by size, well under tonic's default 4MB message limit.
const MAX_BATCH_BYTES: usize = 1024 * 1024;

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Per-line cap so one huge debug dump can't blow up a batch.
const MAX_MESSAGE_BYTES: usize = 8 * 1024;

// the uploader's own traffic logs through these, capturing them would just feed the next batch
const IGNORED_TARGET_PREFIXES: &[&str] = &[
    "h2",
    "hyper",
    "tonic",
    "tower",
    "rustls",
    "kuiper_agent_lib::log_upload",
];

/// Field name that keeps an event out of the upload, for ssh commands (they hold registration tokens / JIT
/// configs) and runner output. Mark it as `local_only = tracing::field::Empty` so the local fmt output doesn't
/// print it, the field still shows up in the event's metadata.
pub const LOCAL_ONLY: &str = "local_only";

/// Shared handle to the capture buffer. Cheap to clone.
#[derive(Clone)]
pub struct LogCapture {
    inner: Arc<Inner>,
}

struct Inner {
    buffer: Mutex<Buffer>,
    /// Wakes the uploader once a full batch is waiting
    batch_ready: Notify,
    /// False when upload is turned off in config, or the coordinator doesn't support it
    enabled: AtomicBool,
}

#[derive(Default)]
struct Buffer {
    records: VecDeque<LogRecord>,
    /// Dropped since the last batch went out
    dropped: u64,
    next_seq: u64,
}

impl Default for LogCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl LogCapture {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                buffer: Mutex::new(Buffer::default()),
                batch_ready: Notify::new(),
                enabled: AtomicBool::new(true),
            }),
        }
    }

    /// Tracing layer that feeds this buffer. Add it to the subscriber registry.
    pub fn layer(&self) -> LogCaptureLayer {
        LogCaptureLayer {
            capture: self.clone(),
        }
    }

    /// Stop capturing and throw away anything buffered, e.g. when upload is turned off in config.
    pub fn disable(&self) {
        self.inner.enabled.store(false, Ordering::Relaxed);
        let mut buffer = self.lock();
        buffer.records.clear();
        buffer.dropped = 0;
    }

    fn enable(&self) {
        self.inner.enabled.store(true, Ordering::Relaxed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Buffer> {
        // a panic mid-push leaves the buffer usable, so ignore poisoning
        self.inner.buffer.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, level: &str, target: &str, message: String) {
        let mut buffer = self.lock();
        if buffer.records.len() >= BUFFER_CAPACITY {
            buffer.records.pop_front();
            buffer.dropped += 1;
        }
        let seq = buffer.next_seq;
        buffer.next_seq += 1;
        buffer.records.push_back(LogRecord {
            timestamp_micros: chrono::Utc::now().timestamp_micros(),
            level: level.to_string(),
            target: target.to_string(),
            message,
            seq,
        });
        if buffer.records.len() == MAX_BATCH {
            self.inner.batch_ready.notify_one();
        }
    }

    /// Take the oldest lines, up to `MAX_BATCH` lines or `MAX_BATCH_BYTES`, plus the dropped count.
    fn take_batch(&self) -> LogBatch {
        let mut buffer = self.lock();
        let mut n = 0;
        let mut bytes = 0;
        for record in buffer.records.iter().take(MAX_BATCH) {
            bytes += record.message.len() + record.target.len();
            if n > 0 && bytes > MAX_BATCH_BYTES {
                break;
            }
            n += 1;
        }
        LogBatch {
            records: buffer.records.drain(..n).collect(),
            dropped: std::mem::take(&mut buffer.dropped),
        }
    }

    /// Put a batch that failed to send back at the front, still respecting the capacity.
    fn requeue(&self, batch: LogBatch) {
        let mut buffer = self.lock();
        buffer.dropped += batch.dropped;
        for record in batch.records.into_iter().rev() {
            if buffer.records.len() >= BUFFER_CAPACITY {
                buffer.dropped += 1;
                continue;
            }
            buffer.records.push_front(record);
        }
    }
}

/// Tracing layer returned by [`LogCapture::layer`].
pub struct LogCaptureLayer {
    capture: LogCapture,
}

impl<S: Subscriber> Layer<S> for LogCaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !self.capture.inner.enabled.load(Ordering::Relaxed) {
            return;
        }
        let meta = event.metadata();
        let target = meta.target();
        if meta.fields().field(LOCAL_ONLY).is_some()
            || IGNORED_TARGET_PREFIXES
                .iter()
                .any(|prefix| target.starts_with(prefix))
        {
            return;
        }

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let mut message = visitor.message;
        if !visitor.fields.is_empty() {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(&visitor.fields);
        }
        truncate_at_char_boundary(&mut message, MAX_MESSAGE_BYTES);

        self.capture.push(meta.level().as_str(), target, message);
    }
}

/// Formats an event as its message followed by `key=value` for every other field.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn push_field(&mut self, name: &str, value: std::fmt::Arguments<'_>) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(self.fields, "{name}={value}");
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.push_field(field.name(), format_args!("{value}"));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            self.push_field(field.name(), format_args!("{value:?}"));
        }
    }
}

fn truncate_at_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(" …[truncated]");
}

/// Upload loop for one session. Returns when the coordinator doesn't support uploads; otherwise runs until the
/// runtime aborts it at the end of the session.
pub(crate) async fn upload_loop(mut client: AgentServiceClient<Channel>, capture: LogCapture) {
    // an older coordinator may have turned us off on a previous session, try again on every new one
    capture.enable();

    loop {
        tokio::select! {
            _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
            _ = capture.inner.batch_ready.notified() => {}
        }

        loop {
            let batch = capture.take_batch();
            if batch.records.is_empty() && batch.dropped == 0 {
                break;
            }
            let more_waiting = !capture.lock().records.is_empty();
            let in_flight = InFlight {
                capture: &capture,
                batch: Some(batch.clone()),
            };

            match client.upload_logs(batch).await {
                Ok(_) => in_flight.done(),
                Err(status) if status.code() == tonic::Code::Unimplemented => {
                    debug!("Coordinator doesn't support log upload, disabling until reconnect");
                    in_flight.done();
                    capture.disable();
                    return;
                }
                // retrying a batch the coordinator refused would just loop forever
                Err(status)
                    if matches!(
                        status.code(),
                        tonic::Code::InvalidArgument | tonic::Code::ResourceExhausted
                    ) =>
                {
                    debug!("Coordinator refused log batch, dropping it: {}", status);
                    in_flight.done();
                }
                Err(status) => {
                    debug!("Log upload failed, will retry: {}", status);
                    drop(in_flight);
                    break;
                }
            }

            // drain a backlog straight away rather than one batch per interval
            if !more_waiting {
                break;
            }
        }
    }
}

/// A batch taken out of the buffer but not yet acked. Puts it back on drop, which also covers the runtime
/// aborting the uploader mid-rpc when the session ends.
struct InFlight<'a> {
    capture: &'a LogCapture,
    batch: Option<LogBatch>,
}

impl InFlight<'_> {
    /// The coordinator has it (or refused it for good), don't requeue.
    fn done(mut self) {
        self.batch = None;
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if let Some(batch) = self.batch.take() {
            self.capture.requeue(batch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    fn messages(batch: &LogBatch) -> Vec<&str> {
        batch.records.iter().map(|r| r.message.as_str()).collect()
    }

    #[test]
    fn test_captures_message_and_fields() {
        let capture = LogCapture::new();
        let subscriber = tracing_subscriber::registry().with(capture.layer());
        tracing::subscriber::with_default(subscriber, || {
            // explicit target, this module's own target is ignored on purpose
            tracing::warn!(target: "kuiper_tart_agent", vm = "runner-1", attempt = 2, "clone failed");
            tracing::info!(target: "h2::codec", "ignored");
        });

        let batch = capture.take_batch();
        assert_eq!(messages(&batch), vec!["clone failed vm=runner-1 attempt=2"]);
        assert_eq!(batch.records[0].level, "WARN");
    }

    #[test]
    fn test_overflow_drops_oldest_and_counts() {
        let capture = LogCapture::new();
        for i in 0..BUFFER_CAPACITY + 3 {
            capture.push("INFO", "t", format!("line {i}"));
        }

        let batch = capture.take_batch();
        assert_eq!(batch.dropped, 3);
        assert_eq!(batch.records.len(), MAX_BATCH);
        assert_eq!(batch.records[0].message, "line 3");

        // dropped count is only reported once
        assert_eq!(capture.take_batch().dropped, 0);
    }

    #[test]
    fn test_batch_capped_by_bytes() {
        let capture = LogCapture::new();
        let big = "x".repeat(MAX_MESSAGE_BYTES);
        for _ in 0..MAX_BATCH {
            capture.push("INFO", "t", big.clone());
        }

        let batch = capture.take_batch();
        assert!(batch.records.len() < MAX_BATCH);
        assert!(batch.records.len() * MAX_MESSAGE_BYTES <= MAX_BATCH_BYTES);
    }

    #[test]
    fn test_requeue_keeps_order() {
        let capture = LogCapture::new();
        for i in 0..3 {
            capture.push("INFO", "t", format!("line {i}"));
        }
        let batch = capture.take_batch();
        capture.push("INFO", "t", "line 3".to_string());
        capture.requeue(batch);

        let batch = capture.take_batch();
        assert_eq!(
            messages(&batch),
            vec!["line 0", "line 1", "line 2", "line 3"]
        );
    }

    #[test]
    fn test_skips_local_only_events() {
        let capture = LogCapture::new();
        let subscriber = tracing_subscriber::registry().with(capture.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "agent", local_only = tracing::field::Empty, "SSH exec: config.sh --token abc");
            tracing::info!(target: "agent", "kept");
        });
        assert_eq!(messages(&capture.take_batch()), vec!["kept"]);
    }

    #[test]
    fn test_dropped_in_flight_batch_is_requeued() {
        let capture = LogCapture::new();
        for i in 0..3 {
            capture.push("INFO", "t", format!("line {i}"));
        }
        let in_flight = InFlight {
            capture: &capture,
            batch: Some(capture.take_batch()),
        };
        drop(in_flight);
        assert_eq!(capture.take_batch().records.len(), 3);

        capture.push("INFO", "t", "line".to_string());
        let in_flight = InFlight {
            capture: &capture,
            batch: Some(capture.take_batch()),
        };
        in_flight.done();
        assert!(capture.take_batch().records.is_empty());
    }

    #[test]
    fn test_disable_clears_and_stops() {
        let capture = LogCapture::new();
        let subscriber = tracing_subscriber::registry().with(capture.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "agent", "before");
            capture.disable();
            tracing::info!(target: "agent", "after");
        });
        assert!(capture.take_batch().records.is_empty());
    }

    #[test]
    fn test_truncate_respects_char_boundary() {
        let mut s = "é".repeat(10);
        truncate_at_char_boundary(&mut s, 5);
        assert!(s.starts_with("éé"));
        assert!(s.ends_with("[truncated]"));
    }
}
