//! Canon CR3: an ISO base media file (the MP4 container). The parts that
//! matter here, per <https://github.com/lclevy/canon_cr3>:
//!
//! ```text
//! ftyp                 major brand `crx `
//! moov
//!   uuid 85c0b687…     Canon metadata
//!     CMT1             EXIF IFD0 as a TIFF file, which holds the orientation
//!     THMB             160x120 JPEG thumbnail
//!   trak               the first track's one sample is the full-size JPEG
//!   trak …             raw image data, metadata
//! uuid eaf42b5e…       preview
//!   PRVW               1620x1080 JPEG
//! mdat                 the track samples
//! ```
//!
//! Only `moov` and the head of the preview box are read; `mdat` is skipped.

use std::{
    io::{Read, Seek},
    ops::Range,
};

use anyhow::{bail, ensure, Result};
use image::metadata::Orientation;

use super::{read_at, read_up_to, Located, MAX_READ_BYTES};

const CANON_UUID: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
const PREVIEW_UUID: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// Enough of the preview box to find the PRVW header and the JPEG's start.
const PREVIEW_HEAD_BYTES: usize = 128;

/// A box header: `size` covers the header too.
struct Header {
    kind: [u8; 4],
    header_len: u64,
    size: u64,
}

/// Parse the box header at the start of `bytes`, of which `available` bytes
/// remain in the enclosing box or file.
fn parse_header(bytes: &[u8], available: u64) -> Result<Header> {
    ensure!(bytes.len() >= 8, "box header is truncated");
    let size32 = u32::from_be_bytes(bytes[0..4].try_into()?);
    let kind: [u8; 4] = bytes[4..8].try_into()?;
    let (header_len, size) = match size32 {
        0 => (8, available),
        1 => {
            ensure!(bytes.len() >= 16, "box header is truncated");
            (16, u64::from_be_bytes(bytes[8..16].try_into()?))
        }
        n => (8, u64::from(n)),
    };
    let name = String::from_utf8_lossy(&kind);
    ensure!(size >= header_len, "`{name}` box has an invalid size");
    ensure!(size <= available, "`{name}` box is truncated");
    Ok(Header {
        kind,
        header_len,
        size,
    })
}

struct Child<'a> {
    kind: [u8; 4],
    body: &'a [u8],
    /// Absolute file offset of `body`.
    offset: u64,
}

/// The boxes laid end to end in `data`, which starts at file offset `offset`.
fn children(data: &[u8], offset: u64) -> Result<Vec<Child<'_>>> {
    let mut boxes = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let rest = &data[pos..];
        let header = parse_header(rest, rest.len() as u64)?;
        let (start, end) = (header.header_len as usize, header.size as usize);
        boxes.push(Child {
            kind: header.kind,
            body: &rest[start..end],
            offset: offset + (pos + start) as u64,
        });
        pos += end;
    }
    Ok(boxes)
}

fn find<'a, 'b>(boxes: &'b [Child<'a>], kind: &[u8; 4]) -> Option<&'b Child<'a>> {
    boxes.iter().find(|b| &b.kind == kind)
}

pub(super) fn locate<R: Read + Seek>(reader: &mut R, file_len: u64) -> Result<Located> {
    let mut located = Located::default();
    let mut pos = 0;
    while pos < file_len {
        let mut head = [0; 16 + 16];
        let n = read_up_to(reader, pos, &mut head)?;
        let header = parse_header(&head[..n], file_len - pos)?;
        let body_start = pos + header.header_len;
        let body_len = header.size - header.header_len;
        match &header.kind {
            b"ftyp" if pos == 0 => {}
            _ if pos == 0 => bail!("not a CR3 file"),
            b"moov" => {
                ensure!(
                    body_len <= MAX_READ_BYTES,
                    "`moov` box is implausibly large"
                );
                let body = read_at(reader, body_start, body_len)?;
                moov(&body, body_start, &mut located)?;
            }
            b"uuid" => {
                let uuid_end = (header.header_len + 16) as usize;
                if body_len >= 16
                    && n >= uuid_end
                    && head[header.header_len as usize..uuid_end] == PREVIEW_UUID
                {
                    let start = body_start + 16;
                    let mut head = [0; PREVIEW_HEAD_BYTES];
                    let len = read_up_to(reader, start, &mut head)?;
                    let len = len.min(usize::try_from(body_len - 16).unwrap_or(usize::MAX));
                    located
                        .ranges
                        .extend(preview(&head[..len], start, pos + header.size));
                }
            }
            _ => {}
        }
        pos += header.size;
    }
    Ok(located)
}

