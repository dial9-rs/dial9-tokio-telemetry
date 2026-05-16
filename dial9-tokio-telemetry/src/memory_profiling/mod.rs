//! Memory profiling — sampled allocation tracking via ring buffers.
//!
//! See `dial9-tokio-telemetry/design/memory-profiling.md` for the full design.
//! The architecture (design §5, §6, §9):
//!
//! 1. The allocator hook (commit 6) does the bare minimum on the allocating
//!    thread: sampling decision, stack capture, push a fixed-size POD record
//!    into one of two process-global lock-free queues.
//! 2. The flush thread (consolidator) drains both queues every flush cycle
//!    via the `Source` trait, interns stacks, and emits `AllocEvent`s and
//!    `FreeEvent`s into the central collector.
//!
//! ## Why two queues
//!
//! Allocs and frees have very different rates and record sizes:
//! - `RawAlloc` (~1 KiB) is pushed only on sampled allocations, ~2K/sec at
//!   default sample rate.
//! - `RawFree` (~24 B) is pushed on every dealloc when liveset tracking is
//!   on, potentially 15M/sec.
//!
//! A unified queue would either over-size the alloc queue or under-size
//! the free queue. Splitting the queues lets us size each independently:
//! the free queue is 8× larger than the alloc queue (design §9 "Sizing
//! the free queue").
//!
//! Gated behind the `memory-profiling` cargo feature.

mod ring;
mod source;

#[allow(unused_imports)]
pub(crate) use ring::{
    DEFAULT_ALLOC_QUEUE_CAPACITY, DEFAULT_FREE_QUEUE_CAPACITY, MAX_FRAMES, RawAlloc, RawFree,
    RingBuffers,
};
#[allow(unused_imports)]
pub(crate) use source::MemoryProfileSource;
