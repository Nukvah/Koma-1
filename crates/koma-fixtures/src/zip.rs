//! A zip writer for tests. It writes what real comic archives contain,
//! including the less common variants a reader must handle.

use crate::checksum::{crc32, deflate_stored};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    Stored,
    /// Method 8, written with stored DEFLATE blocks (valid, not smaller).
    Deflated,
}

/// How a member's name is written.
#[derive(Clone, Debug)]
pub enum Name {
    /// UTF-8 with the UTF-8 flag set.
    Utf8(String),
    /// UTF-8 bytes without the flag, as many tools write them.
    Unflagged(String),
    /// Raw bytes, for code page 437 names.
    Raw(Vec<u8>),
    /// Raw bytes plus the Info-ZIP Unicode Path extra field holding the real name.
    RawWithUnicodePath(Vec<u8>, String),
}

#[derive(Default)]
pub struct ZipWriter {
    out: Vec<u8>,
    central: Vec<u8>,
    count: u64,
    zip64: bool,
    comment: Vec<u8>,
}

impl ZipWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes Zip64 fields and end records even though nothing needs them,
    /// so the Zip64 path is tested without 4 GiB files.
    pub fn force_zip64(mut self) -> Self {
        self.zip64 = true;
        self
    }

    pub fn comment(mut self, comment: &[u8]) -> Self {
        self.comment = comment.to_vec();
        self
    }

    pub fn add(&mut self, name: &str, data: &[u8], compression: Compression) {
        self.add_named(Name::Utf8(name.to_owned()), data, compression);
    }

    pub fn add_named(&mut self, name: Name, data: &[u8], compression: Compression) {
        let mut unicode_path = None;
        let (name, flags) = match name {
            Name::Utf8(s) => (s.into_bytes(), 1u16 << 11),
            Name::Unflagged(s) => (s.into_bytes(), 0),
            Name::Raw(b) => (b, 0),
            Name::RawWithUnicodePath(b, s) => {
                unicode_path = Some(s);
                (b, 0)
            }
        };
        let (method, body) = match compression {
            Compression::Stored => (0u16, data.to_vec()),
            Compression::Deflated => (8u16, deflate_stored(data)),
        };
        let crc = crc32(data);
        let offset = self.out.len() as u64;
        let (csize, usize_, off32) = if self.zip64 {
            (u32::MAX, u32::MAX, u32::MAX)
        } else {
            (body.len() as u32, data.len() as u32, offset as u32)
        };
        let version: u16 = if self.zip64 { 45 } else { 20 };

        // Local header.
        let mut local_extra = Vec::new();
        if self.zip64 {
            local_extra.extend(1u16.to_le_bytes());
            local_extra.extend(16u16.to_le_bytes());
            local_extra.extend((data.len() as u64).to_le_bytes());
            local_extra.extend((body.len() as u64).to_le_bytes());
        }
        let o = &mut self.out;
        o.extend(0x0403_4b50u32.to_le_bytes());
        o.extend(version.to_le_bytes());
        o.extend(flags.to_le_bytes());
        o.extend(method.to_le_bytes());
        o.extend([0, 0, 0x21, 0x5A]); // time, date
        o.extend(crc.to_le_bytes());
        o.extend(csize.to_le_bytes());
        o.extend(usize_.to_le_bytes());
        o.extend((name.len() as u16).to_le_bytes());
        o.extend((local_extra.len() as u16).to_le_bytes());
        o.extend(&name);
        o.extend(&local_extra);
        o.extend(&body);

        // Central header.
        let mut extra = Vec::new();
        if self.zip64 {
            extra.extend(1u16.to_le_bytes());
            extra.extend(24u16.to_le_bytes());
            extra.extend((data.len() as u64).to_le_bytes());
            extra.extend((body.len() as u64).to_le_bytes());
            extra.extend(offset.to_le_bytes());
        }
        if let Some(path) = unicode_path {
            extra.extend(0x7075u16.to_le_bytes());
            extra.extend((5 + path.len() as u16).to_le_bytes());
            extra.push(1);
            extra.extend(crc32(&name).to_le_bytes());
            extra.extend(path.as_bytes());
        }
        let c = &mut self.central;
        c.extend(0x0201_4b50u32.to_le_bytes());
        c.extend((0x0300 | version).to_le_bytes()); // made by: Unix
        c.extend(version.to_le_bytes());
        c.extend(flags.to_le_bytes());
        c.extend(method.to_le_bytes());
        c.extend([0, 0, 0x21, 0x5A]);
        c.extend(crc.to_le_bytes());
        c.extend(csize.to_le_bytes());
        c.extend(usize_.to_le_bytes());
        c.extend((name.len() as u16).to_le_bytes());
        c.extend((extra.len() as u16).to_le_bytes());
        c.extend(0u16.to_le_bytes()); // comment
        c.extend(0u16.to_le_bytes()); // disk
        c.extend(0u16.to_le_bytes()); // internal attributes
        c.extend(0o100644u32.wrapping_shl(16).to_le_bytes());
        c.extend(off32.to_le_bytes());
        c.extend(&name);
        c.extend(&extra);
        self.count += 1;
    }

    pub fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.out.len() as u64;
        let cd_size = self.central.len() as u64;
        self.out.extend(&self.central);
        if self.zip64 {
            let record_pos = self.out.len() as u64;
            let o = &mut self.out;
            o.extend(0x0606_4b50u32.to_le_bytes());
            o.extend(44u64.to_le_bytes());
            o.extend(45u16.to_le_bytes());
            o.extend(45u16.to_le_bytes());
            o.extend(0u32.to_le_bytes());
            o.extend(0u32.to_le_bytes());
            o.extend(self.count.to_le_bytes());
            o.extend(self.count.to_le_bytes());
            o.extend(cd_size.to_le_bytes());
            o.extend(cd_offset.to_le_bytes());
            o.extend(0x0706_4b50u32.to_le_bytes());
            o.extend(0u32.to_le_bytes());
            o.extend(record_pos.to_le_bytes());
            o.extend(1u32.to_le_bytes());
        }
        let (count16, size32, offset32) = if self.zip64 {
            (u16::MAX, u32::MAX, u32::MAX)
        } else {
            (self.count as u16, cd_size as u32, cd_offset as u32)
        };
        let o = &mut self.out;
        o.extend(0x0605_4b50u32.to_le_bytes());
        o.extend(0u16.to_le_bytes());
        o.extend(0u16.to_le_bytes());
        o.extend(count16.to_le_bytes());
        o.extend(count16.to_le_bytes());
        o.extend(size32.to_le_bytes());
        o.extend(offset32.to_le_bytes());
        o.extend((self.comment.len() as u16).to_le_bytes());
        o.extend(&self.comment);
        self.out
    }
}
