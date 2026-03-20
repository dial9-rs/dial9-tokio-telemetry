// Field types and value encoding

use crate::codec::{MAX_TIMESTAMP_DELTA_NS, TAG_TIMESTAMP_RESET};
use std::io::{self, Write};

/// Wire type tags for field types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FieldType {
    I64 = 1,
    F64 = 2,
    Bool = 3,
    String = 4,
    Bytes = 5,
    // Tag 6 was legacy Timestamp (removed).
    PooledString = 7,
    StackFrames = 8,
    Varint = 9,
    StringMap = 10,
    U8 = 11,
    U16 = 12,
    U32 = 13,
}

/// Newtype for stack frame addresses (leaf-first).
#[derive(Debug, Clone, PartialEq)]
pub struct StackFrames(pub Vec<u64>);

/// An interned string reference (pool ID). Created by [`Encoder::intern_string`](crate::encoder::Encoder::intern_string).
/// On the wire this is a `PooledString` (u32 LE).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InternedString(pub(crate) u32);

#[cfg(feature = "serde")]
impl serde::Serialize for InternedString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.0)
    }
}

impl InternedString {
    /// Construct from a raw pool ID. Intended for building data from external
    /// sources (e.g. wire decoding outside the `Encoder`).
    pub const fn from_raw(id: u32) -> Self {
        Self(id)
    }

    /// Returns the underlying pool ID.
    pub const fn raw_id(self) -> u32 {
        self.0
    }
}

impl std::fmt::Debug for InternedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pool#{}", self.0)
    }
}
/// Owned field value. Decoded from the wire format.
///
/// Note: `U8`, `U16`, and `U32` wire types are decoded into `Varint(v as u64)`.
/// The original fixed-width type is not preserved — use the schema's `FieldType`
/// if you need to distinguish them.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FieldValue {
    I64(i64),
    F64(f64),
    Bool(bool),
    String(String),
    Bytes(Vec<u8>),
    PooledString(InternedString),
    StackFrames(Vec<u64>),
    Varint(u64),
    StringMap(Vec<(Vec<u8>, Vec<u8>)>),
}

impl FieldValue {
    pub fn string(s: &str) -> Self {
        FieldValue::String(s.to_string())
    }
}

impl FieldType {
    pub fn from_tag(tag: u8) -> Option<FieldType> {
        match tag {
            1 => Some(FieldType::I64),
            2 => Some(FieldType::F64),
            3 => Some(FieldType::Bool),
            4 => Some(FieldType::String),
            5 => Some(FieldType::Bytes),
            7 => Some(FieldType::PooledString),
            8 => Some(FieldType::StackFrames),
            9 => Some(FieldType::Varint),
            10 => Some(FieldType::StringMap),
            11 => Some(FieldType::U8),
            12 => Some(FieldType::U16),
            13 => Some(FieldType::U32),
            _ => None,
        }
    }
}

