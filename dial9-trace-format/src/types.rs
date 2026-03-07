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
pub struct InternedString(pub u32);
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FieldValue {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    String(Vec<u8>),
    Bytes(Vec<u8>),
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

    /// Decode a value of the given type from the buffer. Returns (value, remaining_slice).
    pub fn decode(field_type: FieldType, data: &[u8]) -> Option<(FieldValue, &[u8])> {
        match field_type {
            FieldType::U64 => {
                let v = u64::from_le_bytes(data.get(..8)?.try_into().ok()?);
                Some((FieldValue::U64(v), &data[8..]))
            }
            FieldType::I64 => {
                let v = i64::from_le_bytes(data.get(..8)?.try_into().ok()?);
                Some((FieldValue::I64(v), &data[8..]))
            }
            FieldType::F64 => {
                let v = f64::from_le_bytes(data.get(..8)?.try_into().ok()?);
                Some((FieldValue::F64(v), &data[8..]))
            }
            FieldType::Bool => {
                Some((FieldValue::Bool(*data.first()? != 0), &data[1..]))
            }
            FieldType::String | FieldType::Bytes => {
                let len = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
                let bytes = data.get(4..4 + len)?.to_vec();
                let val = if field_type == FieldType::String {
                    FieldValue::String(bytes)
                } else {
                    FieldValue::Bytes(bytes)
                };
                Some((val, &data[4 + len..]))
            }
            FieldType::PooledString => {
                let id = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
                Some((FieldValue::PooledString(id), &data[4..]))
            }
            FieldType::Varint => {
                let (v, consumed) = crate::leb128::decode_unsigned(data)?;
                Some((FieldValue::Varint(v), &data[consumed..]))
            }
            FieldType::StackFrames => {
                let count = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
                let mut pos = 4;
                let mut addrs = Vec::with_capacity(count.min(data.len()));
                let mut prev = 0i64;
                for _ in 0..count {
                    let (delta, consumed) = crate::leb128::decode_signed(&data[pos..])?;
                    prev = prev.wrapping_add(delta);
                    addrs.push(prev as u64);
                    pos += consumed;
                }
                Some((FieldValue::StackFrames(addrs), &data[pos..]))
            }
            FieldType::StringMap => {
                let count = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
                let mut pos = 4;
                let mut pairs = Vec::with_capacity(count.min(data.len() / 8));
                for _ in 0..count {
                    let klen = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
                    pos += 4;
                    let k = data.get(pos..pos + klen)?.to_vec();
                    pos += klen;
                    let vlen = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
                    pos += 4;
                    let v = data.get(pos..pos + vlen)?.to_vec();
                    pos += vlen;
                    pairs.push((k, v));
                }
                Some((FieldValue::StringMap(pairs), &data[pos..]))
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
    String(&'a str),
    Bytes(&'a [u8]),
    PooledString(InternedString),
    /// Raw delta-encoded stack frame bytes. Use [`StackFramesRef::iter`] to decode.
    StackFrames(StackFramesRef<'a>),
    Varint(u64),
    StringMap(StringMapRef<'a>),
}

/// Zero-copy wrapper for delta-encoded stack frame data.
#[derive(Debug, Clone, PartialEq)]
pub struct StackFramesRef<'a> {
    data: &'a [u8],
    count: u32,
}

impl<'a> StackFramesRef<'a> {
    pub fn iter(&self) -> StackFrameIter<'a> {
        StackFrameIter::new(self.data, self.count)
    }

    pub fn count(&self) -> u32 { self.count }
}

/// Zero-copy wrapper for string map data.
#[derive(Debug, Clone, PartialEq)]
pub struct StringMapRef<'a> {
    data: &'a [u8],
    count: u32,
}

impl<'a> StringMapRef<'a> {
    pub fn iter(&self) -> StringMapIter<'a> {
        StringMapIter::new(self.data, self.count)
    }

    pub fn count(&self) -> u32 { self.count }
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
            FieldType::String => {
                let len = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let bytes = d.get(4..4 + len)?;
                let s = std::str::from_utf8(bytes).ok()?;
                Some((FieldValueRef::String(s), 4 + len))
            }
            FieldType::Bytes => {
                let len = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let bytes = d.get(4..4 + len)?;
                Some((FieldValueRef::Bytes(bytes), 4 + len))
            }
            FieldType::PooledString => {
                let id = u32::from_le_bytes(d.get(..4)?.try_into().ok()?);
                Some((FieldValueRef::PooledString(InternedString(id)), 4))
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
                Some((FieldValueRef::StackFrames(StackFramesRef { data: &d[4..pos], count: count as u32 }), pos))
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
                Some((FieldValueRef::StringMap(StringMapRef { data: &d[4..pos], count: count as u32 }), pos))
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
    /// The zero-copy decoded form of this field.
    type Ref<'a>;

    fn field_type() -> FieldType;
    fn encode_field(&self, buf: &mut Vec<u8>);
    /// Extract this field's value from a zero-copy FieldValueRef.
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>>;
}

impl TraceField for u8 {
    type Ref<'a> = u8;
    fn field_type() -> FieldType { FieldType::Varint }
    fn encode_field(&self, buf: &mut Vec<u8>) { crate::leb128::encode_unsigned(*self as u64, buf); }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::Varint(v) => Some(*v as u8), _ => None }
    }
}

impl TraceField for u16 {
    type Ref<'a> = u16;
    fn field_type() -> FieldType { FieldType::Varint }
    fn encode_field(&self, buf: &mut Vec<u8>) { crate::leb128::encode_unsigned(*self as u64, buf); }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::Varint(v) => Some(*v as u16), _ => None }
    }
}

