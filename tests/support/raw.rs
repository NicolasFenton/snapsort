//! Builders for synthetic camera raw files: the container structure snapsort
//! reads, wrapped around small real JPEGs. Shared by the unit tests in
//! src/raw and the end-to-end tests, so it depends only on the `image` crate.
#![allow(dead_code)]

use std::io::Cursor;

use image::{codecs::jpeg::JpegEncoder, ExtendedColorType, ImageReader, Rgb, RgbImage};

/// A gradient JPEG of the given size.
pub fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x * 255 / width) as u8, (y * 255 / height) as u8, 96])
    });
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 85)
        .encode(image.as_raw(), width, height, ExtendedColorType::Rgb8)
        .unwrap();
    bytes
}

/// A JPEG whose EXIF (APP1) segment records `orientation`.
pub fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
    let mut exif = Tiff::new(false);
    exif.ifds = vec![vec![(ORIENTATION, Value::Short(orientation))]];
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend(exif.build());
    let jpeg = jpeg(width, height);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend(app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// The header of a lossless JPEG, as CR2 and DNG use for sensor data:
/// a 6000x4000 frame that image decoders cannot handle.
pub const LOSSLESS_JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x0B, 0x0E, 0x0F, 0xA0, 0x17, 0x70, 0x01, 0x01, 0x11, 0x00, 0xFF,
    0xD9,
];

/// Just the frame header of a baseline JPEG: enough to be located and
/// measured, not decoded. Cheap to make at any size.
pub fn jpeg_header(width: u16, height: u16) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1, 0xFF, 0xD9]);
    out
}

/// For the THMB and PRVW header fields, which snapsort does not rely on.
fn dimensions(jpeg: &[u8]) -> (u16, u16) {
    ImageReader::new(Cursor::new(jpeg))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .map_or((0, 0), |(w, h)| (w as u16, h as u16))
}

// ----- CR3 ------------------------------------------------------------------

const CANON_UUID: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
const PREVIEW_UUID: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// The second track's sample: stands in for CRX-compressed sensor data.
pub const CR3_RAW_TRACK: &[u8] = b"CRX sensor data, which is not a JPEG";

pub fn cr3_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn pad4(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.len().next_multiple_of(4), 0);
}

/// A CR3 laid out like a camera's: ftyp, moov (Canon metadata with CMT1 and
/// THMB, a full-size JPEG track, a raw track), the PRVW preview box, mdat.
#[derive(Debug, Clone, Default)]
pub struct Cr3 {
    pub thumb: Option<Vec<u8>>,
    pub preview: Option<Vec<u8>>,
    pub full: Option<Vec<u8>>,
    /// EXIF orientation in CMT1.
    pub orientation: Option<u16>,
}

impl Cr3 {
    pub fn build(&self) -> Vec<u8> {
        let ftyp = cr3_box(b"ftyp", b"crx \0\0\0\x01crx isom");
        let preview = self.preview.as_deref().map(|jpeg| {
            let (width, height) = dimensions(jpeg);
            let mut prvw = 0u32.to_be_bytes().to_vec();
            for field in [1, width, height, 1] {
                prvw.extend_from_slice(&field.to_be_bytes());
            }
            prvw.extend_from_slice(&(jpeg.len() as u32).to_be_bytes());
            prvw.extend_from_slice(jpeg);
            pad4(&mut prvw);
            let mut body = PREVIEW_UUID.to_vec();
            body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
            body.extend(cr3_box(b"PRVW", &prvw));
            cr3_box(b"uuid", &body)
        });
        let preview = preview.unwrap_or_default();
        // The sample offsets don't change moov's size: measure, then rebuild.
        let mdat_data = ftyp.len() + self.moov(0).len() + preview.len() + 8;
        let moov = self.moov(mdat_data as u64);

        let full = self.full.as_deref().unwrap_or_default();
        let mdat = cr3_box(b"mdat", &[full, CR3_RAW_TRACK].concat());
        [ftyp, moov, preview, mdat].concat()
    }

    fn moov(&self, mdat_data: u64) -> Vec<u8> {
        let mut canon = CANON_UUID.to_vec();
        canon.extend(cr3_box(b"CNCV", b"CanonCR3_001/00.09.00/00.00.00"));
        if let Some(orientation) = self.orientation {
            let mut cmt1 = Tiff::new(false);
            cmt1.ifds = vec![vec![(0x0112, Value::Short(orientation))]];
            canon.extend(cr3_box(b"CMT1", &cmt1.build()));
        }
        if let Some(jpeg) = &self.thumb {
            let (width, height) = dimensions(jpeg);
            let mut thmb = 0u32.to_be_bytes().to_vec();
            thmb.extend_from_slice(&width.to_be_bytes());
            thmb.extend_from_slice(&height.to_be_bytes());
            thmb.extend_from_slice(&(jpeg.len() as u32).to_be_bytes());
            thmb.extend_from_slice(&[0, 1, 0, 0]);
            thmb.extend_from_slice(jpeg);
            pad4(&mut thmb);
            canon.extend(cr3_box(b"THMB", &thmb));
        }

        let mut moov = cr3_box(b"uuid", &canon);
        moov.extend(cr3_box(b"mvhd", &[0; 100]));
        let full_len = self.full.as_ref().map_or(0, Vec::len);
        if full_len > 0 {
            moov.extend(trak(mdat_data, full_len));
        }
        moov.extend(trak(mdat_data + full_len as u64, CR3_RAW_TRACK.len()));
        cr3_box(b"moov", &moov)
    }
}