impl FieldValue {
    /// Encode this value into the writer.
    pub fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            FieldValue::I64(v) => w.write_all(&v.to_le_bytes()),
            FieldValue::F64(v) => w.write_all(&v.to_le_bytes()),
            FieldValue::Bool(v) => w.write_all(&[if *v { 1 } else { 0 }]),
            FieldValue::String(v) => {
                let bytes = v.as_bytes();
                w.write_all(&(bytes.len() as u32).to_le_bytes())?;
                w.write_all(bytes)
            }
            FieldValue::Bytes(v) => {
                w.write_all(&(v.len() as u32).to_le_bytes())?;
                w.write_all(v)
            }
            FieldValue::PooledString(id) => w.write_all(&id.0.to_le_bytes()),
            FieldValue::Varint(v) => crate::leb128::encode_unsigned(*v, w),
            FieldValue::StackFrames(addrs) => {
                w.write_all(&(addrs.len() as u32).to_le_bytes())?;
                for &addr in addrs {
                    w.write_all(&addr.to_le_bytes())?;
                }
                Ok(())
            }
            FieldValue::StringMap(pairs) => {
                w.write_all(&(pairs.len() as u32).to_le_bytes())?;
                for (k, v) in pairs {
                    w.write_all(&(k.len() as u32).to_le_bytes())?;
                    w.write_all(k)?;
                    w.write_all(&(v.len() as u32).to_le_bytes())?;
                    w.write_all(v)?;
                }
                Ok(())
            }
        }
    }

    /// Decode a value of the given type from the buffer. Returns (value, remaining_slice).
    pub fn decode(field_type: FieldType, data: &[u8]) -> Option<(FieldValue, &[u8])> {
        match field_type {
            FieldType::I64 => {
                let v = i64::from_le_bytes(data.get(..8)?.try_into().ok()?);
                Some((FieldValue::I64(v), &data[8..]))
            }
            FieldType::F64 => {
                let v = f64::from_le_bytes(data.get(..8)?.try_into().ok()?);
                Some((FieldValue::F64(v), &data[8..]))
            }
            FieldType::Bool => Some((FieldValue::Bool(*data.first()? != 0), &data[1..])),
            FieldType::String => {
                let len = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
                let bytes = data.get(4..4 + len)?;
                let s = std::str::from_utf8(bytes)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|_| {
                        // Spec requires UTF-8 but we don't want to silently drop the
                        // entire event. Replace invalid sequences so decoding can continue.
                        String::from_utf8_lossy(bytes).into_owned()
                    });
                Some((FieldValue::String(s), &data[4 + len..]))
            }
            FieldType::Bytes => {
                let len = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
                let bytes = data.get(4..4 + len)?.to_vec();
                Some((FieldValue::Bytes(bytes), &data[4 + len..]))
            }
            FieldType::PooledString => {
                let id = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
                Some((FieldValue::PooledString(InternedString(id)), &data[4..]))
            }
            FieldType::Varint => {
                let (v, consumed) = crate::leb128::decode_unsigned(data)?;
                Some((FieldValue::Varint(v), &data[consumed..]))
            }
            FieldType::U8 => Some((FieldValue::Varint(*data.first()? as u64), &data[1..])),
            FieldType::U16 => {
                let v = u16::from_le_bytes(data.get(..2)?.try_into().ok()?);
                Some((FieldValue::Varint(v as u64), &data[2..]))
            }
            FieldType::U32 => {
                let v = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
                Some((FieldValue::Varint(v as u64), &data[4..]))
            }
            FieldType::StackFrames => {
                let count = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
                let mut pos = 4;
                let mut addrs = Vec::with_capacity(count.min(data.len() / 8));
                for _ in 0..count {
                    let addr = u64::from_le_bytes(data.get(pos..pos + 8)?.try_into().ok()?);
                    addrs.push(addr);
                    pos += 8;
                }
                Some((FieldValue::StackFrames(addrs), &data[pos..]))
            }
            FieldType::StringMap => {
                let count = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
                let mut pos = 4;
                let mut pairs = Vec::with_capacity(count.min(data.len() / 8));
                for _ in 0..count {
                    let klen =
                        u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
                    pos += 4;
                    let k = data.get(pos..pos + klen)?.to_vec();
                    pos += klen;
                    let vlen =
                        u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
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
#[non_exhaustive]
pub enum FieldValueRef<'a> {
    I64(i64),
    F64(f64),
    Bool(bool),
    String(&'a str),
    Bytes(&'a [u8]),
    PooledString(InternedString),
    /// Raw stack frame bytes. Use [`StackFramesRef::iter`] to iterate addresses.
    StackFrames(StackFramesRef<'a>),
    Varint(u64),
    StringMap(StringMapRef<'a>),
}

/// Zero-copy wrapper for delta-encoded stack frame data.
#[derive(Clone, PartialEq)]
pub struct StackFramesRef<'a> {
    data: &'a [u8],
    count: u32,
}

impl std::fmt::Debug for StackFramesRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let addrs: Vec<u64> = self.iter().collect();
        write!(f, "[")?;
        for (i, a) in addrs.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "0x{a:x}")?;
        }
        write!(f, "]")
    }
}

impl<'a> StackFramesRef<'a> {
    pub fn iter(&self) -> StackFrameIter<'a> {
        StackFrameIter::new(self.data, self.count)
    }

    pub fn count(&self) -> u32 {
        self.count
    }
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

    pub fn count(&self) -> u32 {
        self.count
    }
}

