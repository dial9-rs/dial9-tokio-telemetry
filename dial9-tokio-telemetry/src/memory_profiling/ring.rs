//! Two lock-free MPMC queues — one for sampled allocations, one for frees.

use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

/// Maximum frames captured per allocation. 128 × 8 B = 1 KiB stack budget.
pub(crate) const MAX_FRAMES: usize = 128;

/// Default number of `RawAlloc` slots. ~4 MiB total (design §5).
#[allow(dead_code)]
pub(crate) const DEFAULT_ALLOC_QUEUE_CAPACITY: usize = 4096;

/// Default number of `RawFree` slots. 8× the alloc queue (design §9).
#[allow(dead_code)]
pub(crate) const DEFAULT_FREE_QUEUE_CAPACITY: usize = DEFAULT_ALLOC_QUEUE_CAPACITY * 8;

/// One sampled allocation captured on the producer thread.
#[derive(Debug, Clone)]
pub(crate) struct RawAlloc {
    pub(crate) tid: u32,
    pub(crate) size: u64,
    pub(crate) addr: u64,
    pub(crate) ts_ns: u64,
    pub(crate) frames: [u64; MAX_FRAMES],
    pub(crate) frame_count: u8,
}

impl RawAlloc {
    pub(crate) fn frames(&self) -> &[u64] {
        &self.frames[..self.frame_count as usize]
    }
}

/// One free captured on the producer thread when liveset tracking is on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawFree {
    pub(crate) tid: u32,
    pub(crate) addr: u64,
    #[allow(dead_code)]
    pub(crate) size: u64,
    pub(crate) ts_ns: u64,
}

/// Process-global pair of lock-free queues.
#[allow(dead_code)]
pub(crate) struct RingBuffers {
    alloc_queue: Arc<ArrayQueue<RawAlloc>>,
    free_queue: Arc<ArrayQueue<RawFree>>,
    dropped_allocs: Arc<AtomicU64>,
    dropped_frees: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl RingBuffers {
    pub(crate) fn new(alloc_capacity: usize, free_capacity: usize) -> Self {
        Self {
            alloc_queue: Arc::new(ArrayQueue::new(alloc_capacity)),
            free_queue: Arc::new(ArrayQueue::new(free_capacity)),
            dropped_allocs: Arc::new(AtomicU64::new(0)),
            dropped_frees: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn alloc_queue(&self) -> &Arc<ArrayQueue<RawAlloc>> {
        &self.alloc_queue
    }

    pub(crate) fn free_queue(&self) -> &Arc<ArrayQueue<RawFree>> {
        &self.free_queue
    }

    pub(crate) fn dropped_allocs(&self) -> &Arc<AtomicU64> {
        &self.dropped_allocs
    }

    pub(crate) fn dropped_frees(&self) -> &Arc<AtomicU64> {
        &self.dropped_frees
    }
}