impl TraceField for u32 {
    type Ref<'a> = u32;
    fn field_type() -> FieldType { FieldType::Varint }
    fn encode_field(&self, buf: &mut Vec<u8>) { crate::leb128::encode_unsigned(*self as u64, buf); }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::Varint(v) => Some(*v as u32), _ => None }
    }
}

impl TraceField for u64 {
    type Ref<'a> = u64;
    fn field_type() -> FieldType { FieldType::Varint }
    fn encode_field(&self, buf: &mut Vec<u8>) { crate::leb128::encode_unsigned(*self, buf); }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::Varint(v) => Some(*v), _ => None }
    }
}

impl TraceField for i64 {
    type Ref<'a> = i64;
    fn field_type() -> FieldType { FieldType::I64 }
    fn encode_field(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::I64(v) => Some(*v), _ => None }
    }
}

impl TraceField for f64 {
    type Ref<'a> = f64;
    fn field_type() -> FieldType { FieldType::F64 }
    fn encode_field(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::F64(v) => Some(*v), _ => None }
    }
}

impl TraceField for bool {
    type Ref<'a> = bool;
    fn field_type() -> FieldType { FieldType::Bool }
    fn encode_field(&self, buf: &mut Vec<u8>) { buf.push(if *self { 1 } else { 0 }); }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::Bool(v) => Some(*v), _ => None }
    }
}

impl TraceField for String {
    type Ref<'a> = &'a str;
    fn field_type() -> FieldType { FieldType::String }
    fn encode_field(&self, buf: &mut Vec<u8>) {
        let bytes = self.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::String(s) => Some(s), _ => None }
    }
}

impl TraceField for Vec<u8> {
    type Ref<'a> = &'a [u8];
    fn field_type() -> FieldType { FieldType::Bytes }
    fn encode_field(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.len() as u32).to_le_bytes());
        buf.extend_from_slice(self);
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::Bytes(b) => Some(b), _ => None }
    }
}

impl TraceField for StackFrames {
    type Ref<'a> = StackFramesRef<'a>;
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
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::StackFrames(r) => Some(r.clone()), _ => None }
    }
}

impl TraceField for InternedString {
    type Ref<'a> = InternedString;
    fn field_type() -> FieldType { FieldType::PooledString }
    fn encode_field(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.0.to_le_bytes()); }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::PooledString(id) => Some(*id), _ => None }
    }
}

impl TraceField for Vec<(String, String)> {
    type Ref<'a> = StringMapRef<'a>;
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
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val { FieldValueRef::StringMap(r) => Some(r.clone()), _ => None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_type_round_trip() {
        for tag in [0, 1, 2, 3, 4, 5, 7, 8, 9, 10u8] {
            let ft = FieldType::from_tag(tag).unwrap();
            assert_eq!(ft as u8, tag);
        }
        assert!(FieldType::from_tag(6).is_none());
        assert!(FieldType::from_tag(11).is_none());
    }


    #[test]
    fn encode_decode_u64() {
        let val = FieldValue::U64(0xDEAD_BEEF_CAFE_BABE);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 8);
        let (decoded, rest) = FieldValue::decode(FieldType::U64, &buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_i64() {
        let val = FieldValue::I64(-123456789);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        let (decoded, _) = FieldValue::decode(FieldType::I64, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_f64() {
        let val = FieldValue::F64(std::f64::consts::PI);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        let (decoded, _) = FieldValue::decode(FieldType::F64, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_bool() {
        for b in [true, false] {
            let val = FieldValue::Bool(b);
            let mut buf = Vec::new();
            val.encode(&mut buf);
            assert_eq!(buf.len(), 1);
            let (decoded, rest) = FieldValue::decode(FieldType::Bool, &buf).unwrap();
            assert!(rest.is_empty());
            assert_eq!(decoded, val);
        }
    }

    #[test]
    fn encode_decode_string() {
        let val = FieldValue::String(b"hello world".to_vec());
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 4 + 11);
        let (decoded, rest) = FieldValue::decode(FieldType::String, &buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_bytes() {
        let val = FieldValue::Bytes(vec![0xff, 0x00, 0xab]);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        let (decoded, _) = FieldValue::decode(FieldType::Bytes, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_pooled_string() {
        let val = FieldValue::PooledString(42);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 4);
        let (decoded, _) = FieldValue::decode(FieldType::PooledString, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_stack_frames() {
        let addrs = vec![0x5555_5555_1234u64, 0x5555_5555_0a00, 0x5555_5555_0800, 0x5555_5555_0100];
        let val = FieldValue::StackFrames(addrs.clone());
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 17);
        let (decoded, _) = FieldValue::decode(FieldType::StackFrames, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn decode_with_offset() {
        let mut buf = vec![0xAA, 0xBB]; // garbage prefix
        let val = FieldValue::U64(999);
        val.encode(&mut buf);
        let (decoded, _) = FieldValue::decode(FieldType::U64, &buf[2..]).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_varint_small() {
        let val = FieldValue::Varint(3);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 1);
        let (decoded, rest) = FieldValue::decode(FieldType::Varint, &buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_varint_large() {
        let val = FieldValue::Varint(u64::MAX);
        let mut buf = Vec::new();
        val.encode(&mut buf);
        assert_eq!(buf.len(), 10); // u64::MAX needs 10 LEB128 bytes
        let (decoded, rest) = FieldValue::decode(FieldType::Varint, &buf).unwrap();
        assert!(rest.is_empty());
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
