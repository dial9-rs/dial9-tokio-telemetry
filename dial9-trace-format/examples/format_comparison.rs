//! Compare old (TOKIOTRC v16) vs new (TRC\0 v1) trace format.
//! Reads demo-trace.bin in the old format, re-encodes to new format,
//! compares raw + gzipped sizes, and benchmarks encode throughput.
//!
//! Usage: cargo run --example format_comparison --release

use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::{StackFrames, TraceEvent};
use std::io::{Cursor, Read, Write};
use std::time::Instant;

// --- Old format reader (inline, minimal) ---

const OLD_MAGIC: &[u8; 8] = b"TOKIOTRC";

#[derive(Debug)]
enum OldEvent {
    PollStart {
        ts_ns: u64,
        worker: u8,
        local_q: u8,
        task_id: u32,
        spawn_loc: u16,
    },
    PollEnd {
        ts_ns: u64,
        worker: u8,
    },
    WorkerPark {
        ts_ns: u64,
        worker: u8,
        local_q: u8,
        cpu_us: u32,
    },
    WorkerUnpark {
        ts_ns: u64,
        worker: u8,
        local_q: u8,
        cpu_us: u32,
        sched_wait_us: u32,
    },
    QueueSample {
        ts_ns: u64,
        global_q: u8,
    },
    SpawnLocationDef {
        id: u16,
        location: String,
    },
    TaskSpawn {
        ts_ns: u64,
        task_id: u32,
        spawn_loc: u16,
    },
    CpuSample {
        ts_ns: u64,
        worker: u8,
        tid: u32,
        source: u8,
        frames: Vec<u64>,
    },
    CallframeDef {
        address: u64,
        symbol: String,
        location: Option<String>,
    },
    ThreadNameDef {
        tid: u32,
        name: String,
    },
    WakeEvent {
        ts_ns: u64,
        waker: u32,
        woken: u32,
        target_worker: u8,
    },
    TaskTerminate {
        ts_ns: u64,
        task_id: u32,
    },
}

