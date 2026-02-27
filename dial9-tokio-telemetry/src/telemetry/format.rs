//! Binary trace wire format (v11).
//!
//! ## File layout
//! ```text
//! Header:  MAGIC (8 bytes) + VERSION (u32 LE) = 12 bytes
//!
//! Wire codes:
//!   0: PollStart                    → code(u8) + timestamp_us(u32) + worker_id(u8) + local_queue(u8) + task_id(u32) + spawn_loc_id(u16) = 13 bytes
//!   1: PollEnd                      → code(u8) + timestamp_us(u32) + worker_id(u8)                                                       = 6 bytes
//!   2: WorkerPark                   → code(u8) + timestamp_us(u32) + worker_id(u8) + local_queue(u8) + cpu_us(u32)                      = 11 bytes
//!   3: WorkerUnpark                 → code(u8) + timestamp_us(u32) + worker_id(u8) + local_queue(u8) + cpu_us(u32) + sched_wait_us(u32) = 15 bytes
//!   4: QueueSample                  → code(u8) + timestamp_us(u32) + global_queue(u8)                                                   = 6 bytes
//!   5: SpawnLocationDef             → code(u8) + spawn_loc_id(u16) + string_len(u16) + string_bytes(N)                                 = 5 + N bytes
//!   6: TaskSpawn                    → code(u8) + task_id(u32) + spawn_loc_id(u16)                                                       = 7 bytes
//!   7: CpuSample                    → code(u8) + timestamp_us(u32) + worker_id(u8) + tid(u32) + source(u8) + num_frames(u8) + frames(N * u64)  = 12 + 8N bytes
//!   8: CallframeDef                 → code(u8) + address(u64) + string_len(u16) + string_bytes(N)                                      = 11 + N bytes
//!   9: WakeEvent                    → code(u8) + timestamp_us(u32) + waker_task_id(u32) + woken_task_id(u32) + target_worker(u8)        = 14 bytes
//!  10: ThreadNameDef                → code(u8) + tid(u32) + string_len(u16) + string_bytes(N)                                          = 7 + N bytes
//! ```
//!
//! Timestamps are microseconds since trace start. u32 micros supports traces up to ~71 minutes.
//! `cpu_us` is thread CPU time (from `CLOCK_THREAD_CPUTIME_ID`) in microseconds since thread start.
//! `sched_wait_us` is the scheduling wait delta from schedstat during the park period.
//! In-memory representation remains nanoseconds; conversion happens at the wire boundary.
//!
//! ### v10 → v11 changes
//! - Added WakeEvent (code 9): emitted by Traced<F> waker wrapper, records who woke a task
//!
//! ### v12 → v13 changes
//! - Added tid(u32) field to CpuSample (code 8) for thread identification
//! - Added ThreadNameDef (code 10): maps OS tid to thread name for non-worker grouping

use crate::telemetry::events::TelemetryEvent;
use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};
use std::io::{Read, Result, Write};

pub const MAGIC: &[u8; 8] = b"TOKIOTRC";
pub const VERSION: u32 = 13;
pub const HEADER_SIZE: usize = 12; // 8 magic + 4 version

// Wire codes
const WIRE_POLL_START: u8 = 0;
const WIRE_POLL_END: u8 = 1;
const WIRE_WORKER_PARK: u8 = 2;
const WIRE_WORKER_UNPARK: u8 = 3;
const WIRE_QUEUE_SAMPLE: u8 = 4;
const WIRE_SPAWN_LOCATION_DEF: u8 = 5;
const WIRE_TASK_SPAWN: u8 = 6;
const WIRE_WAKE_EVENT: u8 = 7;
const WIRE_CPU_SAMPLE: u8 = 8;
const WIRE_CALLFRAME_DEF: u8 = 9;
const WIRE_THREAD_NAME_DEF: u8 = 10;

/// Returns the wire size of an event.
pub fn wire_event_size(event: &TelemetryEvent) -> usize {
    match event {
        TelemetryEvent::PollStart { .. } => 13,
        TelemetryEvent::PollEnd { .. } => 6,
        TelemetryEvent::QueueSample { .. } => 6,
        TelemetryEvent::WorkerPark { .. } => 11,
        TelemetryEvent::WorkerUnpark { .. } => 15,
        TelemetryEvent::SpawnLocationDef { location, .. } => 1 + 2 + 2 + location.len(),
        TelemetryEvent::TaskSpawn { .. } => 7,
        TelemetryEvent::CpuSample { callchain, .. } => 12 + 8 * callchain.len(),
        TelemetryEvent::CallframeDef {
            symbol, location, ..
        } => 1 + 8 + 2 + symbol.len() + 2 + location.as_ref().map_or(0, |l| l.len()),
        TelemetryEvent::ThreadNameDef { name, .. } => 1 + 4 + 2 + name.len(),
        TelemetryEvent::WakeEvent { .. } => 14,
    }
}