fn moov(body: &[u8], offset: u64, located: &mut Located) -> Result<()> {
    for child in children(body, offset)? {
        match &child.kind {
            b"uuid" if child.body.starts_with(&CANON_UUID) => {
                for meta in children(&child.body[16..], child.offset + 16)? {
                    match &meta.kind {
                        b"CMT1" => located.orientation = Orientation::from_exif_chunk(meta.body),
                        b"THMB" => located.ranges.extend(jpeg_in(
                            meta.body,
                            meta.offset,
                            meta.offset + meta.body.len() as u64,
                        )),
                        _ => {}
                    }
                }
            }
            b"trak" => located
                .ranges
                .extend(first_sample(child.body, child.offset)?),
            _ => {}
        }
    }
    Ok(())
}

/// The byte range of a track's first sample, from its sample size (`stsz`)
/// and chunk offset (`co64`/`stco`) tables.
fn first_sample(trak: &[u8], offset: u64) -> Result<Option<Range<u64>>> {
    let mut boxes = children(trak, offset)?;
    for kind in [b"mdia", b"minf", b"stbl"] {
        let Some(child) = find(&boxes, kind) else {
            return Ok(None);
        };
        boxes = children(child.body, child.offset)?;
    }
    let size = find(&boxes, b"stsz").and_then(|stsz| match be_u32(stsz.body, 4)? {
        0 => be_u32(stsz.body, 12),
        size => Some(size),
    });
    let start = match (find(&boxes, b"co64"), find(&boxes, b"stco")) {
        (Some(co64), _) => be_u64(co64.body, 8),
        (None, Some(stco)) => be_u32(stco.body, 8).map(u64::from),
        _ => None,
    };
    Ok(start
        .zip(size)
        .map(|(start, size)| start..start.saturating_add(size.into())))
}

/// The JPEG in the preview box, given the first bytes of its body (after the
/// UUID) at file offset `offset`. The PRVW box follows a few header bytes.
fn preview(head: &[u8], offset: u64, end: u64) -> Option<Range<u64>> {
    let at = head.windows(4).position(|w| w == b"PRVW")?.checked_sub(4)?;
    let header = parse_header(&head[at..], end - offset - at as u64).ok()?;
    let body = at + header.header_len as usize;
    let body_end = offset + (at as u64) + header.size;
    jpeg_in(head.get(body..)?, offset + body as u64, body_end)
}