impl<'a> FieldValueRef<'a> {
    /// Decode a value of the given type, borrowing from `data` at `offset`.
    /// Returns (value, bytes_consumed).
    pub fn decode(field_type: FieldType, data: &'a [u8], offset: usize) -> Option<(Self, usize)> {
        let d = &data[offset..];
        match field_type {
            FieldType::I64 => {
                let v = i64::from_le_bytes(d.get(..8)?.try_into().ok()?);
                Some((FieldValueRef::I64(v), 8))
            }
            FieldType::F64 => {
                let v = f64::from_le_bytes(d.get(..8)?.try_into().ok()?);
                Some((FieldValueRef::F64(v), 8))
            }
            FieldType::Bool => Some((FieldValueRef::Bool(*d.first()? != 0), 1)),
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
            FieldType::U8 => Some((FieldValueRef::Varint(*d.first()? as u64), 1)),
            FieldType::U16 => {
                let v = u16::from_le_bytes(d.get(..2)?.try_into().ok()?);
                Some((FieldValueRef::Varint(v as u64), 2))
            }
            FieldType::U32 => {
                let v = u32::from_le_bytes(d.get(..4)?.try_into().ok()?);
                Some((FieldValueRef::Varint(v as u64), 4))
            }
            // StackFrames: scan to find end position, then wrap as zero-copy ref.
            FieldType::StackFrames => {
                let count = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
                let data_len = count * 8;
                let pos = 4 + data_len;
                d.get(4..pos)?; // bounds check
                Some((
                    FieldValueRef::StackFrames(StackFramesRef {
                        data: &d[4..pos],
                        count: count as u32,
                    }),
                    pos,
                ))
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
                Some((
                    FieldValueRef::StringMap(StringMapRef {
                        data: &d[4..pos],
                        count: count as u32,
                    }),
                    pos,
                ))
            }
        }
    }
}

/// Iterator over stack frame addresses (u64 LE).
pub struct StackFrameIter<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u32,
}

impl<'a> StackFrameIter<'a> {
    pub fn new(data: &'a [u8], count: u32) -> Self {
        Self {
            data,
            pos: 0,
            remaining: count,
        }
    }
}

impl Iterator for StackFrameIter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        if self.remaining == 0 {
            return None;
        }
        let addr = u64::from_le_bytes(self.data.get(self.pos..self.pos + 8)?.try_into().ok()?);
        self.pos += 8;
        self.remaining -= 1;
        Some(addr)
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
        Self {
            data,
            pos: 0,
            remaining: count,
        }
    }
}

impl<'a> Iterator for StringMapIter<'a> {
    type Item = (&'a str, &'a str);
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let klen =
            u32::from_le_bytes(self.data.get(self.pos..self.pos + 4)?.try_into().ok()?) as usize;
        self.pos += 4;
        let k = std::str::from_utf8(self.data.get(self.pos..self.pos + klen)?).ok()?;
        self.pos += klen;
        let vlen =
            u32::from_le_bytes(self.data.get(self.pos..self.pos + 4)?.try_into().ok()?) as usize;
        self.pos += 4;
        let v = std::str::from_utf8(self.data.get(self.pos..self.pos + vlen)?).ok()?;
        self.pos += vlen;
        self.remaining -= 1;
        Some((k, v))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining as usize, Some(self.remaining as usize))
    }
}

impl ExactSizeIterator for StringMapIter<'_> {}

