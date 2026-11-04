//! Pixel and perceptual hashes, to find images that are the same picture but not the same file.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::decode::{
    decode_in_process, decode_with_ffmpeg, image_decoder, DecodeError, Decoder, Tool,
};
use crate::media::Dimensions;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::DynamicImage;
use image_hasher::{HashAlg, HasherConfig};
use ring::digest::{Context, SHA256};
use std::path::Path;

/// The hashes of one image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageHashes {
    /// SHA-256 of the decoded pixels as lowercase hex. Equal for copies that differ only in
    /// metadata.
    pub pixels: String,
    /// `None` for images with almost no detail (blank, black, a single color): their hashes are
    /// all alike, so they would all look similar to each other.
    pub visual: Option<Visual>,
    /// Size of the upright image.
    pub dimensions: Dimensions,
}

/// What similar images are found by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visual {
    /// 64-bit gradient hash of the upright image. Close (in Hamming distance) for resized and
    /// recompressed copies.
    pub phash: u64,
    /// The upright image as `FINGERPRINT_SIDE` x `FINGERPRINT_SIDE` grayscale pixels, to tell
    /// resized copies (nearly the same everywhere) from burst shots (different where something
    /// moved) among images with close perceptual hashes.
    pub fingerprint: Vec<u8>,
}

/// Version of the hash definitions. Bump it when `pixel_hash`, `perceptual_hash` or the
/// fingerprint in `hash_small` change, so every image is decoded again.
pub const HASH_VERSION: u32 = 2;

/// Side of the grayscale fingerprint in pixels.
pub const FINGERPRINT_SIDE: u32 = 32;

/// Longest side images are shrunk to before the perceptual hash, which only looks at 9 x 8
/// pixels anyway. Much faster and lighter than working at full size.
const PERCEPTUAL_SIZE: u32 = 256;

/// Images whose brightness varies less than this (standard deviation, 0 to 255) get no
/// perceptual hash.
const MIN_DETAIL: f64 = 3.0;

