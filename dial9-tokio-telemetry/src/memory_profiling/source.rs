//! `Source` impl that drains the alloc and free queues each flush cycle.

use crate::memory_profiling::ring::{RawAlloc, RawFree};
use crate::telemetry::buffer::with_encoder;
use crate::telemetry::format::{AllocEvent, FreeEvent};
use crate::telemetry::recorder::source::{FlushContext, Source};
use crossbeam_queue::ArrayQueue;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Liveset entry held by the consolidator.
#[derive(Debug, Clone, Copy)]
struct LivesetEntry {
    size: u64,
    timestamp_ns: u64,
}

/// Drains the alloc and free queues into the trace each flush cycle.
pub(crate) struct MemoryProfileSource {
    alloc_queue: Arc<ArrayQueue<RawAlloc>>,
    free_queue: Arc<ArrayQueue<RawFree>>,
    liveset: Option<HashMap<u64, LivesetEntry>>,
    dropped_allocs: Arc<AtomicU64>,
    dropped_frees: Arc<AtomicU64>,
}

impl MemoryProfileSource {
    #[allow(dead_code)]
    pub(crate) fn new(
        alloc_queue: Arc<ArrayQueue<RawAlloc>>,
        free_queue: Arc<ArrayQueue<RawFree>>,
        dropped_allocs: Arc<AtomicU64>,
        dropped_frees: Arc<AtomicU64>,
        track_liveset: bool,
    ) -> Self {
        Self {
            alloc_queue,
            free_queue,
            liveset: if track_liveset {
                Some(HashMap::new())
            } else {
                None
            },
            dropped_allocs,
            dropped_frees,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn dropped(&self) -> (u64, u64) {
        (
            self.dropped_allocs.load(Ordering::Relaxed),
            self.dropped_frees.load(Ordering::Relaxed),
        )
    }

    fn handle_alloc(&mut self, a: RawAlloc, ctx: &FlushContext<'_>) {
        let frames = a.frames().to_vec();
        let ts_ns = a.ts_ns;
        let tid = a.tid;
        let size = a.size;
        let addr = a.addr;
        with_encoder(
            |enc| {
                let callchain = enc.intern_stack_frames(&frames);
                enc.encode(&AllocEvent {
                    timestamp_ns: ts_ns,
                    tid,
                    size,
                    addr,
                    callchain,
                });
            },
            ctx.collector,
            ctx.drain_epoch,
        );
        if let Some(liveset) = self.liveset.as_mut() {
            liveset.insert(
                addr,
                LivesetEntry {
                    size,
                    timestamp_ns: ts_ns,
                },
            );
        }
    }

    fn handle_free(&mut self, f: RawFree, ctx: &FlushContext<'_>) {
        let Some(liveset) = self.liveset.as_mut() else {
            return;
        };
        let Some(entry) = liveset.remove(&f.addr) else {
            return;
        };
        with_encoder(
            |enc| {
                enc.encode(&FreeEvent {
                    timestamp_ns: f.ts_ns,
                    tid: f.tid,
                    addr: f.addr,
                    size: entry.size,
                    alloc_timestamp_ns: entry.timestamp_ns,
                });
            },
            ctx.collector,
            ctx.drain_epoch,
        );
    }
}

impl Source for MemoryProfileSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        while let Some(a) = self.alloc_queue.pop() {
            self.handle_alloc(a, ctx);
        }
        while let Some(f) = self.free_queue.pop() {
            self.handle_free(f, ctx);
        }
    }

    fn name(&self) -> &'static str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_profiling::ring::{MAX_FRAMES, RawAlloc, RawFree};
    use crate::telemetry::buffer::drain_to_collector;
    use crate::telemetry::collector::CentralCollector;
    use crate::telemetry::events::{TelemetryEvent, ThreadRole};
    use crate::telemetry::format::decode_events;
    use crate::telemetry::recorder::source::FlushContext;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    fn make_raw_alloc(addr: u64, size: u64, ts_ns: u64) -> RawAlloc {
        let mut frames = [0u64; MAX_FRAMES];
        frames[0] = 0xAAAA;
        frames[1] = 0xBBBB;
        frames[2] = 0xCCCC;
        RawAlloc {
            tid: 1,
            size,
            addr,
            ts_ns,
            frames,
            frame_count: 3,
        }
    }

    fn make_raw_free(addr: u64, ts_ns: u64) -> RawFree {
        RawFree {
            tid: 2,
            addr,
            size: 0, // size on the free side is informational; consolidator uses liveset
            ts_ns,
        }
    }