fn read_old_events(data: &[u8]) -> Vec<OldEvent> {
    let mut r = Cursor::new(data);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).unwrap();
    assert_eq!(&magic, OLD_MAGIC);
    let mut ver = [0u8; 4];
    r.read_exact(&mut ver).unwrap();

    let mut events = Vec::new();
    let mut tag = [0u8; 1];
    while r.read_exact(&mut tag).is_ok() {
        let ev = match tag[0] {
            5 => {
                // SpawnLocationDef
                let mut b = [0u8; 2];
                r.read_exact(&mut b).unwrap();
                let id = u16::from_le_bytes(b);
                r.read_exact(&mut b).unwrap();
                let len = u16::from_le_bytes(b) as usize;
                let mut s = vec![0u8; len];
                r.read_exact(&mut s).unwrap();
                OldEvent::SpawnLocationDef {
                    id,
                    location: String::from_utf8_lossy(&s).into(),
                }
            }
            6 => {
                // TaskSpawn
                let mut ts = [0u8; 4];
                r.read_exact(&mut ts).unwrap();
                let mut tid = [0u8; 4];
                r.read_exact(&mut tid).unwrap();
                let mut sl = [0u8; 2];
                r.read_exact(&mut sl).unwrap();
                OldEvent::TaskSpawn {
                    ts_ns: u32::from_le_bytes(ts) as u64 * 1000,
                    task_id: u32::from_le_bytes(tid),
                    spawn_loc: u16::from_le_bytes(sl),
                }
            }
            172 => {
                // TaskTerminate
                let mut ts = [0u8; 4];
                r.read_exact(&mut ts).unwrap();
                let mut tid = [0u8; 4];
                r.read_exact(&mut tid).unwrap();
                OldEvent::TaskTerminate {
                    ts_ns: u32::from_le_bytes(ts) as u64 * 1000,
                    task_id: u32::from_le_bytes(tid),
                }
            }
            9 => {
                // CallframeDef
                let mut ab = [0u8; 8];
                r.read_exact(&mut ab).unwrap();
                let mut lb = [0u8; 2];
                r.read_exact(&mut lb).unwrap();
                let len = u16::from_le_bytes(lb) as usize;
                let mut s = vec![0u8; len];
                r.read_exact(&mut s).unwrap();
                r.read_exact(&mut lb).unwrap();
                let loc_len = u16::from_le_bytes(lb);
                let location = if loc_len == 0xFFFF {
                    None
                } else {
                    let mut ls = vec![0u8; loc_len as usize];
                    r.read_exact(&mut ls).unwrap();
                    Some(String::from_utf8_lossy(&ls).into())
                };
                OldEvent::CallframeDef {
                    address: u64::from_le_bytes(ab),
                    symbol: String::from_utf8_lossy(&s).into(),
                    location,
                }
            }
            10 => {
                // ThreadNameDef
                let mut tb = [0u8; 4];
                r.read_exact(&mut tb).unwrap();
                let mut lb = [0u8; 2];
                r.read_exact(&mut lb).unwrap();
                let len = u16::from_le_bytes(lb) as usize;
                let mut s = vec![0u8; len];
                r.read_exact(&mut s).unwrap();
                OldEvent::ThreadNameDef {
                    tid: u32::from_le_bytes(tb),
                    name: String::from_utf8_lossy(&s).into(),
                }
            }
            _ => {
                // Events with timestamp prefix
                let mut ts = [0u8; 4];
                r.read_exact(&mut ts).unwrap();
                let ts_ns = u32::from_le_bytes(ts) as u64 * 1000;
                match tag[0] {
                    0 => {
                        // PollStart
                        let mut b1 = [0u8; 1];
                        r.read_exact(&mut b1).unwrap();
                        let mut b2 = [0u8; 1];
                        r.read_exact(&mut b2).unwrap();
                        let mut tid = [0u8; 4];
                        r.read_exact(&mut tid).unwrap();
                        let mut sl = [0u8; 2];
                        r.read_exact(&mut sl).unwrap();
                        OldEvent::PollStart {
                            ts_ns,
                            worker: b1[0],
                            local_q: b2[0],
                            task_id: u32::from_le_bytes(tid),
                            spawn_loc: u16::from_le_bytes(sl),
                        }
                    }
                    1 => {
                        let mut b = [0u8; 1];
                        r.read_exact(&mut b).unwrap();
                        OldEvent::PollEnd {
                            ts_ns,
                            worker: b[0],
                        }
                    }
                    2 => {
                        let mut w = [0u8; 1];
                        r.read_exact(&mut w).unwrap();
                        let mut lq = [0u8; 1];
                        r.read_exact(&mut lq).unwrap();
                        let mut cpu = [0u8; 4];
                        r.read_exact(&mut cpu).unwrap();
                        OldEvent::WorkerPark {
                            ts_ns,
                            worker: w[0],
                            local_q: lq[0],
                            cpu_us: u32::from_le_bytes(cpu),
                        }
                    }
                    3 => {
                        let mut w = [0u8; 1];
                        r.read_exact(&mut w).unwrap();
                        let mut lq = [0u8; 1];
                        r.read_exact(&mut lq).unwrap();
                        let mut cpu = [0u8; 4];
                        r.read_exact(&mut cpu).unwrap();
                        let mut sw = [0u8; 4];
                        r.read_exact(&mut sw).unwrap();
                        OldEvent::WorkerUnpark {
                            ts_ns,
                            worker: w[0],
                            local_q: lq[0],
                            cpu_us: u32::from_le_bytes(cpu),
                            sched_wait_us: u32::from_le_bytes(sw),
                        }
                    }
                    4 => {
                        let mut b = [0u8; 1];
                        r.read_exact(&mut b).unwrap();
                        OldEvent::QueueSample {
                            ts_ns,
                            global_q: b[0],
                        }
                    }
                    7 => {
                        // WakeEvent
                        let mut wk = [0u8; 4];
                        r.read_exact(&mut wk).unwrap();
                        let mut wn = [0u8; 4];
                        r.read_exact(&mut wn).unwrap();
                        let mut tw = [0u8; 1];
                        r.read_exact(&mut tw).unwrap();
                        OldEvent::WakeEvent {
                            ts_ns,
                            waker: u32::from_le_bytes(wk),
                            woken: u32::from_le_bytes(wn),
                            target_worker: tw[0],
                        }
                    }
                    8 => {
                        // CpuSample
                        let mut w = [0u8; 1];
                        r.read_exact(&mut w).unwrap();
                        let mut tb = [0u8; 4];
                        r.read_exact(&mut tb).unwrap();
                        let mut src = [0u8; 1];
                        r.read_exact(&mut src).unwrap();
                        let mut nf = [0u8; 1];
                        r.read_exact(&mut nf).unwrap();
                        let n = nf[0] as usize;
                        let mut frames = Vec::with_capacity(n);
                        for _ in 0..n {
                            let mut ab = [0u8; 8];
                            r.read_exact(&mut ab).unwrap();
                            frames.push(u64::from_le_bytes(ab));
                        }
                        OldEvent::CpuSample {
                            ts_ns,
                            worker: w[0],
                            tid: u32::from_le_bytes(tb),
                            source: src[0],
                            frames,
                        }
                    }
                    _ => {
                        eprintln!("unknown tag {}", tag[0]);
                        break;
                    }
                }
            }
        };
        events.push(ev);
    }
    events
}