/// How an image is decoded for hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageDecoder<'t> {
    /// By the `image` crate, with the crate that decodes the format as `Decoder::InProcess`
    /// names it.
    InProcess(Option<&'static str>),
    /// By `ffmpeg` (HEIC, AVIF).
    Ffmpeg(&'t Tool),
}

impl<'t> ImageDecoder<'t> {
    /// The decoder for the image at `rel`, or `None` if there is no way to decode it: RAW
    /// formats, or HEIC and AVIF without `ffmpeg`.
    pub fn for_path(rel: &str, ffmpeg: Option<&'t Tool>) -> Option<Self> {
        match image_decoder(rel)? {
            Decoder::InProcess(format_crate) => Some(Self::InProcess(format_crate)),
            Decoder::Ffmpeg => ffmpeg.map(Self::Ffmpeg),
        }
    }

    /// The version images decoded so are hashed with: the hash definitions and the major and
    /// minor versions of the crates that decode the format and, for `ffmpeg`, of `ffmpeg`. An
    /// image hashed with another version is hashed again, since a different decoder can
    /// produce slightly different pixels.
    pub fn version(self) -> String {
        let base = format!(
            "{}; image {}; image_hasher {}",
            HASH_VERSION,
            env!("PIXRS_CRATE_image"),
            env!("PIXRS_CRATE_image_hasher")
        );
        match self {
            Self::Ffmpeg(ffmpeg) => format!(
                "{base}; ffmpeg {}; png {}",
                ffmpeg.version,
                env!("PIXRS_CRATE_png")
            ),
            Self::InProcess(Some(format_crate)) => format!("{base}; {format_crate}"),
            Self::InProcess(None) => base,
        }
    }

    /// Hash the image at `source`.
    ///
    /// The pixel hash covers the pixels as stored, before EXIF orientation, so a copy that only
    /// changed its rotation tag has the same pixel hash. Images decoded by `ffmpeg` come out
    /// upright already. The perceptual hash is of the upright image, so it matches copies with
    /// the rotation applied to the pixels.
    pub fn hash(self, source: &Path) -> Result<ImageHashes, DecodeError> {
        let (pixels, small, dimensions) = match self {
            Self::InProcess(_) => {
                let (img, orientation) = decode_in_process(source)?;
                let pixels = pixel_hash(&img);
                let dimensions = Dimensions {
                    width: img.width(),
                    height: img.height(),
                };
                // Shrink before turning upright, so the full-size image is never copied.
                let mut small = shrink(&img);
                drop(img);
                small.apply_orientation(orientation);
                let dimensions = if quarter_turn(orientation) {
                    dimensions.transposed()
                } else {
                    dimensions
                };
                (pixels, small, dimensions)
            }
            Self::Ffmpeg(ffmpeg) => {
                let img = decode_with_ffmpeg(&ffmpeg.path, source)?;
                let dimensions = Dimensions {
                    width: img.width(),
                    height: img.height(),
                };
                (pixel_hash(&img), shrink(&img), dimensions)
            }
        };
        Ok(ImageHashes {
            pixels,
            visual: hash_small(&small),
            dimensions,
        })
    }
}

/// Number of bits in which two perceptual hashes differ.
pub const fn distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// The perceptual hash and fingerprint of an upright image, as `hash_image` computes them;
/// `None` if the image has almost no detail.
pub fn perceptual_hash(upright: &DynamicImage) -> Option<Visual> {
    hash_small(&shrink(upright))
}

/// Largest fingerprint difference that still counts as a copy, see `fingerprints_match`. In
/// calibration it kept 99.6% of resized and recompressed copies and let in about 5% of burst
/// shots with close perceptual hashes.
pub const FINGERPRINT_LIMIT: u8 = 6;

/// Whether two images with close perceptual hashes are copies of the same picture: their
/// fingerprints differ little even at the 99th percentile pixel. Resized and recompressed
/// copies differ a little everywhere; burst shots differ a lot where something moved.
///
/// The same test as `fingerprint_differences` sorted and indexed at the 99th percentile, without
/// allocating or sorting: it runs for every pair of images with close perceptual hashes.
pub fn fingerprints_match(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let shift = mean(a) - mean(b);
    // The 99th percentile is at most the limit when no more pixels than lie above it in sorted
    // order exceed the limit.
    let allowed = a.len() - 1 - (a.len() * 99 / 100).min(a.len() - 1);
    let mut over = 0;
    for (&x, &y) in a.iter().zip(b) {
        if (f64::from(x) - f64::from(y) - shift).abs().round() > f64::from(FINGERPRINT_LIMIT) {
            over += 1;
            if over > allowed {
                return false;
            }
        }
    }
    true
}

/// The mean brightness of a fingerprint.
#[expect(clippy::cast_precision_loss, reason = "fingerprints have 1024 pixels")]
fn mean(fingerprint: &[u8]) -> f64 {
    fingerprint.iter().map(|&v| f64::from(v)).sum::<f64>() / fingerprint.len() as f64
}

/// How two fingerprints differ: the absolute differences of their pixels after subtracting each
/// fingerprint's mean brightness (so slightly different decoders or resizers do not count),
/// sorted ascending. Empty if the sizes differ.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "differences fit in a byte"
)]
pub fn fingerprint_differences(a: &[u8], b: &[u8]) -> Vec<u8> {
    if a.len() != b.len() || a.is_empty() {
        return vec![];
    }
    let shift = mean(a) - mean(b);
    let mut diffs: Vec<u8> = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            (f64::from(x) - f64::from(y) - shift)
                .abs()
                .round()
                .min(255.0) as u8
        })
        .collect();
    diffs.sort_unstable();
    diffs
}

/// Whether an orientation turns the image by 90 degrees, swapping width and height.
const fn quarter_turn(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// The image shrunk to fit `PERCEPTUAL_SIZE`, or a copy if it is that small already.
fn shrink(img: &DynamicImage) -> DynamicImage {
    if img.width() > PERCEPTUAL_SIZE || img.height() > PERCEPTUAL_SIZE {
        img.thumbnail(PERCEPTUAL_SIZE, PERCEPTUAL_SIZE)
    } else {
        img.clone()
    }
}

/// 64-bit gradient hash and grayscale fingerprint of a shrunk upright image, unless it has
/// almost no detail.
#[expect(
    clippy::cast_precision_loss,
    reason = "images have far fewer than 2^52 pixels"
)]
fn hash_small(small: &DynamicImage) -> Option<Visual> {
    let luma = small.to_luma8();
    let n = luma.len().max(1) as f64;
    let mean = luma.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let variance = luma
        .iter()
        .map(|&v| (f64::from(v) - mean).powi(2))
        .sum::<f64>()
        / n;
    if variance.sqrt() < MIN_DETAIL {
        return None;
    }
    let hasher = HasherConfig::new()
        .hash_alg(HashAlg::Gradient)
        .hash_size(8, 8)
        .to_hasher();
    let hash = hasher.hash_image(small);
    let bytes = *hash.as_bytes().first_chunk::<8>()?;
    let fingerprint = small
        .resize_exact(FINGERPRINT_SIDE, FINGERPRINT_SIDE, FilterType::Triangle)
        .to_luma8()
        .into_raw();
    Some(Visual {
        phash: u64::from_be_bytes(bytes),
        fingerprint,
    })
}

