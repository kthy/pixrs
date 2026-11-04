//! Thumbnails and previews, cached outside the library.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use super::Located;
use crate::apply::{reserve_temp, still_ours};
use crate::catalog::FileRecord;
use crate::decode::{self, decode_in_process, file_url, image_decoder, DecodeError, TOOL_TIMEOUT};
use crate::media::MediaType;
use crate::text::{printable_lines, shown};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, RgbImage};
use log::{debug, warn};
use ring::digest::{digest, SHA256};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::thread::available_parallelism;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::timeout;

/// Image extensions browsers show, lowercase.
const SHOWN_BY_BROWSERS: &[&str] = &["avif", "bmp", "gif", "jpeg", "jpg", "png", "webp"];

/// The renditions the viewer makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// For listings.
    Thumb,
    /// For viewing an image the browser cannot show.
    Preview,
}

impl Size {
    /// Longest side in pixels.
    const fn pixels(self) -> u32 {
        match self {
            Self::Thumb => 320,
            Self::Preview => 2048,
        }
    }

    /// Cache subfolder.
    const fn dir(self) -> &'static str {
        match self {
            Self::Thumb => "320",
            Self::Preview => "2048",
        }
    }
}

/// How a file is turned into a JPEG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decoder {
    InProcess,
    FfmpegImage,
    FfmpegVideo,
}

/// The lowercase extension of a cataloged file.
fn extension(file: &FileRecord) -> String {
    crate::media::extension(Path::new(&file.path)).unwrap_or_default()
}

/// What the viewer shows for an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewSource {
    Original,
    Preview,
    Placeholder,
}

/// What the viewer shows for an image: the original if browsers can show it, otherwise a
/// preview if pixrs can decode it (HEIC, TIFF), otherwise the placeholder (e.g. RAW files).
pub fn view_source(file: &FileRecord) -> ViewSource {
    if SHOWN_BY_BROWSERS.contains(&extension(file).as_str()) {
        ViewSource::Original
    } else if image_decoder(&file.path).is_some() {
        ViewSource::Preview
    } else {
        ViewSource::Placeholder
    }
}

/// How the viewer makes renditions of the file, if it can.
fn decoder(file: &FileRecord) -> Option<Decoder> {
    match file.media_type {
        MediaType::Image => match image_decoder(&file.path)? {
            decode::Decoder::InProcess(_) => Some(Decoder::InProcess),
            decode::Decoder::Ffmpeg => Some(Decoder::FfmpegImage),
        },
        MediaType::Video => Some(Decoder::FfmpegVideo),
        MediaType::Audio => None,
    }
}

/// Makes and caches renditions.
pub struct Thumbnailer {
    cache_dir: Option<PathBuf>,
    ffmpeg: Option<PathBuf>,
    /// Limits how many renditions are made at once.
    permits: Semaphore,
    /// Cache keys of files that could not be decoded, so they are not retried on every request.
    failed: Mutex<HashSet<String>>,
}

impl Thumbnailer {
    pub fn new(cache_dir: Option<PathBuf>, ffmpeg: Option<PathBuf>) -> Self {
        let workers = available_parallelism().map_or(2, std::num::NonZero::get);
        Self {
            cache_dir,
            ffmpeg,
            permits: Semaphore::new(workers),
            failed: Mutex::new(HashSet::new()),
        }
    }

    /// A JPEG of the file at `size`, from the cache or made now. `None` if the file cannot be
    /// decoded, or there is no cache.
    pub async fn get(&self, located: &Located, size: Size) -> Option<Vec<u8>> {
        let cache_dir = self.cache_dir.as_ref()?;
        let decoder = decoder(&located.file)?;
        let ffmpeg = match decoder {
            Decoder::InProcess => None,
            Decoder::FfmpegImage | Decoder::FfmpegVideo => Some(self.ffmpeg.clone()?),
        };
        let key = cache_key(located, size);
        // Spread over folders by the key's first two hex digits, so none holds too many files.
        let dir = cache_dir
            .join(size.dir())
            .join(key.get(..2).unwrap_or_default());
        let cached = dir.join(format!("{key}.jpg"));
        if let Ok(jpeg) = tokio::fs::read(&cached).await {
            return Some(jpeg);
        }
        if self.failed().contains(&key) {
            return None;
        }

        let _permit = self.permits.acquire().await.ok()?;
        // Another request may have made it while this one waited.
        if let Ok(jpeg) = tokio::fs::read(&cached).await {
            return Some(jpeg);
        }
        let source = located.full_path.clone();
        let made = match ffmpeg {
            None => tokio::task::spawn_blocking(move || render_in_process(&source, size.pixels()))
                .await
                .unwrap_or(Err(DecodeError::Crashed)),
            Some(ffmpeg) => {
                decode_with_ffmpeg(&ffmpeg, &source, size, decoder == Decoder::FfmpegVideo).await
            }
        };
        match made {
            Ok(jpeg) => tokio::task::spawn_blocking(move || {
                if let Err(e) = write_atomically(&dir, &cached, &jpeg) {
                    warn!("Unable to cache {}: {}", shown(&cached), e);
                }
                jpeg
            })
            .await
            .ok(),
            Err(e) => {
                warn!(
                    "No thumbnail for {}: {}",
                    shown(&located.full_path),
                    printable_lines(&e.to_string())
                );
                // A tool that ran out of time is not remembered as a failure: the system may
                // only have been busy.
                if !matches!(e, DecodeError::TimedOut) {
                    self.failed().insert(key);
                }
                None
            }
        }
    }

