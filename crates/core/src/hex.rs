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

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn encodes_lowercase_two_chars_per_byte() {
        assert_eq!(encode(&[]), "");
        assert_eq!(encode(&[0x00]), "00");
        assert_eq!(encode(&[0x0f]), "0f");
        assert_eq!(encode(&[0xff, 0xa0, 0x01]), "ffa001");
        assert_eq!(encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }
}
