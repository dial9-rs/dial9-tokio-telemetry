//! Binary trace wire format (v6).
//!
//! ## File layout
//! ```text
//! Header:  MAGIC (8 bytes) + VERSION (u32 LE) = 12 bytes
//!
//! Wire codes:
//!   0: PollStart   (local_queue=0)  → code(u8) + timestamp_us(u32) + worker_id(u8)                                                    = 6 bytes
//!   1: PollEnd     (local_queue=0)  → code(u8) + timestamp_us(u32) + worker_id(u8)                                                    = 6 bytes
//!   2: WorkerPark                   → code(u8) + timestamp_us(u32) + worker_id(u8) + local_queue(u8) + cpu_us(u32)                     = 11 bytes
//!   3: WorkerUnpark                 → code(u8) + timestamp_us(u32) + worker_id(u8) + local_queue(u8) + cpu_us(u32) + sched_wait_us(u32) = 15 bytes
//!   4: QueueSample                  → code(u8) + timestamp_us(u32) + global_queue(u8)                                                  = 6 bytes
//!   5: PollStart   (local_queue>0)  → code(u8) + timestamp_us(u32) + worker_id(u8) + local_queue(u8)                                  = 7 bytes
//!   6: PollEnd     (local_queue>0)  → code(u8) + timestamp_us(u32) + worker_id(u8) + local_queue(u8)                                  = 7 bytes
//! ```
//!
//! Timestamps are microseconds since trace start. u32 micros supports traces up to ~71 minutes.
//! `cpu_us` is thread CPU time (from `CLOCK_THREAD_CPUTIME_ID`) in microseconds since thread start.
//! `sched_wait_us` is the scheduling wait delta from schedstat during the park period.
//! In-memory representation remains nanoseconds; conversion happens at the wire boundary.
//!
//! ### v5 → v6 changes
//! - WorkerUnpark now carries `sched_wait_us(u32)` — scheduling wait delta from
//!   `/proc/self/task/<tid>/schedstat` measured across the park period. This reveals
//!   OS-level scheduling delay between wakeup signal and thread actually running.

use crate::telemetry::events::{EventType, MetricsSnapshot, TelemetryEvent};
use std::io::{Read, Result, Write};

pub const MAGIC: &[u8; 8] = b"TOKIOTRC";
pub const VERSION: u32 = 6;
pub const HEADER_SIZE: usize = 12; // 8 magic + 4 version

// Wire codes — these are NOT the same as EventType repr values for codes 5/6.
const WIRE_POLL_START: u8 = 0;
const WIRE_POLL_END: u8 = 1;
const WIRE_WORKER_PARK: u8 = 2;
const WIRE_WORKER_UNPARK: u8 = 3;
const WIRE_QUEUE_SAMPLE: u8 = 4;
const WIRE_POLL_START_WITH_QUEUE: u8 = 5;
const WIRE_POLL_END_WITH_QUEUE: u8 = 6;

/// Returns the wire size of an event. For PollStart/PollEnd this depends on local_queue.
pub fn wire_event_size(event: &TelemetryEvent) -> usize {
    match event.event_type {
        EventType::PollStart | EventType::PollEnd => {
            if event.metrics.worker_local_queue_depth == 0 {
                6
            } else {
                7
            }
        }
        EventType::QueueSample => 6,
        EventType::WorkerPark => 11,
        EventType::WorkerUnpark => 15,
    }
}

pub fn write_header(w: &mut impl Write) -> Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())
}

pub fn write_event(w: &mut impl Write, event: &TelemetryEvent) -> Result<()> {
    let timestamp_us = (event.metrics.timestamp_nanos / 1000) as u32;
    let lq = event.metrics.worker_local_queue_depth;

    match event.event_type {
        EventType::PollStart if lq == 0 => {
            w.write_all(&[WIRE_POLL_START])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[event.metrics.worker_id as u8])?;
        }
        EventType::PollEnd if lq == 0 => {
            w.write_all(&[WIRE_POLL_END])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[event.metrics.worker_id as u8])?;
        }
        EventType::PollStart => {
            w.write_all(&[WIRE_POLL_START_WITH_QUEUE])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[event.metrics.worker_id as u8])?;
            w.write_all(&[lq as u8])?;
        }
        EventType::PollEnd => {
            w.write_all(&[WIRE_POLL_END_WITH_QUEUE])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[event.metrics.worker_id as u8])?;
            w.write_all(&[lq as u8])?;
        }
        EventType::WorkerPark => {
            w.write_all(&[WIRE_WORKER_PARK])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[event.metrics.worker_id as u8])?;
            w.write_all(&[lq as u8])?;
            let cpu_us = (event.metrics.cpu_time_nanos / 1000) as u32;
            w.write_all(&cpu_us.to_le_bytes())?;
        }
        EventType::WorkerUnpark => {
            w.write_all(&[WIRE_WORKER_UNPARK])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[event.metrics.worker_id as u8])?;
            w.write_all(&[lq as u8])?;
            let cpu_us = (event.metrics.cpu_time_nanos / 1000) as u32;
            w.write_all(&cpu_us.to_le_bytes())?;
            let sched_wait_us = (event.metrics.sched_wait_delta_nanos / 1000) as u32;
            w.write_all(&sched_wait_us.to_le_bytes())?;
        }
        EventType::QueueSample => {
            w.write_all(&[WIRE_QUEUE_SAMPLE])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[event.metrics.global_queue_depth as u8])?;
        }
    }
    Ok(())
}

