// Field types and value encoding

/// Wire type tags for field types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FieldType {
    U64 = 0,
    I64 = 1,
    F64 = 2,
    Bool = 3,
    String = 4,
    Bytes = 5,
    U64Array = 6,
    PooledString = 7,
    StackFrames = 8,
    Varint = 9,
    StringMap = 10,
}

/// Newtype for stack frame addresses (leaf-first). Delta-encoded on the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct StackFrames(pub Vec<u64>);

/// An interned string reference (pool ID). Created by [`Encoder::intern_string`].
/// On the wire this is a `PooledString` (u32 LE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InternedString(pub(crate) u32);

impl InternedString {
    /// Returns the raw pool ID for wire encoding.
    pub fn pool_id(self) -> u32 { self.0 }
}
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    String(Vec<u8>),
    Bytes(Vec<u8>),
    U64Array(Vec<u64>),
    PooledString(u32),
    StackFrames(Vec<u64>),
    Varint(u64),
    StringMap(Vec<(Vec<u8>, Vec<u8>)>),
}

impl FieldType {
    pub fn from_tag(tag: u8) -> Option<FieldType> {
        match tag {
            0 => Some(FieldType::U64),
            1 => Some(FieldType::I64),
            2 => Some(FieldType::F64),
            3 => Some(FieldType::Bool),
            4 => Some(FieldType::String),
            5 => Some(FieldType::Bytes),
            6 => Some(FieldType::U64Array),
            7 => Some(FieldType::PooledString),
            8 => Some(FieldType::StackFrames),
            9 => Some(FieldType::Varint),
            10 => Some(FieldType::StringMap),
            _ => None,
        }
    }
}

impl FieldValue {
    /// Encode this value into the buffer.
    pub fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            FieldValue::U64(v) => buf.extend_from_slice(&v.to_le_bytes()),
            FieldValue::I64(v) => buf.extend_from_slice(&v.to_le_bytes()),
            FieldValue::F64(v) => buf.extend_from_slice(&v.to_le_bytes()),
            FieldValue::Bool(v) => buf.push(if *v { 1 } else { 0 }),
            FieldValue::String(v) | FieldValue::Bytes(v) => {
                buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
                buf.extend_from_slice(v);
            }
            FieldValue::U64Array(v) => {
                buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for val in v {
                    buf.extend_from_slice(&val.to_le_bytes());
                }
            }
            FieldValue::PooledString(id) => buf.extend_from_slice(&id.to_le_bytes()),
            FieldValue::Varint(v) => crate::leb128::encode_unsigned(*v, buf),
            FieldValue::StackFrames(addrs) => {
                buf.extend_from_slice(&(addrs.len() as u32).to_le_bytes());
                let mut prev = 0i64;
                for &addr in addrs {
                    let signed = addr as i64;
                    let delta = signed.wrapping_sub(prev);
                    crate::leb128::encode_signed(delta, buf);
                    prev = signed;
                }
            }
            FieldValue::StringMap(pairs) => {
                buf.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
                for (k, v) in pairs {
                    buf.extend_from_slice(&(k.len() as u32).to_le_bytes());
                    buf.extend_from_slice(k);
                    buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    buf.extend_from_slice(v);
                }
            }
        }
    }

    /// Decode a value of the given type from the buffer at offset. Returns (value, bytes_consumed).
    pub fn decode(field_type: FieldType, data: &[u8], offset: usize) -> Option<(FieldValue, usize)> {
        let d = &data[offset..];
        match field_type {
            FieldType::U64 => {
                let v = u64::from_le_bytes(d.get(..8)?.try_into().ok()?);
                Some((FieldValue::U64(v), 8))
            }
            FieldType::I64 => {
                let v = i64::from_le_bytes(d.get(..8)?.try_into().ok()?);
                Some((FieldValue::I64(v), 8))
            }
            FieldType::F64 => {
                let v = f64::from_le_bytes(d.get(..8)?.try_into().ok()?);
                Some((FieldValue::F64(v), 8))
            }
            FieldType::Bool => {
                Some((FieldValue::Bool(*d.first()? != 0), 1))
            }
            FieldType::String | FieldType::Bytes => {
                let len = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let bytes = d.get(4..4 + len)?.to_vec();
                let val = if field_type == FieldType::String {
                    FieldValue::String(bytes)
                } else {
                    FieldValue::Bytes(bytes)
                };
                Some((val, 4 + len))
            }
            FieldType::U64Array => {
                let count = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let mut vals = Vec::with_capacity(count.min(d.len() / 8));
                for i in 0..count {
                    let start = 4 + i * 8;
                    let v = u64::from_le_bytes(d.get(start..start + 8)?.try_into().ok()?);
                    vals.push(v);
                }
                Some((FieldValue::U64Array(vals), 4 + count * 8))
            }
            FieldType::PooledString => {
                let id = u32::from_le_bytes(d.get(..4)?.try_into().ok()?);
                Some((FieldValue::PooledString(id), 4))
            }
            FieldType::Varint => {
                let (v, consumed) = crate::leb128::decode_unsigned(d)?;
                Some((FieldValue::Varint(v), consumed))
            }
            FieldType::StackFrames => {
                let count = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let mut pos = 4;
                let mut addrs = Vec::with_capacity(count.min(d.len()));
                let mut prev = 0i64;
                for _ in 0..count {
                    let (delta, consumed) = crate::leb128::decode_signed(&d[pos..])?;
                    prev = prev.wrapping_add(delta);
                    addrs.push(prev as u64);
                    pos += consumed;
                }
                Some((FieldValue::StackFrames(addrs), pos))
            }
            FieldType::StringMap => {
                let count = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let mut pos = 4;
                let mut pairs = Vec::with_capacity(count.min(d.len() / 8));
                for _ in 0..count {
                    let klen = u32::from_le_bytes(d.get(pos..pos + 4)?.try_into().ok()?) as usize;
                    pos += 4;
                    let k = d.get(pos..pos + klen)?.to_vec();
                    pos += klen;
                    let vlen = u32::from_le_bytes(d.get(pos..pos + 4)?.try_into().ok()?) as usize;
                    pos += 4;
                    let v = d.get(pos..pos + vlen)?.to_vec();
                    pos += vlen;
                    pairs.push((k, v));
                }
                Some((FieldValue::StringMap(pairs), pos))
            }
        }
    }
}