/// A writer wrapper that counts bytes written through it.
pub(crate) struct CountingWriter<W> {
    inner: W,
    bytes_written: u64,
}

impl<W> CountingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    pub fn inner(&self) -> &W {
        &self.inner
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner.write_all(buf)?;
        self.bytes_written += buf.len() as u64;
        Ok(())
    }
}

/// Encoding state: carries the output writer and timestamp base for delta encoding.
/// Used internally by [`EventEncoder`] and [`crate::encoder::Encoder`].
pub(crate) struct EncodeState<W: Write> {
    pub(crate) writer: CountingWriter<W>,
    /// Current timestamp base (set by TimestampReset frames).
    pub(crate) timestamp_base_ns: u64,
}

impl<W: Write> EncodeState<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: CountingWriter::new(writer),
            timestamp_base_ns: 0,
        }
    }

    /// Compute the timestamp delta, emitting a TimestampReset frame if needed
    /// (delta overflow or backwards timestamp).
    ///
    /// The base advances to `ts_ns` after every event so that consecutive
    /// inter-event deltas stay small — critical for gzip compressibility.
    pub(crate) fn encode_timestamp_delta(&mut self, ts_ns: u64) -> io::Result<u32> {
        if ts_ns < self.timestamp_base_ns || ts_ns - self.timestamp_base_ns > MAX_TIMESTAMP_DELTA_NS
        {
            self.writer.write_all(&[TAG_TIMESTAMP_RESET])?;
            self.writer.write_all(&ts_ns.to_le_bytes())?;
            self.timestamp_base_ns = ts_ns;
            Ok(0)
        } else {
            let delta = (ts_ns - self.timestamp_base_ns) as u32;
            self.timestamp_base_ns = ts_ns;
            Ok(delta)
        }
    }
}

/// Short-lived encoder for writing the fields of a single event.
/// Created by [`crate::encoder::Encoder::write`] and passed to
/// [`crate::TraceEvent::encode_fields`], analogous to `fmt::Formatter`.
pub struct EventEncoder<'a, W: Write = Vec<u8>> {
    pub(crate) state: &'a mut EncodeState<W>,
}

impl<'a, W: Write> EventEncoder<'a, W> {
    pub(crate) fn new(state: &'a mut EncodeState<W>) -> Self {
        Self { state }
    }

    pub fn write_u8(&mut self, v: u8) -> io::Result<()> {
        self.state.writer.write_all(&[v])
    }

    pub fn write_u16(&mut self, v: u16) -> io::Result<()> {
        self.state.writer.write_all(&v.to_le_bytes())
    }

    pub fn write_u32(&mut self, v: u32) -> io::Result<()> {
        self.state.writer.write_all(&v.to_le_bytes())
    }

    pub fn write_u64(&mut self, v: u64) -> io::Result<()> {
        crate::leb128::encode_unsigned(v, &mut self.state.writer)
    }

    pub fn write_i64(&mut self, v: i64) -> io::Result<()> {
        self.state.writer.write_all(&v.to_le_bytes())
    }

    pub fn write_f64(&mut self, v: f64) -> io::Result<()> {
        self.state.writer.write_all(&v.to_le_bytes())
    }

    pub fn write_bool(&mut self, v: bool) -> io::Result<()> {
        self.state.writer.write_all(&[if v { 1 } else { 0 }])
    }

    pub fn write_string(&mut self, v: &str) -> io::Result<()> {
        let bytes = v.as_bytes();
        self.state
            .writer
            .write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.state.writer.write_all(bytes)
    }

    pub fn write_bytes(&mut self, v: &[u8]) -> io::Result<()> {
        self.state
            .writer
            .write_all(&(v.len() as u32).to_le_bytes())?;
        self.state.writer.write_all(v)
    }

    pub fn write_interned(&mut self, v: InternedString) -> io::Result<()> {
        self.state.writer.write_all(&v.0.to_le_bytes())
    }

