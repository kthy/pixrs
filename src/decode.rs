//! Decoding images to pixels, in-process or with the `ffmpeg` binary.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageReader};
use log::debug;
use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long `ffmpeg` and `ffprobe` may take for one file.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// Why an image could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The file could not be read, or `ffmpeg` could not be started. Worth trying again later:
    /// a drive may have dropped out.
    Unreadable(String),
    /// `ffmpeg` ran out of time. Worth trying again later: the system may only have been busy.
    TimedOut,
    /// The file is not an image the decoder understands.
    Invalid(String),
}

impl DecodeError {
    /// Whether trying again later might succeed.
    pub fn is_transient(&self) -> bool {
        !matches!(self, DecodeError::Invalid(_))
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Unreadable(msg) | DecodeError::Invalid(msg) => write!(f, "{}", msg),
            DecodeError::TimedOut => write!(f, "timed out"),
        }
    }
}

impl From<ImageError> for DecodeError {
    fn from(e: ImageError) -> Self {
        match e {
            // A file that ends early is broken, not unreadable: trying again will not help.
            ImageError::IoError(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                DecodeError::Invalid(e.to_string())
            }
            ImageError::IoError(e) => DecodeError::Unreadable(e.to_string()),
            e => DecodeError::Invalid(e.to_string()),
        }
    }
}

impl From<std::io::Error> for DecodeError {
    fn from(e: std::io::Error) -> Self {
        DecodeError::Unreadable(e.to_string())
    }
}

/// The oldest `ffmpeg` trusted with HEIC: older ones may decode only one tile of the grid
/// phones store HEIC photos as.
const FFMPEG_FOR_HEIC: (u32, u32) = (7, 1);

/// Image extensions the `image` crate decodes, lowercase.
const DECODED_IN_PROCESS: &[&str] = &["bmp", "gif", "jpeg", "jpg", "png", "tif", "tiff", "webp"];

/// Image extensions only `ffmpeg` decodes, lowercase.
const DECODED_BY_FFMPEG: &[&str] = &["avif", "heic", "heif"];

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
/// says how to show them upright is returned alongside.
pub fn decode_in_process(source: &Path) -> Result<(DynamicImage, Orientation), DecodeError> {
    let mut decoder = ImageReader::open(source)?
        .with_guessed_format()?
        .into_decoder()?;
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
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-protocol_whitelist", "file", "-i"])
        .arg(file_url(source))
        .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"]);
    let png = run_with_timeout(command)?;
    if png.is_empty() {
        return Err(DecodeError::Invalid(String::from(
            "ffmpeg produced no frame",
        )));
    }
    image::load_from_memory(&png).map_err(|e| DecodeError::Invalid(e.to_string()))
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
    debug!("{} on PATH: {}", name, found);
    found.then(|| PathBuf::from(name))
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
fn major_minor(version: &str) -> String {
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
        .map(|part| part.parse::<u32>());
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= FFMPEG_FOR_HEIC,
        _ => true,
    }
}

/// Run a tool and return its standard output, or an error with its standard error. Kills it
/// after `TOOL_TIMEOUT`.
fn run_with_timeout(mut command: Command) -> Result<Vec<u8>, DecodeError> {
    debug!("Running {:?}", command);
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
        Err(DecodeError::Invalid(
            String::from_utf8_lossy(&err).trim().to_string(),
        ))
    }
}

#[cfg(test)]
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
        assert!(!err.is_transient(), "{:?}", err);
    }

    #[test]
    fn test_missing_file_is_transient() {
        let err =
            decode_with_ffmpeg(Path::new("ffmpeg"), Path::new("/no/such/file.heic")).unwrap_err();
        assert!(err.is_transient(), "{:?}", err);
        let err = decode_in_process(Path::new("/no/such/file.jpg")).unwrap_err();
        assert!(err.is_transient(), "{:?}", err);
    }
}
