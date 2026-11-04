//! Decoding images to pixels, in-process or with the `ffmpeg` binary.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageReader, Limits};
use log::debug;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long `ffmpeg` and `ffprobe` may take for one file.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a file could not be decoded.
#[derive(Debug)]
pub enum DecodeError {
    /// The file could not be read, or the tool could not be started. Worth trying again later:
    /// a drive may have dropped out.
    Unreadable(io::Error),
    /// The tool ran out of time. Worth trying again later: the system may only have been busy.
    TimedOut,
    /// The file is larger than pixrs decodes in-process.
    TooLarge { bytes: u64 },
    /// The `image` crate cannot decode it.
    Image(ImageError),
    /// The tool failed; its standard error.
    Tool { stderr: String },
    /// The tool's output is not what it should print.
    Output(serde_json::Error),
    /// `ffmpeg` produced no frame.
    NoFrame,
    /// `ffprobe` found no duration.
    NoDuration,
    /// The decoder panicked.
    Crashed,
}

impl DecodeError {
    /// Whether trying again later might succeed.
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Unreadable(_) | Self::TimedOut)
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "{e}"),
            Self::TimedOut => write!(f, "timed out"),
            Self::TooLarge { bytes } => write!(
                f,
                "the file is {} MB, more than the {} MB pixrs decodes",
                bytes / 1_000_000,
                MAX_IMAGE_FILE_BYTES / 1_000_000
            ),
            Self::Image(e) => write!(f, "{e}"),
            Self::Tool { stderr } => write!(f, "{stderr}"),
            Self::Output(e) => write!(f, "{e}"),
            Self::NoFrame => write!(f, "ffmpeg produced no frame"),
            Self::NoDuration => write!(f, "ffprobe found no duration"),
            Self::Crashed => write!(f, "the decoder crashed"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable(e) => Some(e),
            Self::Image(e) => Some(e),
            Self::Output(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ImageError> for DecodeError {
    fn from(e: ImageError) -> Self {
        match e {
            // A file that ends early is broken, not unreadable: trying again will not help.
            ImageError::IoError(e) if e.kind() != io::ErrorKind::UnexpectedEof => {
                Self::Unreadable(e)
            }
            e => Self::Image(e),
        }
    }
}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        Self::Unreadable(e)
    }
}

impl From<serde_json::Error> for DecodeError {
    fn from(e: serde_json::Error) -> Self {
        Self::Output(e)
    }
}

/// The oldest `ffmpeg` trusted with HEIC: older ones may decode only one tile of the grid
/// phones store HEIC photos as.
const FFMPEG_FOR_HEIC: (u32, u32) = (7, 1);

/// Image extensions the `image` crate decodes, lowercase.
const DECODED_IN_PROCESS: &[&str] = &["bmp", "gif", "jpeg", "jpg", "png", "tif", "tiff", "webp"];

/// Image extensions only `ffmpeg` decodes, lowercase.
const DECODED_BY_FFMPEG: &[&str] = &["avif", "heic", "heif"];

/// Most bytes of pixels an image decoded in-process may take (the `image` crate's default): a
/// file can claim any size in a few bytes, and allocating what it claims aborts the process.
/// The largest camera images, 100 megapixels, take about 400 MB.
pub const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

/// Most pixels along either side of an image decoded in-process.
pub const MAX_DIMENSION: u32 = 65_536;

/// Largest image file decoded in-process: a JPEG is read into memory whole before it is
/// decoded, and a real image that large would exceed `MAX_DECODED_BYTES` as pixels anyway.
pub const MAX_IMAGE_FILE_BYTES: u64 = MAX_DECODED_BYTES;

/// Most pixels in a frame `ffmpeg` decodes: the in-process budget at 4 bytes a pixel, about 134
/// megapixels. A file can claim any frame size in a few bytes, and `ffmpeg` allocates what it
/// claims. Phones store larger HEIC and AVIF photos as grids of small tiles, which stay below it.
pub const MAX_FFMPEG_PIXELS: u64 = MAX_DECODED_BYTES / 4;