fn trak(offset: u64, size: usize) -> Vec<u8> {
    let mut stsz = vec![0; 4];
    stsz.extend_from_slice(&(size as u32).to_be_bytes());
    stsz.extend_from_slice(&1u32.to_be_bytes());
    let mut co64 = vec![0; 4];
    co64.extend_from_slice(&1u32.to_be_bytes());
    co64.extend_from_slice(&offset.to_be_bytes());
    let stbl = [
        cr3_box(b"stsd", &[0; 16]),
        cr3_box(b"stsz", &stsz),
        cr3_box(b"co64", &co64),
    ]
    .concat();
    let minf = [cr3_box(b"vmhd", &[0; 12]), cr3_box(b"stbl", &stbl)].concat();
    let mdia = [
        cr3_box(b"mdhd", &[0; 24]),
        cr3_box(b"hdlr", &[0; 24]),
        cr3_box(b"minf", &minf),
    ]
    .concat();
    cr3_box(
        b"trak",
        &[cr3_box(b"tkhd", &[0; 84]), cr3_box(b"mdia", &mdia)].concat(),
    )
}

// ----- TIFF-based raws ------------------------------------------------------

pub enum Value {
    Short(u16),
    Long(u32),
    /// A LONG holding the file offset of `blobs[i]`.
    Offset(usize),
    /// UNDEFINED bytes: `blobs[i]` by offset and length.
    Undefined(usize),
    /// Offsets of other IFDs, by index. IFDs listed here are left out of
    /// the IFD0 → IFD1 → … chain.
    SubIfds(Vec<usize>),
}

/// A TIFF file: header, then blobs, then IFDs. `ifds[0]` is IFD0.
pub struct Tiff {
    pub big_endian: bool,
    pub magic: u16,
    pub ifds: Vec<Vec<(u16, Value)>>,
    pub blobs: Vec<Vec<u8>>,
}

impl Tiff {
    pub fn new(big_endian: bool) -> Self {
        Tiff {
            big_endian,
            magic: 42,
            ifds: Vec::new(),
            blobs: Vec::new(),
        }
    }

    /// Add a blob, returning its index for [`Value::Offset`].
    pub fn blob(&mut self, bytes: &[u8]) -> usize {
        self.blobs.push(bytes.to_vec());
        self.blobs.len() - 1
    }

    fn u16(&self, v: u16) -> [u8; 2] {
        if self.big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    }

    fn u32(&self, v: u32) -> [u8; 4] {
        if self.big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    }

    pub fn build(&self) -> Vec<u8> {
        let mut blob_offsets = Vec::new();
        let mut pos = 8;
        for blob in &self.blobs {
            blob_offsets.push(pos as u32);
            pos += blob.len().next_multiple_of(2);
        }
        let mut ifd_offsets = Vec::new();
        for ifd in &self.ifds {
            ifd_offsets.push(pos as u32);
            pos += 2 + 12 * ifd.len() + 4 + 4 * extra_longs(ifd);
        }
        let sub_ifds: Vec<usize> = self
            .ifds
            .iter()
            .flatten()
            .flat_map(|(_, v)| match v {
                Value::SubIfds(list) => list.clone(),
                _ => Vec::new(),
            })
            .collect();
        let chain: Vec<usize> = (0..self.ifds.len())
            .filter(|&i| i == 0 || !sub_ifds.contains(&i))
            .collect();

        let mut out = Vec::new();
        out.extend_from_slice(if self.big_endian { b"MM" } else { b"II" });
        out.extend(self.u16(self.magic));
        out.extend(self.u32(ifd_offsets.first().copied().unwrap_or(0)));
        for blob in &self.blobs {
            out.extend_from_slice(blob);
            out.resize(out.len().next_multiple_of(2), 0);
        }
        for (i, ifd) in self.ifds.iter().enumerate() {
            let mut extra = Vec::new();
            let extra_at = ifd_offsets[i] as usize + 2 + 12 * ifd.len() + 4;
            out.extend(self.u16(ifd.len() as u16));
            for (tag, value) in ifd {
                let (kind, count, field) = match value {
                    Value::Short(v) => {
                        let mut field = [0; 4];
                        field[..2].copy_from_slice(&self.u16(*v));
                        (3, 1, field)
                    }
                    Value::Long(v) => (4, 1, self.u32(*v)),
                    Value::Offset(blob) => (4, 1, self.u32(blob_offsets[*blob])),
                    Value::Undefined(blob) => (
                        7,
                        self.blobs[*blob].len() as u32,
                        self.u32(blob_offsets[*blob]),
                    ),
                    Value::SubIfds(list) if list.len() == 1 => {
                        (4, 1, self.u32(ifd_offsets[list[0]]))
                    }
                    Value::SubIfds(list) => {
                        let at = (extra_at + extra.len()) as u32;
                        for &ifd in list {
                            extra.extend(self.u32(ifd_offsets[ifd]));
                        }
                        (4, list.len() as u32, self.u32(at))
                    }
                };
                out.extend(self.u16(*tag));
                out.extend(self.u16(kind));
                out.extend(self.u32(count));
                out.extend(field);
            }
            let next = chain
                .iter()
                .position(|&c| c == i)
                .and_then(|p| chain.get(p + 1))
                .map_or(0, |&n| ifd_offsets[n]);
            out.extend(self.u32(next));
            out.extend(extra);
        }
        out
    }
}

