//! Lock-free ring buffer for SIGPROF samples.
//!
//! Implements a sequence-number ring buffer (Vyukov-style, though single-consumer)
//!
//! Signal handlers produce; one drain thread consumes.
//! Write path must stay async-signal-safe.
//!
//! Each slot has a `seq` counter that indicates whether this slot is ready for
//! the current producer ticket, already published for drain, or still from an older lap.
//! Producers claim tickets via CAS on `tail` so buffer-full drops never
//! leave cursor holes that stall the drain.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;

use super::unwind::MAX_FRAMES;

/// The drain thread drains every ~5 ms. At 64 threads × 999 Hz × 5 ms ≈ 320
/// samples per cycle. 4096 gives ~12.8× headroom, it tolerates the drain thread
/// stalling up to ~64 ms before the first drop.
const BUFFER_CAP: usize = 4096;

/// Mutable payload written by the producer and read by drain.
struct SampleData {
    pid: u32,
    tid: u32,
    time: u64,
    cpu: u32,
    period: u64,
    num_frames: u32,
    frames: [u64; MAX_FRAMES],
}

/// A single sample slot.
///
/// `seq` is first so the drain can check readiness without touching the
/// larger `data` field.
#[repr(C)]
struct SampleSlot {
    seq: AtomicUsize,
    data: UnsafeCell<SampleData>,
}

impl SampleSlot {
    const fn new(initial_seq: usize) -> Self {
        Self {
            seq: AtomicUsize::new(initial_seq),
            data: UnsafeCell::new(SampleData {
                pid: 0,
                tid: 0,
                time: 0,
                cpu: 0,
                period: 0,
                num_frames: 0,
                frames: [0u64; MAX_FRAMES],
            }),
        }
    }
}

// SAFETY: access is safe across threads because the `seq` protocol gives
// each producer exclusive ownership of a slot's data until commit, and the
// drain reads only after observing that no producer is writing to it.
unsafe impl Sync for SampleSlot {}

/// Build the slot array with `seq[i] = i`.
const fn make_slots() -> [SampleSlot; BUFFER_CAP] {
    let mut slots: [SampleSlot; BUFFER_CAP] = [const { SampleSlot::new(0) }; BUFFER_CAP];
    let mut i = 0;
    while i < BUFFER_CAP {
        slots[i] = SampleSlot::new(i);
        i += 1;
    }
    slots
}

/// The global sample buffer. Static so signal handlers can access it
/// without indirection.
static BUFFER: SampleBuffer = SampleBuffer {
    slots: make_slots(),
    tail: CachePadded::new(AtomicUsize::new(0)),
    head: CachePadded::new(AtomicUsize::new(0)),
    dropped: CachePadded::new(AtomicUsize::new(0)),
};

struct SampleBuffer {
    slots: [SampleSlot; BUFFER_CAP],
    /// Producer cursor. Monotonically increasing; producers CAS this to
    /// claim a unique ticket. CAS only advances on success.
    tail: CachePadded<AtomicUsize>,
    /// Consumer cursor. Advanced only by the single drain thread.
    head: CachePadded<AtomicUsize>,
    /// Count of dropped samples (buffer full or claim retries exhausted).
    dropped: CachePadded<AtomicUsize>,
}

/// Data extracted from a sample slot for the drain thread.
pub(crate) struct DrainedSample {
    pub pid: u32,
    pub tid: u32,
    pub time: u64,
    pub cpu: u32,
    /// Effective sampling period in nanoseconds, accounting for timer overruns.
    /// `interval_ns * (1 + overrun_count)`, used as sample weight in flamegraphs.
    pub period: u64,
    pub num_frames: u32,
    pub frames: [u64; MAX_FRAMES],
}

