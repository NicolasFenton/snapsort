//! TIFF-based raws: CR2 (Canon), NEF (Nikon), ARW (Sony), DNG and RW2
//! (Panasonic). Their previews hang off the IFD chain and SubIFDs in one of
//! three ways:
//! - JPEGInterchangeFormat/Length tags (NEF, ARW, and CR2's thumbnail);
//! - a JPEG-compressed IFD stored as a single strip (CR2's full-size JPEG,
//!   DNG previews);
//! - Panasonic's JpgFromRaw tag (RW2).

use std::io::{Read, Seek};

use anyhow::{ensure, Result};
use image::metadata::Orientation;

use super::{read_at, read_up_to, Located};

const PANASONIC_JPG_FROM_RAW: u16 = 0x002E;
const COMPRESSION: u16 = 0x0103;
const STRIP_OFFSETS: u16 = 0x0111;
const ORIENTATION: u16 = 0x0112;
const STRIP_BYTE_COUNTS: u16 = 0x0117;
const SUB_IFDS: u16 = 0x014A;
const JPEG_OFFSET: u16 = 0x0201;
const JPEG_LENGTH: u16 = 0x0202;

const SHORT: u16 = 3;
const LONG: u16 = 4;
const UNDEFINED: u16 = 7;
const IFD: u16 = 13;

/// Bounds on the walk so a corrupt or cyclic file cannot stall it. Real
/// raws have a handful of IFDs with well under 100 entries each.
const MAX_IFDS: usize = 32;
const MAX_ENTRIES: u64 = 1024;
const MAX_SUB_IFDS: u32 = 16;

struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: [u8; 4],
}

struct Tiff<'r, R> {
    reader: &'r mut R,
    file_len: u64,
    big_endian: bool,
}

impl<R: Read + Seek> Tiff<'_, R> {
    fn u16(&self, bytes: &[u8]) -> u16 {
        let bytes = [bytes[0], bytes[1]];
        if self.big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        }
    }

    fn u32(&self, bytes: &[u8]) -> u32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    }

    /// The entries of the IFD at `offset`, and the offset of the next IFD.
    fn ifd(&mut self, offset: u64) -> Result<(Vec<Entry>, u64)> {
        let mut count = [0; 2];
        let n = read_up_to(self.reader, offset, &mut count)?;
        ensure!(n == 2, "IFD at byte {offset} is truncated");
        let count = u64::from(self.u16(&count));
        ensure!(count <= MAX_ENTRIES, "IFD at byte {offset} is corrupt");
        let len = count * 12 + 4;
        ensure!(
            offset + 2 + len <= self.file_len,
            "IFD at byte {offset} is truncated"
        );
        let bytes = read_at(self.reader, offset + 2, len)?;
        let (entries, next) = bytes.split_at(bytes.len() - 4);
        let entries = entries
            .chunks_exact(12)
            .map(|e| Entry {
                tag: self.u16(&e[0..2]),
                kind: self.u16(&e[2..4]),
                count: self.u32(&e[4..8]),
                value: [e[8], e[9], e[10], e[11]],
            })
            .collect();
        Ok((entries, self.u32(next).into()))
    }

    /// The value of a single SHORT or LONG entry.
    fn single(&self, entry: &Entry) -> Option<u64> {
        match (entry.kind, entry.count) {
            (SHORT, 1) => Some(self.u16(&entry.value).into()),
            (LONG | IFD, 1) => Some(self.u32(&entry.value).into()),
            _ => None,
        }
    }

    /// Up to `max` values of a LONG or IFD array entry, such as SubIFDs.
    fn offsets(&mut self, entry: &Entry, max: u32) -> Result<Vec<u64>> {
        if !matches!(entry.kind, LONG | IFD) {
            return Ok(Vec::new());
        }
        let count = entry.count.min(max);
        let bytes = if count <= 1 {
            entry.value[..4 * count as usize].to_vec()
        } else {
            let offset = self.u32(&entry.value).into();
            let len = 4 * u64::from(count);
            ensure!(offset + len <= self.file_len, "SubIFD list is truncated");
            read_at(self.reader, offset, len)?
        };
        Ok(bytes.chunks_exact(4).map(|b| self.u32(b).into()).collect())
    }
}