// --- New format event types ---

#[derive(TraceEvent)]
struct PollStart {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
    local_queue_depth: u8,
    task_id: u32,
    spawn_loc_id: u16,
}
#[derive(TraceEvent)]
struct PollEnd {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
}
#[derive(TraceEvent)]
struct WorkerPark {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
    local_queue_depth: u8,
    cpu_time_us: u32,
}
#[derive(TraceEvent)]
struct WorkerUnpark {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
    local_queue_depth: u8,
    cpu_time_us: u32,
    sched_wait_us: u32,
}
#[derive(TraceEvent)]
struct QueueSample {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    global_queue_depth: u8,
}
#[derive(TraceEvent)]
struct TaskSpawn {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    task_id: u32,
    spawn_loc_id: u16,
}
#[derive(TraceEvent)]
struct TaskTerminate {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    task_id: u32,
}
#[derive(TraceEvent)]
struct WakeEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    waker_task_id: u32,
    woken_task_id: u32,
    target_worker: u8,
}
#[derive(TraceEvent)]
struct CpuSample {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
    tid: u32,
    source: u8,
    frames: StackFrames,
}
#[derive(TraceEvent)]
struct SpawnLocationDef {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    spawn_loc_id: u16,
    location: String,
}
#[derive(TraceEvent)]
struct CallframeDef {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    address: u64,
    symbol: String,
    location: String,
}
#[derive(TraceEvent)]
struct ThreadNameDef {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    tid: u32,
    name: String,
}

/// Pre-converted new-format events (no allocations during encode)
enum NewEvent {
    PollStart(PollStart),
    PollEnd(PollEnd),
    WorkerPark(WorkerPark),
    WorkerUnpark(WorkerUnpark),
    QueueSample(QueueSample),
    TaskSpawn(TaskSpawn),
    TaskTerminate(TaskTerminate),
    WakeEvent(WakeEvent),
    CpuSample(CpuSample),
    SpawnLocationDef(SpawnLocationDef),
    CallframeDef(CallframeDef),
    ThreadNameDef(ThreadNameDef),
}

