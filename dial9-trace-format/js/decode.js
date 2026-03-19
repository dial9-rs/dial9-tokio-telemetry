// dial9-trace-format decoder (read-only)
// See SPEC.md for the binary format specification.

const MAGIC = [0x54, 0x52, 0x43, 0x00];
const TAG_SCHEMA = 0x01;
const TAG_EVENT = 0x02;
const TAG_STRING_POOL = 0x03;
// Tag 0x04 is reserved (formerly SymbolTable, now a schema-based event).
const TAG_TIMESTAMP_RESET = 0x05;

const FieldType = {
  I64: 1, F64: 2, Bool: 3, String: 4,
  Bytes: 5, PooledString: 7, StackFrames: 8, Varint: 9,
  StringMap: 10, U8: 11, U16: 12, U32: 13,
};

function decodeULEB128(view, offset) {
  let result = 0n;
  let shift = 0n;
  let pos = offset;
  while (true) {
    const byte = view.getUint8(pos++);
    result |= BigInt(byte & 0x7f) << shift;
    shift += 7n;
    if ((byte & 0x80) === 0) return [result, pos - offset];
  }
}

function decodeFieldValue(view, offset, fieldType) {
  switch (fieldType) {
    case FieldType.I64: return [view.getBigInt64(offset, true), 8];
    case FieldType.F64: return [view.getFloat64(offset, true), 8];
    case FieldType.Bool: return [view.getUint8(offset) !== 0, 1];
    case FieldType.String:
    case FieldType.Bytes: {
      const len = view.getUint32(offset, true);
      const bytes = new Uint8Array(view.buffer, view.byteOffset + offset + 4, len);
      const val = fieldType === FieldType.String
        ? new TextDecoder().decode(bytes)
        : Array.from(new Uint8Array(bytes));
      return [val, 4 + len];
    }
    case FieldType.Varint: {
      const [val, consumed] = decodeULEB128(view, offset);
      return [val.toString(), consumed];
    }
    case FieldType.PooledString: return [view.getUint32(offset, true), 4];
    case FieldType.StackFrames: {
      const count = view.getUint32(offset, true);
      let pos = 4;
      const addrs = [];
      for (let i = 0; i < count; i++) {
        const lo = view.getUint32(offset + pos, true);
        const hi = view.getUint32(offset + pos + 4, true);
        addrs.push((BigInt(hi) << 32n | BigInt(lo)).toString());
        pos += 8;
      }
      return [addrs, pos];
    }
    case FieldType.StringMap: {
      const count = view.getUint32(offset, true);
      let pos = 4;
      const pairs = {};
      const td = new TextDecoder();
      for (let i = 0; i < count; i++) {
        const kLen = view.getUint32(offset + pos, true); pos += 4;
        const key = td.decode(new Uint8Array(view.buffer, view.byteOffset + offset + pos, kLen)); pos += kLen;
        const vLen = view.getUint32(offset + pos, true); pos += 4;
        const val = td.decode(new Uint8Array(view.buffer, view.byteOffset + offset + pos, vLen)); pos += vLen;
        if (key in pairs) console.warn(`StringMap: duplicate key "${key}", overwriting previous value`);
        pairs[key] = val;
      }
      return [pairs, pos];
    }
    case FieldType.U8: return [view.getUint8(offset), 1];
    case FieldType.U16: return [view.getUint16(offset, true), 2];
    case FieldType.U32: return [view.getUint32(offset, true), 4];
    default: throw new Error(`Unknown field type: ${fieldType}`);
  }
}

class TraceDecoder {
  constructor(buffer) {
    const ab = buffer instanceof ArrayBuffer ? buffer : buffer.buffer;
    const off = buffer.byteOffset || 0;
    const len = buffer.byteLength;
    this._view = new DataView(ab, off, len);
    this._pos = 0;
    this.schemas = new Map();
    this.stringPool = new Map();
    this.version = 0;
    this._timestampBaseNs = 0n;
  }

  decodeHeader() {
    for (let i = 0; i < 4; i++) {
      if (this._view.getUint8(this._pos + i) !== MAGIC[i]) return false;
    }
    this.version = this._view.getUint8(this._pos + 4);
    this._pos += 5;
    return true;
  }

