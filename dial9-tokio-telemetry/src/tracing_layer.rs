//! Tracing subscriber layer that emits span events into dial9 traces.
//!
//! Requires the `tracing-layer` feature.
//!
//! # Usage
//!
//! ```ignore
//! use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
//! use tracing_subscriber::prelude::*;
//!
//! tracing_subscriber::registry()
//!     .with(Dial9TokioLayer::new())
//!     .init();
//! ```
//!
//! The layer emits events only on threads owned by a dial9-traced runtime.
//! On other threads, span enter/exit is silently skipped.
//!
//! # High-frequency spans
//!
//! Every span enter and exit produces a trace event. If you instrument tight
//! loops, the volume can be large. Libraries like the AWS SDK emit many
//! internal spans (`deserialization`, `serialization`, `try_attempt`, etc.)
//! that can produce over 100K span events per second and quickly fill the
//! trace buffer.
//!
//! You can use per-layer `tracing_subscriber::filter::Targets` filter to restrict
//! which spans reach the dial9 layer. This keeps your fmt/logging layer
//! unaffected while controlling trace volume:
//!
//! ```ignore
//! use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
//! use tracing_subscriber::prelude::*;
//!
//! tracing_subscriber::registry()
//!     .with(tracing_subscriber::fmt::layer())
//!     .with(
//!         Dial9TokioLayer::new().with_filter(
//!             tracing_subscriber::filter::Targets::new()
//!                 .with_target("my_app", tracing::Level::DEBUG)
//!                 .with_default(tracing::Level::WARN),
//!         ),
//!     )
//!     .init();
//! ```
//!
//! This captures all spans from `my_app` while filtering out noisy
//! third-party spans (AWS SDK, hyper, tower, etc.).
//!
//! # Overhead
//!
//! Each span enter+exit pair costs roughly **300ns** total (tracing dispatch
//! plus dial9 encoding), of which **~50-100ns** is the dial9 encoding overhead.
//! Measured with `NullWriter` on a `current_thread` runtime to isolate
//! encoding from I/O. This scales linearly with nesting depth and is
//! comparable to the cost of a single poll event, so the layer is suitable
//! for production use with appropriate span filtering.

use crate::telemetry::{TelemetryHandle, clock_monotonic_ns, current_worker_id};
use dial9_trace_format::encoder::Schema;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use tracing::callsite::Identifier;
use tracing::span;
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

// ── Per-callsite schema cache ───────────────────────────────────────────────

/// Cached schemas for a single callsite (one for enter, one for exit).
#[derive(Clone)]
struct CallsiteSchemas {
    enter: Schema,
    exit: Schema,
    /// Field names from the callsite metadata, in order.
    field_names: Vec<&'static str>,
}

/// Build the enter and exit schemas for a callsite.
fn build_callsite_schemas(meta: &'static tracing::Metadata<'static>) -> CallsiteSchemas {
    let file = meta.file().unwrap_or("unknown");
    let line = meta.line().unwrap_or(0);
    let schema_id = format!("{}::{}:{}:{}", meta.target(), meta.name(), file, line);

    // Base fields present on all span events
    let mut enter_fields = vec![
        FieldDef {
            name: "worker_id".into(),
            field_type: FieldType::Varint,
        },
        FieldDef {
            name: "span_id".into(),
            field_type: FieldType::Varint,
        },
        FieldDef {
            name: "parent_span_id".into(),
            field_type: FieldType::OptionalVarint,
        },
        FieldDef {
            name: "span_name".into(),
            field_type: FieldType::PooledString,
        },
    ];
    let mut exit_fields = vec![
        FieldDef {
            name: "worker_id".into(),
            field_type: FieldType::Varint,
        },
        FieldDef {
            name: "span_id".into(),
            field_type: FieldType::Varint,
        },
        FieldDef {
            name: "span_name".into(),
            field_type: FieldType::PooledString,
        },
    ];

    // Add user-defined fields as optional interned strings
    let mut field_names = Vec::new();
    for field in meta.fields() {
        let name = field.name();
        field_names.push(name);
        let def = FieldDef {
            name: name.to_string(),
            field_type: FieldType::OptionalPooledString,
        };
        enter_fields.push(def.clone());
        exit_fields.push(def);
    }

    CallsiteSchemas {
        enter: Schema::new(&format!("SpanEnter:{schema_id}"), enter_fields),
        exit: Schema::new(&format!("SpanExit:{schema_id}"), exit_fields),
        field_names,
    }
}

// ── Per-span storage ────────────────────────────────────────────────────────

/// Data stored in span extensions, captured at `on_new_span` and updated by `on_record`.
struct SpanData {
    meta: &'static tracing::Metadata<'static>,
    parent_id: Option<span::Id>,
    /// Field values keyed by field name.
    field_values: Vec<(&'static str, String)>,
}

impl fmt::Debug for SpanData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpanData")
            .field("name", &self.meta.name())
            .finish_non_exhaustive()
    }
}

