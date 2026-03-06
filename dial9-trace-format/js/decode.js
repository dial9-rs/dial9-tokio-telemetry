// dial9-trace-format decoder (read-only)
// See SPEC.md for the binary format specification.

const MAGIC = [0x54, 0x52, 0x43, 0x00];
const TAG_SCHEMA = 0x01;
const TAG_EVENT = 0x02;
const TAG_STRING_POOL = 0x03;
const TAG_SYMBOL_TABLE = 0x04;

const FieldType = {
  U64: 0, I64: 1, F64: 2, Bool: 3, String: 4,
  Bytes: 5, U64Array: 6, PooledString: 7, StackFrames: 8, Varint: 9,
  StringMap: 10,
  StringMap: 10,
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

function decodeSLEB128(view, offset) {
  let result = 0n;
  let shift = 0n;
  let pos = offset;
  let byte;
  while (true) {
    byte = view.getUint8(pos++);
    result |= BigInt(byte & 0x7f) << shift;
    shift += 7n;
    if ((byte & 0x80) === 0) break;
  }
  if (shift < 64n && (byte & 0x40) !== 0) {
    result |= -(1n << shift);
  }
  return [result, pos - offset];
}

function decodeFieldValue(view, offset, fieldType) {
  switch (fieldType) {
    case FieldType.U64: return [view.getBigUint64(offset, true), 8];
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
    case FieldType.U64Array: {
      const count = view.getUint32(offset, true);
      const arr = [];
      for (let i = 0; i < count; i++) arr.push(view.getBigUint64(offset + 4 + i * 8, true).toString());
      return [arr, 4 + count * 8];
    }
    case FieldType.PooledString: return [view.getUint32(offset, true), 4];
    case FieldType.Varint: {
      const [val, consumed] = decodeULEB128(view, offset);
      return [val.toString(), consumed];
    }
    case FieldType.StackFrames: {
      const count = view.getUint32(offset, true);
      let pos = 4;
      let prev = 0n;
      const addrs = [];
      for (let i = 0; i < count; i++) {
        const [delta, consumed] = decodeSLEB128(view, offset + pos);
        prev += delta;
        addrs.push(BigInt.asUintN(64, prev).toString());
        pos += consumed;
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
        pairs[key] = val;
      }
      return [pairs, pos];
    }
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
    const tag = this._view.getUint8(this._pos);
    this._pos++;
    switch (tag) {
      case TAG_SCHEMA: return this._decodeSchema();
      case TAG_EVENT: return this._decodeEvent();
      case TAG_STRING_POOL: return this._decodeStringPool();
      case TAG_SYMBOL_TABLE: return this._decodeSymbolTable();
      default: throw new Error(`Unknown frame tag: 0x${tag.toString(16)}`);
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
    const schema = { typeId, name, fields };
    this.schemas.set(typeId, schema);
    return { type: 'schema', ...schema };
  }

  _decodeEvent() {
    const typeId = this._view.getUint16(this._pos, true); this._pos += 2;
    const schema = this.schemas.get(typeId);
    if (!schema) throw new Error(`Unknown type_id: ${typeId}`);
    const values = {};
    for (const field of schema.fields) {
      const [val, consumed] = decodeFieldValue(this._view, this._pos, field.fieldType);
      values[field.name] = val;
      this._pos += consumed;
    }
    return { type: 'event', typeId, name: schema.name, values };
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

  _decodeSymbolTable() {
    const count = this._view.getUint32(this._pos, true); this._pos += 4;
    const entries = [];
    for (let i = 0; i < count; i++) {
      const baseAddr = this._view.getBigUint64(this._pos, true); this._pos += 8;
      const size = this._view.getUint32(this._pos, true); this._pos += 4;
      const symbolId = this._view.getUint32(this._pos, true); this._pos += 4;
      entries.push({ baseAddr: baseAddr.toString(), size, symbolId });
    }
    return { type: 'symbol_table', entries };
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
  // BigInt-safe JSON serialization
  const json = JSON.stringify({
    version: dec.version,
    frames,
    stringPool: Object.fromEntries(dec.stringPool),
  }, (_, v) => typeof v === 'bigint' ? v.toString() : v, 2);
  console.log(json);
}

module.exports = { TraceDecoder, FieldType };