  nextFrame() {
    if (this._pos >= this._view.byteLength) return null;
    try {
      const tag = this._view.getUint8(this._pos);
      this._pos++;
      switch (tag) {
        case TAG_SCHEMA: return this._decodeSchema();
        case TAG_EVENT: return this._decodeEvent();
        case TAG_STRING_POOL: return this._decodeStringPool();
        case TAG_TIMESTAMP_RESET: {
          const lo = this._view.getUint32(this._pos, true);
          const hi = this._view.getUint32(this._pos + 4, true);
          this._timestampBaseNs = BigInt(hi) << 32n | BigInt(lo);
          this._pos += 8;
          return this.nextFrame(); // consume silently
        }
        default: throw new Error(`Unknown frame tag: 0x${tag.toString(16)}`);
      }
    } catch (e) {
      if (e instanceof RangeError) {
        // Truncated frame at end of segment; stop gracefully.
        this._pos = this._view.byteLength;
        return null;
      }
      throw e;
    }
  }

  decodeAll() {
    const frames = [];
    let f;
    while ((f = this.nextFrame()) !== null) frames.push(f);
    return frames;
  }

  _decodeSchema() {
    const typeId = this._view.getUint16(this._pos, true); this._pos += 2;
    const nameLen = this._view.getUint16(this._pos, true); this._pos += 2;
    const name = new TextDecoder().decode(
      new Uint8Array(this._view.buffer, this._view.byteOffset + this._pos, nameLen));
    this._pos += nameLen;
    const hasTimestamp = this._view.getUint8(this._pos) !== 0; this._pos += 1;
    const fieldCount = this._view.getUint16(this._pos, true); this._pos += 2;
    const fields = [];
    for (let i = 0; i < fieldCount; i++) {
      const fnLen = this._view.getUint16(this._pos, true); this._pos += 2;
      const fn_ = new TextDecoder().decode(
        new Uint8Array(this._view.buffer, this._view.byteOffset + this._pos, fnLen));
      this._pos += fnLen;
      const ft = this._view.getUint8(this._pos); this._pos++;
      fields.push({ name: fn_, fieldType: ft });
    }
    const schema = { typeId, name, hasTimestamp, fields };
    this.schemas.set(typeId, schema);
    return { type: 'schema', ...schema };
  }

  _decodeEvent() {
    const typeId = this._view.getUint16(this._pos, true); this._pos += 2;
    const schema = this.schemas.get(typeId);
    if (!schema) throw new Error(`Unknown type_id: ${typeId}`);

    let timestampNs = null;
    if (schema.hasTimestamp) {
      const b0 = this._view.getUint8(this._pos);
      const b1 = this._view.getUint8(this._pos + 1);
      const b2 = this._view.getUint8(this._pos + 2);
      const deltaNs = b0 | (b1 << 8) | (b2 << 16);
      this._pos += 3;
      timestampNs = (this._timestampBaseNs + BigInt(deltaNs)).toString();
      this._timestampBaseNs = this._timestampBaseNs + BigInt(deltaNs);
    }

    const values = {};
    for (const field of schema.fields) {
      const [val, consumed] = decodeFieldValue(this._view, this._pos, field.fieldType);
      if (field.fieldType === FieldType.PooledString) {
        values[field.name] = this.stringPool.get(val) ?? `<unresolved pool#${val}>`;
      } else {
        values[field.name] = val;
      }
      this._pos += consumed;
    }
    const result = { type: 'event', typeId, name: schema.name, values };
    if (timestampNs !== null) result.timestamp_ns = timestampNs;
    return result;
  }

  _decodeStringPool() {
    const count = this._view.getUint32(this._pos, true); this._pos += 4;
    const entries = [];
    for (let i = 0; i < count; i++) {
      const poolId = this._view.getUint32(this._pos, true); this._pos += 4;
      const len = this._view.getUint32(this._pos, true); this._pos += 4;
      const data = new TextDecoder().decode(
        new Uint8Array(this._view.buffer, this._view.byteOffset + this._pos, len));
      this._pos += len;
      this.stringPool.set(poolId, data);
      entries.push({ poolId, data });
    }
    return { type: 'string_pool', entries };
  }

}

// --- CLI: decode a file and print JSON ---
if (typeof require !== 'undefined' && require.main === module) {
  const fs = require('fs');
  const file = process.argv[2];
  if (!file) { console.error('Usage: node decode.js <trace-file>'); process.exit(1); }
  const buf = fs.readFileSync(file);
  const dec = new TraceDecoder(buf);
  if (!dec.decodeHeader()) { console.error('Bad header'); process.exit(1); }
  const frames = dec.decodeAll();
  const json = JSON.stringify({
    version: dec.version,
    frames,
    stringPool: Object.fromEntries(dec.stringPool),
  }, (_, v) => typeof v === 'bigint' ? v.toString() : v, 2);
  console.log(json);
}

if (typeof module !== 'undefined') module.exports = { TraceDecoder, FieldType };