fn extra_longs(ifd: &[(u16, Value)]) -> usize {
    ifd.iter()
        .map(|(_, v)| match v {
            Value::SubIfds(list) if list.len() > 1 => list.len(),
            _ => 0,
        })
        .sum()
}

const COMPRESSION: u16 = 0x0103;
const STRIP_OFFSETS: u16 = 0x0111;
const ORIENTATION: u16 = 0x0112;
const STRIP_BYTE_COUNTS: u16 = 0x0117;
const SUB_IFDS: u16 = 0x014A;
const JPEG_OFFSET: u16 = 0x0201;
const JPEG_LENGTH: u16 = 0x0202;

/// A CR2: IFD0 is the full-size JPEG as one old-style JPEG strip, IFD1 the
/// thumbnail, and the last IFD the lossless JPEG sensor data.
pub fn cr2(full: &[u8], thumb: &[u8], orientation: u16) -> Vec<u8> {
    let mut tiff = Tiff::new(false);
    let (full_blob, thumb_blob, raw_blob) =
        (tiff.blob(full), tiff.blob(thumb), tiff.blob(LOSSLESS_JPEG));
    tiff.ifds = vec![
        vec![
            (COMPRESSION, Value::Short(6)),
            (STRIP_OFFSETS, Value::Offset(full_blob)),
            (ORIENTATION, Value::Short(orientation)),
            (STRIP_BYTE_COUNTS, Value::Long(full.len() as u32)),
        ],
        vec![
            (JPEG_OFFSET, Value::Offset(thumb_blob)),
            (JPEG_LENGTH, Value::Long(thumb.len() as u32)),
        ],
        vec![
            (COMPRESSION, Value::Short(6)),
            (STRIP_OFFSETS, Value::Offset(raw_blob)),
            (STRIP_BYTE_COUNTS, Value::Long(LOSSLESS_JPEG.len() as u32)),
        ],
    ];
    tiff.build()
}

/// A big-endian NEF: IFD0 links two SubIFDs, the JPEG preview and the raw.
pub fn nef(preview: &[u8], orientation: u16) -> Vec<u8> {
    let mut tiff = Tiff::new(true);
    let (preview_blob, raw_blob) = (tiff.blob(preview), tiff.blob(b"Nikon compressed raw"));
    tiff.ifds = vec![
        vec![
            (ORIENTATION, Value::Short(orientation)),
            (SUB_IFDS, Value::SubIfds(vec![1, 2])),
        ],
        vec![
            (JPEG_OFFSET, Value::Offset(preview_blob)),
            (JPEG_LENGTH, Value::Long(preview.len() as u32)),
        ],
        vec![
            (COMPRESSION, Value::Short(34713)),
            (STRIP_OFFSETS, Value::Offset(raw_blob)),
            (STRIP_BYTE_COUNTS, Value::Long(20)),
        ],
    ];
    tiff.build()
}

/// A Panasonic RW2: TIFF with magic 0x55 and the JPEG in tag 0x2E.
pub fn rw2(jpeg: &[u8]) -> Vec<u8> {
    let mut tiff = Tiff::new(false);
    tiff.magic = 0x55;
    let blob = tiff.blob(jpeg);
    tiff.ifds = vec![vec![(0x002E, Value::Undefined(blob))]];
    tiff.build()
}

// ----- RAF ------------------------------------------------------------------

const RAF_HEADER_LEN: usize = 160;

/// A Fujifilm RAF: a fixed header whose bytes 84..92 locate the JPEG.
pub fn raf(jpeg: &[u8]) -> Vec<u8> {
    raf_with_range(jpeg, RAF_HEADER_LEN as u32, jpeg.len() as u32)
}

/// A RAF whose header points at `offset..offset + len`, wherever that is.
pub fn raf_with_range(jpeg: &[u8], offset: u32, len: u32) -> Vec<u8> {
    let mut out = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
    out.resize(84, 0);
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(&len.to_be_bytes());
    out.resize(RAF_HEADER_LEN, 0);
    out.extend_from_slice(jpeg);
    out.extend_from_slice(b"Fujifilm CFA sensor data");
    out
}