    pub fn write_stack_frames(&mut self, v: &StackFrames) -> io::Result<()> {
        self.state
            .writer
            .write_all(&(v.0.len() as u32).to_le_bytes())?;
        for &addr in &v.0 {
            self.state.writer.write_all(&addr.to_le_bytes())?;
        }
        Ok(())
    }

    pub fn write_string_map(&mut self, v: &[(String, String)]) -> io::Result<()> {
        self.state
            .writer
            .write_all(&(v.len() as u32).to_le_bytes())?;
        for (k, val) in v {
            let kb = k.as_bytes();
            self.state
                .writer
                .write_all(&(kb.len() as u32).to_le_bytes())?;
            self.state.writer.write_all(kb)?;
            let vb = val.as_bytes();
            self.state
                .writer
                .write_all(&(vb.len() as u32).to_le_bytes())?;
            self.state.writer.write_all(vb)?;
        }
        Ok(())
    }

    /// Write a [`FieldValue`] with its associated [`FieldType`].
    pub fn write_field_value(&mut self, value: &FieldValue) -> io::Result<()> {
        value.encode(&mut self.state.writer)
    }
}

/// Trait for types that can be used as trace fields.
/// Provides schema metadata (`field_type`), encoding (`encode`), and
/// decoding (`decode_ref`).
pub trait TraceField {
    /// The zero-copy decoded form of this field.
    type Ref<'a>;

    fn field_type() -> FieldType;
    /// Encode this field's value into the event encoder.
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()>;
    /// Extract this field's value from a zero-copy FieldValueRef.
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>>;
}

impl TraceField for u8 {
    type Ref<'a> = u8;
    fn field_type() -> FieldType {
        FieldType::U8
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u8(*self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(*v as u8),
            _ => None,
        }
    }
}

impl TraceField for u16 {
    type Ref<'a> = u16;
    fn field_type() -> FieldType {
        FieldType::U16
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u16(*self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(*v as u16),
            _ => None,
        }
    }
}

impl TraceField for u32 {
    type Ref<'a> = u32;
    fn field_type() -> FieldType {
        FieldType::U32
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u32(*self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(*v as u32),
            _ => None,
        }
    }
}

impl TraceField for u64 {
    type Ref<'a> = u64;
    fn field_type() -> FieldType {
        FieldType::Varint
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u64(*self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(*v),
            _ => None,
        }
    }
}

impl TraceField for i64 {
    type Ref<'a> = i64;
    fn field_type() -> FieldType {
        FieldType::I64
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_i64(*self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::I64(v) => Some(*v),
            _ => None,
        }
    }
}

impl TraceField for f64 {
    type Ref<'a> = f64;
    fn field_type() -> FieldType {
        FieldType::F64
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_f64(*self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::F64(v) => Some(*v),
            _ => None,
        }
    }
}

impl TraceField for bool {
    type Ref<'a> = bool;
    fn field_type() -> FieldType {
        FieldType::Bool
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_bool(*self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Bool(v) => Some(*v),
            _ => None,
        }
    }
}

impl TraceField for String {
    type Ref<'a> = &'a str;
    fn field_type() -> FieldType {
        FieldType::String
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_string(self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::String(s) => Some(s),
            _ => None,
        }
    }
}

