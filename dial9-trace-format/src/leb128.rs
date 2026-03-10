// LEB128 encoding/decoding

use std::io::{self, Write};

#[inline]
pub fn encode_unsigned(mut value: u64, w: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 10];
    let mut i = 0;
    while value >= 0x80 {
        buf[i] = (value as u8) | 0x80;
        value >>= 7;
        i += 1;
    }
    buf[i] = value as u8;
    i += 1;
    w.write_all(&buf[..i])
}

/// Returns (value, bytes_consumed).
pub fn decode_unsigned(data: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    let mut pos = 0;
    loop {
        let byte = *data.get(pos)?;
        pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Some((result, pos));
        }
        if pos >= 10 {
            return None;
        } // u64 LEB128 is at most 10 bytes
    }
}

#[inline]
pub fn encode_signed(mut value: i64, w: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 10];
    let mut i = 0;
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        if !done {
            byte |= 0x80;
        }
        buf[i] = byte;
        i += 1;
        if done {
            break;
        }
    }
    w.write_all(&buf[..i])
}

/// Returns (value, bytes_consumed).
pub fn decode_signed(data: &[u8]) -> Option<(i64, usize)> {
    let mut result: i64 = 0;
    let mut shift = 0u32;
    let mut pos = 0;
    loop {
        let byte = *data.get(pos)?;
        pos += 1;
        result |= ((byte & 0x7f) as i64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            // Sign extend if needed
            if shift < 64 && byte & 0x40 != 0 {
                result |= !0i64 << shift;
            }
            return Some((result, pos));
        }
        if pos >= 10 {
            return None;
        } // i64 LEB128 is at most 10 bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_zero() {
        let mut buf = Vec::new();
        encode_signed(0, &mut buf).unwrap();
        assert_eq!(buf, [0x00]);
        assert_eq!(decode_signed(&buf), Some((0, 1)));
    }

    #[test]
    fn encode_decode_positive() {
        let mut buf = Vec::new();
        encode_signed(624485, &mut buf).unwrap();
        assert_eq!(decode_signed(&buf).unwrap().0, 624485);
    }

    #[test]
    fn encode_decode_negative() {
        let mut buf = Vec::new();
        encode_signed(-123456, &mut buf).unwrap();
        assert_eq!(decode_signed(&buf).unwrap().0, -123456);
    }

    #[test]
    fn encode_decode_minus_one() {
        let mut buf = Vec::new();
        encode_signed(-1, &mut buf).unwrap();
        assert_eq!(buf, [0x7f]);
        assert_eq!(decode_signed(&buf), Some((-1, 1)));
    }

    #[test]
    fn small_deltas_are_compact() {
        let mut buf = Vec::new();
        encode_signed(-0x834, &mut buf).unwrap();
        assert!(
            buf.len() <= 3,
            "delta -0x834 should be compact, got {} bytes",
            buf.len()
        );
        assert_eq!(decode_signed(&buf).unwrap().0, -0x834);
    }

    #[test]
    fn large_absolute_address() {
        let addr = 0x5555_5555_1234i64;
        let mut buf = Vec::new();
        encode_signed(addr, &mut buf).unwrap();
        let (decoded, _) = decode_signed(&buf).unwrap();
        assert_eq!(decoded, addr);
    }

    #[test]
    fn round_trip_extremes() {
        for val in [i64::MIN, i64::MAX, 0, 1, -1, 127, -128, 128, -129] {
            let mut buf = Vec::new();
            encode_signed(val, &mut buf).unwrap();
            let (decoded, consumed) = decode_signed(&buf).unwrap();
            assert_eq!(decoded, val, "failed for {val}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn unsigned_zero() {
        let mut buf = Vec::new();
        encode_unsigned(0, &mut buf).unwrap();
        assert_eq!(buf, [0x00]);
        assert_eq!(decode_unsigned(&buf), Some((0, 1)));
    }

    #[test]
    fn unsigned_small_values_compact() {
        // worker_id = 3 should be 1 byte
        let mut buf = Vec::new();
        encode_unsigned(3, &mut buf).unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(decode_unsigned(&buf).unwrap().0, 3);

        // 127 fits in 1 byte
        buf.clear();
        encode_unsigned(127, &mut buf).unwrap();
        assert_eq!(buf.len(), 1);

        // 128 needs 2 bytes
        buf.clear();
        encode_unsigned(128, &mut buf).unwrap();
        assert_eq!(buf.len(), 2);
        assert_eq!(decode_unsigned(&buf).unwrap().0, 128);
    }

    #[test]
    fn unsigned_timestamp_compact() {
        // 50,000 ns (typical poll duration) should be 3 bytes
        let mut buf = Vec::new();
        encode_unsigned(50_000, &mut buf).unwrap();
        assert!(
            buf.len() <= 3,
            "50k should fit in 3 bytes, got {}",
            buf.len()
        );
        assert_eq!(decode_unsigned(&buf).unwrap().0, 50_000);
    }

    #[test]
    fn unsigned_round_trip_extremes() {
        for val in [0u64, 1, 127, 128, 16383, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            encode_unsigned(val, &mut buf).unwrap();
            let (decoded, consumed) = decode_unsigned(&buf).unwrap();
            assert_eq!(decoded, val, "failed for {val}");
            assert_eq!(consumed, buf.len());
        }
    }
}