/// SHA-256 of the width, height and RGBA pixels, so the same picture hashes the same whatever
/// color type its file stored.
fn pixel_hash(img: &DynamicImage) -> String {
    let mut context = Context::new(&SHA256);
    context.update(&img.width().to_le_bytes());
    context.update(&img.height().to_le_bytes());
    match img {
        DynamicImage::ImageRgba8(buf) => context.update(buf.as_raw()),
        // The common case for photos: widen to RGBA in chunks instead of copying the image.
        DynamicImage::ImageRgb8(buf) => {
            let mut chunk = Vec::with_capacity(64 * 1024);
            for rgb in buf.as_raw().as_chunks::<3>().0 {
                chunk.extend_from_slice(rgb);
                chunk.push(u8::MAX);
                if chunk.len() >= 64 * 1024 {
                    context.update(&chunk);
                    chunk.clear();
                }
            }
            context.update(&chunk);
        }
        other => context.update(other.to_rgba8().as_raw()),
    }
    data_encoding::HEXLOWER.encode(context.finish().as_ref())
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "test fixtures with small values"
)]
pub(crate) mod tests {
    use super::*;

    use image::codecs::jpeg::JpegEncoder;
    use image::imageops::FilterType;
    use image::{Rgb, RgbImage};
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::tempdir;

