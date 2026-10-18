//! Camera raw support. Raw sensor data needs a demosaicing pipeline per camera
//! model, but every raw file also embeds JPEGs the camera rendered for its own
//! screen. snapsort classifies from those: it locates them in the container,
//! reads only the one it needs, and never touches the multi-megabyte raw data.
//!
//! Supported containers:
//! - CR3 (Canon), an ISO base media file; see [`cr3`].
//! - TIFF-based raws: CR2 (Canon), NEF (Nikon), ARW (Sony), DNG and RW2
//!   (Panasonic); see [`tiff`].
//! - RAF (Fujifilm); see [`raf`].

mod cr3;
mod raf;
mod tiff;

use std::{
    io::{Read, Seek, SeekFrom},
    ops::Range,
};

use anyhow::{bail, Context, Result};
use image::metadata::Orientation;

/// Upper bound for anything read into memory from a raw file. Real previews
/// and metadata boxes are far smaller; a larger size means a corrupt field.
const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Cr3,
    Tiff,
    Raf,
}

impl Format {
    /// Identify a raw container from the first 16 bytes of a file.
    pub fn sniff(magic: &[u8]) -> Option<Format> {
        match magic {
            [_, _, _, _, b'f', b't', b'y', b'p', b'c', b'r', b'x', b' ', ..] => Some(Format::Cr3),
            // Panasonic's RW2 uses 0x55 where TIFF has 42.
            [b'I', b'I', 42 | 0x55, 0, ..] | [b'M', b'M', 0, 42, ..] => Some(Format::Tiff),
            _ if magic.starts_with(b"FUJIFILMCCD-RAW") => Some(Format::Raf),
            _ => None,
        }
    }
}

/// What a container parser found: candidate JPEG byte ranges (not yet
/// checked) and the orientation recorded in the raw's own metadata.
#[derive(Debug, Default)]
struct Located {
    ranges: Vec<Range<u64>>,
    orientation: Option<Orientation>,
}

/// A decodable JPEG embedded in a raw file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedJpeg {
    pub range: Range<u64>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub struct Previews {
    /// Best first for the requested preview size; never empty.
    pub jpegs: Vec<EmbeddedJpeg>,
    /// Orientation from the raw's metadata. Embedded JPEGs are stored in
    /// sensor orientation and usually carry no EXIF of their own.
    pub orientation: Option<Orientation>,
}

/// Find the JPEGs embedded in a raw file, ordered so the first is the
/// smallest one that still fills `max_dim` (or, if none does, the largest).
/// Canon's 1620px CR3 preview decodes far faster than its full-size JPEG and
/// is plenty for the default 1280px.
pub fn find_previews<R: Read + Seek>(
    reader: &mut R,
    format: Format,
    max_dim: u32,
) -> Result<Previews> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    let located = match format {
        Format::Cr3 => cr3::locate(reader, file_len)?,
        Format::Tiff => tiff::locate(reader, file_len)?,
        Format::Raf => raf::locate(reader)?,
    };

    let mut jpegs: Vec<EmbeddedJpeg> = Vec::new();
    for range in located.ranges {
        if range.start >= range.end
            || range.end > file_len
            || range.end - range.start > MAX_READ_BYTES
            || jpegs.iter().any(|j| j.range.start == range.start)
        {
            continue;
        }
        if let Some((width, height)) = jpeg_dimensions(reader, range.start) {
            jpegs.push(EmbeddedJpeg {
                range,
                width,
                height,
            });
        }
    }
    if jpegs.is_empty() {
        bail!("no embedded JPEG preview found");
    }
    jpegs.sort_by_key(|j| {
        let longest = i64::from(j.width.max(j.height));
        if longest >= i64::from(max_dim) {
            (false, longest)
        } else {
            (true, -longest)
        }
    });
    Ok(Previews {
        jpegs,
        orientation: located.orientation,
    })
}

pub fn read_jpeg<R: Read + Seek>(reader: &mut R, jpeg: &EmbeddedJpeg) -> Result<Vec<u8>> {
    read_at(reader, jpeg.range.start, jpeg.range.end - jpeg.range.start)
        .context("embedded JPEG is truncated")
}

/// Read `len` bytes at `offset`; `len` must already be bounded by the caller.
fn read_at<R: Read + Seek>(reader: &mut R, offset: u64, len: u64) -> Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0; usize::try_from(len)?];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read up to `buf.len()` bytes at `offset`, returning how many were read.
fn read_up_to<R: Read + Seek>(reader: &mut R, offset: u64, buf: &mut [u8]) -> Result<usize> {
    reader.seek(SeekFrom::Start(offset))?;
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(filled)
}