/// The options every `ffmpeg` run starts with, before its input: no prompts, errors only, local
/// files only (a media file can reference other resources), one decoding thread (several files
/// are decoded at once already, and each thread holds frames of its own), and no frame over
/// `MAX_FFMPEG_PIXELS`.
pub(crate) fn ffmpeg_input_options() -> Vec<String> {
    input_options(MAX_FFMPEG_PIXELS)
}

/// `ffmpeg_input_options` with a frame limit of `max_pixels`.
fn input_options(max_pixels: u64) -> Vec<String> {
    let options = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-threads",
        "1",
    ];
    let local = ["-protocol_whitelist", "file", "-max_pixels"];
    options
        .into_iter()
        .chain(local)
        .map(String::from)
        .chain([max_pixels.to_string()])
        .collect()
}

/// How an image is decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoder {
    InProcess,
    Ffmpeg,
}

/// The lowercase extension of a `/`-separated path.
pub fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// How an image at `path` can be decoded, if at all (RAW formats cannot).
pub fn image_decoder(path: &str) -> Option<Decoder> {
    let ext = extension(path);
    if DECODED_IN_PROCESS.contains(&ext.as_str()) {
        Some(Decoder::InProcess)
    } else if DECODED_BY_FFMPEG.contains(&ext.as_str()) {
        Some(Decoder::Ffmpeg)
    } else {
        None
    }
}

/// Decode an image with the `image` crate. The pixels are as stored; the EXIF orientation that
/// says how to show them upright is returned alongside. An image beyond `MAX_DIMENSION`,
/// `MAX_DECODED_BYTES` or `MAX_IMAGE_FILE_BYTES` is invalid, not decoded.
pub fn decode_in_process(source: &Path) -> Result<(DynamicImage, Orientation), DecodeError> {
    let file_bytes = std::fs::metadata(source)?.len();
    if file_bytes > MAX_IMAGE_FILE_BYTES {
        return Err(DecodeError::TooLarge { bytes: file_bytes });
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    let mut reader = ImageReader::open(source)?.with_guessed_format()?;
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    // `into_decoder` checks the dimensions, but only `ImageReader::decode` charges the pixel
    // buffer against the memory limit: do that here, before `from_decoder` allocates it.
    limits.reserve(decoder.total_bytes())?;
    let orientation = decoder.orientation()?;
    let img = DynamicImage::from_decoder(decoder)?;
    Ok((img, orientation))
}

/// Decode the first frame of an image with `ffmpeg`, upright (`ffmpeg` applies the rotation the
/// file records).
pub fn decode_with_ffmpeg(ffmpeg: &Path, source: &Path) -> Result<DynamicImage, DecodeError> {
    // `ffmpeg` reports a file it cannot read like one it cannot decode, so check first.
    std::fs::File::open(source)?;
    let mut command = Command::new(ffmpeg);
    command
        .args(ffmpeg_input_options())
        .arg("-i")
        .arg(file_url(source))
        .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"]);
    let png = run_with_timeout(command)?;
    if png.is_empty() {
        return Err(DecodeError::NoFrame);
    }
    image::load_from_memory(&png).map_err(DecodeError::Image)
}

/// `source` as an `ffmpeg` input. The `file:` prefix keeps names like `a:b.mov` from being read
/// as a protocol.
pub fn file_url(source: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(source);
    url
}

/// `name` if it is a program on `PATH` that runs.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let found = Command::new(name)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    debug!("{name} on PATH: {found}");
    found.then(|| PathBuf::from(name))
}

/// The external tools the hash pass can use, found on `PATH`, with their versions.
#[derive(Clone, Debug, Default)]
pub struct Tools {
    pub ffmpeg: Option<(PathBuf, String)>,
    pub ffprobe: Option<PathBuf>,
    pub fpcalc: Option<(PathBuf, String)>,
}

