//! Tracing: spans in W3C trace context, kept or not by the shared sampling
//! rule, sent as OTLP (`fixwire-protocol` §3, §9).

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use crate::client::{Client, new_id};
use crate::hub::{FutureExt, Hub, lock};
use crate::limits::bounded;
use crate::otlp::{Export, attributes, nanos};
use crate::transport::{Category, Request};

/// OpenTelemetry's span kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    /// Work inside the process.
    Internal = 1,
    /// Serving a request.
    Server = 2,
    /// Calling another service, a database, …
    Client = 3,
    /// Sending a message.
    Producer = 4,
    /// Handling a message.
    Consumer = 5,
}

impl SpanKind {
    /// The kind an operation implies: `http.server` serves, `db.query`
    /// calls, `queue.publish` produces, `queue.process` consumes.
    fn of(op: &str) -> SpanKind {
        if op.ends_with(".server") {
            SpanKind::Server
        } else if op == "http.client" || op.starts_with("db") || op.ends_with(".client") {
            SpanKind::Client
        } else if op.ends_with(".publish") {
            SpanKind::Producer
        } else if op.ends_with(".process") {
            SpanKind::Consumer
        } else {
            SpanKind::Internal
        }
    }
}

/// A timed piece of work in a trace. A span without a parent in this process
/// (a request, a job) is a segment: it is sent with the spans finished under
/// it when it finishes. Cloning a span gives another handle to it.
#[derive(Clone)]
pub struct Span {
    inner: Arc<SpanInner>,
}

struct SpanInner {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    op: String,
    kind: SpanKind,
    sampled: bool,
    remote_parent: bool,
    tracestate: Option<String>,
    baggage: Option<String>,
    start: SystemTime,
    /// The segment this span belongs to; `None` when it is one.
    segment: Option<Span>,
    client: Option<Arc<Client>>,
    state: Mutex<SpanState>,
}

struct SpanState {
    name: String,
    attributes: Map<String, Value>,
    error: Option<String>,
    end: Option<SystemTime>,
    /// A segment's finished spans, until it is sent.
    children: Vec<Vec<u8>>,
    sent: bool,
}

/// Bounds the spans a segment keeps until it is sent.
const MAX_CHILDREN: usize = 1000;
/// Bounds a span's attributes, `fixwire.op` among them.
const MAX_ATTRIBUTES: usize = 128;
/// The longest `tracestate` and `baggage` passed on (the W3C limits).
const MAX_TRACESTATE: usize = 512;
const MAX_BAGGAGE: usize = 8192;
/// Bounds the spans of one request, and its bytes.
const MAX_BATCH: usize = 100;
const MAX_BATCH_BYTES: usize = 5 << 20;

impl Span {
    fn build(
        name: String,
        op: &str,
        trace: (String, Option<String>, bool, bool),
        context: (Option<String>, Option<String>),
        segment: Option<Span>,
        client: Option<Arc<Client>>,
    ) -> Span {
        let (trace_id, parent_span_id, sampled, remote_parent) = trace;
        Span {
            inner: Arc::new(SpanInner {
                trace_id,
                span_id: new_id(8),
                parent_span_id,
                op: op.to_owned(),
                kind: SpanKind::of(op),
                sampled,
                remote_parent,
                tracestate: context.0,
                baggage: context.1,
                start: SystemTime::now(),
                segment,
                client,
                state: Mutex::new(SpanState {
                    name,
                    attributes: Map::new(),
                    error: None,
                    end: None,
                    children: Vec::new(),
                    sent: false,
                }),
            }),
        }
    }

    /// A new trace, kept by the client's `traces_sample_rate`.
    pub fn new_trace(name: impl Into<String>, op: &str, client: Option<Arc<Client>>) -> Span {
        let trace_id = new_id(16);
        let rate = client
            .as_ref()
            .map_or(0.0, |c| c.options().traces_sample_rate);
        let sampled = sample_trace(&trace_id, rate);
        Span::build(
            name.into(),
            op,
            (trace_id, None, sampled, false),
            (None, None),
            None,
            client,
        )
    }

    /// A span continuing a caller's trace, from its W3C `traceparent`,
    /// `tracestate` and `baggage` headers; a new trace when `traceparent`
    /// is missing or malformed. A `tracestate` over 512 bytes or a `baggage`
    /// over 8,192, or either with a control character, is not passed on.
    pub fn continue_trace(
        traceparent: Option<&str>,
        tracestate: Option<&str>,
        baggage: Option<&str>,
        name: impl Into<String>,
        op: &str,
        client: Option<Arc<Client>>,
    ) -> Span {
        match traceparent.and_then(parse_traceparent) {
            Some((trace, parent, sampled)) => Span::build(
                name.into(),
                op,
                (trace, Some(parent), sampled, true),
                (
                    propagated(tracestate, MAX_TRACESTATE),
                    propagated(baggage, MAX_BAGGAGE),
                ),
                None,
                client,
            ),
            None => Span::new_trace(name, op, client),
        }
    }

