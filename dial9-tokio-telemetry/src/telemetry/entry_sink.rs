//! EntrySink implementation that writes metrique entries into the dial9 trace buffer.

use crate::telemetry::buffer::BUFFER;
use metrique::writer::value::{FlagConstructor, ForceFlag, MetricOptions};
use metrique::writer::{Entry, EntrySink, MetricFlags, sink::FlushWait};
use std::marker::PhantomData;

/// Marker type for span-boundary metrics. Downcasted by [`BinaryEntryWriter`] to set
/// bit 0 of the per-metric flags byte in the data blob, indicating this field
/// defines the logical span duration (used to compute start time from end timestamp).
#[derive(Debug)]
pub struct SpanFlag;
impl MetricOptions for SpanFlag {}

/// Implementation detail for [`Span`]. Not public API.
#[doc(hidden)]
pub struct SpanFlagCtor;
impl FlagConstructor for SpanFlagCtor {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&SpanFlag)
    }
}

/// Wraps a value to mark it as a span-boundary metric in the dial9 trace.
/// Fields marked `Span` define the logical duration of the operation — the
/// viewer uses them to compute start time and for drill-down filtering.
///
/// ```ignore
/// #[metrics]
/// struct MyEntry {
///     #[metrics(unit = Millisecond)]
///     duration: Span<Timer>,
/// }
/// ```
pub type Span<T> = ForceFlag<T, SpanFlagCtor>;

/// Backwards-compatible alias.
#[deprecated(note = "renamed to Span")]
pub type Kpi<T> = Span<T>;

/// An [`EntrySink`] that serializes metrique entries into the dial9 thread-local
/// trace buffer. Entries appear as `MetriqueEvent`s in the trace.
///
/// `entry_name` is a static label written into the trace so the viewer can
/// identify the entry type (e.g. `"RequestMetrics"`).
pub struct Dial9EntrySink<E> {
    entry_name: &'static str,
    _phantom: PhantomData<fn(E)>,
}

impl<E> Dial9EntrySink<E> {
    pub const fn new(entry_name: &'static str) -> Self {
        Self {
            entry_name,
            _phantom: PhantomData,
        }
    }
}

impl<E: Entry> EntrySink<E> for Dial9EntrySink<E> {
    fn append(&self, entry: E) {
        BUFFER.with(|buf| {
            let mut buf = buf.borrow_mut();
            buf.record_metrique_entry(&entry, 0, self.entry_name);
        });
    }

    fn flush_async(&self) -> FlushWait {
        FlushWait::ready()
    }
}

impl<E: Entry> EntrySink<E> for &Dial9EntrySink<E> {
    fn append(&self, entry: E) {
        (*self).append(entry);
    }

    fn flush_async(&self) -> FlushWait {
        FlushWait::ready()
    }
}
