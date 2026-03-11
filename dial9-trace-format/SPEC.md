# dial9-trace-format Binary Specification

Version: 1

## Overview

A self-describing binary trace format. The stream is a sequence of frames preceded by a header. Schema frames describe event layouts; event frames carry data whose structure is defined by a previously-seen schema. String pool, symbol table, and timestamp reset frames provide auxiliary data.

All multi-byte integers are **little-endian** unless stated otherwise. Variable-length integers use **LEB128** encoding.

## Stream Layout

```
Header | Frame | Frame | Frame | ...
```

A valid stream starts with exactly one header, followed by zero or more frames. Frames may appear in any order, with one constraint: a schema frame for a given `type_id` **must** appear before any event frame that references that `type_id`.

## Header

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 4 | Magic bytes: `0x54 0x52 0x43 0x00` (`TRC\0`) |
| 4 | 1 | Version (`0x01`) |

Total: **5 bytes**.

A decoder **must** reject streams whose magic bytes do not match or whose version is unsupported.

## Frames

Every frame begins with a 1-byte tag:

| Tag | Frame Type |
|-----|------------|
| `0x01` | Schema |
| `0x02` | Event |
| `0x03` | String Pool |
| `0x04` | Symbol Table |
| `0x05` | Timestamp Reset |

Unknown tags **must** cause the decoder to stop (the stream cannot be advanced without knowing the frame size).

Frames may appear in any order, with one constraint: a schema frame for a given `type_id` **must** appear before any event frame that references that `type_id`.

### Schema Frame (`0x01`)

Defines the layout of an event type.

| Field | Type | Description |
|-------|------|-------------|
| tag | u8 | `0x01` |
| type_id | u16 | Unique event type identifier |
| name_len | u16 | Length of name in bytes |
| name | [u8; name_len] | UTF-8 event type name |
| has_timestamp | u8 | `1` if events of this type carry a packed timestamp, `0` otherwise |
| field_count | u16 | Number of fields |
| fields | [FieldDef; field_count] | Field definitions (see below) |

Each **FieldDef**:

| Field | Type | Description |
|-------|------|-------------|
| name_len | u16 | Length of field name in bytes |
| name | [u8; name_len] | UTF-8 field name |
| field_type | u8 | Field type tag (see Field Types) |

A `type_id` **must not** be registered more than once in a stream with a different schema. Re-registering the same `type_id` with an identical schema is permitted (idempotent) and decoders **must** accept it.

The `has_timestamp` flag indicates whether events of this type include a packed nanosecond timestamp in the event frame header. When set, the timestamp is encoded in the event header (see Event Frame) and is **not** included in the field list. The schema's `field_count` and `fields` describe only the non-timestamp payload fields.

### Event Frame (`0x02`)

Carries one event whose layout is defined by a previously-registered schema.

**Without timestamp** (`has_timestamp = 0`):

| Field | Type | Description |
|-------|------|-------------|
| tag | u8 | `0x02` |
| type_id | u16 | References a schema's `type_id` |
| values | ... | Field values, encoded in schema field order |

**With timestamp** (`has_timestamp = 1`):

| Field | Type | Description |
|-------|------|-------------|
| tag | u8 | `0x02` |
| type_id | u16 | References a schema's `type_id` |
| timestamp_delta_ns | u24 | Nanosecond delta from the current timestamp base (3 bytes LE) |
| values | ... | Field values, encoded in schema field order |

The `timestamp_delta_ns` is a 24-bit unsigned integer (0–16,777,215) representing nanoseconds elapsed since the current timestamp base. This gives ~16.7 ms of range per reset. The encoder **must** emit a Timestamp Reset frame before any event whose delta would exceed 16,777,215 ns or whose timestamp is earlier than the current base.

Each event's absolute timestamp is computed as `base + delta_ns`. After decoding a timestamped event, the decoder **must** set `timestamp_base_ns = base + delta_ns` (i.e., advance the base to the event's absolute timestamp). This keeps inter-event deltas small, which is critical for compression.

The decoder **must** know the schema for `type_id` to determine how many fields to read and their types. If the schema is unknown, decoding **must** fail.

### String Pool Frame (`0x03`)

Provides string data that can be referenced by `PooledString` fields.

| Field | Type | Description |
|-------|------|-------------|
| tag | u8 | `0x03` |
| count | u32 | Number of entries |
| entries | [PoolEntry; count] | Pool entries (see below) |

Each **PoolEntry**:

| Field | Type | Description |
|-------|------|-------------|
| pool_id | u32 | Identifier referenced by `PooledString` values |
| data_len | u32 | Length of data in bytes |
| data | [u8; data_len] | UTF-8 string data |

Multiple string pool frames may appear in a stream. A `pool_id` should be defined before it is referenced, but a decoder may choose to resolve references lazily.