pub fn write_header(w: &mut impl Write) -> Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())
}

/// Write any event to the wire format.
pub fn write_event(w: &mut impl Write, event: &TelemetryEvent) -> Result<()> {
    match event {
        TelemetryEvent::PollStart {
            timestamp_nanos,
            worker_id,
            worker_local_queue_depth,
            task_id,
            spawn_loc_id,
        } => {
            let timestamp_us = (*timestamp_nanos / 1000) as u32;
            w.write_all(&[WIRE_POLL_START])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[*worker_id as u8])?;
            w.write_all(&[*worker_local_queue_depth as u8])?;
            w.write_all(&task_id.to_u32().to_le_bytes())?;
            w.write_all(&spawn_loc_id.as_u16().to_le_bytes())?;
        }
        TelemetryEvent::PollEnd {
            timestamp_nanos,
            worker_id,
        } => {
            let timestamp_us = (*timestamp_nanos / 1000) as u32;
            w.write_all(&[WIRE_POLL_END])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[*worker_id as u8])?;
        }
        TelemetryEvent::WorkerPark {
            timestamp_nanos,
            worker_id,
            worker_local_queue_depth,
            cpu_time_nanos,
        } => {
            let timestamp_us = (*timestamp_nanos / 1000) as u32;
            w.write_all(&[WIRE_WORKER_PARK])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[*worker_id as u8])?;
            w.write_all(&[*worker_local_queue_depth as u8])?;
            let cpu_us = (*cpu_time_nanos / 1000) as u32;
            w.write_all(&cpu_us.to_le_bytes())?;
        }
        TelemetryEvent::WorkerUnpark {
            timestamp_nanos,
            worker_id,
            worker_local_queue_depth,
            cpu_time_nanos,
            sched_wait_delta_nanos,
        } => {
            let timestamp_us = (*timestamp_nanos / 1000) as u32;
            w.write_all(&[WIRE_WORKER_UNPARK])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[*worker_id as u8])?;
            w.write_all(&[*worker_local_queue_depth as u8])?;
            let cpu_us = (*cpu_time_nanos / 1000) as u32;
            w.write_all(&cpu_us.to_le_bytes())?;
            let sched_wait_us = (*sched_wait_delta_nanos / 1000) as u32;
            w.write_all(&sched_wait_us.to_le_bytes())?;
        }
        TelemetryEvent::QueueSample {
            timestamp_nanos,
            global_queue_depth,
        } => {
            let timestamp_us = (*timestamp_nanos / 1000) as u32;
            w.write_all(&[WIRE_QUEUE_SAMPLE])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[*global_queue_depth as u8])?;
        }
        TelemetryEvent::SpawnLocationDef { id, location } => {
            w.write_all(&[WIRE_SPAWN_LOCATION_DEF])?;
            w.write_all(&id.as_u16().to_le_bytes())?;
            let len = location.len() as u16;
            w.write_all(&len.to_le_bytes())?;
            w.write_all(location.as_bytes())?;
        }
        TelemetryEvent::TaskSpawn {
            task_id,
            spawn_loc_id,
        } => {
            w.write_all(&[WIRE_TASK_SPAWN])?;
            w.write_all(&task_id.to_u32().to_le_bytes())?;
            w.write_all(&spawn_loc_id.as_u16().to_le_bytes())?;
        }
        TelemetryEvent::CpuSample {
            timestamp_nanos,
            worker_id,
            tid,
            source,
            callchain,
        } => {
            let timestamp_us = (*timestamp_nanos / 1000) as u32;
            w.write_all(&[WIRE_CPU_SAMPLE])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&[*worker_id as u8])?;
            w.write_all(&tid.to_le_bytes())?;
            w.write_all(&[*source as u8])?;
            let num_frames = callchain.len().min(255) as u8;
            w.write_all(&[num_frames])?;
            for &addr in callchain.iter().take(num_frames as usize) {
                w.write_all(&addr.to_le_bytes())?;
            }
        }
        TelemetryEvent::CallframeDef {
            address,
            symbol,
            location,
        } => {
            w.write_all(&[WIRE_CALLFRAME_DEF])?;
            w.write_all(&address.to_le_bytes())?;
            let len = symbol.len() as u16;
            w.write_all(&len.to_le_bytes())?;
            w.write_all(symbol.as_bytes())?;
            // Write location as optional string: 0xFFFF = None, otherwise length + bytes
            match location {
                Some(loc) => {
                    let loc_len = loc.len() as u16;
                    w.write_all(&loc_len.to_le_bytes())?;
                    w.write_all(loc.as_bytes())?;
                }
                None => {
                    w.write_all(&0xFFFFu16.to_le_bytes())?;
                }
            }
        }
        TelemetryEvent::ThreadNameDef { tid, name } => {
            w.write_all(&[WIRE_THREAD_NAME_DEF])?;
            w.write_all(&tid.to_le_bytes())?;
            let len = name.len() as u16;
            w.write_all(&len.to_le_bytes())?;
            w.write_all(name.as_bytes())?;
        }
        TelemetryEvent::WakeEvent {
            timestamp_nanos,
            waker_task_id,
            woken_task_id,
            target_worker,
        } => {
            let timestamp_us = (*timestamp_nanos / 1000) as u32;
            w.write_all(&[WIRE_WAKE_EVENT])?;
            w.write_all(&timestamp_us.to_le_bytes())?;
            w.write_all(&waker_task_id.to_u32().to_le_bytes())?;
            w.write_all(&woken_task_id.to_u32().to_le_bytes())?;
            w.write_all(&[*target_worker])?;
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

/// Read one event from the wire. Returns `Ok(None)` at EOF.
///
/// All event types are returned, including `SpawnLocationDef` and `TaskSpawn`.
/// Callers that want to filter metadata records can check
/// [`TelemetryEvent::is_runtime_event()`].
pub fn read_event(r: &mut impl Read) -> Result<Option<TelemetryEvent>> {
    let mut tag = [0u8; 1];
    if r.read_exact(&mut tag).is_err() {
        return Ok(None);
    }

    // SpawnLocationDef and TaskSpawn have no timestamp — handle before reading ts
    if tag[0] == WIRE_SPAWN_LOCATION_DEF {
        let mut spawn_loc_id_bytes = [0u8; 2];
        r.read_exact(&mut spawn_loc_id_bytes)?;
        let id = SpawnLocationId::from_u16(u16::from_le_bytes(spawn_loc_id_bytes));

        let mut len_bytes = [0u8; 2];
        r.read_exact(&mut len_bytes)?;
        let len = u16::from_le_bytes(len_bytes) as usize;

        let mut string_bytes = vec![0u8; len];
        r.read_exact(&mut string_bytes)?;
        let location = String::from_utf8(string_bytes)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid UTF-8"))?;

        return Ok(Some(TelemetryEvent::SpawnLocationDef { id, location }));
    }
    if tag[0] == WIRE_TASK_SPAWN {
        let mut task_id_bytes = [0u8; 4];
        let mut spawn_loc_id_bytes = [0u8; 2];
        r.read_exact(&mut task_id_bytes)?;
        r.read_exact(&mut spawn_loc_id_bytes)?;
        return Ok(Some(TelemetryEvent::TaskSpawn {
            task_id: TaskId::from_u32(u32::from_le_bytes(task_id_bytes)),
            spawn_loc_id: SpawnLocationId::from_u16(u16::from_le_bytes(spawn_loc_id_bytes)),
        }));
    }
    if tag[0] == WIRE_CALLFRAME_DEF {
        let mut addr_bytes = [0u8; 8];
        r.read_exact(&mut addr_bytes)?;
        let address = u64::from_le_bytes(addr_bytes);

        let mut len_bytes = [0u8; 2];
        r.read_exact(&mut len_bytes)?;
        let len = u16::from_le_bytes(len_bytes) as usize;

        let mut string_bytes = vec![0u8; len];
        r.read_exact(&mut string_bytes)?;
        let symbol = String::from_utf8(string_bytes)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid UTF-8"))?;

        // Read optional location field
        let mut loc_len_bytes = [0u8; 2];
        r.read_exact(&mut loc_len_bytes)?;
        let loc_len = u16::from_le_bytes(loc_len_bytes);
        let location = if loc_len == 0xFFFF {
            None
        } else {
            let mut loc_bytes = vec![0u8; loc_len as usize];
            r.read_exact(&mut loc_bytes)?;
            Some(String::from_utf8(loc_bytes).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid UTF-8")
            })?)
        };

        return Ok(Some(TelemetryEvent::CallframeDef {
            address,
            symbol,
            location,
        }));
    }
    if tag[0] == WIRE_THREAD_NAME_DEF {
        let mut tid_bytes = [0u8; 4];
        r.read_exact(&mut tid_bytes)?;
        let tid = u32::from_le_bytes(tid_bytes);

        let mut len_bytes = [0u8; 2];
        r.read_exact(&mut len_bytes)?;
        let len = u16::from_le_bytes(len_bytes) as usize;

        let mut string_bytes = vec![0u8; len];
        r.read_exact(&mut string_bytes)?;
        let name = String::from_utf8(string_bytes)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid UTF-8"))?;

        return Ok(Some(TelemetryEvent::ThreadNameDef { tid, name }));
    }
    let mut ts = [0u8; 4];
    r.read_exact(&mut ts)?;
    let timestamp_nanos = u32::from_le_bytes(ts) as u64 * 1000;

    let event = match tag[0] {
        WIRE_POLL_START => {
            let mut wid = [0u8; 1];
            let mut lq = [0u8; 1];
            let mut task_id_bytes = [0u8; 4];
            let mut spawn_loc_id_bytes = [0u8; 2];
            r.read_exact(&mut wid)?;
            r.read_exact(&mut lq)?;
            r.read_exact(&mut task_id_bytes)?;
            r.read_exact(&mut spawn_loc_id_bytes)?;

            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id: wid[0] as usize,
                worker_local_queue_depth: lq[0] as usize,
                task_id: TaskId::from_u32(u32::from_le_bytes(task_id_bytes)),
                spawn_loc_id: SpawnLocationId::from_u16(u16::from_le_bytes(spawn_loc_id_bytes)),
            }
        }
        WIRE_POLL_END => {
            let mut wid = [0u8; 1];
            r.read_exact(&mut wid)?;
            TelemetryEvent::PollEnd {
                timestamp_nanos,
                worker_id: wid[0] as usize,
            }
        }
        WIRE_WORKER_PARK => {
            let mut wid = [0u8; 1];
            let mut lq = [0u8; 1];
            r.read_exact(&mut wid)?;
            r.read_exact(&mut lq)?;
            let mut cpu = [0u8; 4];
            r.read_exact(&mut cpu)?;
            let cpu_time_nanos = u32::from_le_bytes(cpu) as u64 * 1000;
            TelemetryEvent::WorkerPark {
                timestamp_nanos,
                worker_id: wid[0] as usize,
                worker_local_queue_depth: lq[0] as usize,
                cpu_time_nanos,
            }
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
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id: wid[0] as usize,
                worker_local_queue_depth: lq[0] as usize,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            }
        }
        WIRE_QUEUE_SAMPLE => {
            let mut gq = [0u8; 1];
            r.read_exact(&mut gq)?;
            TelemetryEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth: gq[0] as usize,
            }
        }
        WIRE_CPU_SAMPLE => {
            let mut wid = [0u8; 1];
            let mut tid_bytes = [0u8; 4];
            let mut src = [0u8; 1];
            let mut nf = [0u8; 1];
            r.read_exact(&mut wid)?;
            r.read_exact(&mut tid_bytes)?;
            r.read_exact(&mut src)?;
            r.read_exact(&mut nf)?;
            let num_frames = nf[0] as usize;
            let mut callchain = Vec::with_capacity(num_frames);
            for _ in 0..num_frames {
                let mut addr = [0u8; 8];
                r.read_exact(&mut addr)?;
                callchain.push(u64::from_le_bytes(addr));
            }
            TelemetryEvent::CpuSample {
                timestamp_nanos,
                worker_id: wid[0] as usize,
                tid: u32::from_le_bytes(tid_bytes),
                source: crate::telemetry::events::CpuSampleSource::from_u8(src[0]),
                callchain,
            }
        }
        WIRE_WAKE_EVENT => {
            let mut waker_task_id_bytes = [0u8; 4];
            let mut woken_task_id_bytes = [0u8; 4];
            let mut tw = [0u8; 1];
            r.read_exact(&mut waker_task_id_bytes)?;
            r.read_exact(&mut woken_task_id_bytes)?;
            r.read_exact(&mut tw)?;
            TelemetryEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id: TaskId::from_u32(u32::from_le_bytes(waker_task_id_bytes)),
                woken_task_id: TaskId::from_u32(u32::from_le_bytes(woken_task_id_bytes)),
                target_worker: tw[0],
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::task_metadata::{
        SpawnLocationId, TaskId, UNKNOWN_SPAWN_LOCATION_ID, UNKNOWN_TASK_ID,
    };
    use std::io::Cursor;

    /// Write an event and read it back, asserting the wire size matches.
    fn roundtrip(event: &TelemetryEvent) -> TelemetryEvent {
        let mut buf = Vec::new();
        write_event(&mut buf, event).unwrap();
        assert_eq!(buf.len(), wire_event_size(event));
        let decoded = read_event(&mut Cursor::new(buf)).unwrap().unwrap();
        decoded
    }

    /// Helper: read the next runtime event, skipping metadata records.
    fn read_runtime_event(r: &mut impl Read) -> Result<Option<TelemetryEvent>> {
        loop {
            match read_event(r)? {
                None => return Ok(None),
                Some(e) if e.is_runtime_event() => return Ok(Some(e)),
                Some(_) => continue,
            }
        }
    }

    #[test]
    fn test_poll_start_zero_queue_roundtrip() {
        let event = TelemetryEvent::PollStart {
            timestamp_nanos: 123_456_000,
            worker_id: 3,
            worker_local_queue_depth: 0,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        assert_eq!(wire_event_size(&event), 13);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                ..
            } => {
                assert_eq!(timestamp_nanos, 123_456_000);
                assert_eq!(worker_id, 3);
                assert_eq!(worker_local_queue_depth, 0);
            }
            _ => panic!("expected PollStart"),
        }
    }

    #[test]
    fn test_poll_start_with_queue_roundtrip() {
        let event = TelemetryEvent::PollStart {
            timestamp_nanos: 123_456_000,
            worker_id: 3,
            worker_local_queue_depth: 17,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        assert_eq!(wire_event_size(&event), 13);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                ..
            } => {
                assert_eq!(timestamp_nanos, 123_456_000);
                assert_eq!(worker_id, 3);
                assert_eq!(worker_local_queue_depth, 17);
            }
            _ => panic!("expected PollStart"),
        }
    }

    #[test]
    fn test_poll_end_zero_queue_roundtrip() {
        let event = TelemetryEvent::PollEnd {
            timestamp_nanos: 999_000,
            worker_id: 1,
        };
        assert_eq!(wire_event_size(&event), 6);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::PollEnd {
                worker_id,
                timestamp_nanos,
            } => {
                assert_eq!(worker_id, 1);
                assert_eq!(timestamp_nanos, 999_000);
            }
            _ => panic!("expected PollEnd"),
        }
    }

    #[test]
    fn test_poll_end_with_queue_roundtrip() {
        // PollEnd doesn't carry local_queue on the wire, so the value is lost
        let event = TelemetryEvent::PollEnd {
            timestamp_nanos: 999_000,
            worker_id: 1,
        };
        assert_eq!(wire_event_size(&event), 6);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::PollEnd { worker_id, .. } => {
                assert_eq!(worker_id, 1);
            }
            _ => panic!("expected PollEnd"),
        }
    }

    #[test]
    fn test_sub_microsecond_truncation() {
        let event = TelemetryEvent::PollStart {
            timestamp_nanos: 123_456_789,
            worker_id: 0,
            worker_local_queue_depth: 0,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        let decoded = roundtrip(&event);
        // Sub-microsecond precision is lost: 123_456_789 -> 123_456_000
        assert_eq!(decoded.timestamp_nanos(), Some(123_456_000));
    }

    #[test]
    fn test_park_roundtrip() {
        let event = TelemetryEvent::WorkerPark {
            timestamp_nanos: 5_000_000_000,
            worker_id: 7,
            worker_local_queue_depth: 200,
            cpu_time_nanos: 1_234_567_000,
        };
        assert_eq!(wire_event_size(&event), 11);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => {
                assert_eq!(timestamp_nanos, 5_000_000_000);
                assert_eq!(worker_id, 7);
                assert_eq!(worker_local_queue_depth, 200);
                assert_eq!(cpu_time_nanos, 1_234_567_000);
            }
            _ => panic!("expected WorkerPark"),
        }
    }

    #[test]
    fn test_park_zero_queue_roundtrip() {
        let event = TelemetryEvent::WorkerPark {
            timestamp_nanos: 1_000_000,
            worker_id: 0,
            worker_local_queue_depth: 0,
            cpu_time_nanos: 0,
        };
        assert_eq!(wire_event_size(&event), 11);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::WorkerPark {
                worker_local_queue_depth,
                ..
            } => {
                assert_eq!(worker_local_queue_depth, 0);
            }
            _ => panic!("expected WorkerPark"),
        }
    }

    #[test]
    fn test_unpark_roundtrip() {
        let event = TelemetryEvent::WorkerUnpark {
            timestamp_nanos: 1_000_000,
            worker_id: 2,
            worker_local_queue_depth: 55,
            cpu_time_nanos: 999_000,
            sched_wait_delta_nanos: 42_000,
        };
        assert_eq!(wire_event_size(&event), 15);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::WorkerUnpark {
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
                ..
            } => {
                assert_eq!(worker_id, 2);
                assert_eq!(worker_local_queue_depth, 55);
                assert_eq!(cpu_time_nanos, 999_000);
                assert_eq!(sched_wait_delta_nanos, 42_000);
            }
            _ => panic!("expected WorkerUnpark"),
        }
    }

    #[test]
    fn test_queue_sample_roundtrip() {
        let event = TelemetryEvent::QueueSample {
            timestamp_nanos: 10_000_000_000,
            global_queue_depth: 128,
        };
        assert_eq!(wire_event_size(&event), 6);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => {
                assert_eq!(timestamp_nanos, 10_000_000_000);
                assert_eq!(global_queue_depth, 128);
            }
            _ => panic!("expected QueueSample"),
        }
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
        // PollStart always 13 bytes
        let poll_zero = TelemetryEvent::PollStart {
            timestamp_nanos: 0,
            worker_id: 0,
            worker_local_queue_depth: 0,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        assert_eq!(wire_event_size(&poll_zero), 13);

        let poll_nonzero = TelemetryEvent::PollStart {
            timestamp_nanos: 0,
            worker_id: 0,
            worker_local_queue_depth: 1,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        assert_eq!(wire_event_size(&poll_nonzero), 13);

        // Park always 11 bytes
        let park = TelemetryEvent::WorkerPark {
            timestamp_nanos: 0,
            worker_id: 0,
            worker_local_queue_depth: 0,
            cpu_time_nanos: 0,
        };
        assert_eq!(wire_event_size(&park), 11);

        // Unpark always 15 bytes
        let unpark = TelemetryEvent::WorkerUnpark {
            timestamp_nanos: 0,
            worker_id: 0,
            worker_local_queue_depth: 0,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        };
        assert_eq!(wire_event_size(&unpark), 15);

        // QueueSample always 6 bytes
        let qs = TelemetryEvent::QueueSample {
            timestamp_nanos: 0,
            global_queue_depth: 0,
        };
        assert_eq!(wire_event_size(&qs), 6);

        // TaskSpawn always 7 bytes
        let ts = TelemetryEvent::TaskSpawn {
            task_id: TaskId::from_u32(1),
            spawn_loc_id: SpawnLocationId::from_u16(1),
        };
        assert_eq!(wire_event_size(&ts), 7);

        // SpawnLocationDef is 5 + N bytes
        let def = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(1),
            location: "src/main.rs:10:5".to_string(),
        };
        assert_eq!(wire_event_size(&def), 5 + 16);

        let def_empty = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(1),
            location: String::new(),
        };
        assert_eq!(wire_event_size(&def_empty), 5);
    }

    #[test]
    fn test_max_timestamp() {
        // u32 micros max = 4_294_967_295 µs ≈ 71.6 minutes
        let event = TelemetryEvent::PollEnd {
            timestamp_nanos: 4_294_967_295_000,
            worker_id: 0,
        };
        let decoded = roundtrip(&event);
        assert_eq!(decoded.timestamp_nanos(), Some(4_294_967_295_000));
    }

    #[test]
    fn test_mixed_event_stream() {
        let events: Vec<TelemetryEvent> = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::QueueSample {
                timestamp_nanos: 2_000_000,
                global_queue_depth: 42,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 3_000_000,
                worker_id: 0,
            },
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 4_000_000,
                worker_id: 1,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 500_000_000,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 5_000_000,
                worker_id: 2,
                worker_local_queue_depth: 10,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 6_000_000,
                worker_id: 2,
            },
        ];

        let mut buf = Vec::new();
        for e in &events {
            write_event(&mut buf, e).unwrap();
        }

        // 13 + 6 + 6 + 11 + 13 + 6 = 55
        assert_eq!(buf.len(), 55);

        let mut cursor = Cursor::new(buf);
        let d0 = read_event(&mut cursor).unwrap().unwrap();
        assert!(matches!(
            d0,
            TelemetryEvent::PollStart {
                worker_local_queue_depth: 0,
                ..
            }
        ));

        let d1 = read_event(&mut cursor).unwrap().unwrap();
        assert!(matches!(
            d1,
            TelemetryEvent::QueueSample {
                global_queue_depth: 42,
                ..
            }
        ));

        let d2 = read_event(&mut cursor).unwrap().unwrap();
        assert!(matches!(d2, TelemetryEvent::PollEnd { .. }));

        let d3 = read_event(&mut cursor).unwrap().unwrap();
        assert!(matches!(
            d3,
            TelemetryEvent::WorkerPark {
                worker_id: 1,
                cpu_time_nanos: 500_000_000,
                ..
            }
        ));

        let d4 = read_event(&mut cursor).unwrap().unwrap();
        assert!(matches!(
            d4,
            TelemetryEvent::PollStart {
                worker_local_queue_depth: 10,
                ..
            }
        ));

        let d5 = read_event(&mut cursor).unwrap().unwrap();
        assert!(matches!(d5, TelemetryEvent::PollEnd { .. }));

        assert!(read_event(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn test_wire_codes_are_correct() {
        let mut buf = Vec::new();

        let poll_start = TelemetryEvent::PollStart {
            timestamp_nanos: 0,
            worker_id: 0,
            worker_local_queue_depth: 0,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        write_event(&mut buf, &poll_start).unwrap();
        assert_eq!(buf[0], WIRE_POLL_START);

        buf.clear();
        let poll_start_nonzero = TelemetryEvent::PollStart {
            timestamp_nanos: 0,
            worker_id: 0,
            worker_local_queue_depth: 5,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        write_event(&mut buf, &poll_start_nonzero).unwrap();
        assert_eq!(buf[0], WIRE_POLL_START);

        buf.clear();
        let poll_end = TelemetryEvent::PollEnd {
            timestamp_nanos: 0,
            worker_id: 0,
        };
        write_event(&mut buf, &poll_end).unwrap();
        assert_eq!(buf[0], WIRE_POLL_END);

        buf.clear();
        let spawn_def = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(1),
            location: "test".to_string(),
        };
        write_event(&mut buf, &spawn_def).unwrap();
        assert_eq!(buf[0], WIRE_SPAWN_LOCATION_DEF);

        buf.clear();
        let task_spawn = TelemetryEvent::TaskSpawn {
            task_id: TaskId::from_u32(1),
            spawn_loc_id: SpawnLocationId::from_u16(1),
        };
        write_event(&mut buf, &task_spawn).unwrap();
        assert_eq!(buf[0], WIRE_TASK_SPAWN);
    }

    #[test]
    fn test_poll_start_v7_with_task_metadata() {
        let event = TelemetryEvent::PollStart {
            timestamp_nanos: 123_456_000,
            worker_id: 3,
            worker_local_queue_depth: 17,
            task_id: TaskId::from_u32(42),
            spawn_loc_id: SpawnLocationId::from_u16(5),
        };

        assert_eq!(wire_event_size(&event), 13);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                spawn_loc_id,
            } => {
                assert_eq!(timestamp_nanos, 123_456_000);
                assert_eq!(worker_id, 3);
                assert_eq!(worker_local_queue_depth, 17);
                assert_eq!(task_id.to_u32(), 42);
                assert_eq!(spawn_loc_id.as_u16(), 5);
            }
            _ => panic!("expected PollStart"),
        }
    }

    #[test]
    fn test_spawn_location_def_roundtrip() {
        let event = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(42),
            location: "src/main.rs:123:45".to_string(),
        };

        let mut buf = Vec::new();
        write_event(&mut buf, &event).unwrap();

        // Check wire format: code(1) + id(2) + len(2) + string
        assert_eq!(buf[0], WIRE_SPAWN_LOCATION_DEF);
        assert_eq!(buf.len(), wire_event_size(&event));

        let mut cursor = Cursor::new(buf);
        let decoded = read_event(&mut cursor).unwrap().unwrap();
        match decoded {
            TelemetryEvent::SpawnLocationDef { id, location } => {
                assert_eq!(id.as_u16(), 42);
                assert_eq!(location, "src/main.rs:123:45");
            }
            _ => panic!("expected SpawnLocationDef"),
        }
    }

    #[test]
    fn test_task_spawn_roundtrip() {
        let event = TelemetryEvent::TaskSpawn {
            task_id: TaskId::from_u32(99),
            spawn_loc_id: SpawnLocationId::from_u16(7),
        };

        let mut buf = Vec::new();
        write_event(&mut buf, &event).unwrap();
        assert_eq!(buf.len(), wire_event_size(&event));
        assert_eq!(buf[0], WIRE_TASK_SPAWN);

        let mut cursor = Cursor::new(buf);
        let decoded = read_event(&mut cursor).unwrap().unwrap();
        match decoded {
            TelemetryEvent::TaskSpawn {
                task_id,
                spawn_loc_id,
            } => {
                assert_eq!(task_id.to_u32(), 99);
                assert_eq!(spawn_loc_id.as_u16(), 7);
            }
            _ => panic!("expected TaskSpawn"),
        }
    }

    #[test]
    fn test_mixed_stream_with_metadata_records() {
        // Write a stream that includes SpawnLocationDef and TaskSpawn interleaved with
        // runtime events, then read it back verifying order is preserved.
        let events: Vec<TelemetryEvent> = vec![
            TelemetryEvent::SpawnLocationDef {
                id: SpawnLocationId::from_u16(1),
                location: "src/main.rs:10:1".to_string(),
            },
            TelemetryEvent::TaskSpawn {
                task_id: TaskId::from_u32(100),
                spawn_loc_id: SpawnLocationId::from_u16(1),
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                worker_local_queue_depth: 3,
                task_id: TaskId::from_u32(100),
                spawn_loc_id: SpawnLocationId::from_u16(1),
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: 0,
            },
        ];

        let mut buf = Vec::new();
        for e in &events {
            write_event(&mut buf, e).unwrap();
        }

        let expected_size: usize = events.iter().map(|e| wire_event_size(e)).sum();
        assert_eq!(buf.len(), expected_size);

        // Read all events back (including metadata)
        let mut cursor = Cursor::new(&buf);
        let mut decoded = Vec::new();
        while let Some(e) = read_event(&mut cursor).unwrap() {
            decoded.push(e);
        }
        assert_eq!(decoded.len(), 4);
        assert!(matches!(
            decoded[0],
            TelemetryEvent::SpawnLocationDef { .. }
        ));
        assert!(matches!(decoded[1], TelemetryEvent::TaskSpawn { .. }));
        assert!(matches!(decoded[2], TelemetryEvent::PollStart { .. }));
        assert!(matches!(decoded[3], TelemetryEvent::PollEnd { .. }));

        // Read only runtime events using the helper
        let mut cursor2 = Cursor::new(&buf);
        let mut runtime_events = Vec::new();
        while let Some(e) = read_runtime_event(&mut cursor2).unwrap() {
            runtime_events.push(e);
        }
        assert_eq!(runtime_events.len(), 2);
        assert!(matches!(
            runtime_events[0],
            TelemetryEvent::PollStart { .. }
        ));
        assert!(matches!(runtime_events[1], TelemetryEvent::PollEnd { .. }));
    }

    #[test]
    fn test_spawn_location_def_wire_size_matches_written() {
        // Verify wire_event_size matches actual written bytes for various string lengths
        for len in [0, 1, 10, 100, 255] {
            let location: String = "x".repeat(len);
            let event = TelemetryEvent::SpawnLocationDef {
                id: SpawnLocationId::from_u16(1),
                location,
            };
            let mut buf = Vec::new();
            write_event(&mut buf, &event).unwrap();
            assert_eq!(
                buf.len(),
                wire_event_size(&event),
                "size mismatch for string length {len}"
            );
        }
    }

    #[test]
    fn test_wake_event_roundtrip() {
        let event = TelemetryEvent::WakeEvent {
            timestamp_nanos: 5_000_000,
            waker_task_id: TaskId::from_u32(10),
            woken_task_id: TaskId::from_u32(20),
            target_worker: 3,
        };
        assert_eq!(wire_event_size(&event), 14);
        let decoded = roundtrip(&event);
        match decoded {
            TelemetryEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            } => {
                assert_eq!(timestamp_nanos, 5_000_000);
                assert_eq!(waker_task_id.to_u32(), 10);
                assert_eq!(woken_task_id.to_u32(), 20);
                assert_eq!(target_worker, 3);
            }
            _ => panic!("expected WakeEvent"),
        }
    }
}