    /// The video codec of a file, from `ffprobe`. Shares the limit on concurrent tool runs.
    pub async fn video_codec(&self, ffprobe: &Path, source: &Path) -> Option<String> {
        let _permit = self.permits.acquire().await.ok()?;
        let mut command = Command::new(ffprobe);
        command
            .args([
                "-v",
                "error",
                "-protocol_whitelist",
                "file",
                "-select_streams",
                "v:0",
            ])
            .args(["-show_entries", "stream=codec_name"])
            .args(["-of", "default=noprint_wrappers=1:nokey=1"])
            .arg(file_url(source));
        let output = run(command).await.ok()?;
        let codec = String::from_utf8_lossy(&output).trim().to_string();
        (!codec.is_empty()).then_some(codec)
    }

    /// Files that could not be decoded. A panic elsewhere does not invalidate the set.
    fn failed(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The cache file name: a hash of where the file is and what it looked like when scanned, so a
/// changed file gets a new rendition.
fn cache_key(located: &Located, size: Size) -> String {
    let id = format!(
        "{}\0{}\0{}\0{}\0{}",
        located.root,
        located.file.path,
        located.file.stamp.size,
        located.file.stamp.mtime_ns,
        size.pixels()
    );
    data_encoding::HEXLOWER.encode(digest(&SHA256, id.as_bytes()).as_ref())
}

/// Decode an image with the `image` crate, upright, shrunk to fit `max` pixels, as JPEG.
fn render_in_process(source: &Path, max: u32) -> Result<Vec<u8>, DecodeError> {
    let (mut img, orientation) = decode_in_process(source)?;
    img.apply_orientation(orientation);
    if img.width() > max || img.height() > max {
        img = img.thumbnail(max, max);
    }
    let rgb = flatten(&img);
    let mut jpeg = vec![];
    JpegEncoder::new_with_quality(&mut jpeg, 80)
        .encode_image(&rgb)
        .map_err(DecodeError::Image)?;
    Ok(jpeg)
}

/// The image without transparency, on a white background.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a blend of two bytes stays below 256"
)]
fn flatten(img: &DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend =
            |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

/// Decode the first frame of an image, or a frame one second into a video, with `ffmpeg`.
async fn decode_with_ffmpeg(
    ffmpeg: &Path,
    source: &Path,
    size: Size,
    video: bool,
) -> Result<Vec<u8>, DecodeError> {
    let max = size.pixels();
    let attempt = |seek: bool| {
        let mut command = Command::new(ffmpeg);
        command.args(decode::ffmpeg_input_options());
        if seek {
            command.args(["-ss", "1"]);
        }
        command
            .arg("-i")
            .arg(file_url(source))
            .args(["-frames:v", "1", "-vf"])
            .arg(format!(
                "scale='min({max},iw)':'min({max},ih)':force_original_aspect_ratio=decrease"
            ))
            .args(["-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "3", "-"]);
        run(command)
    };
    let jpeg = if video {
        // Videos shorter than a second have no frame there.
        match attempt(true).await {
            Ok(jpeg) if !jpeg.is_empty() => jpeg,
            _ => attempt(false).await?,
        }
    } else {
        attempt(false).await?
    };
    if jpeg.is_empty() {
        Err(DecodeError::NoFrame)
    } else {
        Ok(jpeg)
    }
}

/// Run a tool and return its standard output, or an error with its standard error.
async fn run(mut command: Command) -> Result<Vec<u8>, DecodeError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    debug!("Running {command:?}");
    let output = timeout(TOOL_TIMEOUT, command.output())
        .await
        .map_err(|_| DecodeError::TimedOut)??;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(DecodeError::Tool {
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// Write `path` in the folder `dir` so that readers never see a partial file. The contents go
/// through a new temporary file pixrs created, which only takes the place of `path` if its name
/// still leads to that file.
fn write_atomically(dir: &Path, path: &Path, contents: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let (temp, mut file) = reserve_temp(dir)?;
    let written = file.write_all(contents).and_then(|()| {
        if still_ours(&temp, &file) {
            Ok(())
        } else {
            Err(io::Error::other(format!("{} was replaced", temp.display())))
        }
    });
    drop(file);
    match written {
        Ok(()) => fs::rename(&temp, path).inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        }),
        Err(e) => Err(e),
    }
}

/// An SVG placeholder showing the file's extension.
pub fn placeholder(file: &FileRecord) -> String {
    let label: String = extension(file)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(5)
        .collect::<String>()
        .to_ascii_uppercase();
    let fill = match file.media_type {
        MediaType::Image => "#d8dee4",
        MediaType::Video => "#d4d8ee",
        MediaType::Audio => "#dcecd8",
    };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 320" width="320" height="320"><rect width="320" height="320" fill="{fill}"/><text x="160" y="175" font-family="sans-serif" font-size="48" text-anchor="middle" fill="#57606a">{label}</text></svg>"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::{MediaMeta, Stamp};
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    fn record(path: &str, media_type: MediaType) -> FileRecord {
        FileRecord {
            path: String::from(path),
            stamp: Stamp {
                size: 1,
                mtime_ns: 1,
            },
            media_type,
            sha256: None,
            meta: MediaMeta::default(),
        }
    }

    #[test]
    fn test_decoder_choice() {
        assert_eq!(
            decoder(&record("a/B.JPG", MediaType::Image)),
            Some(Decoder::InProcess)
        );
        assert_eq!(
            decoder(&record("b.heic", MediaType::Image)),
            Some(Decoder::FfmpegImage)
        );
        assert_eq!(
            decoder(&record("c.mov", MediaType::Video)),
            Some(Decoder::FfmpegVideo)
        );
        assert_eq!(decoder(&record("d.cr2", MediaType::Image)), None);
        assert_eq!(decoder(&record("e.mp3", MediaType::Audio)), None);
        assert_eq!(
            view_source(&record("f.TIFF", MediaType::Image)),
            ViewSource::Preview
        );
        assert_eq!(
            view_source(&record("g.webp", MediaType::Image)),
            ViewSource::Original
        );
        assert_eq!(
            view_source(&record("h.nef", MediaType::Image)),
            ViewSource::Placeholder
        );
        for name in ["i.HEIC", "j.heif", "k.tif"] {
            assert_eq!(
                view_source(&record(name, MediaType::Image)),
                ViewSource::Preview,
                "{name}"
            );
        }
        assert_eq!(
            view_source(&record("l.avif", MediaType::Image)),
            ViewSource::Original
        );
    }

    #[test]
    fn test_in_process_applies_size_and_alpha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("clear.png");
        image::RgbaImage::from_pixel(1000, 500, image::Rgba([0, 0, 0, 0]))
            .save(&path)
            .unwrap();
        let jpeg = render_in_process(&path, 320).unwrap();
        let img = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (320, 160));
        assert!(img.get_pixel(10, 10).0.iter().all(|&c| c > 240));
    }

    #[tokio::test]
    async fn test_video_poster_with_ffmpeg() {
        let ffmpeg = Path::new("ffmpeg");
        let dir = tempdir().unwrap();
        let video = dir.path().join("short clip.mp4");
        // A half-second clip, so the frame at one second does not exist and the fallback runs.
        let mut command = Command::new(ffmpeg);
        command
            .args(["-nostdin", "-loglevel", "error", "-f", "lavfi"])
            .args(["-i", "testsrc=duration=0.5:size=640x360:rate=10"])
            .arg(&video);
        if run(command).await.is_err() {
            eprintln!("ffmpeg not available, skipping");
            return;
        }
        let jpeg = decode_with_ffmpeg(ffmpeg, &video, Size::Thumb, true)
            .await
            .unwrap();
        let img = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (320, 180));
    }

    #[test]
    fn test_placeholder_label() {
        let svg = placeholder(&record("x/y.Cr2", MediaType::Image));
        assert!(svg.contains(">CR2<"));
    }
}
