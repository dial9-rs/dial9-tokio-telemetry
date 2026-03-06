# dial9-trace-format Progress

## Status: COMPLETE ✅

All 61 tests pass (54 unit + 2 round-trip integration + 5 derive macro).

## Crates
- `dial9-trace-format` — main crate with format types, encoder, decoder
- `dial9-trace-format-derive` — proc macro crate with `#[derive(TraceEvent)]`

## Completed
- [x] Crate skeleton, added to workspace
- [x] Field types enum (U64, I64, F64, Bool, String, Bytes, U64Array, PooledString, StackFrames, Varint)
- [x] Field value wire encoding/decoding
- [x] LEB128 signed + unsigned encoding/decoding
- [x] Schema types (FieldDef, SchemaEntry) and SchemaRegistry
- [x] Header encoding/decoding (magic `TRC\0` + version byte)
- [x] Schema registration frame (tag 0x01)
- [x] Event frame (tag 0x02) — no payload_len, fields decoded from schema
- [x] String pool frame (tag 0x03)
- [x] Symbol table frame (tag 0x04)
- [x] Encoder (high-level API: header, register schema, write event, intern strings, symbol table)
- [x] Decoder (streaming frame-by-frame with schema registry rebuild and string pool)
- [x] SchemaBuilder runtime API for registering schemas
- [x] Round-trip integration tests (multi-schema, all field types, string pool, stack frames, symbol table)
- [x] `#[derive(TraceEvent)]` proc macro (maps u8/u16/u32/u64→Varint, i64, f64, bool, String, Vec<u8>, Vec<u64>)
- [x] Derive macro tests (event_name, field_defs, to_values, full round-trip, small int types)
- [x] Varint field type (unsigned LEB128) for compact integer encoding
- [x] Removed payload_len from event frames (schema provides field types)
- [x] Example: tokio_telemetry_events showing all event types with compactness comparison

## Compactness Results
- PollEnd: 7 bytes (was 23 with U64, vs 6 in old hand-rolled format)
- Total example trace: 676 bytes (was 845 with U64)
- Worker ID (0-16): 1 byte via varint (was 8 bytes as U64)
- Timestamp (~1M ns): 3 bytes via varint (was 8 bytes as U64)

## Module Structure
```
dial9-trace-format/src/
├── lib.rs        — TraceEvent trait, re-exports derive macro
├── types.rs      — FieldType (10 variants), FieldValue with encode/decode
├── leb128.rs     — signed + unsigned LEB128 encode/decode
├── schema.rs     — FieldDef, SchemaEntry, SchemaRegistry, SchemaBuilder
├── codec.rs      — frame-level encode/decode (header, schema, event, pool, symbols)
├── encoder.rs    — high-level Encoder API
└── decoder.rs    — streaming Decoder with registry rebuild
```