    /// A photo-like test image: smooth gradients and a few shapes, different per `seed`.
    #[expect(
        clippy::suboptimal_flops,
        reason = "a fused multiply-add rounds differently, which would change the test images"
    )]
    pub fn picture(seed: u32, width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
            let s = seed as f32;
            let r = ((fx * (3.0 + s)).sin() * 0.5 + 0.5) * 255.0;
            let g = ((fy * (2.0 + s * 0.7)).cos() * 0.5 + 0.5) * 255.0;
            let in_circle = (fx - 0.3 - 0.1 * (s % 3.0)).powi(2) + (fy - 0.6).powi(2) < 0.04;
            let b = if in_circle { 30.0 } else { (fx + fy) * 120.0 };
            Rgb([r as u8, g as u8, b as u8])
        })
    }

    /// `img` as JPEG at `quality`.
    pub fn jpeg(img: &RgbImage, quality: u8) -> Vec<u8> {
        let mut out = vec![];
        JpegEncoder::new_with_quality(&mut out, quality)
            .encode_image(img)
            .unwrap();
        out
    }

    /// The same JPEG with an EXIF segment added after the start-of-image marker.
    pub fn with_exif(jpeg: &[u8]) -> Vec<u8> {
        // Little-endian TIFF header and an empty IFD.
        let tiff = b"II*\0\x08\0\0\0\0\0\0\0\0\0";
        let mut out = jpeg[..2].to_vec();
        out.extend([0xff, 0xe1]);
        out.extend(((2 + 6 + tiff.len()) as u16).to_be_bytes());
        out.extend(b"Exif\0\0");
        out.extend(tiff);
        out.extend(&jpeg[2..]);
        out
    }

    fn hashes(dir: &Path, name: &str, bytes: &[u8]) -> ImageHashes {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        ImageDecoder::for_path(name, None)
            .unwrap()
            .hash(&path)
            .unwrap()
    }

    #[test]
    fn test_tiers() {
        let dir = tempdir().unwrap();
        let original = picture(1, 800, 600);
        let original_jpeg = jpeg(&original, 90);
        let a = hashes(dir.path(), "a.jpg", &original_jpeg);
        let stripped = hashes(dir.path(), "b.jpg", &with_exif(&original_jpeg));
        let small = image::imageops::resize(&original, 400, 300, FilterType::Triangle);
        let resized = hashes(dir.path(), "c.jpg", &jpeg(&small, 60));
        let other = hashes(dir.path(), "d.jpg", &jpeg(&picture(7, 800, 600), 90));

        assert_eq!(a.pixels, stripped.pixels);
        assert_eq!(a.visual, stripped.visual);
        assert_ne!(a.pixels, resized.pixels);
        let (a, resized, other) = (
            a.visual.unwrap().phash,
            resized.visual.unwrap().phash,
            other.visual.unwrap().phash,
        );
        assert!(distance(a, resized) <= 4);
        assert!(distance(a, other) > 16);
    }

    #[test]
    fn test_hash_version_names_only_the_format_decoder() {
        let ffmpeg = Tool {
            path: std::path::PathBuf::from("ffmpeg"),
            version: String::from("7.1"),
        };
        let hash_version =
            |rel| ImageDecoder::for_path(rel, Some(&ffmpeg)).map(ImageDecoder::version);
        let jpeg = hash_version("a/b.JPG").unwrap();
        assert!(jpeg.starts_with(&format!("{HASH_VERSION}; image 0.")));
        assert!(jpeg.contains("; zune-jpeg 0."));
        assert!(!jpeg.contains("png") && !jpeg.contains("ffmpeg"));
        let png = hash_version("c.png").unwrap();
        assert!(png.contains("; png 0.") && !png.contains("zune"));
        assert!(hash_version("d.heic").unwrap().contains("; ffmpeg 7.1; "));
        assert_eq!(hash_version("e.nef"), None);
    }

    #[test]
    fn test_flat_images_get_no_perceptual_hash() {
        let dir = tempdir().unwrap();
        let black = RgbImage::from_pixel(640, 480, Rgb([0, 0, 0]));
        let navy = RgbImage::from_pixel(800, 600, Rgb([20, 20, 90]));
        let black = hashes(dir.path(), "black.jpg", &jpeg(&black, 90));
        assert_eq!(black.visual, None);
        assert_eq!(
            hashes(dir.path(), "navy.jpg", &jpeg(&navy, 90)).visual,
            None
        );
    }

    #[test]
    fn test_dimensions_are_upright() {
        // An EXIF orientation of 6 (rotate 90) swaps width and height.
        let dir = tempdir().unwrap();
        let mut tiff = b"II*\0\x08\0\0\0\x01\0".to_vec();
        tiff.extend([0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0]);
        let photo = jpeg(&picture(4, 400, 300), 90);
        let mut tagged = photo[..2].to_vec();
        tagged.extend([0xff, 0xe1]);
        tagged.extend(((2 + 6 + tiff.len()) as u16).to_be_bytes());
        tagged.extend(b"Exif\0\0");
        tagged.extend(&tiff);
        tagged.extend(&photo[2..]);
        let h = hashes(dir.path(), "turned.jpg", &tagged);
        assert_eq!(
            h.dimensions,
            Dimensions {
                width: 300,
                height: 400
            }
        );
        assert_eq!(h.pixels, hashes(dir.path(), "plain.jpg", &photo).pixels);
    }

    #[test]
    fn test_fingerprints_match_agrees_with_the_sorted_percentile() {
        let sorted_test = |a: &[u8], b: &[u8]| {
            let d = fingerprint_differences(a, b);
            !d.is_empty() && d[(d.len() * 99 / 100).min(d.len() - 1)] <= FINGERPRINT_LIMIT
        };
        let base: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
        for outliers in [0, 9, 10, 11, 12, 40] {
            for bump in [6u8, 7, 30] {
                let mut other = base.clone();
                for v in other.iter_mut().take(outliers) {
                    *v = v.saturating_add(bump);
                }
                assert_eq!(
                    fingerprints_match(&base, &other),
                    sorted_test(&base, &other),
                    "{} outliers by {}",
                    outliers,
                    bump
                );
            }
        }
        assert!(!fingerprints_match(&[], &[]));
        assert!(!fingerprints_match(&[1, 2], &[1]));
    }

    #[test]
    fn test_pixel_hash_ignores_color_type() {
        let rgb = DynamicImage::ImageRgb8(picture(2, 50, 40));
        let rgba = DynamicImage::ImageRgba8(rgb.to_rgba8());
        assert_eq!(pixel_hash(&rgb), pixel_hash(&rgba));
    }

    #[test]
    fn test_no_decoder() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.heic");
        fs::write(&path, b"whatever").unwrap();
        assert_eq!(ImageDecoder::for_path("x.heic", None), None);
        assert_eq!(ImageDecoder::for_path("x.nef", None), None);
        let jpeg = ImageDecoder::for_path("x.jpg", None).unwrap();
        assert!(jpeg.hash(&path).is_err());
    }
}
