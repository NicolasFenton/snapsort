//! In-memory preview generation. Runs on the blocking threadpool: decoding and
//! resizing large photos is CPU-bound and must not stall the async runtime.

use std::{
    borrow::Cow,
    fs::File,
    io::{Cursor, Read, Seek},
    path::Path,
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{
    codecs::jpeg::JpegEncoder, imageops::FilterType, metadata::Orientation, DynamicImage,
    ExtendedColorType, GenericImageView, ImageDecoder, ImageReader, RgbImage,
};

use crate::raw;

/// SubQ rejects images larger than 4 MiB once base64-decoded.
pub const MAX_PREVIEW_BYTES: usize = 4 * 1024 * 1024;

/// Floors for the fallback loop that shrinks oversized previews.
const MIN_FALLBACK_QUALITY: u8 = 40;
const MIN_FALLBACK_DIM: u32 = 256;

#[derive(Debug, Clone, Copy)]
pub struct PreviewOptions {
    pub max_dim: u32,
    pub quality: u8,
}

#[derive(Debug)]
pub struct Preview {
    /// Raw single-line base64 JPEG (no `data:` URL prefix).
    pub base64: String,
}

/// Read, decode, downscale and JPEG-encode `path`. The file on disk is only read.
pub fn build(path: &Path, opts: PreviewOptions) -> Result<Preview> {
    let mut file = File::open(path).context("failed to read file")?;
    let image = load(&mut file, opts.max_dim)?;
    let jpeg = encode_jpeg(&image, opts)?;
    Ok(Preview {
        base64: STANDARD.encode(jpeg),
    })
}

/// Decode by sniffing the content (not the extension). Camera raws are
/// decoded from a JPEG they embed, reading only that part of the file.
fn load<R: Read + Seek>(reader: &mut R, max_dim: u32) -> Result<DynamicImage> {
    let mut magic = Vec::with_capacity(16);
    reader
        .by_ref()
        .take(16)
        .read_to_end(&mut magic)
        .and_then(|_| reader.rewind())
        .context("failed to read file")?;
    if let Some(format) = raw::Format::sniff(&magic) {
        return decode_raw(reader, format, max_dim).context("failed to decode raw image");
    }
    let mut data = Vec::new();
    reader
        .read_to_end(&mut data)
        .context("failed to read file")?;
    decode(&data).context("failed to decode image")
}

/// Decode the embedded JPEG best suited to `max_dim`, falling back to the
/// others if it is damaged.
fn decode_raw<R: Read + Seek>(
    reader: &mut R,
    format: raw::Format,
    max_dim: u32,
) -> Result<DynamicImage> {
    let previews = raw::find_previews(reader, format, max_dim)?;
    let mut first_error = None;
    for jpeg in &previews.jpegs {
        match raw::read_jpeg(reader, jpeg).and_then(|bytes| decode_unoriented(&bytes)) {
            Ok((mut image, embedded)) => {
                image.apply_orientation(previews.orientation.unwrap_or(embedded));
                return Ok(image);
            }
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| anyhow!("no embedded JPEG preview found")))
}

/// Decode and apply EXIF orientation so portrait phone photos are not sent
/// sideways.
pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let (mut image, orientation) = decode_unoriented(data)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// Decode without rotating, returning the EXIF orientation to apply.
fn decode_unoriented(data: &[u8]) -> Result<(DynamicImage, Orientation)> {
    let mut decoder = ImageReader::new(Cursor::new(data))
        .with_guessed_format()?
        .into_decoder()
        .map_err(flatten_image_error)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let image = DynamicImage::from_decoder(decoder).map_err(flatten_image_error)?;
    Ok((image, orientation))
}

/// `ImageError`'s message already embeds its source, and some decoder
/// messages span lines; collapse to one line so `{:#}` doesn't repeat it.
fn flatten_image_error(err: image::ImageError) -> anyhow::Error {
    let message = err.to_string();
    anyhow::anyhow!(
        "{}",
        message.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

/// Encode a JPEG preview that fits within `max_dim` and is strictly under
/// [`MAX_PREVIEW_BYTES`]. If the requested quality and size are too large,
/// quality is lowered first, then dimensions.
pub fn encode_jpeg(image: &DynamicImage, opts: PreviewOptions) -> Result<Vec<u8>> {
    let mut max_dim = opts.max_dim;
    let mut quality = opts.quality.clamp(1, 100);
    loop {
        let rgb = downscale_to_rgb(image, max_dim);
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                ExtendedColorType::Rgb8,
            )
            .context("failed to encode JPEG preview")?;

        if bytes.len() < MAX_PREVIEW_BYTES {
            return Ok(bytes);
        }

        let longest = rgb.width().max(rgb.height());
        if quality > MIN_FALLBACK_QUALITY {
            quality = quality.saturating_sub(15).max(MIN_FALLBACK_QUALITY);
        } else if longest > MIN_FALLBACK_DIM {
            max_dim = (longest * 3 / 4).max(MIN_FALLBACK_DIM);
        } else {
            bail!("could not compress the preview below 4 MiB");
        }
    }
}

fn downscale_to_rgb(image: &DynamicImage, max_dim: u32) -> RgbImage {
    let (width, height) = image.dimensions();
    let scaled = if width > max_dim || height > max_dim {
        // `resize` fits within the bounds while preserving aspect ratio.
        Cow::Owned(image.resize(max_dim, max_dim, FilterType::Triangle))
    } else {
        Cow::Borrowed(image)
    };
    flatten_to_rgb(&scaled)
}

/// JPEG has no alpha channel. Composite transparent pixels onto white instead
/// of letting them collapse to black, which would mislead the classifier.
fn flatten_to_rgb(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());
    for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
        let alpha = u16::from(src[3]);
        for c in 0..3 {
            dst[c] = ((u16::from(src[c]) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        }
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::fixtures;
    use image::{Rgb, Rgba, RgbaImage};

    fn load_bytes(data: &[u8], max_dim: u32) -> Result<DynamicImage> {
        load(&mut Cursor::new(data), max_dim)
    }

    fn noisy(width: u32, height: u32) -> DynamicImage {
        // Deterministic noise defeats JPEG compression, for size-limit tests.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |_, _| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let b = state.to_le_bytes();
            Rgb([b[0], b[1], b[2]])
        }))
    }

    #[test]
    fn downscales_preserving_aspect_ratio() {
        let image = DynamicImage::ImageRgb8(RgbImage::new(4000, 3000));
        let jpeg = encode_jpeg(
            &image,
            PreviewOptions {
                max_dim: 1280,
                quality: 80,
            },
        )
        .unwrap();
        assert_eq!(decode(&jpeg).unwrap().dimensions(), (1280, 960));
    }

    #[test]
    fn never_upscales_small_images() {
        let image = DynamicImage::ImageRgb8(RgbImage::new(300, 200));
        let jpeg = encode_jpeg(
            &image,
            PreviewOptions {
                max_dim: 1280,
                quality: 80,
            },
        )
        .unwrap();
        assert_eq!(decode(&jpeg).unwrap().dimensions(), (300, 200));
    }

    #[test]
    fn stays_under_size_limit_even_with_extreme_settings() {
        let image = noisy(2600, 2600);
        let jpeg = encode_jpeg(
            &image,
            PreviewOptions {
                max_dim: 16384,
                quality: 100,
            },
        )
        .unwrap();
        assert!(jpeg.len() < MAX_PREVIEW_BYTES, "{} bytes", jpeg.len());
    }

    #[test]
    fn composites_transparency_onto_white() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 0])));
        assert_eq!(
            flatten_to_rgb(&image).get_pixel(0, 0),
            &Rgb([255, 255, 255])
        );
        let opaque = DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([10, 20, 30, 255])));
        assert_eq!(flatten_to_rgb(&opaque).get_pixel(0, 0), &Rgb([10, 20, 30]));
    }

    #[test]
    fn builds_raw_base64_without_data_url_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.png");
        noisy(64, 48).save(&path).unwrap();
        let preview = build(
            &path,
            PreviewOptions {
                max_dim: 1280,
                quality: 80,
            },
        )
        .unwrap();
        assert!(!preview.base64.starts_with("data:"));
        assert!(!preview.base64.contains('\n'));
        let bytes = STANDARD.decode(&preview.base64).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "preview must be a JPEG");
    }

    #[test]
    fn rejects_non_images() {
        assert!(decode(b"definitely not an image").is_err());
        assert!(load_bytes(b"definitely not an image", 1280).is_err());
    }

    #[test]
    fn decodes_raws_from_the_smallest_embedded_jpeg_that_fills_max_dim() {
        let cr3 = fixtures::Cr3 {
            thumb: Some(fixtures::jpeg(160, 120)),
            preview: Some(fixtures::jpeg(1620, 1080)),
            // A different aspect ratio, to tell which JPEG was decoded.
            full: Some(fixtures::jpeg(1800, 900)),
            orientation: None,
        }
        .build();
        assert_eq!(load_bytes(&cr3, 1280).unwrap().dimensions(), (1620, 1080));
        assert_eq!(load_bytes(&cr3, 1700).unwrap().dimensions(), (1800, 900));
    }

    #[test]
    fn rotates_raws_by_their_metadata_or_else_the_jpeg_exif() {
        let cr3 = fixtures::Cr3 {
            preview: Some(fixtures::jpeg_with_orientation(300, 200, 1)),
            orientation: Some(6),
            ..Default::default()
        }
        .build();
        assert_eq!(load_bytes(&cr3, 1280).unwrap().dimensions(), (200, 300));
        let cr2 = fixtures::cr2(&fixtures::jpeg(300, 200), &fixtures::jpeg(16, 12), 8);
        assert_eq!(load_bytes(&cr2, 1280).unwrap().dimensions(), (200, 300));
        let nef = fixtures::nef(&fixtures::jpeg(300, 200), 1);
        assert_eq!(load_bytes(&nef, 1280).unwrap().dimensions(), (300, 200));
        let raf = fixtures::raf(&fixtures::jpeg_with_orientation(300, 200, 6));
        assert_eq!(load_bytes(&raf, 1280).unwrap().dimensions(), (200, 300));
    }

    #[test]
    fn falls_back_to_another_embedded_jpeg_when_one_is_damaged() {
        let preview = fixtures::jpeg(1620, 1080);
        // Cut before the Huffman tables: the frame header still reads fine.
        let dht = preview.windows(2).position(|w| w == [0xFF, 0xC4]).unwrap();
        let cr3 = fixtures::Cr3 {
            preview: Some(preview[..dht].to_vec()),
            full: Some(fixtures::jpeg(300, 200)),
            ..Default::default()
        }
        .build();
        assert_eq!(load_bytes(&cr3, 1280).unwrap().dimensions(), (300, 200));
    }

    #[test]
    fn corrupt_raws_fail_with_a_clear_error() {
        let cr3 = fixtures::Cr3 {
            preview: Some(fixtures::jpeg(64, 48)),
            ..Default::default()
        }
        .build();
        let err = load_bytes(&cr3[..cr3.len() - 1], 1280).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "failed to decode raw image: `mdat` box is truncated"
        );
        let err = load_bytes(b"II*\0\xff\xff\0\0", 1280).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "failed to decode raw image: IFD at byte 65535 is truncated"
        );
    }

    #[test]
    fn builds_previews_for_raw_files_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "a.cr3",
                fixtures::Cr3 {
                    preview: Some(fixtures::jpeg(480, 320)),
                    orientation: Some(1),
                    ..Default::default()
                }
                .build(),
            ),
            (
                "b.cr2",
                fixtures::cr2(&fixtures::jpeg(480, 320), &fixtures::jpeg(160, 120), 1),
            ),
            ("c.nef", fixtures::nef(&fixtures::jpeg(480, 320), 1)),
            ("d.rw2", fixtures::rw2(&fixtures::jpeg(480, 320))),
            ("e.raf", fixtures::raf(&fixtures::jpeg(480, 320))),
        ];
        for (name, bytes) in files {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let preview = build(
                &path,
                PreviewOptions {
                    max_dim: 240,
                    quality: 80,
                },
            )
            .unwrap();
            let jpeg = STANDARD.decode(&preview.base64).unwrap();
            assert!(jpeg.len() < MAX_PREVIEW_BYTES, "{name}");
            assert_eq!(decode(&jpeg).unwrap().dimensions(), (240, 160), "{name}");
        }
    }

    /// Checks real camera files, which are too big to check in. Point
    /// `SNAPSORT_RAW_SAMPLES` at a folder of them (raw.pixls.us has CC0
    /// samples); set `SNAPSORT_RAW_PREVIEWS` to a folder to also save each
    /// preview for a visual check of rotation and content.
    #[test]
    #[ignore = "needs SNAPSORT_RAW_SAMPLES=<folder of real raw files>"]
    fn real_raw_samples() {
        let samples = std::env::var_os("SNAPSORT_RAW_SAMPLES").expect("SNAPSORT_RAW_SAMPLES");
        let save_to = std::env::var_os("SNAPSORT_RAW_PREVIEWS");
        let files = crate::scan::scan(Path::new(&samples), true, &[]).files;
        assert!(!files.is_empty(), "no supported files in {samples:?}");
        let opts = PreviewOptions {
            max_dim: 1280,
            quality: 80,
        };
        let mut failures = Vec::new();
        for path in &files {
            let name = path.file_name().unwrap().to_string_lossy();
            let mut file = File::open(path).unwrap();
            let mut magic = [0; 16];
            file.read_exact(&mut magic).unwrap();
            let format = raw::Format::sniff(&magic).expect("a raw container");
            let started = std::time::Instant::now();
            let found = raw::find_previews(&mut file, format, opts.max_dim);
            let preview = build(path, opts);
            let elapsed = started.elapsed();
            let jpegs = found.as_ref().map_or_else(
                |e| format!("{e:#}"),
                |p| {
                    let sizes: Vec<String> = p
                        .jpegs
                        .iter()
                        .map(|j| {
                            format!(
                                "{}x{} {}KB",
                                j.width,
                                j.height,
                                (j.range.end - j.range.start) / 1024
                            )
                        })
                        .collect();
                    format!("{} orientation {:?}", sizes.join(", "), p.orientation)
                },
            );
            match preview {
                Ok(preview) => {
                    let jpeg = STANDARD.decode(&preview.base64).unwrap();
                    let (w, h) = decode(&jpeg).unwrap().dimensions();
                    println!(
                        "{name}: {format:?} [{jpegs}] -> {w}x{h} {}KB in {elapsed:.0?}",
                        jpeg.len() / 1024
                    );
                    assert!(jpeg.len() < MAX_PREVIEW_BYTES);
                    if let Some(dir) = &save_to {
                        std::fs::write(Path::new(dir).join(format!("{name}.jpg")), jpeg).unwrap();
                    }
                }
                Err(err) => {
                    println!("{name}: {format:?} [{jpegs}] FAILED: {err:#}");
                    failures.push(name.into_owned());
                }
            }
        }
        assert!(failures.is_empty(), "failed: {failures:?}");
    }
}