    /// A span under this one.
    pub fn start_child(&self, name: impl Into<String>, op: &str) -> Span {
        let i = &self.inner;
        let segment = i.segment.clone().unwrap_or_else(|| self.clone());
        Span::build(
            name.into(),
            op,
            (
                i.trace_id.clone(),
                Some(i.span_id.clone()),
                i.sampled,
                false,
            ),
            (i.tracestate.clone(), i.baggage.clone()),
            Some(segment),
            i.client.clone(),
        )
    }

    /// The trace's id: 32 hex characters.
    pub fn trace_id(&self) -> &str {
        &self.inner.trace_id
    }

    /// The span's id: 16 hex characters.
    pub fn span_id(&self) -> &str {
        &self.inner.span_id
    }

    /// Whether the trace is kept: an unsampled span still carries the trace
    /// to the services it calls.
    pub fn sampled(&self) -> bool {
        self.inner.sampled
    }

    /// The W3C `traceparent` header that continues the trace in a service
    /// this one calls.
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{}",
            self.inner.trace_id,
            self.inner.span_id,
            if self.inner.sampled { "01" } else { "00" }
        )
    }

    /// The caller's `tracestate`, passed on.
    pub fn tracestate(&self) -> Option<&str> {
        self.inner.tracestate.as_deref()
    }

    /// The caller's `baggage`, passed on.
    pub fn baggage(&self) -> Option<&str> {
        self.inner.baggage.as_deref()
    }

    /// Renames the span, such as after the route a request matched.
    pub fn set_name(&self, name: impl Into<String>) {
        lock(&self.inner.state).name = name.into();
    }

    /// Sets an attribute (OpenTelemetry's semantic conventions). A span keeps
    /// at most 128, `fixwire.op` among them: past that, new keys are dropped.
    pub fn set_attribute(&self, key: impl Into<String>, value: impl Into<Value>) {
        let key = key.into();
        let mut state = lock(&self.inner.state);
        if state.attributes.len() < MAX_ATTRIBUTES - 1 || state.attributes.contains_key(&key) {
            state.attributes.insert(key, value.into());
        }
    }

    /// Marks the span failed, with what went wrong.
    pub fn set_error(&self, message: impl Into<String>) {
        lock(&self.inner.state).error = Some(message.into());
    }

    /// Ends the span. A segment is sent with the spans finished under it; a
    /// span finishing after its segment is sent alone. Finishing again does
    /// nothing.
    pub fn finish(&self) {
        {
            let mut state = lock(&self.inner.state);
            if state.end.is_some() {
                return;
            }
            state.end = Some(SystemTime::now());
        }
        let Some(client) = self
            .inner
            .client
            .as_ref()
            .filter(|c| self.inner.sampled && c.is_enabled())
        else {
            return;
        };
        crate::hub::guarded(|| {
            let Ok(me) = serde_json::to_vec(&self.to_json(client)) else {
                return;
            };
            match &self.inner.segment {
                None => {
                    let mut spans = {
                        let mut state = lock(&self.inner.state);
                        state.sent = true;
                        std::mem::take(&mut state.children)
                    };
                    spans.push(me);
                    send_spans(client, spans);
                }
                Some(segment) => {
                    let mut state = lock(&segment.inner.state);
                    if state.sent {
                        drop(state);
                        send_spans(client, vec![me]);
                    } else if state.children.len() < MAX_CHILDREN {
                        state.children.push(me);
                    }
                }
            }
        });
    }

    /// The span as OTLP JSON, redacted by `client`, its attributes' values
    /// bounded and its strings cut to `max_value_length`.
    fn to_json(&self, client: &Client) -> Value {
        let i = &self.inner;
        let state = lock(&i.state);
        let mut attrs: Map<String, Value> = state
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), bounded(v)))
            .collect();
        attrs.insert("fixwire.op".into(), i.op.clone().into());
        let attrs = client.scrub(attrs, &[]);
        let flags = 0x100 | if i.remote_parent { 0x200 } else { 0 } | u32::from(i.sampled);
        let status = match &state.error {
            Some(m) => json!({"code": 2, "message": client.clean(m)}),
            None => json!({"code": 1}),
        };
        let mut out = json!({
            "traceId": i.trace_id, "spanId": i.span_id, "name": client.clean(&state.name), "kind": i.kind as u8,
            "startTimeUnixNano": nanos(i.start), "endTimeUnixNano": nanos(state.end.unwrap_or_else(SystemTime::now)),
            "attributes": attributes(&attrs), "status": status, "flags": flags,
        });
        if let Some(parent) = &i.parent_span_id {
            out["parentSpanId"] = parent.clone().into();
        }
        out
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Span")
            .field("name", &lock(&self.inner.state).name)
            .field("op", &self.inner.op)
            .field("trace_id", &self.inner.trace_id)
            .field("span_id", &self.inner.span_id)
            .field("sampled", &self.inner.sampled)
            .finish_non_exhaustive()
    }
}

