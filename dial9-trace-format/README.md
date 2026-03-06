# dial9-trace-format

A self-describing binary trace format. Schemas are embedded in the stream so readers don't need out-of-band type definitions. Designed for tokio runtime telemetry but usable for any structured event stream.

See [SPEC.md](SPEC.md) for the wire format specification.

## Usage

### Derive macro

For event types known at compile time, use `#[derive(TraceEvent)]`:

```rust
use dial9_trace_format::{TraceEvent, StackFrames};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::decoder::{Decoder, DecodedFrame};

#[derive(TraceEvent)]
struct PollStart {
    timestamp_ns: u64,
    worker_id: u64,
    task_id: u64,
}

#[derive(TraceEvent)]
struct CpuSample {
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
        DecodedFrame::Event { type_id, values } => {
            let name = &dec.registry().get(type_id).unwrap().name;
            println!("{name}: {values:?}");
        }
        DecodedFrame::StringPool(entries) => println!("{} pool entries", entries.len()),
        DecodedFrame::SymbolTable(entries) => println!("{} symbols", entries.len()),
    }
}
```

Integer fields (`u8`, `u16`, `u32`, `u64`) are encoded as LEB128 varints. `StackFrames` fields are delta-encoded. The derive macro handles the mapping automatically.

### Manual schema registration

For event types not known at compile time, register schemas with a marker type:

```rust
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};

struct HttpRequest; // marker type

let mut enc = Encoder::new();
enc.register_schema_for::<HttpRequest>("HttpRequest", vec![
    FieldDef { name: "timestamp_ns".into(), field_type: FieldType::Varint },
    FieldDef { name: "method".into(), field_type: FieldType::String },
    FieldDef { name: "status".into(), field_type: FieldType::Varint },
]);

enc.write_event_for::<HttpRequest>(&[
    FieldValue::Varint(1_000_000),
    FieldValue::String(b"GET".to_vec()),
    FieldValue::Varint(200),
]);
```

### String pool and symbol table

```rust
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::codec::SymbolEntry;

let mut enc = Encoder::new();
// Intern strings for deduplication (returns a pool ID)
let id = enc.intern_string("my_function");

// Attach symbol table for stack frame symbolization
enc.write_symbol_table(&[
    SymbolEntry { base_addr: 0x1000, size: 256, symbol_id: id },
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
// dec.stringPool has pool_id -> string mappings
```

Or from the command line:

```
node js/decode.js trace.bin
```

## Field types

| Rust type | Wire type | Notes |
|-----------|-----------|-------|
| `u8`, `u16`, `u32`, `u64` | Varint | LEB128, 1–10 bytes |
| `i64` | I64 | 8 bytes LE |
| `f64` | F64 | 8 bytes LE |
| `bool` | Bool | 1 byte |
| `String` | String | u32 length + UTF-8 |
| `Vec<u8>` | Bytes | u32 length + raw |
| `StackFrames` | StackFrames | Delta-encoded signed LEB128 |

`PooledString` (u32 pool ID) and `U64Array` (u32 count + 8 bytes each) are available via manual schema registration.

## Planned: metrique distribution support

We plan to support [metrique](https://docs.rs/metrique) metric values natively. A distribution carries multiple numeric observations, a unit, and a set of dimensions — matching the [`ValueWriter::metric()`](https://docs.rs/metrique/latest/metrique/writer/trait.ValueWriter.html#tymethod.metric) interface. This will require a schema implementation in metrique itself. The goal is to be able to embed metric distributions (e.g. latency histograms with unit and dimension metadata) directly in the trace stream alongside scheduling and profiling events.
