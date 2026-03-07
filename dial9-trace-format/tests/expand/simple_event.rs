use dial9_trace_format::TraceEvent;

#[derive(TraceEvent)]
struct SimpleEvent {
    timestamp_ns: u64,
    value: u32,
}

fn main() {}
