//! Base64url without padding (RFC 4648 §5): tokens in cookies and links,
//! and the binary fields of passkeys, which browsers send this way.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Decodes, also accepting standard base64's `+` and `/` and trailing `=`.
/// `None` for anything else.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut n = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        n = n << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
            n &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero, so each value has one spelling.
    (n == 0).then_some(out)
}

/// A 32-byte token from its encoding.
pub fn token(s: &str) -> Option<[u8; 32]> {
    decode(s).and_then(|b| <[u8; 32]>::try_from(b).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let e = encode(&bytes);
            assert!(!e.contains('='));
            assert_eq!(decode(&e).as_deref(), Some(&bytes[..]), "{len}");
        }
        assert_eq!(encode(b"Moonforge"), "TW9vbmZvcmdl");
        assert_eq!(decode("TW9vbg"), Some(b"Moon".to_vec()));
        assert_eq!(decode("TW9vbg=="), Some(b"Moon".to_vec()));
        assert_eq!(decode("TW9vbh"), None, "stray bits");
        assert_eq!(decode("T"), None);
        assert_eq!(decode("a b"), None);
    }
}