/// Width and height of the JPEG starting at `offset`, read from its frame
/// header without decoding. `None` unless it is a baseline, extended or
/// progressive JPEG: raws also store their sensor data as lossless JPEG (CR2,
/// DNG), which starts with the same marker but no image decoder handles.
fn jpeg_dimensions<R: Read + Seek>(reader: &mut R, offset: u64) -> Option<(u32, u32)> {
    let mut soi = [0; 2];
    if read_up_to(reader, offset, &mut soi).ok()? != 2 || soi != [0xFF, 0xD8] {
        return None;
    }
    let mut pos = offset + 2;
    // Real files reach the frame header within a handful of segments.
    for _ in 0..64 {
        let mut segment = [0; 9];
        let n = read_up_to(reader, pos, &mut segment).ok()?;
        if n < 4 || segment[0] != 0xFF {
            return None;
        }
        match segment[1] {
            // Fill byte before a marker.
            0xFF => {
                pos += 1;
                continue;
            }
            0xC0..=0xC2 if n == 9 => {
                let height = u16::from_be_bytes([segment[5], segment[6]]);
                let width = u16::from_be_bytes([segment[7], segment[8]]);
                return (width > 0 && height > 0).then(|| (width.into(), height.into()));
            }
            // Any other frame type, or image data before a frame header.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xD9 | 0xDA => return None,
            _ => pos += 2 + u64::from(u16::from_be_bytes([segment[2], segment[3]])),
        }
    }
    None
}

#[cfg(test)]
#[path = "../../tests/support/raw.rs"]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn previews(data: &[u8], max_dim: u32) -> Result<Previews> {
        let format = Format::sniff(data).expect("recognised raw");
        find_previews(&mut Cursor::new(data), format, max_dim)
    }

    fn sizes(previews: &Previews) -> Vec<(u32, u32)> {
        previews.jpegs.iter().map(|j| (j.width, j.height)).collect()
    }

    #[test]
    fn sniffs_containers_by_content() {
        let cr3 = fixtures::Cr3::default().build();
        assert_eq!(Format::sniff(&cr3), Some(Format::Cr3));
        assert_eq!(
            Format::sniff(b"II*\0\x10\0\0\0CR\x02\0"),
            Some(Format::Tiff)
        );
        assert_eq!(Format::sniff(b"MM\0*\0\0\0\x08"), Some(Format::Tiff));
        assert_eq!(Format::sniff(b"IIU\0\x18\0\0\0"), Some(Format::Tiff));
        assert_eq!(Format::sniff(b"FUJIFILMCCD-RAW 0201"), Some(Format::Raf));
        assert_eq!(Format::sniff(&fixtures::jpeg(8, 8)), None);
        assert_eq!(Format::sniff(b"\0\0\0\x18ftypheic"), None);
        assert_eq!(Format::sniff(b""), None);
    }

    #[test]
    fn prefers_the_smallest_jpeg_that_fills_max_dim() {
        let cr3 = fixtures::Cr3 {
            thumb: Some(fixtures::jpeg_header(160, 120)),
            preview: Some(fixtures::jpeg_header(1620, 1080)),
            full: Some(fixtures::jpeg_header(3000, 2000)),
            ..Default::default()
        }
        .build();
        assert_eq!(
            sizes(&previews(&cr3, 1280).unwrap()),
            [(1620, 1080), (3000, 2000), (160, 120)]
        );
        assert_eq!(
            sizes(&previews(&cr3, 2000).unwrap()),
            [(3000, 2000), (1620, 1080), (160, 120)]
        );
        assert_eq!(
            sizes(&previews(&cr3, 4000).unwrap()),
            [(3000, 2000), (1620, 1080), (160, 120)],
            "largest first when nothing is big enough"
        );
    }

    #[test]
    fn skips_lossless_jpeg_sensor_data() {
        let cr2 = fixtures::cr2(&fixtures::jpeg(64, 48), &fixtures::jpeg(16, 12), 1);
        // The fixture's raw IFD holds a lossless JPEG claiming 6000x4000.
        assert_eq!(sizes(&previews(&cr2, 1280).unwrap()), [(64, 48), (16, 12)]);
    }

    #[test]
    fn reads_frame_headers_after_exif_segments() {
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x06, b'E', b'x', b'i', b'f'];
        jpeg.extend_from_slice(&fixtures::jpeg(40, 30)[2..]);
        assert_eq!(jpeg_dimensions(&mut Cursor::new(&jpeg), 0), Some((40, 30)));
        assert_eq!(jpeg_dimensions(&mut Cursor::new(&jpeg[..12]), 0), None);
        assert_eq!(jpeg_dimensions(&mut Cursor::new(b"\xFF\xD8"), 0), None);
        assert_eq!(jpeg_dimensions(&mut Cursor::new(b"not a jpeg"), 0), None);
    }

    #[test]
    fn rejects_out_of_range_and_non_jpeg_candidates() {
        let raf = fixtures::raf_with_range(&fixtures::jpeg(32, 32), 10_000, 50);
        let err = previews(&raf, 1280).unwrap_err();
        assert_eq!(err.to_string(), "no embedded JPEG preview found");
    }

    #[test]
    fn never_panics_on_truncated_files() {
        let files = [
            fixtures::Cr3 {
                thumb: Some(fixtures::jpeg(160, 120)),
                preview: Some(fixtures::jpeg(320, 240)),
                full: Some(fixtures::jpeg(640, 480)),
                orientation: Some(6),
            }
            .build(),
            fixtures::cr2(&fixtures::jpeg(64, 48), &fixtures::jpeg(16, 12), 8),
            fixtures::nef(&fixtures::jpeg(64, 48), 3),
            fixtures::rw2(&fixtures::jpeg(64, 48)),
            fixtures::raf(&fixtures::jpeg(64, 48)),
        ];
        for file in &files {
            let format = Format::sniff(file).unwrap();
            for len in 0..file.len() {
                let _ = find_previews(&mut Cursor::new(&file[..len]), format, 1280);
            }
        }
    }
}