/// The JPEG inside a THMB or PRVW box body that starts at `offset` and ends
/// at `end`. Their header fields differ by version, so look for the JPEG's
/// start-of-image marker rather than trusting a fixed offset.
fn jpeg_in(body: &[u8], offset: u64, end: u64) -> Option<Range<u64>> {
    let start = body
        .windows(3)
        .take(64)
        .position(|w| w == [0xFF, 0xD8, 0xFF])?;
    Some(offset + start as u64..end)
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::super::{find_previews, fixtures, Format};
    use super::*;
    use std::io::Cursor;

    fn locate_bytes(data: &[u8]) -> Result<Located> {
        locate(&mut Cursor::new(data), data.len() as u64)
    }

    fn full_cr3() -> fixtures::Cr3 {
        fixtures::Cr3 {
            thumb: Some(fixtures::jpeg(160, 120)),
            preview: Some(fixtures::jpeg(324, 216)),
            full: Some(fixtures::jpeg(600, 400)),
            orientation: Some(8),
        }
    }

    #[test]
    fn finds_all_three_jpegs_and_the_orientation() {
        let cr3 = full_cr3();
        let data = cr3.build();
        let located = locate_bytes(&data).unwrap();
        assert_eq!(located.orientation, Some(Orientation::Rotate270));

        let found: Vec<&[u8]> = located
            .ranges
            .iter()
            .map(|r| &data[r.start as usize..r.end as usize])
            .collect();
        // The raw track's sample is located too, and dropped later because
        // it is not a JPEG. THMB and PRVW ranges include trailing padding.
        assert_eq!(found.len(), 4, "THMB, full-size, raw track, PRVW");
        assert!(found[0].starts_with(cr3.thumb.as_ref().unwrap()));
        assert_eq!(found[1], cr3.full.as_ref().unwrap().as_slice());
        assert_eq!(found[2], fixtures::CR3_RAW_TRACK);
        assert!(found[3].starts_with(cr3.preview.as_ref().unwrap()));
    }

    #[test]
    fn works_with_only_some_previews() {
        let only_full = fixtures::Cr3 {
            full: Some(fixtures::jpeg(300, 200)),
            ..Default::default()
        }
        .build();
        let previews = find_previews(&mut Cursor::new(&only_full), Format::Cr3, 1280).unwrap();
        assert_eq!(previews.jpegs.len(), 1);
        assert_eq!(previews.orientation, None);

        let only_prvw = fixtures::Cr3 {
            preview: Some(fixtures::jpeg(300, 200)),
            ..Default::default()
        }
        .build();
        let previews = find_previews(&mut Cursor::new(&only_prvw), Format::Cr3, 1280).unwrap();
        assert_eq!(
            (previews.jpegs[0].width, previews.jpegs[0].height),
            (300, 200)
        );
    }

    #[test]
    fn parses_64_bit_and_to_end_of_file_box_sizes() {
        let jpeg = fixtures::jpeg(32, 24);
        let mut data = fixtures::cr3_box(b"ftyp", b"crx \0\0\0\x01crx isom");
        // A `uuid` with a 64-bit size, then a size-0 (to end of file) box.
        let mut body = PREVIEW_UUID.to_vec();
        body.extend_from_slice(&[0; 8]);
        body.extend(fixtures::cr3_box(
            b"PRVW",
            &[&[0u8; 16][..], &jpeg[..]].concat(),
        ));
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(b"uuid");
        data.extend_from_slice(&(16 + body.len() as u64).to_be_bytes());
        data.extend(body);
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&[0xAB; 100]);

        let located = locate_bytes(&data).unwrap();
        assert_eq!(located.ranges.len(), 1);
        let range = &located.ranges[0];
        assert_eq!(&data[range.start as usize..range.end as usize], jpeg);
    }

    #[test]
    fn truncated_or_corrupt_files_fail_cleanly() {
        let data = full_cr3().build();

        let cut_in_mdat = &data[..data.len() - 10];
        let err = locate_bytes(cut_in_mdat).unwrap_err().to_string();
        assert!(err.contains("`mdat` box is truncated"), "{err}");

        let moov_at = data.windows(4).position(|w| w == b"moov").unwrap();
        let err = locate_bytes(&data[..moov_at + 200])
            .unwrap_err()
            .to_string();
        assert!(err.contains("`moov` box is truncated"), "{err}");

        let mut bad_size = data.clone();
        bad_size[moov_at - 4..moov_at].copy_from_slice(&3u32.to_be_bytes());
        let err = locate_bytes(&bad_size).unwrap_err().to_string();
        assert!(err.contains("invalid size"), "{err}");

        let mut bad_child = data.clone();
        let cmt1_at = data.windows(4).position(|w| w == b"CMT1").unwrap();
        bad_child[cmt1_at - 4..cmt1_at].copy_from_slice(&u32::MAX.to_be_bytes());
        let err = locate_bytes(&bad_child).unwrap_err().to_string();
        assert!(err.contains("`CMT1` box is truncated"), "{err}");

        let mut not_ftyp = data.clone();
        not_ftyp[4..8].copy_from_slice(b"free");
        let err = locate_bytes(&not_ftyp).unwrap_err().to_string();
        assert_eq!(err, "not a CR3 file");

        let ftyp_only = &data[..data.windows(4).position(|w| w == b"moov").unwrap() - 4];
        let err = find_previews(&mut Cursor::new(ftyp_only), Format::Cr3, 1280).unwrap_err();
        assert_eq!(err.to_string(), "no embedded JPEG preview found");
    }
}
