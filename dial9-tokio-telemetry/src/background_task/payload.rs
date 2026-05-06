//! Segment payload buffer.
//!
//! [`Payload`] is the byte container threaded through the processor pipeline.
//! It owns one or more [`Bytes`] chunks so that processors that produce
//! a payload by appending (e.g. symbolization, which emits the original
//! trace plus a symbol table) can do so without copying the original input.
//! For consumers that need a contiguous view, [`Payload::into_bytes`] /
//! [`Payload::into_vec`] produce one — fast-pathing zero-copy when the
//! payload already has a single chunk.

use bytes::{Bytes, BytesMut};

/// A segment payload — zero or more contiguous [`Bytes`] chunks.
///
/// Cloning a `Payload` is cheap: each chunk is `Bytes`, which clones via an
/// `Arc` bump.
#[derive(Default, Clone)]
pub struct Payload {
    chunks: Vec<Bytes>,
    len: usize,
}

impl Payload {
    /// Create an empty payload.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap a single `Bytes` as a payload.
    pub fn from_bytes(b: Bytes) -> Self {
        let mut p = Self::new();
        p.push(b);
        p
    }

    /// Wrap a `Vec<u8>` as a payload (zero-copy via `Bytes::from`).
    pub fn from_vec(v: Vec<u8>) -> Self {
        Self::from_bytes(Bytes::from(v))
    }

    /// Total byte length across all chunks.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the payload has no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of chunks. Useful for tests that want to verify zero-copy.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Borrow the chunk slice. Stable order; may be empty.
    pub fn chunks(&self) -> &[Bytes] {
        &self.chunks
    }

    /// Iterate over chunks in order.
    pub fn iter(&self) -> std::slice::Iter<'_, Bytes> {
        self.chunks.iter()
    }

    /// Append a chunk. Empty chunks are dropped to keep `chunk_count` honest.
    pub fn push(&mut self, b: Bytes) {
        if b.is_empty() {
            return;
        }
        self.len += b.len();
        self.chunks.push(b);
    }

    /// Check whether the payload begins with `prefix`. Walks across chunk
    /// boundaries so callers do not have to know the internal layout.
    pub fn starts_with(&self, prefix: &[u8]) -> bool {
        if prefix.len() > self.len {
            return false;
        }
        let mut remaining = prefix;
        for chunk in &self.chunks {
            if remaining.is_empty() {
                return true;
            }
            let take = remaining.len().min(chunk.len());
            if chunk[..take] != remaining[..take] {
                return false;
            }
            remaining = &remaining[take..];
        }
        remaining.is_empty()
    }

    /// Concatenate chunks into a single contiguous [`Bytes`].
    ///
    /// Fast path: if there is exactly one chunk, it is returned as-is
    /// (zero-copy). Otherwise a `BytesMut` of `self.len()` is allocated and
    /// each chunk is copied in order.
    pub fn into_bytes(mut self) -> Bytes {
        match self.chunks.len() {
            0 => Bytes::new(),
            1 => self.chunks.pop().unwrap(),
            _ => {
                let mut out = BytesMut::with_capacity(self.len);
                for chunk in self.chunks {
                    out.extend_from_slice(&chunk);
                }
                out.freeze()
            }
        }
    }

    /// Concatenate chunks into a single contiguous `Vec<u8>`.
    pub fn into_vec(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len);
        for chunk in &self.chunks {
            out.extend_from_slice(chunk);
        }
        out
    }
}

impl From<Bytes> for Payload {
    fn from(b: Bytes) -> Self {
        Self::from_bytes(b)
    }
}

impl From<Vec<u8>> for Payload {
    fn from(v: Vec<u8>) -> Self {
        Self::from_vec(v)
    }
}

impl From<&'static [u8]> for Payload {
    fn from(s: &'static [u8]) -> Self {
        Self::from_bytes(Bytes::from_static(s))
    }
}

impl<const N: usize> From<&'static [u8; N]> for Payload {
    fn from(s: &'static [u8; N]) -> Self {
        Self::from_bytes(Bytes::from_static(s))
    }
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Payload")
            .field("len", &self.len)
            .field("chunks", &self.chunks.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn empty_payload_round_trips() {
        let p = Payload::new();
        check!(p.is_empty());
        check!(p.len() == 0);
        check!(p.chunk_count() == 0);
        check!(p.into_vec().is_empty());
    }

    #[test]
    fn from_vec_is_single_chunk() {
        let p = Payload::from_vec(b"hello".to_vec());
        check!(p.len() == 5);
        check!(p.chunk_count() == 1);
    }

    #[test]
    fn push_drops_empty_chunks() {
        let mut p = Payload::new();
        p.push(Bytes::new());
        p.push(Bytes::from_static(b"abc"));
        p.push(Bytes::new());
        check!(p.chunk_count() == 1);
        check!(p.len() == 3);
    }

    #[test]
    fn into_bytes_single_chunk_is_zero_copy() {
        let original = Bytes::from(vec![1u8, 2, 3, 4, 5]);
        let original_ptr = original.as_ptr();
        let p = Payload::from_bytes(original);
        let out = p.into_bytes();
        check!(out.as_ptr() == original_ptr);
    }

    #[test]
    fn into_bytes_multi_chunk_concatenates() {
        let mut p = Payload::new();
        p.push(Bytes::from_static(b"ab"));
        p.push(Bytes::from_static(b"cde"));
        p.push(Bytes::from_static(b"f"));
        check!(p.chunk_count() == 3);
        let out = p.into_bytes();
        check!(&out[..] == b"abcdef");
    }

    #[test]
    fn starts_with_walks_chunks() {
        let mut p = Payload::new();
        p.push(Bytes::from_static(&[0x1f]));
        p.push(Bytes::from_static(&[0x8b, 0x08]));
        check!(p.starts_with(&[0x1f, 0x8b]));
        check!(p.starts_with(&[0x1f, 0x8b, 0x08]));
        check!(!p.starts_with(&[0x1f, 0x8b, 0x08, 0x00]));
        check!(!p.starts_with(&[0x1f, 0x00]));
    }

    #[test]
    fn starts_with_empty_prefix_is_true() {
        let p = Payload::from_vec(b"x".to_vec());
        check!(p.starts_with(&[]));
        let empty = Payload::new();
        check!(empty.starts_with(&[]));
        check!(!empty.starts_with(&[0]));
    }
}
