//! Tracing subscriber layer for emitting span events into dial9 traces.

use dial9_tokio_telemetry::telemetry::{
    Encodable, TelemetryHandle, ThreadLocalEncoder, WorkerId, clock_monotonic_ns, current_worker_id,
};
use dial9_trace_format::{InternedString, TraceEvent};
use std::fmt;
use tracing::span;
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

// ── Wire events ─────────────────────────────────────────────────────────────

/// Wire event emitted when a tracing span is entered.
#[derive(TraceEvent)]
struct SpanEnterEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: WorkerId,
    span_id: u64,
    parent_span_id: Option<u64>,
    span_name: InternedString,
    fields: Vec<(String, String)>,
}

/// Wire event emitted when a tracing span is exited.
#[derive(TraceEvent)]
struct SpanExitEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: WorkerId,
    span_id: u64,
    span_name: InternedString,
    fields: Vec<(String, String)>,
}

// ── Per-span storage ────────────────────────────────────────────────────────

/// Data stored in span extensions, captured at `on_new_span` and updated by `on_record`.
#[derive(Debug, Clone)]
struct SpanData {
    name: &'static str,
    parent_id: Option<span::Id>,
    fields: Vec<(String, String)>,
}

/// Visitor that collects span field values into a `Vec<(String, String)>`.
struct FieldVisitor<'a> {
    fields: &'a mut Vec<(String, String)>,
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.fields
            .push((field.name().to_owned(), format!("{value:?}")));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .push((field.name().to_owned(), value.to_owned()));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }
}

// ── Layer ───────────────────────────────────────────────────────────────────

/// A [`tracing_subscriber::Layer`] that emits span enter/exit events into
/// the dial9 trace buffer.
///
/// Span events land in the same thread-local buffer as poll, park, and wake
/// events, carrying monotonic timestamps for correlation. The viewer renders
/// them as nested bars within poll blocks.
///
/// # Setup
///
/// ```ignore
/// use dial9_tracing::Dial9TokioLayer;
/// use tracing_subscriber::prelude::*;
///
/// tracing_subscriber::registry()
///     .with(Dial9TokioLayer::new())
///     .init();
/// ```
#[derive(Debug)]
pub struct Dial9TokioLayer {
    _private: (),
}

impl Dial9TokioLayer {
    /// Create a new layer.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for Dial9TokioLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of span data needed for emitting events.
struct SpanSnapshot {
    name: &'static str,
    fields: Vec<(String, String)>,
    parent_span_id: Option<u64>,
}

/// Read a span's own data from extensions.
fn span_snapshot<S>(id: &span::Id, ctx: &Context<'_, S>) -> Option<SpanSnapshot>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let span = ctx.span(id)?;
    let ext = span.extensions();
    let data = ext.get::<SpanData>()?;
    Some(SpanSnapshot {
        name: data.name,
        fields: data.fields.clone(),
        parent_span_id: data.parent_id.as_ref().map(|id| id.into_u64()),
    })
}

/// Enter event that interns the span name during encoding.
struct EnterEncodable {
    timestamp_ns: u64,
    worker_id: WorkerId,
    span_id: u64,
    parent_span_id: Option<u64>,
    span_name: String,
    fields: Vec<(String, String)>,
}

impl Encodable for EnterEncodable {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let interned_name = enc.intern_string(&self.span_name);
        enc.encode(&SpanEnterEvent {
            timestamp_ns: self.timestamp_ns,
            worker_id: self.worker_id,
            span_id: self.span_id,
            parent_span_id: self.parent_span_id,
            span_name: interned_name,
            fields: self.fields.clone(),
        });
    }
}

/// Exit event that interns the span name during encoding.
struct ExitEncodable {
    timestamp_ns: u64,
    worker_id: WorkerId,
    span_id: u64,
    span_name: String,
    fields: Vec<(String, String)>,
}

impl Encodable for ExitEncodable {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let interned_name = enc.intern_string(&self.span_name);
        enc.encode(&SpanExitEvent {
            timestamp_ns: self.timestamp_ns,
            worker_id: self.worker_id,
            span_id: self.span_id,
            span_name: interned_name,
            fields: self.fields.clone(),
        });
    }
}

impl<S> Layer<S> for Dial9TokioLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut fields = Vec::new();
        attrs.record(&mut FieldVisitor {
            fields: &mut fields,
        });

        let data = SpanData {
            name: attrs.metadata().name(),
            parent_id: attrs
                .parent()
                .cloned()
                .or_else(|| ctx.current_span().id().cloned()),
            fields,
        };

        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(data);
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(data) = extensions.get_mut::<SpanData>() {
                values.record(&mut FieldVisitor {
                    fields: &mut data.fields,
                });
            }
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        let Some(handle) = TelemetryHandle::try_current() else {
            return;
        };

        let worker_id = current_worker_id();
        let span_id = id.into_u64();
        let snap = span_snapshot(id, &ctx);

        dial9_tokio_telemetry::telemetry::record_event(
            EnterEncodable {
                timestamp_ns: clock_monotonic_ns(),
                worker_id,
                span_id,
                parent_span_id: snap.as_ref().and_then(|s| s.parent_span_id),
                span_name: snap.as_ref().map_or("unknown", |s| s.name).to_owned(),
                fields: snap.map(|s| s.fields).unwrap_or_default(),
            },
            &handle,
        );
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        let Some(handle) = TelemetryHandle::try_current() else {
            return;
        };

        let worker_id = current_worker_id();
        let span_id = id.into_u64();
        let snap = span_snapshot(id, &ctx);

        dial9_tokio_telemetry::telemetry::record_event(
            ExitEncodable {
                timestamp_ns: clock_monotonic_ns(),
                worker_id,
                span_id,
                span_name: snap.as_ref().map_or("unknown", |s| s.name).to_owned(),
                fields: snap.map(|s| s.fields).unwrap_or_default(),
            },
            &handle,
        );
    }
}