fn convert_events(events: &[OldEvent]) -> Vec<NewEvent> {
    events
        .iter()
        .map(|ev| match ev {
            OldEvent::PollStart {
                ts_ns,
                worker,
                local_q,
                task_id,
                spawn_loc,
            } => NewEvent::PollStart(PollStart {
                timestamp_ns: *ts_ns,
                worker_id: *worker,
                local_queue_depth: *local_q,
                task_id: *task_id,
                spawn_loc_id: *spawn_loc,
            }),
            OldEvent::PollEnd { ts_ns, worker } => NewEvent::PollEnd(PollEnd {
                timestamp_ns: *ts_ns,
                worker_id: *worker,
            }),
            OldEvent::WorkerPark {
                ts_ns,
                worker,
                local_q,
                cpu_us,
            } => NewEvent::WorkerPark(WorkerPark {
                timestamp_ns: *ts_ns,
                worker_id: *worker,
                local_queue_depth: *local_q,
                cpu_time_us: *cpu_us,
            }),
            OldEvent::WorkerUnpark {
                ts_ns,
                worker,
                local_q,
                cpu_us,
                sched_wait_us,
            } => NewEvent::WorkerUnpark(WorkerUnpark {
                timestamp_ns: *ts_ns,
                worker_id: *worker,
                local_queue_depth: *local_q,
                cpu_time_us: *cpu_us,
                sched_wait_us: *sched_wait_us,
            }),
            OldEvent::QueueSample { ts_ns, global_q } => NewEvent::QueueSample(QueueSample {
                timestamp_ns: *ts_ns,
                global_queue_depth: *global_q,
            }),
            OldEvent::TaskSpawn {
                ts_ns,
                task_id,
                spawn_loc,
            } => NewEvent::TaskSpawn(TaskSpawn {
                timestamp_ns: *ts_ns,
                task_id: *task_id,
                spawn_loc_id: *spawn_loc,
            }),
            OldEvent::TaskTerminate { ts_ns, task_id } => NewEvent::TaskTerminate(TaskTerminate {
                timestamp_ns: *ts_ns,
                task_id: *task_id,
            }),
            OldEvent::WakeEvent {
                ts_ns,
                waker,
                woken,
                target_worker,
            } => NewEvent::WakeEvent(WakeEvent {
                timestamp_ns: *ts_ns,
                waker_task_id: *waker,
                woken_task_id: *woken,
                target_worker: *target_worker,
            }),
            OldEvent::CpuSample {
                ts_ns,
                worker,
                tid,
                source,
                frames,
            } => NewEvent::CpuSample(CpuSample {
                timestamp_ns: *ts_ns,
                worker_id: *worker,
                tid: *tid,
                source: *source,
                frames: StackFrames(frames.clone()),
            }),
            OldEvent::SpawnLocationDef { id, location } => {
                NewEvent::SpawnLocationDef(SpawnLocationDef {
                    timestamp_ns: 0,
                    spawn_loc_id: *id,
                    location: location.clone(),
                })
            }
            OldEvent::CallframeDef {
                address,
                symbol,
                location,
            } => NewEvent::CallframeDef(CallframeDef {
                timestamp_ns: 0,
                address: *address,
                symbol: symbol.clone(),
                location: location.clone().unwrap_or_default(),
            }),
            OldEvent::ThreadNameDef { tid, name } => NewEvent::ThreadNameDef(ThreadNameDef {
                timestamp_ns: 0,
                tid: *tid,
                name: name.clone(),
            }),
        })
        .collect()
}

fn encode_new(events: &[NewEvent]) -> Vec<u8> {
    let mut enc = Encoder::new();
    for ev in events {
        match ev {
            NewEvent::PollStart(e) => enc.write(e).unwrap(),
            NewEvent::PollEnd(e) => enc.write(e).unwrap(),
            NewEvent::WorkerPark(e) => enc.write(e).unwrap(),
            NewEvent::WorkerUnpark(e) => enc.write(e).unwrap(),
            NewEvent::QueueSample(e) => enc.write(e).unwrap(),
            NewEvent::TaskSpawn(e) => enc.write(e).unwrap(),
            NewEvent::TaskTerminate(e) => enc.write(e).unwrap(),
            NewEvent::WakeEvent(e) => enc.write(e).unwrap(),
            NewEvent::CpuSample(e) => enc.write(e).unwrap(),
            NewEvent::SpawnLocationDef(e) => enc.write(e).unwrap(),
            NewEvent::CallframeDef(e) => enc.write(e).unwrap(),
            NewEvent::ThreadNameDef(e) => enc.write(e).unwrap(),
        }
    }
    enc.finish()
}