impl Tools {
    /// The tools on `PATH`.
    pub fn find() -> Self {
        Self {
            ffmpeg: find_tool("ffmpeg").map(|f| {
                let version = ffmpeg_version(&f);
                (f, version)
            }),
            ffprobe: find_tool("ffprobe"),
            fpcalc: find_tool("fpcalc").map(|f| {
                let version = crate::avhash::fpcalc_version(&f);
                (f, version)
            }),
        }
    }
}

/// The major and minor version `ffmpeg -version` reports, e.g. `7.1` for `7.1.5-0+deb13u1`, or
/// the whole version if it does not start with numbers (git builds), or `unknown`. Patch
/// releases and distribution updates do not change decoded pixels on purpose.
pub fn ffmpeg_version(ffmpeg: &Path) -> String {
    let mut command = Command::new(ffmpeg);
    command.arg("-version");
    run_with_timeout(command)
        .ok()
        .and_then(|out| {
            // "ffmpeg version 7.1.5-0+deb13u1 Copyright (c) ..."
            let out = String::from_utf8_lossy(&out).into_owned();
            out.split_whitespace().nth(2).map(major_minor)
        })
        .unwrap_or_else(|| String::from("unknown"))
}

/// `7.1` for `7.1.5-0+deb13u1` or `n7.1.2`; other versions unchanged.
pub(crate) fn major_minor(version: &str) -> String {
    let digits = version.trim_start_matches('n');
    let parts: Vec<&str> = digits.splitn(3, '.').collect();
    let numeric = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if parts.len() >= 2 && numeric(parts[0]) {
        let minor: String = parts[1].chars().take_while(char::is_ascii_digit).collect();
        if !minor.is_empty() {
            return format!("{}.{}", parts[0], minor);
        }
    }
    String::from(version)
}

/// Whether `ffmpeg` of `version` (as `ffmpeg_version` returns it) is new enough for HEIC.
/// Versions that do not parse, like git builds, are assumed to be new.
pub fn ffmpeg_handles_heic(version: &str) -> bool {
    let mut parts = version
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| !c.is_ascii_digit())
        .map(str::parse::<u32>);
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= FFMPEG_FOR_HEIC,
        _ => true,
    }
}

