# Periodic TL Buffer Drain — Progress

## Goal
Flush thread drains idle/silent thread-local buffers every ~30s, skipping busy workers via FlushEpoch.

## Tasks
- [ ] Task 1: Add FlushEpoch newtype and global drain_epoch to SharedState
- [ ] Task 2: Replace RefCell with Arc<Mutex> in ThreadLocalBuffer and update thread-local storage
- [ ] Task 3: Add TlBufferHandle registration and drain_all_tl_buffers to SharedState
- [ ] Task 4: Wire periodic drain into the flush thread loop
- [ ] Task 5: Document the lock-free Left-Right alternative

## Notes
- FlushEpoch: newtype around Arc<AtomicU64>, stamped by workers on self-flush
- drain_epoch: AtomicU64 on SharedState, bumped by flush thread every ~30s
- Busy workers (epoch current) are skipped — zero contention
- Dead Weak handles cleaned up lazily via retain