pub fn read_header(r: &mut impl Read) -> Result<(String, u32)> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    let mut version = [0u8; 4];
    r.read_exact(&mut version)?;
    Ok((
        String::from_utf8_lossy(&magic).to_string(),
        u32::from_le_bytes(version),
    ))
}

/// Read one event. Returns `Ok(None)` at EOF.
pub fn read_event(r: &mut impl Read) -> Result<Option<TelemetryEvent>> {
    let mut tag = [0u8; 1];
    if r.read_exact(&mut tag).is_err() {
        return Ok(None);
    }

    let mut ts = [0u8; 4];
    r.read_exact(&mut ts)?;
    let timestamp_nanos = u32::from_le_bytes(ts) as u64 * 1000;

    match tag[0] {
        WIRE_POLL_START | WIRE_POLL_END => {
            let mut wid = [0u8; 1];
            r.read_exact(&mut wid)?;
            let event_type = if tag[0] == WIRE_POLL_START {
                EventType::PollStart
            } else {
                EventType::PollEnd
            };
            Ok(Some(TelemetryEvent::new(
                event_type,
                MetricsSnapshot {
                    timestamp_nanos,
                    worker_id: wid[0] as usize,
                    global_queue_depth: 0,
                    worker_local_queue_depth: 0,
                    cpu_time_nanos: 0,
                    sched_wait_delta_nanos: 0,
                },
            )))
        }
        WIRE_POLL_START_WITH_QUEUE | WIRE_POLL_END_WITH_QUEUE => {
            let mut wid = [0u8; 1];
            let mut lq = [0u8; 1];
            r.read_exact(&mut wid)?;
            r.read_exact(&mut lq)?;
            let event_type = if tag[0] == WIRE_POLL_START_WITH_QUEUE {
                EventType::PollStart
            } else {
                EventType::PollEnd
            };
            Ok(Some(TelemetryEvent::new(
                event_type,
                MetricsSnapshot {
                    timestamp_nanos,
                    worker_id: wid[0] as usize,
                    global_queue_depth: 0,
                    worker_local_queue_depth: lq[0] as usize,
                    cpu_time_nanos: 0,
                    sched_wait_delta_nanos: 0,
                },
            )))
        }
        WIRE_WORKER_PARK => {
            let mut wid = [0u8; 1];
            let mut lq = [0u8; 1];
            r.read_exact(&mut wid)?;
            r.read_exact(&mut lq)?;
            let mut cpu = [0u8; 4];
            r.read_exact(&mut cpu)?;
            let cpu_time_nanos = u32::from_le_bytes(cpu) as u64 * 1000;
            Ok(Some(TelemetryEvent::new(
                EventType::WorkerPark,
                MetricsSnapshot {
                    timestamp_nanos,
                    worker_id: wid[0] as usize,
                    global_queue_depth: 0,
                    worker_local_queue_depth: lq[0] as usize,
                    cpu_time_nanos,
                    sched_wait_delta_nanos: 0,
                },
            )))
        }
        WIRE_WORKER_UNPARK => {
            let mut wid = [0u8; 1];
            let mut lq = [0u8; 1];
            r.read_exact(&mut wid)?;
            r.read_exact(&mut lq)?;
            let mut cpu = [0u8; 4];
            r.read_exact(&mut cpu)?;
            let cpu_time_nanos = u32::from_le_bytes(cpu) as u64 * 1000;
            let mut sw = [0u8; 4];
            r.read_exact(&mut sw)?;
            let sched_wait_delta_nanos = u32::from_le_bytes(sw) as u64 * 1000;
            Ok(Some(TelemetryEvent::new(
                EventType::WorkerUnpark,
                MetricsSnapshot {
                    timestamp_nanos,
                    worker_id: wid[0] as usize,
                    global_queue_depth: 0,
                    worker_local_queue_depth: lq[0] as usize,
                    cpu_time_nanos,
                    sched_wait_delta_nanos,
                },
            )))
        }
        WIRE_QUEUE_SAMPLE => {
            let mut gq = [0u8; 1];
            r.read_exact(&mut gq)?;
            Ok(Some(TelemetryEvent::new(
                EventType::QueueSample,
                MetricsSnapshot {
                    timestamp_nanos,
                    worker_id: 0,
                    global_queue_depth: gq[0] as usize,
                    worker_local_queue_depth: 0,
                    cpu_time_nanos: 0,
                    sched_wait_delta_nanos: 0,
                },
            )))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(event: &TelemetryEvent) -> TelemetryEvent {
        let mut buf = Vec::new();
        write_event(&mut buf, event).unwrap();
        assert_eq!(buf.len(), wire_event_size(event));
        read_event(&mut Cursor::new(buf)).unwrap().unwrap()
    }

    #[test]
    fn test_poll_start_zero_queue_roundtrip() {
        let event = TelemetryEvent::new(
            EventType::PollStart,
            MetricsSnapshot {
                timestamp_nanos: 123_456_000,
                worker_id: 3,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&event), 6);
        let decoded = roundtrip(&event);
        assert_eq!(decoded.event_type, EventType::PollStart);
        assert_eq!(decoded.metrics.timestamp_nanos, 123_456_000);
        assert_eq!(decoded.metrics.worker_id, 3);
        assert_eq!(decoded.metrics.worker_local_queue_depth, 0);
    }

    #[test]
    fn test_poll_start_with_queue_roundtrip() {
        let event = TelemetryEvent::new(
            EventType::PollStart,
            MetricsSnapshot {
                timestamp_nanos: 123_456_000,
                worker_id: 3,
                global_queue_depth: 0,
                worker_local_queue_depth: 17,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&event), 7);
        let decoded = roundtrip(&event);
        assert_eq!(decoded.event_type, EventType::PollStart);
        assert_eq!(decoded.metrics.timestamp_nanos, 123_456_000);
        assert_eq!(decoded.metrics.worker_id, 3);
        assert_eq!(decoded.metrics.worker_local_queue_depth, 17);
    }

    #[test]
    fn test_poll_end_zero_queue_roundtrip() {
        let event = TelemetryEvent::new(
            EventType::PollEnd,
            MetricsSnapshot {
                timestamp_nanos: 999_000,
                worker_id: 1,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&event), 6);
        let decoded = roundtrip(&event);
        assert_eq!(decoded.event_type, EventType::PollEnd);
        assert_eq!(decoded.metrics.worker_id, 1);
        assert_eq!(decoded.metrics.worker_local_queue_depth, 0);
    }

    #[test]
    fn test_poll_end_with_queue_roundtrip() {
        let event = TelemetryEvent::new(
            EventType::PollEnd,
            MetricsSnapshot {
                timestamp_nanos: 999_000,
                worker_id: 1,
                global_queue_depth: 0,
                worker_local_queue_depth: 42,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&event), 7);
        let decoded = roundtrip(&event);
        assert_eq!(decoded.event_type, EventType::PollEnd);
        assert_eq!(decoded.metrics.worker_id, 1);
        assert_eq!(decoded.metrics.worker_local_queue_depth, 42);
    }

    #[test]
    fn test_sub_microsecond_truncation() {
        let event = TelemetryEvent::new(
            EventType::PollStart,
            MetricsSnapshot {
                timestamp_nanos: 123_456_789,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        let decoded = roundtrip(&event);
        // Sub-microsecond precision is lost: 123_456_789 -> 123_456_000
        assert_eq!(decoded.metrics.timestamp_nanos, 123_456_000);
    }

    #[test]
    fn test_park_roundtrip() {
        let event = TelemetryEvent::new(
            EventType::WorkerPark,
            MetricsSnapshot {
                timestamp_nanos: 5_000_000_000,
                worker_id: 7,
                global_queue_depth: 0,
                worker_local_queue_depth: 200,
                cpu_time_nanos: 1_234_567_000,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&event), 11);
        let decoded = roundtrip(&event);
        assert_eq!(decoded.event_type, EventType::WorkerPark);
        assert_eq!(decoded.metrics.timestamp_nanos, 5_000_000_000);
        assert_eq!(decoded.metrics.worker_id, 7);
        assert_eq!(decoded.metrics.worker_local_queue_depth, 200);
        assert_eq!(decoded.metrics.cpu_time_nanos, 1_234_567_000);
    }

    #[test]
    fn test_park_zero_queue_roundtrip() {
        // Park with local_queue=0 is still 11 bytes (no special wire code for park)
        let event = TelemetryEvent::new(
            EventType::WorkerPark,
            MetricsSnapshot {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&event), 11);
        let decoded = roundtrip(&event);
        assert_eq!(decoded.event_type, EventType::WorkerPark);
        assert_eq!(decoded.metrics.worker_local_queue_depth, 0);
    }

    #[test]
    fn test_unpark_roundtrip() {
        let event = TelemetryEvent::new(
            EventType::WorkerUnpark,
            MetricsSnapshot {
                timestamp_nanos: 1_000_000,
                worker_id: 2,
                global_queue_depth: 0,
                worker_local_queue_depth: 55,
                cpu_time_nanos: 999_000,
                sched_wait_delta_nanos: 42_000,
            },
        );
        assert_eq!(wire_event_size(&event), 15);
        let decoded = roundtrip(&event);
        assert_eq!(decoded.event_type, EventType::WorkerUnpark);
        assert_eq!(decoded.metrics.worker_id, 2);
        assert_eq!(decoded.metrics.worker_local_queue_depth, 55);
        assert_eq!(decoded.metrics.cpu_time_nanos, 999_000);
        assert_eq!(decoded.metrics.sched_wait_delta_nanos, 42_000);
    }

    #[test]
    fn test_queue_sample_roundtrip() {
        let event = TelemetryEvent::new(
            EventType::QueueSample,
            MetricsSnapshot {
                timestamp_nanos: 10_000_000_000,
                worker_id: 0,
                global_queue_depth: 128,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&event), 6);
        let decoded = roundtrip(&event);
        assert_eq!(decoded.event_type, EventType::QueueSample);
        assert_eq!(decoded.metrics.timestamp_nanos, 10_000_000_000);
        assert_eq!(decoded.metrics.global_queue_depth, 128);
        assert_eq!(decoded.metrics.worker_id, 0);
        assert_eq!(decoded.metrics.worker_local_queue_depth, 0);
    }

    #[test]
    fn test_header_roundtrip() {
        let mut buf = Vec::new();
        write_header(&mut buf).unwrap();
        assert_eq!(buf.len(), HEADER_SIZE);
        let (magic, version) = read_header(&mut Cursor::new(buf)).unwrap();
        assert_eq!(magic, "TOKIOTRC");
        assert_eq!(version, VERSION);
    }

    #[test]
    fn test_wire_event_sizes() {
        // PollStart/PollEnd with zero queue = 6 bytes
        let poll_zero = TelemetryEvent::new(
            EventType::PollStart,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&poll_zero), 6);

        // PollStart/PollEnd with non-zero queue = 7 bytes
        let poll_nonzero = TelemetryEvent::new(
            EventType::PollStart,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 1,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&poll_nonzero), 7);

        // Park always 11 bytes
        let park = TelemetryEvent::new(
            EventType::WorkerPark,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&park), 11);

        // Unpark always 15 bytes (includes sched_wait_us)
        let unpark = TelemetryEvent::new(
            EventType::WorkerUnpark,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&unpark), 15);

        // QueueSample always 6 bytes
        let qs = TelemetryEvent::new(
            EventType::QueueSample,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        assert_eq!(wire_event_size(&qs), 6);
    }

    #[test]
    fn test_max_timestamp() {
        // u32 micros max = 4_294_967_295 µs ≈ 71.6 minutes
        let event = TelemetryEvent::new(
            EventType::PollEnd,
            MetricsSnapshot {
                timestamp_nanos: 4_294_967_295_000,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        let decoded = roundtrip(&event);
        assert_eq!(decoded.metrics.timestamp_nanos, 4_294_967_295_000);
    }

    #[test]
    fn test_mixed_event_stream() {
        let events = vec![
            // PollStart with local_queue=0: 6 bytes
            TelemetryEvent::new(
                EventType::PollStart,
                MetricsSnapshot {
                    timestamp_nanos: 1_000_000,
                    worker_id: 0,
                    global_queue_depth: 0,
                    worker_local_queue_depth: 0,
                    cpu_time_nanos: 0,
                    sched_wait_delta_nanos: 0,
                },
            ),
            // QueueSample: 6 bytes
            TelemetryEvent::new(
                EventType::QueueSample,
                MetricsSnapshot {
                    timestamp_nanos: 2_000_000,
                    worker_id: 0,
                    global_queue_depth: 42,
                    worker_local_queue_depth: 0,
                    cpu_time_nanos: 0,
                    sched_wait_delta_nanos: 0,
                },
            ),
            // PollEnd with local_queue>0: 7 bytes
            TelemetryEvent::new(
                EventType::PollEnd,
                MetricsSnapshot {
                    timestamp_nanos: 3_000_000,
                    worker_id: 0,
                    global_queue_depth: 0,
                    worker_local_queue_depth: 4,
                    cpu_time_nanos: 0,
                    sched_wait_delta_nanos: 0,
                },
            ),
            // WorkerPark: 11 bytes
            TelemetryEvent::new(
                EventType::WorkerPark,
                MetricsSnapshot {
                    timestamp_nanos: 4_000_000,
                    worker_id: 1,
                    global_queue_depth: 0,
                    worker_local_queue_depth: 0,
                    cpu_time_nanos: 500_000_000,
                    sched_wait_delta_nanos: 0,
                },
            ),
            // PollStart with local_queue>0: 7 bytes
            TelemetryEvent::new(
                EventType::PollStart,
                MetricsSnapshot {
                    timestamp_nanos: 5_000_000,
                    worker_id: 2,
                    global_queue_depth: 0,
                    worker_local_queue_depth: 10,
                    cpu_time_nanos: 0,
                    sched_wait_delta_nanos: 0,
                },
            ),
            // PollEnd with local_queue=0: 6 bytes
            TelemetryEvent::new(
                EventType::PollEnd,
                MetricsSnapshot {
                    timestamp_nanos: 6_000_000,
                    worker_id: 2,
                    global_queue_depth: 0,
                    worker_local_queue_depth: 0,
                    cpu_time_nanos: 0,
                    sched_wait_delta_nanos: 0,
                },
            ),
        ];

        let mut buf = Vec::new();
        for e in &events {
            write_event(&mut buf, e).unwrap();
        }

        // 6 + 6 + 7 + 11 + 7 + 6 = 43
        assert_eq!(buf.len(), 43);

        let mut cursor = Cursor::new(buf);
        let d0 = read_event(&mut cursor).unwrap().unwrap();
        assert_eq!(d0.event_type, EventType::PollStart);
        assert_eq!(d0.metrics.worker_local_queue_depth, 0);

        let d1 = read_event(&mut cursor).unwrap().unwrap();
        assert_eq!(d1.event_type, EventType::QueueSample);
        assert_eq!(d1.metrics.global_queue_depth, 42);

        let d2 = read_event(&mut cursor).unwrap().unwrap();
        assert_eq!(d2.event_type, EventType::PollEnd);
        assert_eq!(d2.metrics.worker_local_queue_depth, 4);

        let d3 = read_event(&mut cursor).unwrap().unwrap();
        assert_eq!(d3.event_type, EventType::WorkerPark);
        assert_eq!(d3.metrics.worker_id, 1);
        assert_eq!(d3.metrics.cpu_time_nanos, 500_000_000);

        let d4 = read_event(&mut cursor).unwrap().unwrap();
        assert_eq!(d4.event_type, EventType::PollStart);
        assert_eq!(d4.metrics.worker_local_queue_depth, 10);

        let d5 = read_event(&mut cursor).unwrap().unwrap();
        assert_eq!(d5.event_type, EventType::PollEnd);
        assert_eq!(d5.metrics.worker_local_queue_depth, 0);

        assert!(read_event(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn test_wire_codes_are_correct() {
        // Verify that zero-queue polls use codes 0/1 and non-zero use 5/6
        let mut buf = Vec::new();

        let poll_zero = TelemetryEvent::new(
            EventType::PollStart,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        write_event(&mut buf, &poll_zero).unwrap();
        assert_eq!(buf[0], WIRE_POLL_START);

        buf.clear();
        let poll_nonzero = TelemetryEvent::new(
            EventType::PollStart,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 5,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        write_event(&mut buf, &poll_nonzero).unwrap();
        assert_eq!(buf[0], WIRE_POLL_START_WITH_QUEUE);

        buf.clear();
        let end_zero = TelemetryEvent::new(
            EventType::PollEnd,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        write_event(&mut buf, &end_zero).unwrap();
        assert_eq!(buf[0], WIRE_POLL_END);

        buf.clear();
        let end_nonzero = TelemetryEvent::new(
            EventType::PollEnd,
            MetricsSnapshot {
                timestamp_nanos: 0,
                worker_id: 0,
                global_queue_depth: 0,
                worker_local_queue_depth: 3,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        );
        write_event(&mut buf, &end_nonzero).unwrap();
        assert_eq!(buf[0], WIRE_POLL_END_WITH_QUEUE);
    }
}