/// Run a tool and return its standard output, or an error with its standard error. Kills it
/// after `TOOL_TIMEOUT`.
pub(crate) fn run_with_timeout(mut command: Command) -> Result<Vec<u8>, DecodeError> {
    debug!("Running {command:?}");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let out = thread::spawn(move || {
        let mut buf = vec![];
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    let err = thread::spawn(move || {
        let mut buf = vec![];
        stderr.read_to_end(&mut buf).map(|_| buf)
    });

    let deadline = Instant::now() + TOOL_TIMEOUT;
    let status = loop {
        let waited = child.try_wait();
        let timed_out = Instant::now() >= deadline;
        match waited {
            Ok(Some(status)) => break status,
            Ok(None) if !timed_out => thread::sleep(Duration::from_millis(10)),
            other => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match other {
                    Err(e) => DecodeError::from(e),
                    _ => DecodeError::TimedOut,
                });
            }
        }
    };
    let out = out.join().expect("reader thread")?;
    let err = err.join().expect("reader thread").unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        Err(DecodeError::Tool {
            stderr: String::from_utf8_lossy(&err).trim().to_string(),
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "test fixtures with small values"
)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn test_image_decoder() {
        assert_eq!(image_decoder("a/B.JPG"), Some(Decoder::InProcess));
        assert_eq!(image_decoder("b.heic"), Some(Decoder::Ffmpeg));
        assert_eq!(image_decoder("d.cr2"), None);
        assert_eq!(image_decoder("no extension"), None);
    }

    #[test]
    fn test_ffmpeg_handles_heic() {
        assert!(ffmpeg_handles_heic("7.1.5-0+deb13u1"));
        assert!(ffmpeg_handles_heic("n8.0"));
        assert!(ffmpeg_handles_heic("N-112345-g0123abc"));
        assert!(!ffmpeg_handles_heic("5.1.6-0+deb12u1"));
        assert!(!ffmpeg_handles_heic("7.0.2"));
    }

    #[test]
    fn test_major_minor() {
        assert_eq!(major_minor("7.1.5-0+deb13u1"), "7.1");
        assert_eq!(major_minor("n7.1.2"), "7.1");
        assert_eq!(major_minor("6.0"), "6.0");
        assert_eq!(major_minor("N-112345-g0123abc"), "N-112345-g0123abc");
    }

    #[test]
    fn test_truncated_png_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.png");
        let img = image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8, y as u8, 7]));
        img.save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        let err = decode_in_process(&path).unwrap_err();
        assert!(!err.is_transient(), "{err:?}");
    }

    #[test]
    fn test_ffmpeg_refuses_frames_over_the_pixel_limit() {
        let Some(ffmpeg) = find_tool("ffmpeg") else {
            eprintln!("ffmpeg not available, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("64x64.png");
        let img = image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8, y as u8, 7]));
        img.save(&path).unwrap();
        // The options must come where `ffmpeg` applies them to the decoder, before `-i`.
        let decode = |max_pixels: u64| {
            let mut command = Command::new(&ffmpeg);
            command
                .args(input_options(max_pixels))
                .arg("-i")
                .arg(file_url(&path))
                .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"]);
            run_with_timeout(command)
        };
        let err = decode(64 * 64 - 1).unwrap_err();
        assert!(!err.is_transient(), "{err:?}");
        assert_ne!(decode(64 * 64).unwrap(), [] as [u8; 0]);
        let decoded = decode_with_ffmpeg(&ffmpeg, &path).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 64));
        assert!(ffmpeg_input_options().contains(&MAX_FFMPEG_PIXELS.to_string()));
    }

    #[test]
    fn test_missing_file_is_transient() {
        let err =
            decode_with_ffmpeg(Path::new("ffmpeg"), Path::new("/no/such/file.heic")).unwrap_err();
        assert!(err.is_transient(), "{err:?}");
        let err = decode_in_process(Path::new("/no/such/file.jpg")).unwrap_err();
        assert!(err.is_transient(), "{err:?}");
    }

    /// A PNG whose header claims `width` x `height` RGBA pixels, with no pixel data to speak of.
    fn png_claiming(width: u32, height: u32) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = 0xffff_ffff_u32;
            for &byte in data {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        fn chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            png.extend_from_slice(&body);
            png.extend_from_slice(&crc32(&body).to_be_bytes());
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = vec![];
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        // 8 bits per channel, RGBA, deflate, adaptive filtering, no interlace.
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut png, *b"IHDR", &ihdr);
        chunk(&mut png, *b"IDAT", &[0x78, 0x9c]);
        chunk(&mut png, *b"IEND", &[]);
        png
    }

    #[test]
    fn test_images_claiming_too_much_are_invalid() {
        let dir = tempfile::tempdir().unwrap();
        // More pixels than fit the memory limit: 20,000 x 20,000 RGBA is 1.6 GB.
        let path = dir.path().join("bomb.png");
        std::fs::write(&path, png_claiming(20_000, 20_000)).unwrap();
        let err = decode_in_process(&path).unwrap_err();
        assert!(!err.is_transient(), "{err:?}");
        assert_eq!(err.to_string(), "Memory limit exceeded");
        // Beyond the dimension limit: 40 GB, refused before the decoder starts.
        std::fs::write(&path, png_claiming(100_000, 100_000)).unwrap();
        let err = decode_in_process(&path).unwrap_err();
        assert!(!err.is_transient(), "{err:?}");
        assert_eq!(err.to_string(), "Image size exceeds limit");
        // A file too large to read whole, without looking inside (sparse, so cheap to make).
        let path = dir.path().join("huge.jpg");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_IMAGE_FILE_BYTES + 1)
            .unwrap();
        let err = decode_in_process(&path).unwrap_err();
        assert!(!err.is_transient(), "{err:?}");
        assert!(err.to_string().contains("more than"), "{}", err);
        // A small image still decodes.
        let path = dir.path().join("ok.png");
        let img = image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8, y as u8, 7]));
        img.save(&path).unwrap();
        assert_eq!(decode_in_process(&path).unwrap().0.width(), 64);
    }
}