/// Claim a slot for writing from a signal handler.
///
/// # Safety
/// - Must be called from a signal handler context (async-signal-safe).
/// - The returned writer is valid only until [`SlotWriter::commit`] or drop.
/// - Exclusive access to the claimed slot is guaranteed by the `tail`
///   CAS: no two callers can win the same ticket.
pub(crate) unsafe fn claim_slot() -> Option<SlotWriter> {
    // Hard cap so the signal handler can't spin unboundedly under
    // heavy contention. 128 is well past typical fan-in.
    const MAX_ATTEMPTS: usize = 128;

    let mut ticket = BUFFER.tail.load(Ordering::Relaxed);
    for _ in 0..MAX_ATTEMPTS {
        let slot = &BUFFER.slots[ticket % BUFFER_CAP];

        // Acquire syncs with:
        //   - drain's `seq.store(head + CAP, Release)` (slot recycled), or
        //   - another producer's commit `seq.store(prev + 1, Release)`.
        let seq = slot.seq.load(Ordering::Acquire);

        if seq == ticket {
            // Slot ready for `ticket`. Try to reserve it.
            match BUFFER.tail.compare_exchange(
                ticket,
                ticket.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(SlotWriter {
                        slot: slot as *const SampleSlot as *mut SampleSlot,
                        ticket,
                        committed: false,
                    });
                }
                Err(current) => {
                    // Lost the race to another producer, retry with its value.
                    ticket = current;
                }
            }
        } else {
            // Slot doesn't match `ticket`.
            let diff = seq.wrapping_sub(ticket) as isize;
            if diff < 0 {
                // Prior-lap sample still sitting in the slot, buffer is full.
                BUFFER.dropped.fetch_add(1, Ordering::Relaxed);
                return None;
            } else {
                // Another producer committed past our stale cursor, reload.
                ticket = BUFFER.tail.load(Ordering::Relaxed);
            }
        }
    }

    // Exceeded retry limit, drop the sample.
    BUFFER.dropped.fetch_add(1, Ordering::Relaxed);
    None
}

/// Handle for writing sample data into a claimed slot.
///
/// If dropped without calling [`commit`](Self::commit), the slot is published
/// with `num_frames = 0` so the drain can advance past it. Every claimed
/// ticket must land a matching `seq` publish or the drain's FIFO advance
/// stalls forever.
pub(crate) struct SlotWriter {
    slot: *mut SampleSlot,
    ticket: usize,
    committed: bool,
}

impl SlotWriter {
    #[inline]
    pub(crate) unsafe fn write(&mut self, pid: u32, tid: u32, time: u64, cpu: u32, period: u64) {
        // SAFETY: slot lives in the 'static BUFFER, and the tail CAS in
        // claim_slot hands us exclusive access until commit.
        unsafe {
            let data = (*self.slot).data.get();
            (*data).pid = pid;
            (*data).tid = tid;
            (*data).time = time;
            (*data).cpu = cpu;
            (*data).period = period;
        }
    }

    /// Caller should pass a `count` not greater than `frames.len()`.
    #[inline]
    pub(crate) unsafe fn write_frames(&mut self, frames: &[u64], count: u32) {
        debug_assert!(
            (count as usize) <= frames.len(),
            "write_frames count exceeds source slice length"
        );
        let copy_len = (count as usize).min(MAX_FRAMES).min(frames.len());
        // SAFETY: exclusive slot access as in `write`, copy_len <= MAX_FRAMES so the
        // write stays within slot.frames, and copy_len <= frames.len() so the read
        // stays within the source slice.
        unsafe {
            let data = (*self.slot).data.get();
            (*data).num_frames = copy_len as u32;
            let dst = std::ptr::addr_of_mut!((*data).frames) as *mut u64;
            core::ptr::copy_nonoverlapping(frames.as_ptr(), dst, copy_len);
        }
    }

    #[inline]
    pub(crate) unsafe fn commit(mut self) {
        self.committed = true;
        // SAFETY: same exclusivity as `write`. The Release store publishes
        // our writes to any drain that later Acquires this seq.
        unsafe {
            (*self.slot)
                .seq
                .store(self.ticket.wrapping_add(1), Ordering::Release)
        };
    }
}