pub(super) fn locate<R: Read + Seek>(reader: &mut R, file_len: u64) -> Result<Located> {
    let mut header = [0; 8];
    ensure!(
        read_up_to(reader, 0, &mut header)? == header.len(),
        "TIFF header is truncated"
    );
    let mut tiff = Tiff {
        reader,
        file_len,
        big_endian: header[0] == b'M',
    };
    let ifd0 = u64::from(tiff.u32(&header[4..8]));

    let mut located = Located::default();
    let mut pending = vec![ifd0];
    let mut seen = Vec::new();
    while let Some(offset) = pending.pop() {
        if offset == 0 || seen.contains(&offset) || seen.len() == MAX_IFDS {
            continue;
        }
        seen.push(offset);
        // IFD0 must parse; past that, a bad link only loses what it led to.
        let (entries, next) = match tiff.ifd(offset) {
            Ok(ifd) => ifd,
            Err(err) if offset == ifd0 => return Err(err),
            Err(_) => continue,
        };
        let get = |tag| entries.iter().find(|e| e.tag == tag);
        let value = |tag| get(tag).and_then(|e| tiff.single(e));

        if let (Some(start), Some(len)) = (value(JPEG_OFFSET), value(JPEG_LENGTH)) {
            located.ranges.push(start..start + len);
        }
        if let (Some(6 | 7), Some(start), Some(len)) = (
            value(COMPRESSION),
            value(STRIP_OFFSETS),
            value(STRIP_BYTE_COUNTS),
        ) {
            located.ranges.push(start..start + len);
        }
        if let Some(e) = get(PANASONIC_JPG_FROM_RAW).filter(|e| e.kind == UNDEFINED && e.count > 4)
        {
            let start = u64::from(tiff.u32(&e.value));
            located.ranges.push(start..start + u64::from(e.count));
        }
        if offset == ifd0 {
            located.orientation = value(ORIENTATION)
                .and_then(|v| u8::try_from(v).ok())
                .and_then(Orientation::from_exif);
        }
        let sub_ifds = match get(SUB_IFDS) {
            Some(e) => tiff.offsets(e, MAX_SUB_IFDS).unwrap_or_default(),
            None => Vec::new(),
        };
        pending.push(next);
        pending.extend(sub_ifds.into_iter().rev());
    }
    Ok(located)
}

#[cfg(test)]
mod tests {
    use super::super::{find_previews, fixtures, Format};
    use super::*;
    use std::io::Cursor;

    fn previews(data: &[u8]) -> Result<super::super::Previews> {
        find_previews(&mut Cursor::new(data), Format::Tiff, 1280)
    }

    fn sizes(data: &[u8]) -> Vec<(u32, u32)> {
        previews(data)
            .unwrap()
            .jpegs
            .iter()
            .map(|j| (j.width, j.height))
            .collect()
    }

    #[test]
    fn cr2_full_size_strip_and_thumbnail() {
        let cr2 = fixtures::cr2(&fixtures::jpeg(300, 200), &fixtures::jpeg(160, 120), 6);
        assert_eq!(sizes(&cr2), [(300, 200), (160, 120)]);
        assert_eq!(
            previews(&cr2).unwrap().orientation,
            Some(Orientation::Rotate90)
        );
    }

    #[test]
    fn nef_big_endian_sub_ifd_preview() {
        let nef = fixtures::nef(&fixtures::jpeg(300, 200), 3);
        assert_eq!(&nef[..4], b"MM\0*");
        assert_eq!(sizes(&nef), [(300, 200)]);
        assert_eq!(
            previews(&nef).unwrap().orientation,
            Some(Orientation::Rotate180)
        );
    }

    #[test]
    fn rw2_jpg_from_raw() {
        let rw2 = fixtures::rw2(&fixtures::jpeg(300, 200));
        assert_eq!(&rw2[..4], b"IIU\0");
        assert_eq!(sizes(&rw2), [(300, 200)]);
        assert_eq!(previews(&rw2).unwrap().orientation, None);
    }

    #[test]
    fn survives_cycles_and_bad_links_but_not_a_bad_ifd0() {
        let jpeg = fixtures::jpeg(64, 48);
        let mut tiff = fixtures::Tiff::new(false);
        let blob = tiff.blob(&jpeg);
        tiff.ifds = vec![vec![
            (JPEG_OFFSET, fixtures::Value::Offset(blob)),
            (JPEG_LENGTH, fixtures::Value::Long(jpeg.len() as u32)),
            // SubIFDs pointing at itself and past the end of the file.
            (SUB_IFDS, fixtures::Value::SubIfds(vec![0])),
        ]];
        let mut data = tiff.build();
        let ifd0 = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let next_at = data.len() - 4;
        data[next_at..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            locate(&mut Cursor::new(&data), data.len() as u64)
                .unwrap()
                .ranges
                .len(),
            1
        );
        assert_eq!(sizes(&data), [(64, 48)]);

        let past_end = data.len() as u32;
        data[4..8].copy_from_slice(&past_end.to_le_bytes());
        let err = previews(&data).unwrap_err().to_string();
        assert!(err.contains("is truncated"), "{err}");

        data[4..8].copy_from_slice(&ifd0.to_le_bytes());
        data[ifd0 as usize..ifd0 as usize + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        let err = previews(&data).unwrap_err().to_string();
        assert!(err.contains("is corrupt"), "{err}");
    }

    #[test]
    fn ignores_uncompressed_strips() {
        let jpeg = fixtures::jpeg(64, 48);
        let mut tiff = fixtures::Tiff::new(true);
        let blob = tiff.blob(&jpeg);
        tiff.ifds = vec![vec![
            // Uncompressed, though the strip happens to hold a JPEG.
            (COMPRESSION, fixtures::Value::Short(1)),
            (STRIP_OFFSETS, fixtures::Value::Offset(blob)),
            (STRIP_BYTE_COUNTS, fixtures::Value::Long(jpeg.len() as u32)),
        ]];
        let err = previews(&tiff.build()).unwrap_err();
        assert_eq!(err.to_string(), "no embedded JPEG preview found");
    }
}