    fn flush_and_collect(source: &mut MemoryProfileSource) -> Vec<TelemetryEvent> {
        let collector = Arc::new(CentralCollector::new());
        let drain_epoch = AtomicU64::new(0);
        let thread_roles: HashMap<u32, ThreadRole> = HashMap::new();
        let ctx = FlushContext {
            collector: &collector,
            drain_epoch: &drain_epoch,
            thread_roles: &thread_roles,
        };
        source.flush(&ctx);
        drain_to_collector(&collector);
        let mut events = Vec::new();
        while let Some(batch) = collector.next() {
            if let Ok(decoded) = decode_events(&batch.encoded_bytes) {
                events.extend(decoded);
            }
        }
        events
    }

    #[test]
    fn source_emits_alloc_event() {
        let alloc_queue = Arc::new(ArrayQueue::new(16));
        let free_queue = Arc::new(ArrayQueue::new(16));
        alloc_queue.push(make_raw_alloc(0x1000, 4096, 100)).ok();

        let mut source = MemoryProfileSource::new(
            Arc::clone(&alloc_queue),
            Arc::clone(&free_queue),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            false,
        );

        let events = flush_and_collect(&mut source);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
            .collect();
        assert_eq!(allocs.len(), 1);
        match &allocs[0] {
            TelemetryEvent::Alloc {
                timestamp_nanos,
                tid,
                size,
                addr,
                callchain,
            } => {
                assert_eq!(*timestamp_nanos, 100);
                assert_eq!(*tid, 1);
                assert_eq!(*size, 4096);
                assert_eq!(*addr, 0x1000);
                assert_eq!(callchain, &[0xAAAA, 0xBBBB, 0xCCCC]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn source_emits_free_event_for_matching_alloc() {
        let alloc_queue = Arc::new(ArrayQueue::new(16));
        let free_queue = Arc::new(ArrayQueue::new(16));
        alloc_queue.push(make_raw_alloc(0x2000, 512, 200)).ok();
        free_queue.push(make_raw_free(0x2000, 300)).ok();

        let mut source = MemoryProfileSource::new(
            Arc::clone(&alloc_queue),
            Arc::clone(&free_queue),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            true,
        );

        let events = flush_and_collect(&mut source);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
            .collect();
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
            .collect();
        assert_eq!(allocs.len(), 1);
        assert_eq!(frees.len(), 1);
        match &frees[0] {
            TelemetryEvent::Free {
                timestamp_nanos,
                tid,
                addr,
                size,
                alloc_timestamp_nanos,
            } => {
                assert_eq!(*timestamp_nanos, 300);
                assert_eq!(*tid, 2);
                assert_eq!(*addr, 0x2000);
                assert_eq!(*size, 512);
                assert_eq!(*alloc_timestamp_nanos, 200);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn free_without_alloc_is_silently_dropped() {
        let alloc_queue = Arc::new(ArrayQueue::new(16));
        let free_queue = Arc::new(ArrayQueue::new(16));
        free_queue.push(make_raw_free(0x9999, 400)).ok();

        let mut source = MemoryProfileSource::new(
            Arc::clone(&alloc_queue),
            Arc::clone(&free_queue),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            true,
        );

        let events = flush_and_collect(&mut source);
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
            .collect();
        assert_eq!(frees.len(), 0);
    }

    #[test]
    fn liveset_off_drops_all_frees() {
        let alloc_queue = Arc::new(ArrayQueue::new(16));
        let free_queue = Arc::new(ArrayQueue::new(16));
        alloc_queue.push(make_raw_alloc(0x3000, 128, 500)).ok();
        free_queue.push(make_raw_free(0x3000, 600)).ok();

        let mut source = MemoryProfileSource::new(
            Arc::clone(&alloc_queue),
            Arc::clone(&free_queue),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            false,
        );

        let events = flush_and_collect(&mut source);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
            .collect();
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
            .collect();
        assert_eq!(allocs.len(), 1);
        assert_eq!(frees.len(), 0);
    }

    #[test]
    fn alloc_then_free_in_separate_flush_cycles() {
        let alloc_queue = Arc::new(ArrayQueue::new(16));
        let free_queue = Arc::new(ArrayQueue::new(16));

        let mut source = MemoryProfileSource::new(
            Arc::clone(&alloc_queue),
            Arc::clone(&free_queue),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            true,
        );

        // First flush: only the alloc
        alloc_queue.push(make_raw_alloc(0x4000, 256, 700)).ok();
        let events = flush_and_collect(&mut source);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
                .count(),
            0
        );

        // Second flush: the free arrives
        free_queue.push(make_raw_free(0x4000, 800)).ok();
        let events = flush_and_collect(&mut source);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
                .count(),
            0
        );
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
            .collect();
        assert_eq!(frees.len(), 1);
        match &frees[0] {
            TelemetryEvent::Free {
                size,
                alloc_timestamp_nanos,
                ..
            } => {
                assert_eq!(*size, 256);
                assert_eq!(*alloc_timestamp_nanos, 700);
            }
            _ => unreachable!(),
        }
    }
}