### Symbol Table Frame (`0x04`)

Maps address ranges to symbol names (for stack frame symbolization).

| Field | Type | Description |
|-------|------|-------------|
| tag | u8 | `0x04` |
| count | u32 | Number of entries |
| entries | [SymbolEntry; count] | Symbol entries (see below) |

Each **SymbolEntry**:

| Field | Type | Description |
|-------|------|-------------|
| base_addr | u64 | Start address |
| size | u32 | Size of the address range in bytes |
| symbol_id | u32 | Pool ID of the symbol name (references string pool) |

### Timestamp Reset Frame (`0x05`)

Resets the running timestamp base used for packed event timestamps. The encoder emits this frame when the nanosecond delta between the current base and the next event's timestamp exceeds what a u24 can represent (16,777,215 ns ≈ 16.7 ms), or when the next event's timestamp is earlier than the current base.

| Field | Type | Description |
|-------|------|-------------|
| tag | u8 | `0x05` |
| timestamp_ns | u64 | Absolute timestamp in nanoseconds |

Total: **9 bytes**.

After decoding this frame, the decoder sets `timestamp_base_ns = timestamp_ns`. The next event's `timestamp_delta_ns` is relative to this new base.

## Field Types

| Tag | Name | Wire Encoding | Size |
|-----|------|---------------|------|
| 1 | I64 | 8-byte little-endian signed | 8 |
| 2 | F64 | 8-byte IEEE 754 double, little-endian | 8 |
| 3 | Bool | 1 byte (`0x00` = false, nonzero = true) | 1 |
| 4 | String | u32 length prefix + UTF-8 bytes | 4 + len |
| 5 | Bytes | u32 length prefix + raw bytes | 4 + len |
| 7 | PooledString | u32 pool ID | 4 |
| 8 | StackFrames | u32 count + count × u64 LE addresses | 4 + 8×count |
| 9 | Varint | Unsigned LEB128 | 1–10 |
| 10 | StringMap | u32 count + count × (u32 key_len + key bytes + u32 val_len + val bytes) | variable |
| 11 | U8 | 1-byte unsigned | 1 |
| 12 | U16 | 2-byte little-endian unsigned | 2 |
| 13 | U32 | 4-byte little-endian unsigned | 4 |

### Timestamp Encoding

Events with timestamps use the packed header encoding:

1. The schema declares `has_timestamp = 1`.
2. The encoder maintains a `timestamp_base_ns` (initially 0).
3. For each event with a timestamp:
   a. Compute `delta_ns = timestamp_ns - timestamp_base_ns`.
   b. If `delta_ns > 16_777_215` (u24 max) or `timestamp_ns < timestamp_base_ns`, emit a **Timestamp Reset** frame with `timestamp_ns`, set `timestamp_base_ns = timestamp_ns`, and set `delta_ns = 0`.
   c. Write the 3-byte `delta_ns` as u24 LE in the event frame header.
   d. Set `timestamp_base_ns = timestamp_ns` (advance the base to this event's timestamp).
4. Decoding: `timestamp_ns = timestamp_base_ns + delta_ns`, then set `timestamp_base_ns = timestamp_ns`.

The base advances after every timestamped event so that deltas represent inter-event gaps rather than offsets from a distant base. This keeps deltas small and repetitive, which compresses well.

### StackFrames Encoding

Stack frame addresses are stored as raw little-endian u64 values:

1. Write `count` as u32 (number of addresses).
2. For each address (in order), write the address as **u64 LE** (8 bytes).

### StringMap Encoding

A string map carries an ordered list of key-value pairs (both UTF-8 strings):

1. Write `count` as u32 (number of pairs).
2. For each pair, write `key_len` as u32, then key bytes, then `val_len` as u32, then value bytes.

### LEB128

**LEB128 (Little Endian Base 128)**: Variable-length integer encoding. Each byte encodes 7 bits of the value; the MSB is a continuation bit. A `u64` requires at most 10 bytes.

## Limits

| Item | Limit | Notes |
|------|-------|-------|
| type_id | 0–65535 | u16 |
| field_count per schema | 0–65535 | u16 |
| field/event name length | 0–65535 bytes | u16 length prefix |
| string/bytes field length | 0–4,294,967,295 bytes | u32 length prefix |
| StackFrames count | 0–4,294,967,295 | u32 count |
| string pool entry count | 0–4,294,967,295 per frame | u32 count |
| symbol table entry count | 0–4,294,967,295 per frame | u32 count |
| pool_id | 0–4,294,967,295 | u32 |
| Varint | 0–2^64-1 | unsigned LEB128 |
| Timestamp delta | 0–16,777,215 ns | u24; overflow triggers Timestamp Reset frame |
