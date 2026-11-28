//! Large made-up files for upload tests: bytes that depend on their offset,
//! so any piece can be made again on its own and a file checked against it
//! without keeping a copy (2 GB uploads, cut and resumed).

/// The bytes of a bulk file from `offset`, `len` of them. Every 8-byte word
/// is its own index mixed with `seed`.
pub fn bytes(seed: u64, offset: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 16);
    let mut word = offset / 8;
    let skip = (offset % 8) as usize;
    while out.len() < len + skip {
        let v = (word ^ seed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        out.extend_from_slice(&v.to_le_bytes());
        word += 1;
    }
    out.drain(..skip);
    out.truncate(len);
    out
}

/// Whether the file at `path` is exactly the bulk file of `seed` and `len`.
pub fn matches(path: &std::path::Path, seed: u64, len: u64) -> std::io::Result<bool> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    if f.metadata()?.len() != len {
        return Ok(false);
    }
    let mut buf = vec![0u8; 4 << 20];
    let mut at = 0u64;
    while at < len {
        let n = ((len - at) as usize).min(buf.len());
        f.read_exact(&mut buf[..n])?;
        if buf[..n] != bytes(seed, at, n)[..] {
            return Ok(false);
        }
        at += n as u64;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn pieces_join_up() {
        let whole = super::bytes(7, 0, 100);
        assert_eq!(super::bytes(7, 13, 50), whole[13..63]);
        assert_ne!(super::bytes(8, 0, 100), whole);
    }
}
