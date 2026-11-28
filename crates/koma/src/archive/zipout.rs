//! Writing stored ZIPs piece by piece: a local header before each member as
//! it arrives, the central directory at the end. Used to pack uploaded photo
//! folders on the server as their images arrive. ZIP64 where an offset, the
//! directory or the member count needs it, so a set may pass 4 GiB.

/// MS-DOS date 1980-01-01, 00:00: Koma-1 keeps no image times.
const DOS_DATE: u16 = (1 << 5) | 1;
/// Names are UTF-8.
const UTF8: u16 = 1 << 11;
const MAX32: u64 = 0xFFFF_FFFF;

/// A member already written: what the central directory says about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub crc: u32,
    /// Its bytes (stored, so compressed and uncompressed alike); under 4 GiB.
    pub size: u32,
    /// Where its local header starts.
    pub offset: u64,
}

/// The local header of a stored member.
pub fn local_header(name: &str, crc: u32, size: u32) -> Vec<u8> {
    let name = name.as_bytes();
    let mut h = Vec::with_capacity(30 + name.len());
    h.extend(0x0403_4b50u32.to_le_bytes());
    h.extend(20u16.to_le_bytes()); // version needed: 2.0
    h.extend(UTF8.to_le_bytes());
    h.extend(0u16.to_le_bytes()); // stored
    h.extend(0u16.to_le_bytes()); // time
    h.extend(DOS_DATE.to_le_bytes());
    h.extend(crc.to_le_bytes());
    h.extend(size.to_le_bytes());
    h.extend(size.to_le_bytes());
    h.extend((name.len() as u16).to_le_bytes());
    h.extend(0u16.to_le_bytes()); // extra
    h.extend(name);
    h
}

/// The central directory and end records for `members`, to be written at
/// `at`, the end of the last member. `zip64_from`: offsets and sizes from
/// this one on use ZIP64 fields (`0xFFFF_FFFF` normally; lower in tests).
pub fn central_directory(members: &[Member], at: u64, zip64_from: u64) -> Vec<u8> {
    let mut cd = Vec::new();
    for m in members {
        let name = m.name.as_bytes();
        let wide = m.offset >= zip64_from;
        let version: u16 = if wide { 45 } else { 20 };
        cd.extend(0x0201_4b50u32.to_le_bytes());
        cd.extend(version.to_le_bytes()); // made by
        cd.extend(version.to_le_bytes()); // needed
        cd.extend(UTF8.to_le_bytes());
        cd.extend(0u16.to_le_bytes()); // stored
        cd.extend(0u16.to_le_bytes()); // time
        cd.extend(DOS_DATE.to_le_bytes());
        cd.extend(m.crc.to_le_bytes());
        cd.extend(m.size.to_le_bytes());
        cd.extend(m.size.to_le_bytes());
        cd.extend((name.len() as u16).to_le_bytes());
        cd.extend((if wide { 12u16 } else { 0 }).to_le_bytes()); // extra
        cd.extend([0u8; 10]); // comment, disk, internal and external attributes
        let offset = if wide { MAX32 as u32 } else { m.offset as u32 };
        cd.extend(offset.to_le_bytes());
        cd.extend(name);
        if wide {
            cd.extend(1u16.to_le_bytes()); // ZIP64 extra: the offset only
            cd.extend(8u16.to_le_bytes());
            cd.extend(m.offset.to_le_bytes());
        }
    }
    let count = members.len() as u64;
    let cd_len = cd.len() as u64;
    let wide = at >= zip64_from || cd_len >= zip64_from || count >= 0xFFFF;
    let mut out = cd;
    if wide {
        let record_at = at + cd_len;
        out.extend(0x0606_4b50u32.to_le_bytes()); // ZIP64 end of central directory
        out.extend(44u64.to_le_bytes());
        out.extend(45u16.to_le_bytes());
        out.extend(45u16.to_le_bytes());
        out.extend([0u8; 8]); // disk numbers
        out.extend(count.to_le_bytes());
        out.extend(count.to_le_bytes());
        out.extend(cd_len.to_le_bytes());
        out.extend(at.to_le_bytes());
        out.extend(0x0706_4b50u32.to_le_bytes()); // its locator
        out.extend(0u32.to_le_bytes());
        out.extend(record_at.to_le_bytes());
        out.extend(1u32.to_le_bytes());
    }
    let small = |n: u64, max: u64| if wide { max } else { n };
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]); // disk numbers
    out.extend((small(count, 0xFFFF) as u16).to_le_bytes());
    out.extend((small(count, 0xFFFF) as u16).to_le_bytes());
    out.extend((small(cd_len, MAX32) as u32).to_le_bytes());
    out.extend((small(at, MAX32) as u32).to_le_bytes());
    out.extend(0u16.to_le_bytes()); // comment
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::crc32::checksum;
    use crate::archive::zip;

    fn build(pages: &[(&str, &[u8])], zip64_from: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut members = Vec::new();
        for (name, data) in pages {
            let crc = checksum(data);
            members.push(Member {
                name: (*name).to_owned(),
                crc,
                size: data.len() as u32,
                offset: out.len() as u64,
            });
            out.extend(local_header(name, crc, data.len() as u32));
            out.extend(*data);
        }
        let at = out.len() as u64;
        out.extend(central_directory(&members, at, zip64_from));
        out
    }

    #[test]
    fn koma_reads_what_it_writes() {
        let pages: [(&str, &[u8]); 3] = [
            ("001.jpg", b"first page"),
            ("002.jpg", b"second"),
            ("metadata.json", b"{}"),
        ];
        for zip64_from in [MAX32, 20] {
            let bytes = build(&pages, zip64_from);
            let len = bytes.len() as u64;
            let index =
                zip::read_index(bytes.as_slice(), len, &zip::Limits::default()).expect("an index");
            let names: Vec<&str> = index.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(
                names,
                ["001.jpg", "002.jpg", "metadata.json"],
                "zip64 from {zip64_from}"
            );
            for (e, (_, data)) in index.iter().zip(pages) {
                assert_eq!(e.crc32, checksum(data));
            }
        }
    }
}