/// Sends finished spans (their JSON) as OTLP traces exports of at most 100
/// spans and 5 MB each; a span that can't fit in one alone is dropped.
fn send_spans(client: &Client, spans: Vec<Vec<u8>>) {
    let (head, tail) = client.export_ends(Export::Spans);
    let ends = head.len() + tail.len();
    let send = |batch: &[Vec<u8>]| {
        let body = client.export(Export::Spans, batch);
        client.send(Request::json("/v1/traces", Category::Span, body));
    };
    let (mut batch, mut size) = (Vec::new(), ends);
    for span in spans {
        if ends + span.len() > MAX_BATCH_BYTES {
            client.log(|| format!("dropped a span of {} bytes: larger than 5 MB", span.len()));
            continue;
        }
        // The span, and the comma before it.
        let more = span.len() + usize::from(!batch.is_empty());
        if batch.len() == MAX_BATCH || size + more > MAX_BATCH_BYTES {
            send(&batch);
            batch.clear();
            size = ends;
        }
        size += span.len() + usize::from(!batch.is_empty());
        batch.push(span);
    }
    if !batch.is_empty() {
        send(&batch);
    }
}

/// Decides a new trace the way every Fixwire SDK does: kept when its id's
/// last 56 bits, as a fraction of 2^56, are at least `1 - rate`.
pub(crate) fn sample_trace(trace_id: &str, rate: f64) -> bool {
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    let Some(tail) = trace_id
        .get(trace_id.len().saturating_sub(14)..)
        .filter(|t| t.len() == 14)
    else {
        return false;
    };
    match u64::from_str_radix(tail, 16) {
        Ok(n) => n as f64 / (1u64 << 56) as f64 >= 1.0 - rate,
        Err(_) => false,
    }
}

/// A caller's `tracestate` or `baggage` to pass on: every span of the trace
/// holds it and every request it makes carries it, so one over `max` bytes,
/// or one with a control character that would break a header (CR, LF), stays
/// out whole. Tabs are whitespace the W3C headers allow.
fn propagated(header: Option<&str>, max: usize) -> Option<String> {
    header
        .filter(|h| h.len() <= max && !h.bytes().any(|b| b.is_ascii_control() && b != b'\t'))
        .map(str::to_owned)
}

/// Reads `00-<trace id>-<parent id>-<flags>`, in lower-case hex as W3C trace
/// context writes it: the trace, the parent and whether the trace is
/// sampled. Anything else (another version, an id of zeros) is `None`.
pub(crate) fn parse_traceparent(h: &str) -> Option<(String, String, bool)> {
    let parts: Vec<&str> = h.trim().split('-').collect();
    let [version, trace, parent, flags] = parts[..] else {
        return None;
    };
    let hex = |s: &str| s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let zero = |s: &str| s.bytes().all(|b| b == b'0');
    if version != "00" || trace.len() != 32 || parent.len() != 16 || flags.len() != 2 {
        return None;
    }
    if !hex(trace) || !hex(parent) || !hex(flags) || zero(trace) || zero(parent) {
        return None;
    }
    let flags = u8::from_str_radix(flags, 16).ok()?;
    Some((trace.to_owned(), parent.to_owned(), flags & 1 == 1))
}

/// Starts a span under the current scope's span, or a new trace. Finish it
/// when the work ends; `trace` does both.
pub fn start_span(name: impl Into<String>, op: &str) -> Span {
    let hub = Hub::current();
    match hub.try_configure_scope(|s| s.span.clone()).flatten() {
        Some(parent) => parent.start_child(name, op),
        None => Span::new_trace(name, op, hub.client()),
    }
}

/// Runs `f` as a span under the current scope's span (or a new trace): the
/// span is the scope's while `f` runs, and finished after, failed when `f`
/// panicked.
///
/// ```
/// let total = fixwire::trace("SELECT orders", "db.query", |span| {
///     span.set_attribute("db.system.name", "postgresql");
///     42
/// });
/// ```
pub fn trace<T>(name: impl Into<String>, op: &str, f: impl FnOnce(&Span) -> T) -> T {
    let span = start_span(name, op);
    let _finish = FinishOnDrop(&span);
    Hub::current().with_scope(|s| s.set_span(Some(span.clone())), || f(&span))
}