/// Zero-copy field value that borrows from the input buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValueRef<'a> {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    String(&'a [u8]),
    Bytes(&'a [u8]),
    U64Array(&'a [u8]),
    PooledString(u32),
    /// Raw delta-encoded stack frame bytes. Use [`StackFrameIter`] to decode.
    StackFrames(&'a [u8], u32),
    Varint(u64),
    StringMap(&'a [u8], u32),
}

impl<'a> FieldValueRef<'a> {
    /// Decode a value of the given type, borrowing from `data` at `offset`.
    /// Returns (value, bytes_consumed).
    pub fn decode(field_type: FieldType, data: &'a [u8], offset: usize) -> Option<(Self, usize)> {
        let d = &data[offset..];
        match field_type {
            FieldType::U64 => {
                let v = u64::from_le_bytes(d.get(..8)?.try_into().ok()?);
                Some((FieldValueRef::U64(v), 8))
            }
            FieldType::I64 => {
                let v = i64::from_le_bytes(d.get(..8)?.try_into().ok()?);
                Some((FieldValueRef::I64(v), 8))
            }
            FieldType::F64 => {
                let v = f64::from_le_bytes(d.get(..8)?.try_into().ok()?);
                Some((FieldValueRef::F64(v), 8))
            }
            FieldType::Bool => {
                Some((FieldValueRef::Bool(*d.first()? != 0), 1))
            }
            FieldType::String | FieldType::Bytes => {
                let len = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let bytes = d.get(4..4 + len)?;
                let val = if field_type == FieldType::String {
                    FieldValueRef::String(bytes)
                } else {
                    FieldValueRef::Bytes(bytes)
                };
                Some((val, 4 + len))
            }
            FieldType::U64Array => {
                let count = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let byte_len = 4 + count * 8;
                let _ = d.get(..byte_len)?;
                Some((FieldValueRef::U64Array(&d[..byte_len]), byte_len))
            }
            FieldType::PooledString => {
                let id = u32::from_le_bytes(d.get(..4)?.try_into().ok()?);
                Some((FieldValueRef::PooledString(id), 4))
            }
            FieldType::Varint => {
                let (v, consumed) = crate::leb128::decode_unsigned(d)?;
                Some((FieldValueRef::Varint(v), consumed))
            }
            FieldType::StackFrames => {
                let count = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let mut pos = 4;
                for _ in 0..count {
                    let (_, consumed) = crate::leb128::decode_signed(&d[pos..])?;
                    pos += consumed;
                }
                Some((FieldValueRef::StackFrames(&d[4..pos], count as u32), pos))
            }
            FieldType::StringMap => {
                let count = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let mut pos = 4;
                for _ in 0..count {
                    let klen = u32::from_le_bytes(d.get(pos..pos + 4)?.try_into().ok()?) as usize;
                    pos += 4 + klen;
                    let vlen = u32::from_le_bytes(d.get(pos..pos + 4)?.try_into().ok()?) as usize;
                    pos += 4 + vlen;
                }
                Some((FieldValueRef::StringMap(&d[4..pos], count as u32), pos))
            }
        }
    }
}

/// Iterator over delta-encoded stack frame addresses.
pub struct StackFrameIter<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u32,
    prev: i64,
}

impl<'a> StackFrameIter<'a> {
    pub fn new(data: &'a [u8], count: u32) -> Self {
        Self { data, pos: 0, remaining: count, prev: 0 }
    }
}

impl Iterator for StackFrameIter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        if self.remaining == 0 { return None; }
        let (delta, consumed) = crate::leb128::decode_signed(&self.data[self.pos..])?;
        self.prev = self.prev.wrapping_add(delta);
        self.pos += consumed;
        self.remaining -= 1;
        Some(self.prev as u64)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining as usize, Some(self.remaining as usize))
    }
}