impl Drop for SlotWriter {
    fn drop(&mut self) {
        if !self.committed {
            // Abandoned slot: publish as an empty sample so the drain's FIFO
            // advance isn't blocked waiting on a `seq` that never lands.
            // SAFETY: same exclusivity as `write`, we still hold the ticket
            // and no other writer or drain can touch this slot.
            unsafe {
                let data = (*self.slot).data.get();
                (*data).pid = 0;
                (*data).tid = 0;
                (*data).time = 0;
                (*data).cpu = 0;
                (*data).period = 0;
                (*data).num_frames = 0;
                (*self.slot)
                    .seq
                    .store(self.ticket.wrapping_add(1), Ordering::Release);
            }
        }
    }
}

/// Drain all ready samples, calling `f` for each one.
///
/// Single-consumer: must be called from one thread only (the drain thread).
/// `f` must not panic; a panic before the cursor is committed will cause
/// the same samples to be re-delivered on the next call.
pub(crate) fn drain(mut f: impl FnMut(DrainedSample)) {
    let mut head = BUFFER.head.load(Ordering::Relaxed);

    loop {
        let slot = &BUFFER.slots[head % BUFFER_CAP];

        // Acquire syncs with the producer's `seq.store(ticket + 1, Release)` in
        // `commit` (or the abandoned-slot store in `Drop`).
        if slot.seq.load(Ordering::Acquire) != head.wrapping_add(1) {
            // Either the producer is still writing, or the slot's ticket is
            // in the future (impossible under single-consumer), stop here.
            break;
        }

        // SAFETY: seq Acquire above syncs with the producer's Release commit,
        // so all writes to `data` are visible. Single consumer means no
        // concurrent drain reader, no writer can access a READY slot.
        let sample = unsafe {
            let data = &*slot.data.get();
            DrainedSample {
                pid: data.pid,
                tid: data.tid,
                time: data.time,
                cpu: data.cpu,
                period: data.period,
                num_frames: data.num_frames,
                frames: data.frames,
            }
        };

        // Release the slot for the next-lap producer (head + CAP).
        // Release syncs with that producer's Acquire load of seq.
        slot.seq
            .store(head.wrapping_add(BUFFER_CAP), Ordering::Release);

        head = head.wrapping_add(1);
        f(sample);
    }

    BUFFER.head.store(head, Ordering::Relaxed);
}

/// Returns true if there are pending samples to read.
pub(crate) fn has_pending() -> bool {
    let write = BUFFER.tail.load(Ordering::Acquire);
    let read = BUFFER.head.load(Ordering::Relaxed);
    (write.wrapping_sub(read) as isize) > 0
}

/// Returns and resets the count of dropped samples since last call.
pub(crate) fn take_dropped_count() -> usize {
    BUFFER.dropped.swap(0, Ordering::Relaxed)
}