fn re_encode_old(events: &[OldEvent]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 * 1024 * 1024);
    buf.extend_from_slice(OLD_MAGIC);
    buf.extend_from_slice(&16u32.to_le_bytes());
    for ev in events {
        match ev {
            OldEvent::PollStart {
                ts_ns,
                worker,
                local_q,
                task_id,
                spawn_loc,
            } => {
                buf.push(0);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.push(*worker);
                buf.push(*local_q);
                buf.extend_from_slice(&task_id.to_le_bytes());
                buf.extend_from_slice(&spawn_loc.to_le_bytes());
            }
            OldEvent::PollEnd { ts_ns, worker } => {
                buf.push(1);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.push(*worker);
            }
            OldEvent::WorkerPark {
                ts_ns,
                worker,
                local_q,
                cpu_us,
            } => {
                buf.push(2);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.push(*worker);
                buf.push(*local_q);
                buf.extend_from_slice(&cpu_us.to_le_bytes());
            }
            OldEvent::WorkerUnpark {
                ts_ns,
                worker,
                local_q,
                cpu_us,
                sched_wait_us,
            } => {
                buf.push(3);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.push(*worker);
                buf.push(*local_q);
                buf.extend_from_slice(&cpu_us.to_le_bytes());
                buf.extend_from_slice(&sched_wait_us.to_le_bytes());
            }
            OldEvent::QueueSample { ts_ns, global_q } => {
                buf.push(4);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.push(*global_q);
            }
            OldEvent::SpawnLocationDef { id, location } => {
                buf.push(5);
                buf.extend_from_slice(&id.to_le_bytes());
                buf.extend_from_slice(&(location.len() as u16).to_le_bytes());
                buf.extend_from_slice(location.as_bytes());
            }
            OldEvent::TaskSpawn {
                ts_ns,
                task_id,
                spawn_loc,
            } => {
                buf.push(6);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.extend_from_slice(&task_id.to_le_bytes());
                buf.extend_from_slice(&spawn_loc.to_le_bytes());
            }
            OldEvent::WakeEvent {
                ts_ns,
                waker,
                woken,
                target_worker,
            } => {
                buf.push(7);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.extend_from_slice(&waker.to_le_bytes());
                buf.extend_from_slice(&woken.to_le_bytes());
                buf.push(*target_worker);
            }
            OldEvent::CpuSample {
                ts_ns,
                worker,
                tid,
                source,
                frames,
            } => {
                buf.push(8);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.push(*worker);
                buf.extend_from_slice(&tid.to_le_bytes());
                buf.push(*source);
                buf.push(frames.len().min(255) as u8);
                for &a in frames.iter().take(255) {
                    buf.extend_from_slice(&a.to_le_bytes());
                }
            }
            OldEvent::CallframeDef {
                address,
                symbol,
                location,
            } => {
                buf.push(9);
                buf.extend_from_slice(&address.to_le_bytes());
                buf.extend_from_slice(&(symbol.len() as u16).to_le_bytes());
                buf.extend_from_slice(symbol.as_bytes());
                match location {
                    Some(l) => {
                        buf.extend_from_slice(&(l.len() as u16).to_le_bytes());
                        buf.extend_from_slice(l.as_bytes());
                    }
                    None => buf.extend_from_slice(&0xFFFFu16.to_le_bytes()),
                }
            }
            OldEvent::ThreadNameDef { tid, name } => {
                buf.push(10);
                buf.extend_from_slice(&tid.to_le_bytes());
                buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
                buf.extend_from_slice(name.as_bytes());
            }
            OldEvent::TaskTerminate { ts_ns, task_id } => {
                buf.push(172);
                buf.extend_from_slice(&((*ts_ns / 1000) as u32).to_le_bytes());
                buf.extend_from_slice(&task_id.to_le_bytes());
            }
        }
    }
    buf
}

fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        // Try relative paths from likely CWDs
        for p in &[
            "../../dial9-tokio-telemetry/trace_viewer/demo-trace.bin",
            "../dial9-tokio-telemetry/trace_viewer/demo-trace.bin",
            "dial9-tokio-telemetry/trace_viewer/demo-trace.bin",
        ] {
            if std::path::Path::new(p).exists() {
                return p.to_string();
            }
        }
        panic!("demo-trace.bin not found; pass path as argument");
    });

    let old_data = std::fs::read(&path).unwrap();
    println!(
        "Reading old format trace: {} ({} bytes)\n",
        path,
        old_data.len()
    );

    let events = read_old_events(&old_data);
    println!("Parsed {} events", events.len());

    // Count event types
    let mut counts = std::collections::HashMap::new();
    for ev in &events {
        let name = match ev {
            OldEvent::PollStart { .. } => "PollStart",
            OldEvent::PollEnd { .. } => "PollEnd",
            OldEvent::WorkerPark { .. } => "WorkerPark",
            OldEvent::WorkerUnpark { .. } => "WorkerUnpark",
            OldEvent::QueueSample { .. } => "QueueSample",
            OldEvent::SpawnLocationDef { .. } => "SpawnLocationDef",
            OldEvent::TaskSpawn { .. } => "TaskSpawn",
            OldEvent::TaskTerminate { .. } => "TaskTerminate",
            OldEvent::WakeEvent { .. } => "WakeEvent",
            OldEvent::CpuSample { .. } => "CpuSample",
            OldEvent::CallframeDef { .. } => "CallframeDef",
            OldEvent::ThreadNameDef { .. } => "ThreadNameDef",
        };
        *counts.entry(name).or_insert(0usize) += 1;
    }
    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, count) in &sorted {
        println!("  {name}: {count}");
    }
    println!();

    // Encode both formats
    let new_events = convert_events(&events);
    let new_data = encode_new(&new_events);
    let old_reencoded = re_encode_old(&events);

    let old_gz = gzip(&old_reencoded);
    let new_gz = gzip(&new_data);

    println!("=== Size Comparison ===\n");
    println!(
        "{:<20} {:>12} {:>12} {:>8}",
        "", "Old Format", "New Format", "Ratio"
    );
    println!(
        "{:<20} {:>12} {:>12} {:>8.2}x",
        "Raw",
        format!("{}", old_reencoded.len()),
        format!("{}", new_data.len()),
        old_reencoded.len() as f64 / new_data.len() as f64
    );
    println!(
        "{:<20} {:>12} {:>12} {:>8.2}x",
        "Gzipped",
        format!("{}", old_gz.len()),
        format!("{}", new_gz.len()),
        old_gz.len() as f64 / new_gz.len() as f64
    );
    println!();

    // Benchmark encode performance
    let warmup = 3;
    let iters = 500u32;

    // Warm up
    for _ in 0..warmup {
        std::hint::black_box(encode_new(&new_events));
    }
    for _ in 0..warmup {
        std::hint::black_box(re_encode_old(&events));
    }

    let t0 = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(encode_new(&new_events));
    }
    let new_dur = t0.elapsed();

    let t0 = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(re_encode_old(&events));
    }
    let old_dur = t0.elapsed();

    let new_per = new_dur / iters;
    let old_per = old_dur / iters;
    let new_throughput = events.len() as f64 / new_per.as_secs_f64() / 1_000_000.0;
    let old_throughput = events.len() as f64 / old_per.as_secs_f64() / 1_000_000.0;

    println!(
        "=== Encode Performance ({} events, {} iterations) ===\n",
        events.len(),
        iters
    );
    println!("{:<20} {:>12} {:>12}", "", "Old Format", "New Format");
    println!(
        "{:<20} {:>12.2} {:>12.2}",
        "Time/encode (ms)",
        old_per.as_secs_f64() * 1000.0,
        new_per.as_secs_f64() * 1000.0
    );
    println!(
        "{:<20} {:>12.2} {:>12.2}",
        "Throughput (M ev/s)", old_throughput, new_throughput
    );
    println!(
        "{:<20} {:>12.2} {:>12.2}",
        "Throughput (MB/s)",
        old_reencoded.len() as f64 / old_per.as_secs_f64() / 1_000_000.0,
        new_data.len() as f64 / new_per.as_secs_f64() / 1_000_000.0
    );
}