impl ExactSizeIterator for StackFrameIter<'_> {}

/// Iterator over zero-copy string map key-value pairs.
pub struct StringMapIter<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u32,
}

impl<'a> StringMapIter<'a> {
    pub fn new(data: &'a [u8], count: u32) -> Self {
        Self { data, pos: 0, remaining: count }
    }
}

impl<'a> Iterator for StringMapIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 { return None; }
        let klen = u32::from_le_bytes(self.data.get(self.pos..self.pos + 4)?.try_into().ok()?) as usize;
        self.pos += 4;
        let k = self.data.get(self.pos..self.pos + klen)?;
        self.pos += klen;
        let vlen = u32::from_le_bytes(self.data.get(self.pos..self.pos + 4)?.try_into().ok()?) as usize;
        self.pos += 4;
        let v = self.data.get(self.pos..self.pos + vlen)?;
        self.pos += vlen;
        self.remaining -= 1;
        Some((k, v))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining as usize, Some(self.remaining as usize))
    }
}

impl ExactSizeIterator for StringMapIter<'_> {}

/// Trait for types that can be encoded as a trace field.
/// Implement this to add new field types without modifying the derive macro.
pub trait TraceField {
    fn field_type() -> FieldType;
    fn encode_field(&self, buf: &mut Vec<u8>);
}

impl TraceField for u8 {
    fn field_type() -> FieldType { FieldType::Varint }
    fn encode_field(&self, buf: &mut Vec<u8>) { crate::leb128::encode_unsigned(*self as u64, buf); }
}

impl TraceField for u16 {
    fn field_type() -> FieldType { FieldType::Varint }
    fn encode_field(&self, buf: &mut Vec<u8>) { crate::leb128::encode_unsigned(*self as u64, buf); }
}

impl TraceField for u32 {
    fn field_type() -> FieldType { FieldType::Varint }
    fn encode_field(&self, buf: &mut Vec<u8>) { crate::leb128::encode_unsigned(*self as u64, buf); }
}

impl TraceField for u64 {
    fn field_type() -> FieldType { FieldType::Varint }
    fn encode_field(&self, buf: &mut Vec<u8>) { crate::leb128::encode_unsigned(*self, buf); }
}

impl TraceField for i64 {
    fn field_type() -> FieldType { FieldType::I64 }
    fn encode_field(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
}

impl TraceField for f64 {
    fn field_type() -> FieldType { FieldType::F64 }
    fn encode_field(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
}

impl TraceField for bool {
    fn field_type() -> FieldType { FieldType::Bool }
    fn encode_field(&self, buf: &mut Vec<u8>) { buf.push(if *self { 1 } else { 0 }); }
}

impl TraceField for String {
    fn field_type() -> FieldType { FieldType::String }
    fn encode_field(&self, buf: &mut Vec<u8>) {
        let bytes = self.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
    }
}

impl TraceField for Vec<u8> {
    fn field_type() -> FieldType { FieldType::Bytes }
    fn encode_field(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.len() as u32).to_le_bytes());
        buf.extend_from_slice(self);
    }
}

impl TraceField for Vec<u64> {
    fn field_type() -> FieldType { FieldType::U64Array }
    fn encode_field(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.len() as u32).to_le_bytes());
        for val in self { buf.extend_from_slice(&val.to_le_bytes()); }
    }
}

impl TraceField for StackFrames {
    fn field_type() -> FieldType { FieldType::StackFrames }
    fn encode_field(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        let mut prev = 0i64;
        for &addr in &self.0 {
            let signed = addr as i64;
            let delta = signed.wrapping_sub(prev);
            crate::leb128::encode_signed(delta, buf);
            prev = signed;
        }
    }
}