impl TraceField for Vec<u8> {
    type Ref<'a> = &'a [u8];
    fn field_type() -> FieldType {
        FieldType::Bytes
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_bytes(self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

impl TraceField for StackFrames {
    type Ref<'a> = StackFramesRef<'a>;
    fn field_type() -> FieldType {
        FieldType::StackFrames
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_stack_frames(self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::StackFrames(r) => Some(r.clone()),
            _ => None,
        }
    }
}

impl TraceField for InternedString {
    type Ref<'a> = InternedString;
    fn field_type() -> FieldType {
        FieldType::PooledString
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_interned(*self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::PooledString(id) => Some(*id),
            _ => None,
        }
    }
}

impl TraceField for Vec<(String, String)> {
    type Ref<'a> = StringMapRef<'a>;
    fn field_type() -> FieldType {
        FieldType::StringMap
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_string_map(self)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::StringMap(r) => Some(r.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_type_round_trip() {
        for tag in [1, 2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13u8] {
            let ft = FieldType::from_tag(tag).unwrap();
            assert_eq!(ft as u8, tag);
        }
        assert!(FieldType::from_tag(0).is_none());
        assert!(FieldType::from_tag(14).is_none());
    }

    #[test]
    fn encode_decode_i64() {
        let val = FieldValue::I64(-123456789);
        let mut buf = Vec::new();
        val.encode(&mut buf).unwrap();
        let (decoded, _) = FieldValue::decode(FieldType::I64, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_f64() {
        let val = FieldValue::F64(std::f64::consts::PI);
        let mut buf = Vec::new();
        val.encode(&mut buf).unwrap();
        let (decoded, _) = FieldValue::decode(FieldType::F64, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_bool() {
        for b in [true, false] {
            let val = FieldValue::Bool(b);
            let mut buf = Vec::new();
            val.encode(&mut buf).unwrap();
            assert_eq!(buf.len(), 1);
            let (decoded, rest) = FieldValue::decode(FieldType::Bool, &buf).unwrap();
            assert!(rest.is_empty());
            assert_eq!(decoded, val);
        }
    }

    #[test]
    fn encode_decode_string() {
        let val = FieldValue::String("hello world".to_string());
        let mut buf = Vec::new();
        val.encode(&mut buf).unwrap();
        assert_eq!(buf.len(), 4 + 11);
        let (decoded, rest) = FieldValue::decode(FieldType::String, &buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_bytes() {
        let val = FieldValue::Bytes(vec![0xff, 0x00, 0xab]);
        let mut buf = Vec::new();
        val.encode(&mut buf).unwrap();
        let (decoded, _) = FieldValue::decode(FieldType::Bytes, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_pooled_string() {
        let val = FieldValue::PooledString(InternedString(42));
        let mut buf = Vec::new();
        val.encode(&mut buf).unwrap();
        assert_eq!(buf.len(), 4);
        let (decoded, _) = FieldValue::decode(FieldType::PooledString, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_stack_frames() {
        let addrs = vec![
            0x5555_5555_1234u64,
            0x5555_5555_0a00,
            0x5555_5555_0800,
            0x5555_5555_0100,
        ];
        let val = FieldValue::StackFrames(addrs.clone());
        let mut buf = Vec::new();
        val.encode(&mut buf).unwrap();
        assert_eq!(buf.len(), 4 + 4 * 8); // count(4) + 4 raw u64s
        let (decoded, _) = FieldValue::decode(FieldType::StackFrames, &buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_varint_small() {
        let val = FieldValue::Varint(3);
        let mut buf = Vec::new();
        val.encode(&mut buf).unwrap();
        assert_eq!(buf.len(), 1);
        let (decoded, rest) = FieldValue::decode(FieldType::Varint, &buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded, val);
    }

    #[test]
    fn encode_decode_varint_large() {
        let val = FieldValue::Varint(u64::MAX);
        let mut buf = Vec::new();
        val.encode(&mut buf).unwrap();
        assert_eq!(buf.len(), 10); // u64::MAX needs 10 LEB128 bytes
        let (decoded, rest) = FieldValue::decode(FieldType::Varint, &buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded, val);
    }

    #[test]
    fn varint_poll_end_compactness() {
        // PollEnd: timestamp_ns=1_050_000, worker_id=3
        let mut buf = Vec::new();
        FieldValue::Varint(1_050_000).encode(&mut buf).unwrap();
        FieldValue::Varint(3).encode(&mut buf).unwrap();
        // timestamp ~3 bytes, worker 1 byte = ~4 bytes for the payload
        assert!(
            buf.len() <= 4,
            "PollEnd payload should be <=4 bytes, got {}",
            buf.len()
        );
    }
}
