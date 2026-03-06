# dial9-trace-format Binary Specification

Version: 1

## Overview

A self-describing binary trace format. The stream is a sequence of frames preceded by a header. Schema frames describe event layouts; event frames carry data whose structure is defined by a previously-seen schema. String pool and symbol table frames provide auxiliary lookup data.

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

Unknown tags **must** cause the decoder to stop (the stream cannot be advanced without knowing the frame size).

### Schema Frame (`0x01`)

Defines the layout of an event type.

| Field | Type | Description |
|-------|------|-------------|
| tag | u8 | `0x01` |
| type_id | u16 | Unique event type identifier |
| name_len | u16 | Length of name in bytes |
| name | [u8; name_len] | UTF-8 event type name |
| field_count | u16 | Number of fields |
| fields | [FieldDef; field_count] | Field definitions (see below) |

Each **FieldDef**:

| Field | Type | Description |
|-------|------|-------------|
| name_len | u16 | Length of field name in bytes |
| name | [u8; name_len] | UTF-8 field name |
| field_type | u8 | Field type tag (see Field Types) |

A `type_id` **must not** be registered more than once in a stream.

### Event Frame (`0x02`)

Carries one event whose layout is defined by a previously-registered schema.

| Field | Type | Description |
|-------|------|-------------|
| tag | u8 | `0x02` |
| type_id | u16 | References a schema's `type_id` |
| values | ... | Field values, encoded in schema field order |

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

## Field Types

| Tag | Name | Wire Encoding | Size |
|-----|------|---------------|------|
| 0 | U64 | 8-byte little-endian unsigned | 8 |
| 1 | I64 | 8-byte little-endian signed | 8 |
| 2 | F64 | 8-byte IEEE 754 double, little-endian | 8 |
| 3 | Bool | 1 byte (`0x00` = false, nonzero = true) | 1 |
| 4 | String | u32 length prefix + UTF-8 bytes | 4 + len |
| 5 | Bytes | u32 length prefix + raw bytes | 4 + len |
| 6 | U64Array | u32 count + count × 8-byte LE u64 | 4 + count×8 |
| 7 | PooledString | u32 pool ID | 4 |
| 8 | StackFrames | u32 count + count × signed LEB128 deltas | variable |
| 9 | Varint | Unsigned LEB128 | 1–10 |
| 10 | StringMap | u32 count + count × (u32 key_len + key bytes + u32 val_len + val bytes) | variable |

### StackFrames Encoding

Stack frame addresses are delta-encoded to exploit spatial locality:

1. Write `count` as u32 (number of addresses).
2. For each address (in order), compute `delta = (address as i64) - prev` where `prev` starts at 0.
3. Encode each delta as **signed LEB128**.

Decoding reverses this: accumulate deltas starting from 0 to reconstruct absolute addresses.

### StringMap Encoding

A string map carries an ordered list of key-value pairs (both UTF-8 strings):

1. Write `count` as u32 (number of pairs).
2. For each pair, write `key_len` as u32, then key bytes, then `val_len` as u32, then value bytes.

### LEB128

**Unsigned LEB128**: Encode 7 bits per byte, MSB is continuation bit. A `u64` requires at most 10 bytes.

**Signed LEB128**: Same structure, but sign-extended on decode. If the final byte has bit 6 set and `shift < 64`, the result is sign-extended with ones.

## Limits

| Item | Limit | Notes |
|------|-------|-------|
| type_id | 0–65535 | u16 |
| field_count per schema | 0–65535 | u16 |
| field/event name length | 0–65535 bytes | u16 length prefix |
| string/bytes field length | 0–4,294,967,295 bytes | u32 length prefix |
| U64Array count | 0–4,294,967,295 | u32 count |
| StackFrames count | 0–4,294,967,295 | u32 count |
| string pool entry count | 0–4,294,967,295 per frame | u32 count |
| symbol table entry count | 0–4,294,967,295 per frame | u32 count |
| pool_id | 0–4,294,967,295 | u32 |
| Varint | 0–2^64-1 | unsigned LEB128 |