impl TraceField for InternedString {
    fn field_type() -> FieldType { FieldType::PooledString }
    fn encode_field(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.0.to_le_bytes()); }
}

impl TraceField for Vec<(String, String)> {
    fn field_type() -> FieldType { FieldType::StringMap }
    fn encode_field(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.len() as u32).to_le_bytes());
        for (k, v) in self {
            let kb = k.as_bytes();
            buf.extend_from_slice(&(kb.len() as u32).to_le_bytes());
            buf.extend_from_slice(kb);
            let vb = v.as_bytes();
            buf.extend_from_slice(&(vb.len() as u32).to_le_bytes());
            buf.extend_from_slice(vb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_type_round_trip() {
        for tag in 0..=10u8 {
            let ft = FieldType::from_tag(tag).unwrap();
            assert_eq!(ft as u8, tag);
        }
        assert!(FieldType::from_tag(11).is_none());
    }


    #[test]
    fn encode_decode_u64() {
        let val = FieldValue::U64(0xDEAD_BEEF_CAFE_BABE);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 8);
        let (decoded, consumed) = FieldValue::decode(FieldType::U64, &buf, 0).unwrap();
        assert_eq!(consumed, 8);
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_i64() {
        let val = FieldValue::I64(-123456789);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        let (decoded, _) = FieldValue::decode(FieldType::I64, &buf, 0).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_f64() {
        let val = FieldValue::F64(std::f64::consts::PI);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        let (decoded, _) = FieldValue::decode(FieldType::F64, &buf, 0).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_bool() {
        for b in [true, false] {
            let val = FieldValue::Bool(b);
            let mut buf = Vec::new();
            val.encode(&mut buf);
            assert_eq!(buf.len(), 1);
            let (decoded, consumed) = FieldValue::decode(FieldType::Bool, &buf, 0).unwrap();
            assert_eq!(consumed, 1);
            assert_eq!(decoded, val);
        }
    }

    #[test]
    fn encode_decode_string() {
        let val = FieldValue::String(b"hello world".to_vec());
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 4 + 11);
        let (decoded, consumed) = FieldValue::decode(FieldType::String, &buf, 0).unwrap();
        assert_eq!(consumed, 15);
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_bytes() {
        let val = FieldValue::Bytes(vec![0xff, 0x00, 0xab]);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        let (decoded, _) = FieldValue::decode(FieldType::Bytes, &buf, 0).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_u64_array() {
        let val = FieldValue::U64Array(vec![1, 2, 3, u64::MAX]);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 4 + 4 * 8);
        let (decoded, _) = FieldValue::decode(FieldType::U64Array, &buf, 0).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_pooled_string() {
        let val = FieldValue::PooledString(42);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 4);
        let (decoded, _) = FieldValue::decode(FieldType::PooledString, &buf, 0).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_stack_frames() {
        let addrs = vec![0x5555_5555_1234u64, 0x5555_5555_0a00, 0x5555_5555_0800, 0x5555_5555_0100];
        let val = FieldValue::StackFrames(addrs.clone());
        let mut buf = Vec::new();
        val.encode(&mut buf);
        // Should be much smaller than 4 + 4*8 = 36 bytes
        assert!(buf.len() < 36, "stack frames should be compact, got {} bytes", buf.len());
        let (decoded, _) = FieldValue::decode(FieldType::StackFrames, &buf, 0).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn decode_with_offset() {
        let mut buf = vec![0xAA, 0xBB]; // garbage prefix
        let val = FieldValue::U64(999);
        val.encode(&mut buf);
        let (decoded, _) = FieldValue::decode(FieldType::U64, &buf, 2).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_varint_small() {
        let val = FieldValue::Varint(3);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 1);
        let (decoded, consumed) = FieldValue::decode(FieldType::Varint, &buf, 0).unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_varint_large() {
        let val = FieldValue::Varint(u64::MAX);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 10); // u64::MAX needs 10 LEB128 bytes
        let (decoded, consumed) = FieldValue::decode(FieldType::Varint, &buf, 0).unwrap();
        assert_eq!(consumed, 10);
        assert_eq!(decoded, val);
    }

    #[test]
    fn varint_poll_end_compactness() {
        // PollEnd: timestamp_ns=1_050_000, worker_id=3
        let mut buf = Vec::new();
        FieldValue::Varint(1_050_000).encode(&mut buf);
        FieldValue::Varint(3).encode(&mut buf);
        // timestamp ~3 bytes, worker 1 byte = ~4 bytes for the payload
        assert!(buf.len() <= 4, "PollEnd payload should be <=4 bytes, got {}", buf.len());
    }
}
