//! Fujifilm RAF: a fixed big-endian header whose bytes 84..92 give the offset
//! and length of the embedded JPEG. That JPEG carries its own EXIF, so its
//! orientation comes from there.

use std::io::{Read, Seek};

use anyhow::{ensure, Result};

use super::{read_up_to, Located};

pub(super) fn locate<R: Read + Seek>(reader: &mut R) -> Result<Located> {
    let mut header = [0; 92];
    ensure!(
        read_up_to(reader, 0, &mut header)? == header.len(),
        "RAF header is truncated"
    );
    let offset = u64::from(u32::from_be_bytes([
        header[84], header[85], header[86], header[87],
    ]));
    let len = u64::from(u32::from_be_bytes([
        header[88], header[89], header[90], header[91],
    ]));
    let mut located = Located::default();
    located.ranges.push(offset..offset + len);
    Ok(located)
}

#[cfg(test)]
mod tests {
    use super::super::{find_previews, fixtures, Format};
    use std::io::Cursor;

    #[test]
    fn finds_the_header_jpeg() {
        let raf = fixtures::raf(&fixtures::jpeg(300, 200));
        let previews = find_previews(&mut Cursor::new(&raf), Format::Raf, 1280).unwrap();
        assert_eq!(
            previews
                .jpegs
                .iter()
                .map(|j| (j.width, j.height))
                .collect::<Vec<_>>(),
            [(300, 200)]
        );
    }

    #[test]
    fn truncated_header_fails_cleanly() {
        let raf = fixtures::raf(&fixtures::jpeg(32, 32));
        let err = find_previews(&mut Cursor::new(&raf[..90]), Format::Raf, 1280).unwrap_err();
        assert_eq!(err.to_string(), "RAF header is truncated");
    }
}