#[cfg(test)]
fn reset_buffer() {
    // Restore the per-slot `seq = i` invariant and zero the cursors. Only
    // safe in single-threaded tests.
    for (i, slot) in BUFFER.slots.iter().enumerate() {
        slot.seq.store(i, Ordering::Relaxed);
    }
    BUFFER.tail.store(0, Ordering::Relaxed);
    BUFFER.head.store(0, Ordering::Relaxed);
    BUFFER.dropped.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::thread;
    use std::time::Duration;

    // Shared static buffer state means these tests cannot run concurrently.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().expect("test lock poisoned")
    }

    #[test]
    fn round_trip_single_sample() {
        let _guard = test_guard();
        reset_buffer();

        unsafe {
            let mut slot = claim_slot().expect("should claim slot");
            slot.write(1000, 42, 999_000_000, 3, 10_000_000);
            let frames = [0x1234u64, 0x5678, 0x9abc];
            slot.write_frames(&frames, 3);
            slot.commit();
        }

        assert!(has_pending());

        let mut got = Vec::new();
        drain(|s| got.push(s));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pid, 1000);
        assert_eq!(got[0].tid, 42);
        assert_eq!(got[0].time, 999_000_000);
        assert_eq!(got[0].cpu, 3);
        assert_eq!(got[0].period, 10_000_000);
        assert_eq!(got[0].num_frames, 3);
        assert_eq!(got[0].frames[0], 0x1234);
        assert_eq!(got[0].frames[1], 0x5678);
        assert_eq!(got[0].frames[2], 0x9abc);

        assert!(!has_pending());
    }

    #[test]
    fn dropped_slot_writer_commits_empty_so_drain_advances() {
        let _guard = test_guard();
        reset_buffer();

        unsafe {
            let mut slot = claim_slot().unwrap();
            slot.write(1, 1, 100, 0, 0);
            slot.write_frames(&[0xAA], 1);
            slot.commit();
        }

        unsafe {
            let mut slot = claim_slot().unwrap();
            slot.write(2, 2, 200, 0, 0);
            slot.write_frames(&[0xBB], 1);
            drop(slot);
        }

        unsafe {
            let mut slot = claim_slot().unwrap();
            slot.write(3, 3, 300, 0, 0);
            slot.write_frames(&[0xCC], 1);
            slot.commit();
        }

        // Drain sees all three: real, abandoned (0 frames), real
        let mut got = Vec::new();
        drain(|s| got.push(s));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].pid, 1);
        assert_eq!(got[1].pid, 0); // abandoned slot
        assert_eq!(got[1].tid, 0);
        assert_eq!(got[1].time, 0);
        assert_eq!(got[1].num_frames, 0);
        assert_eq!(got[2].pid, 3);
    }

    #[test]
    fn take_dropped_count_resets() {
        let _guard = test_guard();
        reset_buffer();

        let count = take_dropped_count();
        assert_eq!(count, 0);

        // Artificially bump the dropped counter
        BUFFER.dropped.store(5, Ordering::Relaxed);
        assert_eq!(take_dropped_count(), 5);
        assert_eq!(take_dropped_count(), 0);
    }

    #[test]
    fn empty_drain_is_noop() {
        let _guard = test_guard();
        reset_buffer();

        assert!(!has_pending());
        let mut count = 0;
        drain(|_| count += 1);
        assert_eq!(count, 0);
    }

    #[test]
    fn drain_stops_at_first_non_ready_slot() {
        let _guard = test_guard();
        reset_buffer();

        let blocked_writer: SlotWriter;

        unsafe {
            // Slot 0: committed.
            let mut s0 = claim_slot().unwrap();
            s0.write(10, 10, 10, 0, 1);
            s0.write_frames(&[0x10], 1);
            s0.commit();

            // Slot 1: left uncommitted to block ordered drain.
            let mut s1 = claim_slot().unwrap();
            s1.write(20, 20, 20, 0, 1);
            s1.write_frames(&[0x20], 1);
            blocked_writer = s1;

            // Slot 2: committed, but should not be drained yet because slot 1
            // is not ready.
            let mut s2 = claim_slot().unwrap();
            s2.write(30, 30, 30, 0, 1);
            s2.write_frames(&[0x30], 1);
            s2.commit();
        }

        let mut first_pass = Vec::new();
        drain(|s| first_pass.push(s.pid));
        assert_eq!(first_pass, vec![10]);

        // Unblock slot 1, then drain should continue in FIFO order.
        unsafe { blocked_writer.commit() };
        let mut second_pass = Vec::new();
        drain(|s| second_pass.push(s.pid));
        assert_eq!(second_pass, vec![20, 30]);
    }

    #[test]
    fn failed_claims_do_not_advance_tail() {
        let _guard = test_guard();
        reset_buffer();

        // Fill the entire buffer.
        for i in 0..BUFFER_CAP {
            unsafe {
                let mut s = claim_slot().expect("should claim during fill");
                s.write(i as u32, 0, 0, 0, 1);
                s.write_frames(&[], 0);
                s.commit();
            }
        }
        assert_eq!(BUFFER.tail.load(Ordering::Relaxed), BUFFER_CAP);

        assert_eq!(
            BUFFER.tail.load(Ordering::Relaxed),
            BUFFER_CAP,
            "tail must not advance on failed claims",
        );

        // Drain the first batch.
        let mut first_drained = 0;
        drain(|_| first_drained += 1);
        assert_eq!(first_drained, BUFFER_CAP);

        // Write 50 new samples.
        for i in 0..50u32 {
            unsafe {
                let mut s = claim_slot().expect("should claim after drain");
                s.write(10000 + i, 0, 0, 0, 1);
                s.write_frames(&[], 0);
                s.commit();
            }
        }

        // Drain must get all 50.
        let mut got = Vec::new();
        drain(|s| got.push(s.pid));
        assert_eq!(got.len(), 50, "all post-drain samples must be reachable");
        assert_eq!(got[0], 10000);
        assert_eq!(*got.last().unwrap(), 10049);
    }

    /// Spawns N producer threads each calling `claim_slot` + `commit` in a
    /// tight loop while a drainer thread concurrently pulls samples. After
    /// all producers finish and the drainer catches up, every successfully
    /// claimed sample must have been drained exactly once.
    #[test]
    fn concurrent_producers_conserve_samples() {
        let _guard = test_guard();
        reset_buffer();

        const N_PRODUCERS: usize = 8;
        const CLAIMS_PER_PRODUCER: usize = 10_000;
        const DRAIN_SLEEP: Duration = Duration::from_micros(100);

        let stop = Arc::new(AtomicBool::new(false));
        let claimed = Arc::new(AtomicUsize::new(0));
        let drained = Arc::new(AtomicUsize::new(0));

        let producers: Vec<_> = (0..N_PRODUCERS)
            .map(|pid| {
                let claimed = claimed.clone();
                thread::spawn(move || {
                    for i in 0..CLAIMS_PER_PRODUCER {
                        unsafe {
                            if let Some(mut s) = claim_slot() {
                                s.write(pid as u32, i as u32, 0, 0, 1);
                                s.write_frames(&[], 0);
                                s.commit();
                                claimed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                })
            })
            .collect();

        let drainer = {
            let stop = stop.clone();
            let drained = drained.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    drain(|_| {
                        drained.fetch_add(1, Ordering::Relaxed);
                    });
                    thread::sleep(DRAIN_SLEEP);
                }
                // Final catch-up drain after producers signal stop.
                drain(|_| {
                    drained.fetch_add(1, Ordering::Relaxed);
                });
            })
        };

        for p in producers {
            p.join().expect("producer thread panicked");
        }
        stop.store(true, Ordering::Relaxed);
        drainer.join().expect("drainer thread panicked");

        let claimed = claimed.load(Ordering::Relaxed);
        let drained = drained.load(Ordering::Relaxed);
        let dropped = take_dropped_count();
        let attempts = N_PRODUCERS * CLAIMS_PER_PRODUCER;

        // No sample should be lost.
        assert_eq!(
            claimed + dropped,
            attempts,
            "every attempt accounted for: claimed={claimed} + dropped={dropped} != attempts={attempts}",
        );
        // No commited sample should be stuck in the ring.
        assert_eq!(
            drained, claimed,
            "every committed sample reaches drain: drained={drained} != claimed={claimed} (dropped={dropped})",
        );
    }

    #[test]
    fn preserves_order_across_ring_wraparound() {
        let _guard = test_guard();
        reset_buffer();

        BUFFER.tail.store(BUFFER_CAP - 1, Ordering::Relaxed);
        BUFFER.head.store(BUFFER_CAP - 1, Ordering::Relaxed);
        BUFFER.slots[0].seq.store(BUFFER_CAP, Ordering::Relaxed);

        unsafe {
            let mut a = claim_slot().unwrap();
            a.write(111, 1, 1, 0, 1);
            a.write_frames(&[0xAA], 1);
            a.commit();

            let mut b = claim_slot().unwrap();
            b.write(222, 2, 2, 0, 1);
            b.write_frames(&[0xBB], 1);
            b.commit();
        }

        let mut got = Vec::new();
        drain(|s| got.push(s.pid));
        assert_eq!(got, vec![111, 222]);
    }
}
