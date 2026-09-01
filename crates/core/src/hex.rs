//! Lowercase hex encoding of byte slices — the single implementation shared by
//! the session-token code ([`crate::auth`]) and the daemon's SQL literal
//! builder. Std-only, no external crate (in the spirit of the project's other
//! dependency-free helpers, e.g. [`crate::date`]).

/// Encode `bytes` as a lowercase hex string (two ASCII chars per byte).
pub fn encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// The inverse of [`encode`]: `None` when `text` is not an even-length run of
/// hex digits. Used to read the exact bytes of a tree name off the wire
/// ([`crate::metarecord::TreeName`]).
pub fn decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push((digit(pair[0])? << 4) | digit(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn decodes_what_encode_produced() {
        for bytes in [vec![], vec![0x00], vec![0xff, 0xa0, 0x01], b"caf\xe9.mp4".to_vec()] {
            assert_eq!(decode(&encode(&bytes)).as_deref(), Some(bytes.as_slice()));
        }
        assert_eq!(decode("DEADBEEF"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn rejects_anything_that_is_not_whole_hex() {
        assert_eq!(decode("abc"), None); // odd length
        assert_eq!(decode("zz"), None); // not hex digits
        assert_eq!(decode("00 11"), None);
    }

    #[test]
    fn encodes_lowercase_two_chars_per_byte() {
        assert_eq!(encode(&[]), "");
        assert_eq!(encode(&[0x00]), "00");
        assert_eq!(encode(&[0x0f]), "0f");
        assert_eq!(encode(&[0xff, 0xa0, 0x01]), "ffa001");
        assert_eq!(encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }
}
