# dial9-trace-format

A binary trace format for tokio runtime telemetry, usable for any structured event stream.

See [SPEC.md](SPEC.md) for the wire format specification.

## Design principles

1. **Self-describing.** Schemas are embedded in the stream so readers don't need out-of-band type definitions. No code generation or IDL compiler is needed to read a stream.
2. **Relatively compact.** Events average ~15 bytes raw and ~3 bytes gzipped on a real-world tokio trace. The format isn't the absolute smallest possible; it trades a few bytes of overhead for the properties below.
3. **Extremely fast to write.** The encoder does no allocations per event and uses fixed-width or [LEB128](https://en.wikipedia.org/wiki/LEB128) (variable-length integer) fields with no framing overhead beyond a 3-byte header. Benchmarks show ~48M events/s encode throughput on a single core.
4. **Compressible.** Schemas are written once; events are pure data with no repeated field names or tags. Timestamps are delta-encoded as 3-byte offsets. This structure compresses well: gzip reduces a typical trace to ~20% of its raw size (and beats a hand-tuned bespoke format by 1.4x after compression).
5. **Simple enough to port.** The entire wire format is ~5 frame types, LEB128 integers, and little-endian fixed-width fields. A [JavaScript decoder](js/decode.js) is under 200 lines.

## Numbers

On a 42k-event tokio runtime trace (`format_comparison` example, release mode):

| | Raw | Gzipped |
|---|---|---|
| dial9-trace-format | 632 KB (14.8 B/event) | 129 KB (3.0 B/event) |
| Hand-tuned bespoke format | 586 KB (13.7 B/event) | 177 KB (4.1 B/event) |

The self-describing format is ~8% larger raw but **37% smaller after gzip** because its regular structure compresses better than the bespoke format's variable-length tag soup.

At 42k events per trace and a 1-second collection interval, a continuously-running agent produces roughly **11 GB/day raw** or **2.3 GB/day gzipped**.

Throughput (criterion, 1M mixed events, single core):

| Operation | Events/s |
|---|---|
| Encode | ~48M |
| Decode (visitor, zero-alloc) | ~30M |
| Decode (zero-copy ref) | ~7M |
| Decode (owned) | ~6M |

## Usage

### Derive macro

For event types known at compile time, use `#[derive(TraceEvent)]`:

```rust
use dial9_trace_format::{TraceEvent, StackFrames};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::decoder::{Decoder, DecodedFrame};

#[derive(TraceEvent)]
struct PollStart {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    task_id: u64,
}

#[derive(TraceEvent)]
struct CpuSample {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    tid: u32,
    frames: StackFrames,
}

// Encode
let mut enc = Encoder::new();
enc.write(&PollStart { timestamp_ns: 1_000_000, worker_id: 0, task_id: 42 });
enc.write(&CpuSample {
    timestamp_ns: 1_050_000, tid: 12345,
    frames: StackFrames(vec![0x5555_1234, 0x5555_0a00]),
});
let bytes = enc.finish();

// Decode
let mut dec = Decoder::new(&bytes).unwrap();
for frame in dec.decode_all() {
    match frame {
        DecodedFrame::Schema(s) => println!("schema: {}", s.name),
        DecodedFrame::Event { type_id, timestamp_ns, values } => {
            let name = &dec.registry().get(type_id).unwrap().name;
            println!("{name} @ {timestamp_ns:?}: {values:?}");
        }
        DecodedFrame::StringPool(entries) => println!("{} pool entries", entries.len()),
        DecodedFrame::SymbolTable(entries) => println!("{} symbols", entries.len()),
    }
}
```

The `#[traceevent(timestamp)]` attribute marks a `u64` field as the event's timestamp. It is encoded as a u24 nanosecond delta in the event header (not as a regular field), giving nanosecond precision with no accumulation error. The encoder automatically emits `TimestampReset` frames when the delta exceeds ~16.7 ms.

Integer fields use fixed-width little-endian encoding (`u8`, `u16`, `u32`) or LEB128 (`u64`). The derive macro handles the mapping automatically.

### Manual schema registration

For event types whose fields are determined at runtime (e.g., user-defined metrics, kernel tracepoints), register schemas by name:

```rust
use dial9_trace_format::encoder::{Encoder, Schema};
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};

let mut enc = Encoder::new();

// Fields determined at runtime (e.g., from a config file)
let schema = enc.register_schema("CustomMetric", vec![
    FieldDef { name: "name".into(), field_type: FieldType::String },
    FieldDef { name: "value".into(), field_type: FieldType::Varint },
]).unwrap();

// First value is always the timestamp (encoded in the event header)
enc.write_event(&schema, &[
    FieldValue::Varint(1_000_000),       // timestamp_ns
    FieldValue::String("request_count".into()),
    FieldValue::Varint(42),
]).unwrap();

// Schemas are portable — pass the same handle to a different encoder
let mut enc2 = Encoder::new();
enc2.write_event(&schema, &[
    FieldValue::Varint(2_000_000),
    FieldValue::String("error_count".into()),
    FieldValue::Varint(3),
]).unwrap();
```

### String interning

Frequently-repeated strings can be interned to avoid encoding them multiple times:

```rust
use dial9_trace_format::encoder::Encoder;

let mut enc = Encoder::new();
let id = enc.intern_string("my_function");
// Use `id` in InternedString fields of subsequent events
```

### Symbol table

For CPU profile stack frame symbolization, attach a symbol table mapping address ranges to symbol names:

```rust
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::codec::SymbolEntry;

let mut enc = Encoder::new();
let name_id = enc.intern_string("my_function");
enc.write_symbol_table(&[
    SymbolEntry { base_addr: 0x1000, size: 256, symbol_id: name_id },
]);
```

### JavaScript reader

A decode-only JS reader is at [`js/decode.js`](js/decode.js):

```js
const { TraceDecoder } = require('./js/decode.js');
const fs = require('fs');

const dec = new TraceDecoder(fs.readFileSync('trace.bin'));
dec.decodeHeader();
for (const frame of dec.decodeAll()) {
    console.log(frame);
}
```

## Field types

| Rust type | Wire type | Notes |
|-----------|-----------|-------|
| `u8`, `u16`, `u32` | Fixed | 1, 2, or 4 bytes LE |
| `u64` | Varint | LEB128, 1–10 bytes |
| `i64` | I64 | 8 bytes LE |
| `f64` | F64 | 8 bytes LE |
| `bool` | Bool | 1 byte |
| `String` | String | u32 length + UTF-8 |
| `Vec<u8>` | Bytes | u32 length + raw |
| `StackFrames` | StackFrames | u32 count + u64 LE addresses |
| `Vec<(String, String)>` | StringMap | u32 count + key/value pairs |

`PooledString` (u32 pool ID) is available via manual schema registration.