/// Visitor that collects span field values.
struct FieldVisitor<'a> {
    values: &'a mut Vec<(&'static str, String)>,
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.upsert(field.name(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.upsert(field.name(), value.to_owned());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.upsert(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.upsert(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.upsert(field.name(), value.to_string());
    }
}

impl FieldVisitor<'_> {
    fn upsert(&mut self, name: &'static str, value: String) {
        if let Some(entry) = self.values.iter_mut().find(|(k, _)| *k == name) {
            entry.1 = value;
        } else {
            self.values.push((name, value));
        }
    }
}

// ── Layer ───────────────────────────────────────────────────────────────────

/// A [`tracing_subscriber::Layer`] that emits span enter/exit events into
/// the dial9 trace buffer.
///
/// Each unique callsite gets its own wire schema with typed fields,
/// avoiding the overhead of a generic `StringMap` encoding. Field values
/// are interned as `PooledString`s for compact wire representation.
///
/// # Setup
///
/// ```ignore
/// use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
/// use tracing_subscriber::prelude::*;
///
/// tracing_subscriber::registry()
///     .with(Dial9TokioLayer::new())
///     .init();
/// ```
pub struct Dial9TokioLayer {
    schemas: Mutex<HashMap<Identifier, CallsiteSchemas>>,
}

impl fmt::Debug for Dial9TokioLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9TokioLayer").finish_non_exhaustive()
    }
}

impl Dial9TokioLayer {
    /// Create a new layer.
    pub fn new() -> Self {
        Self {
            schemas: Mutex::new(HashMap::new()),
        }
    }

    fn get_schemas(&self, meta: &'static tracing::Metadata<'static>) -> CallsiteSchemas {
        let id = meta.callsite();
        let mut cache = self.schemas.lock().unwrap();
        cache
            .entry(id)
            .or_insert_with(|| build_callsite_schemas(meta))
            .clone()
    }
}

impl Default for Dial9TokioLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Look up a field value by name in the span's field list.
fn field_value<'a>(fields: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.as_str())
}

impl<S> Layer<S> for Dial9TokioLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut field_values = Vec::new();
        attrs.record(&mut FieldVisitor {
            values: &mut field_values,
        });

        let data = SpanData {
            meta: attrs.metadata(),
            parent_id: attrs.parent().cloned(),
            field_values,
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
                    values: &mut data.field_values,
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
        let ts = clock_monotonic_ns();

        let Some(span_ref) = ctx.span(id) else { return };
        let ext = span_ref.extensions();
        let Some(data) = ext.get::<SpanData>() else {
            return;
        };

        let schemas = self.get_schemas(data.meta);

        // We only use explicit parents (span!(parent: &x, ..)), not contextual
        // parents (ctx.current_span()), because contextual parenting is
        // unreliable across tasks on the same worker thread. See:
        // https://chesedo.me/blog/rust-tracing-incorrect-parent-spans-async-futures-joinset/
        // The viewer infers nesting from timestamp containment instead.
        let parent_span_id = data.parent_id.as_ref().map(|id| id.into_u64());
        let span_name = data.meta.name();

        // Encode directly into the thread-local buffer (no clone needed)
        handle.with_encoder(|enc| {
            let mut values = Vec::with_capacity(5 + schemas.field_names.len());
            values.push(FieldValue::Varint(ts));
            values.push(FieldValue::Varint(worker_id.as_u64()));
            values.push(FieldValue::Varint(span_id));
            match parent_span_id {
                Some(pid) => values.push(FieldValue::Varint(pid)),
                None => values.push(FieldValue::None),
            }
            values.push(FieldValue::PooledString(enc.intern_string(span_name)));
            for &name in &schemas.field_names {
                match field_value(&data.field_values, name) {
                    Some(v) => values.push(FieldValue::PooledString(enc.intern_string(v))),
                    None => values.push(FieldValue::None),
                }
            }
            enc.write_event(&schemas.enter, &values);
        });
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        let Some(handle) = TelemetryHandle::try_current() else {
            return;
        };

        let worker_id = current_worker_id();
        let span_id = id.into_u64();
        let ts = clock_monotonic_ns();

        let Some(span_ref) = ctx.span(id) else { return };
        let ext = span_ref.extensions();
        let Some(data) = ext.get::<SpanData>() else {
            return;
        };

        let schemas = self.get_schemas(data.meta);
        let span_name = data.meta.name();

        handle.with_encoder(|enc| {
            let mut values = Vec::with_capacity(4 + schemas.field_names.len());
            values.push(FieldValue::Varint(ts));
            values.push(FieldValue::Varint(worker_id.as_u64()));
            values.push(FieldValue::Varint(span_id));
            values.push(FieldValue::PooledString(enc.intern_string(span_name)));
            for &name in &schemas.field_names {
                match field_value(&data.field_values, name) {
                    Some(v) => values.push(FieldValue::PooledString(enc.intern_string(v))),
                    None => values.push(FieldValue::None),
                }
            }
            enc.write_event(&schemas.exit, &values);
        });
    }
}