/// Runs a future as a span under the current scope's span (or a new
/// trace), in a fork of the current hub whose scope has the span.
pub fn trace_future<F: Future>(
    name: impl Into<String>,
    op: &str,
    future: F,
) -> impl Future<Output = F::Output> {
    let span = start_span(name, op);
    let hub = Hub::current().fork();
    hub.configure_scope(|s| s.set_span(Some(span.clone())));
    async move {
        let _finish = FinishOnDrop(&span);
        future.bind_hub(hub).await
    }
}

/// Finishes a span when dropped: failed when the thread is panicking.
struct FinishOnDrop<'a>(&'a Span);

impl Drop for FinishOnDrop<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.set_error("panicked");
        }
        self.0.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_traces_alike_in_every_sdk() {
        assert!(!sample_trace("4bf92f3577b34da6a3ce929d0e0e4736", 0.0));
        assert!(sample_trace("4bf92f3577b34da6a3ce929d0e0e4736", 1.0));
        // The last 56 bits, 0x4fffffffffffff, are 0.31 of the range: kept from a rate of 0.69.
        assert!(sample_trace("0000000000000000004fffffffffffff", 0.7));
        assert!(!sample_trace("0000000000000000004fffffffffffff", 0.6));
        assert!(!sample_trace("00000000000000000000000000000001", 0.5));
    }

    #[test]
    fn reads_traceparents() {
        assert_eq!(
            parse_traceparent(" 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01 "),
            Some((
                "4bf92f3577b34da6a3ce929d0e0e4736".into(),
                "00f067aa0ba902b7".into(),
                true
            ))
        );
        assert_eq!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
                .map(|t| t.2),
            Some(false)
        );
        for bad in [
            "",
            "00-4bf9-00f0-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0x",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-+1",
        ] {
            assert_eq!(parse_traceparent(bad), None, "{bad}");
        }
    }

    #[test]
    fn children_carry_the_trace() {
        let root = Span::continue_trace(
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            Some("v=1"),
            None,
            "GET /",
            "http.server",
            None,
        );
        let child = root.start_child("SELECT", "db.query");
        assert_eq!(
            (child.trace_id(), child.sampled(), child.tracestate()),
            (root.trace_id(), true, Some("v=1"))
        );
        assert_eq!(child.inner.parent_span_id.as_deref(), Some(root.span_id()));
        assert_eq!(child.inner.kind, SpanKind::Client);
        assert!(child.traceparent().ends_with("-01"));
    }

    #[test]
    fn passes_on_only_sane_trace_headers() {
        let parent = Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
        let huge = "k=v,".repeat(3000);
        let span = Span::continue_trace(
            parent,
            Some("v=1\r\nX-Injected: 1"),
            Some(&huge),
            "GET /",
            "http.server",
            None,
        );
        assert_eq!((span.tracestate(), span.baggage()), (None, None));
        let span = Span::continue_trace(
            parent,
            Some("v=1,\tw=2"),
            Some("plan=team,\tregion=eu"),
            "GET /",
            "http.server",
            None,
        );
        assert_eq!(
            (span.tracestate(), span.baggage()),
            (Some("v=1,\tw=2"), Some("plan=team,\tregion=eu")),
            "a tab is list whitespace"
        );
        // At the limits they pass; a byte over, they are dropped whole.
        let sized = |n: usize| format!("k={}", "v".repeat(n - 2));
        for (state, baggage, passed) in [
            (sized(512), sized(8192), true),
            (sized(513), sized(8192), false),
            (sized(512), sized(8193), false),
        ] {
            let span = Span::continue_trace(
                parent,
                Some(&state),
                Some(&baggage),
                "GET /",
                "http.server",
                None,
            );
            assert_eq!(span.tracestate().is_some(), state.len() <= 512);
            assert_eq!(span.baggage().is_some(), baggage.len() <= 8192);
            assert_eq!(
                span.tracestate().is_some() && span.baggage().is_some(),
                passed
            );
        }
        for control in [
            "\u{0}", "\u{8}", "\u{a}", "\u{b}", "\u{1f}", "\u{7f}", "\r", "\u{1b}",
        ] {
            let header = format!("k=v{control}");
            let span = Span::continue_trace(
                parent,
                Some(&header),
                Some(&header),
                "GET /",
                "http.server",
                None,
            );
            assert_eq!(
                (span.tracestate(), span.baggage()),
                (None, None),
                "{header:?}"
            );
        }
    }
}
