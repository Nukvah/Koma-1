//! CRC-32 (IEEE 802.3), as used by zip and PNG.

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// An incremental CRC-32.
#[derive(Clone, Copy, Debug)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Self(0xFFFF_FFFF)
    }
}

impl Crc32 {
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = TABLE[((self.0 ^ u32::from(b)) & 0xFF) as usize] ^ (self.0 >> 8);
        }
    }

    pub fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

pub fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::default();
    crc.update(bytes);
    crc.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_values() {
        assert_eq!(super::checksum(b""), 0);
        assert_eq!(super::checksum(b"123456789"), 0xCBF4_3926);
        let mut crc = super::Crc32::default();
        crc.update(b"1234");
        crc.update(b"56789");
        assert_eq!(crc.finish(), 0xCBF4_3926);
    }
}
