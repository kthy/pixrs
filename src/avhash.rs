//! Signatures of videos (frames at fixed points) and audio (Chromaprint fingerprints), to find
//! re-encoded and resized copies.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::decode::{ffmpeg_input_options, file_url, major_minor, run_with_timeout, DecodeError};
use crate::imagehash::{distance, perceptual_hash, HASH_VERSION};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// Version of the signature definitions. Bump it when they change, so every file is decoded
/// again.
const AV_VERSION: u32 = 3;

/// Frames sampled per video, at 5%, 15%, ... 85% of its duration.
pub const FRAMES: usize = 9;

/// Frame pairs that must match for videos to be similar; fewer if fewer frames have a hash.
const MATCHING_FRAMES: usize = 7;

/// Frames both videos need a hash for (frames without detail, like black or fades, have none).
const MIN_FRAMES: usize = 5;

/// Bytes per frame in a stored signature: whether it has a hash, and the hash.
const FRAME_BYTES: usize = 1 + 8;

/// Seconds of audio fingerprinted.
const AUDIO_SECONDS: u32 = 120;

/// How far apart two fingerprints are compared, in values (about 2 seconds).
const MAX_OFFSET: isize = 16;

/// Largest share of differing bits at the best offset for audio to be similar.
pub const AUDIO_MAX_BER: f64 = 0.15;

/// A frame's perceptual hash, or `None` without detail.
///
/// Frames are compared by hash alone, not also by fingerprint as images are. Re-encoders
/// usually convert a phone's variable frame rate to a constant one, which moves each frame a
/// few milliseconds; fingerprints see that motion and reject real copies. Calibrated on 38 of
/// the owner's videos: hashes alone matched 36 of 38 re-encoded copies and none of the 703
/// pairs of different videos; with fingerprints, only 29 copies matched.
pub type Frame = Option<u64>;

/// The frames of a video at fixed points, and its duration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSignature {
    pub duration_ms: i64,
    pub frames: Vec<Frame>,
}

/// A Chromaprint fingerprint of the start of an audio file, and its duration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFingerprint {
    pub duration_ms: i64,
    pub values: Vec<u32>,
}

/// The frames as stored in the catalog.
pub fn frames_to_bytes(frames: &[Frame]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames.len() * FRAME_BYTES);
    for frame in frames {
        match frame {
            Some(hash) => {
                out.push(1);
                out.extend(hash.to_le_bytes());
            }
            None => out.extend([0; FRAME_BYTES]),
        }
    }
    out
}

/// Frames from `frames_to_bytes`, or `None` if the bytes are not a whole number of frames.
pub fn frames_from_bytes(bytes: &[u8]) -> Option<Vec<Frame>> {
    if !bytes.len().is_multiple_of(FRAME_BYTES) {
        return None;
    }
    Some(
        bytes
            .as_chunks::<FRAME_BYTES>()
            .0
            .iter()
            .map(|chunk| {
                (chunk[0] == 1).then(|| {
                    let mut hash = [0u8; 8];
                    hash.copy_from_slice(&chunk[1..9]);
                    u64::from_le_bytes(hash)
                })
            })
            .collect(),
    )
}

/// Fingerprint values as stored in the catalog, little-endian.
pub fn values_to_bytes(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Fingerprint values from `values_to_bytes`.
pub fn values_from_bytes(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect()
}

/// The version a video is hashed with: the signature and image hash definitions (frames are
/// hashed like images), the crates that hash frames and `ffmpeg`'s major and minor version.
pub fn video_version(ffmpeg_version: &str) -> String {
    format!(
        "video {}; image hash {}; image {}; image_hasher {}; png {}; ffmpeg {}",
        AV_VERSION,
        HASH_VERSION,
        env!("PIXRS_CRATE_image"),
        env!("PIXRS_CRATE_image_hasher"),
        env!("PIXRS_CRATE_png"),
        ffmpeg_version
    )
}

/// The version audio is fingerprinted with: the signature definitions and `fpcalc`'s major and
/// minor version.
pub fn audio_version(fpcalc_version: &str) -> String {
    format!("audio {AV_VERSION}; fpcalc {fpcalc_version}")
}

/// The major and minor version `fpcalc -version` reports, e.g. `1.5`, or `unknown`.
pub fn fpcalc_version(fpcalc: &Path) -> String {
    let mut command = Command::new(fpcalc);
    command.arg("-version");
    run_with_timeout(command)
        .ok()
        .and_then(|out| {
            // "fpcalc version 1.5.1"
            let out = String::from_utf8_lossy(&out).into_owned();
            out.split_whitespace().nth(2).map(major_minor)
        })
        .unwrap_or_else(|| String::from("unknown"))
}

/// When a video's picture starts and how long it lasts, in seconds, from `ffprobe`: the video
/// stream's own start and duration, which a copy without the audio track (or with a shorter
/// one) shares, falling back to the container's when the stream does not say. The start is
/// relative to the container's start, as `ffmpeg -ss` expects: camcorder MTS files start at
/// several seconds or more.
fn video_span(ffprobe: &Path, source: &Path) -> Result<(f64, f64), DecodeError> {
    let mut command = Command::new(ffprobe);
    command
        .args([
            "-v",
            "error",
            "-protocol_whitelist",
            "file",
            "-select_streams",
            // `V`, not `v`: not a cover picture.
            "V:0",
        ])
        .args([
            "-show_entries",
            "stream=start_time,duration:stream_tags:format=start_time,duration",
        ])
        .args(["-of", "json"])
        .arg(file_url(source));
    let out = run_with_timeout(command)?;
    let json: serde_json::Value = serde_json::from_slice(&out)?;
    let number = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .and_then(|x| x.parse::<f64>().ok())
            .filter(|x| x.is_finite())
    };
    let stream = &json["streams"][0];
    let format = &json["format"];
    // Matroska and WebM keep a stream's duration only in a tag, `DURATION` or with a language,
    // like `DURATION-eng`.
    let tagged = || {
        let tags = stream["tags"].as_object()?;
        tags.iter()
            .find(|(key, _)| {
                let key = key.to_ascii_uppercase();
                key == "DURATION" || key.starts_with("DURATION-")
            })
            .and_then(|(_, value)| value.as_str())
            .and_then(parse_clock)
    };
    let duration = number(stream, "duration")
        .filter(|d| *d > 0.0)
        .or_else(tagged)
        .filter(|d| *d > 0.0)
        .or_else(|| number(format, "duration"))
        .filter(|d| *d > 0.0)
        .ok_or(DecodeError::NoDuration)?;
    let start = match (number(stream, "start_time"), number(format, "start_time")) {
        (Some(stream), Some(format)) => (stream - format).max(0.0),
        _ => 0.0,
    };
    Ok((start, duration))
}

/// Seconds from a clock time like `01:02:03.500000000`.
#[allow(
    clippy::suboptimal_flops,
    reason = "a fused multiply-add rounds differently, which would change the durations read"
)]
fn parse_clock(clock: &str) -> Option<f64> {
    let mut seconds = 0.0;
    for part in clock.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(seconds).filter(|s| s.is_finite())
}

/// The frame at `seconds` after the start, hashed; `None` if there is no frame there or it has
/// no detail.
fn frame_at(ffmpeg: &Path, source: &Path, seconds: f64) -> Result<Frame, DecodeError> {
    let mut command = Command::new(ffmpeg);
    command
        .args(ffmpeg_input_options())
        .arg("-ss")
        .arg(format!("{seconds:.3}"))
        .arg("-i")
        .arg(file_url(source))
        .args(["-frames:v", "1", "-vf"])
        .arg("scale=256:256:force_original_aspect_ratio=decrease")
        .args(["-f", "image2pipe", "-c:v", "png", "-"]);
    let png = run_with_timeout(command)?;
    if png.is_empty() {
        return Ok(None);
    }
    let img = image::load_from_memory(&png).map_err(DecodeError::Image)?;
    Ok(perceptual_hash(&img).map(|(hash, _)| hash))
}

/// The signature of a video: its picture's duration and its frames at 5%, 15%, ... 85% of it.
/// `ffmpeg` turns frames upright as the file says. A frame `ffmpeg` cannot decode counts as one
/// without detail, unless no frame can be decoded at all.
#[allow(
    clippy::suboptimal_flops,
    reason = "a fused multiply-add rounds differently, which would move the frames sampled"
)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "durations in seconds, frame indices below 10"
)]
pub fn video_signature(
    ffmpeg: &Path,
    ffprobe: &Path,
    source: &Path,
) -> Result<VideoSignature, DecodeError> {
    // A file that cannot be opened is unreadable, not invalid, so it is tried again later.
    std::fs::File::open(source)?;
    let (start, duration) = video_span(ffprobe, source)?;
    let mut frames = Vec::with_capacity(FRAMES);
    let (mut decoded, mut invalid) = (0, None);
    for i in 0..FRAMES {
        let at = start + duration * (0.05 + 0.1 * i as f64);
        match frame_at(ffmpeg, source, at) {
            Ok(frame) => {
                frames.push(frame);
                decoded += 1;
            }
            Err(e) if e.is_transient() => return Err(e),
            Err(e) => {
                frames.push(None);
                invalid = Some(e);
            }
        }
    }
    if let Some(e) = invalid.filter(|_| decoded == 0) {
        return Err(e);
    }
    Ok(VideoSignature {
        duration_ms: (duration * 1000.0).round() as i64,
        frames,
    })
}

/// What `fpcalc -raw -json` prints.
#[derive(Deserialize)]
struct Fpcalc {
    duration: f64,
    fingerprint: Vec<i64>,
}

/// The Chromaprint fingerprint of the first `AUDIO_SECONDS` of an audio file, from `fpcalc`.
/// Clips shorter than about 5 seconds give too few values to compare, and never match; under
/// about 3 seconds, `fpcalc` gives none.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "fpcalc prints the 32-bit values signed or unsigned depending on its version"
)]
pub fn audio_fingerprint(fpcalc: &Path, source: &Path) -> Result<AudioFingerprint, DecodeError> {
    std::fs::File::open(source)?;
    let mut command = Command::new(fpcalc);
    command
        .args(["-raw", "-json", "-length"])
        .arg(AUDIO_SECONDS.to_string())
        .arg(source);
    let out = match run_with_timeout(command) {
        // What `fpcalc` says about audio too short to fingerprint.
        Err(DecodeError::Tool { stderr }) if stderr.contains("Empty fingerprint") => {
            return Ok(AudioFingerprint {
                duration_ms: 0,
                values: vec![],
            })
        }
        out => out?,
    };
    let parsed: Fpcalc = serde_json::from_slice(&out)?;
    Ok(AudioFingerprint {
        duration_ms: (parsed.duration * 1000.0).round() as i64,
        values: parsed.fingerprint.iter().map(|&v| v as u32).collect(),
    })
}

/// Whether two video durations are close enough for copies: within 1 second or 1%.
pub fn video_durations_match(a: i64, b: i64) -> bool {
    (a - b).abs() <= 1000.max(a.max(b) / 100)
}

/// Whether two audio durations are close enough for copies: within 2 seconds.
pub const fn audio_durations_match(a: i64, b: i64) -> bool {
    (a - b).abs() <= 2000
}

/// Whether two videos' frames match: of the frame pairs where both have a hash (at least
/// `MIN_FRAMES`), at least `MATCHING_FRAMES` (or all, if fewer) have hashes at most `threshold`
/// bits apart.
pub fn frames_match(a: &[Frame], b: &[Frame], threshold: u32) -> bool {
    let mut both = 0;
    let mut matching = 0;
    for (x, y) in a.iter().zip(b) {
        if let (Some(hx), Some(hy)) = (x, y) {
            both += 1;
            if distance(*hx, *hy) <= threshold {
                matching += 1;
            }
        }
    }
    both >= MIN_FRAMES && matching >= MATCHING_FRAMES.min(both)
}

/// The share of differing bits between two fingerprints at the offset (within about 2 seconds)
/// where they agree best, or 1 if they hardly overlap. Offsets are given up on once they differ
/// in more than `give_up` of the bits they are compared on; 1 compares them all.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "bit counts and offsets far below 2^52"
)]
fn best_bit_error_rate(a: &[u32], b: &[u32], give_up: f64) -> f64 {
    let min_overlap = (a.len().min(b.len()) / 2).max(20);
    let mut best = 1.0_f64;
    for offset in -MAX_OFFSET..=MAX_OFFSET {
        let (a, b) = if offset >= 0 {
            (a.get(offset as usize..).unwrap_or_default(), b)
        } else {
            (a, b.get(offset.unsigned_abs()..).unwrap_or_default())
        };
        let overlap = a.len().min(b.len());
        if overlap < min_overlap {
            continue;
        }
        let limit = (give_up.min(best) * 32.0 * overlap as f64) as u64;
        let mut bits = 0u64;
        // Unrelated audio differs in about half its bits, so most offsets end early.
        for (x, y) in a[..overlap].chunks(64).zip(b[..overlap].chunks(64)) {
            bits += x
                .iter()
                .zip(y)
                .map(|(x, y)| u64::from((x ^ y).count_ones()))
                .sum::<u64>();
            if bits > limit {
                break;
            }
        }
        if bits <= limit {
            best = best.min(bits as f64 / (32.0 * overlap as f64));
        }
    }
    best
}

/// The share of differing bits between two fingerprints at the offset (within about 2 seconds)
/// where they agree best, or 1 if they hardly overlap.
pub fn bit_error_rate(a: &[u32], b: &[u32]) -> f64 {
    best_bit_error_rate(a, b, 1.0)
}

/// Whether a fingerprint has enough distinct values to be compared. Silence and steady tones
/// repeat one value, and all look alike.
pub fn has_detail(values: &[u32]) -> bool {
    let mut distinct = values.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    distinct.len() * 10 >= values.len() && distinct.len() > 1
}

/// Whether two audio fingerprints with matching durations are of the same recording.
pub fn audio_match(a: &[u32], b: &[u32]) -> bool {
    best_bit_error_rate(a, b, AUDIO_MAX_BER) <= AUDIO_MAX_BER
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::decode::find_tool;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// Run `ffmpeg` with `args`, or `None` if it is missing. Panics if it fails, so a broken
    /// fixture does not pass as a skipped test.
    pub fn ffmpeg(args: &[&str]) -> Option<()> {
        let ffmpeg = find_tool("ffmpeg")?;
        let mut command = Command::new(ffmpeg);
        command
            .args(["-nostdin", "-y", "-loglevel", "error"])
            .args(args);
        if let Err(e) = run_with_timeout(command) {
            panic!("ffmpeg {args:?} failed: {e}");
        }
        Some(())
    }

    /// Make `path`: `seconds` of a zoom into the Mandelbrot set from `from` seconds on, whose
    /// frames all differ, with `extra` arguments to `ffmpeg` before the output.
    pub fn zoom(path: &Path, from: u32, seconds: u32, extra: &[&str]) -> Option<()> {
        let source = format!(
            "mandelbrot=size=640x360:rate=25,trim=start={from}:duration={seconds},setpts=PTS-STARTPTS"
        );
        let mut args = vec!["-f", "lavfi", "-i", &source];
        args.extend(extra);
        let path = path.to_string_lossy();
        args.push(&path);
        ffmpeg(&args)
    }

    /// A video, its re-encoded half-size copy, and an unrelated video, made with `ffmpeg`.
    pub fn videos(dir: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let (a, b, c) = (dir.join("a.mp4"), dir.join("b.mp4"), dir.join("c.mp4"));
        let s = |p: &Path| p.to_string_lossy().into_owned();
        zoom(&a, 0, 12, &["-c:v", "mpeg4", "-q:v", "3"])?;
        ffmpeg(&[
            "-i",
            &s(&a),
            "-vf",
            "scale=320:180",
            "-c:v",
            "mpeg4",
            "-q:v",
            "12",
            &s(&b),
        ])?;
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=25:duration=12",
            "-c:v",
            "mpeg4",
            "-q:v",
            "3",
            &s(&c),
        ])?;
        Some((a, b, c))
    }

    /// `ffmpeg` and `ffprobe`, or `None` with a note that the test is skipped.
    fn tools() -> Option<(PathBuf, PathBuf)> {
        let tools = find_tool("ffmpeg").zip(find_tool("ffprobe"));
        if tools.is_none() {
            eprintln!("ffmpeg or ffprobe not available, skipping");
        }
        tools
    }

    #[test]
    fn test_frames_round_trip_bytes() {
        let frames = vec![Some(7u64), None];
        assert_eq!(
            frames_from_bytes(&frames_to_bytes(&frames)).unwrap(),
            frames
        );
        assert_eq!(
            values_from_bytes(&values_to_bytes(&[1, u32::MAX])),
            [1, u32::MAX]
        );
        assert_eq!(frames_from_bytes(&[1, 2, 3]), None);
    }

    #[test]
    fn test_frames_match_rule() {
        let a: Vec<Frame> = (0..9).map(Some).collect();
        let mut b = a.clone();
        assert!(frames_match(&a, &b, 6));
        // Two frames differ: 7 of 9 still match.
        b[0] = Some(u64::MAX);
        b[1] = Some(!1);
        assert!(frames_match(&a, &b, 6));
        b[2] = Some(u64::MAX);
        assert!(!frames_match(&a, &b, 6));
        // Few frames with detail: all of them must match, and at least five.
        let sparse: Vec<Frame> = (0..9).map(|i| (i < 5).then_some(i)).collect();
        assert!(frames_match(&sparse, &a, 6));
        let sparser: Vec<Frame> = (0..9).map(|i| (i < 4).then_some(i)).collect();
        assert!(!frames_match(&sparser, &a, 6));
    }

    #[test]
    fn test_bit_error_rate() {
        let a: Vec<u32> = (0..400u32).map(|i| i.wrapping_mul(2_654_435_761)).collect();
        // The same recording, starting a second later and with some bits flipped.
        let b: Vec<u32> = a[8..].iter().map(|v| v ^ 0b1011).collect();
        assert!(bit_error_rate(&a, &b) < 0.1);
        let c: Vec<u32> = (0..400u32)
            .map(|i| i.wrapping_mul(40_503).rotate_left(7))
            .collect();
        assert!(bit_error_rate(&a, &c) > 0.3);
        // Too short to compare: exactly the "no match" rate.
        assert!(bit_error_rate(&a, &a[..5]).total_cmp(&1.0).is_eq());
    }

    #[test]
    fn test_durations() {
        assert!(video_durations_match(12_000, 12_900));
        assert!(!video_durations_match(12_000, 13_100));
        assert!(video_durations_match(600_000, 605_000));
        assert!(audio_durations_match(180_000, 181_500));
        assert!(!audio_durations_match(180_000, 182_500));
    }

    #[test]
    fn test_parse_clock() {
        assert_eq!(parse_clock("00:00:04.003000000"), Some(4.003));
        assert_eq!(parse_clock("01:02:03.5"), Some(3723.5));
        assert_eq!(parse_clock("N/A"), None);
    }

    #[test]
    fn test_audio_match_agrees_with_bit_error_rate() {
        // Giving up early must not change the answer.
        let a: Vec<u32> = (0..900u32).map(|i| i.wrapping_mul(2_654_435_761)).collect();
        for flips in [0u32, 0b1, 0b1011, 0xF0F, 0xFFFF, 0xFF_FFFF] {
            let b: Vec<u32> = a[5..].iter().map(|v| v ^ flips).collect();
            assert_eq!(
                audio_match(&a, &b),
                bit_error_rate(&a, &b) <= AUDIO_MAX_BER,
                "{:#x}",
                flips
            );
        }
    }

    #[test]
    fn test_re_encoded_video_matches() {
        let dir = tempdir().unwrap();
        let (Some((a, b, c)), Some((ffmpeg, ffprobe))) = (videos(dir.path()), tools()) else {
            return;
        };
        // The same zoom, starting 3 seconds later: same length and shape, different frames.
        let d = dir.path().join("d.mp4");
        zoom(&d, 3, 12, &["-c:v", "mpeg4", "-q:v", "3"]).unwrap();
        let sig = |p: &Path| video_signature(&ffmpeg, &ffprobe, p).unwrap();
        let (a, b, c, d) = (sig(&a), sig(&b), sig(&c), sig(&d));
        assert_eq!(a.frames.len(), FRAMES);
        assert!(a.frames.iter().all(Option::is_some), "{:?}", a.frames);
        assert!(video_durations_match(a.duration_ms, b.duration_ms));
        assert!(frames_match(&a.frames, &b.frames, 6));
        assert!(!frames_match(&a.frames, &c.frames, 6));
        assert!(video_durations_match(a.duration_ms, d.duration_ms));
        assert!(!frames_match(&a.frames, &d.frames, 6));
    }

    #[test]
    fn test_container_start_time_is_not_counted_twice() {
        // Camcorder MTS files start at a large timestamp; `ffmpeg -ss` already counts from it.
        let dir = tempdir().unwrap();
        let (a, b) = (dir.path().join("a.mp4"), dir.path().join("b.mts"));
        let made = zoom(&a, 0, 12, &["-c:v", "mpeg4", "-q:v", "3"]).and_then(|()| {
            zoom(
                &b,
                0,
                12,
                &[
                    "-c:v",
                    "mpeg2video",
                    "-q:v",
                    "4",
                    "-output_ts_offset",
                    "1000",
                    "-f",
                    "mpegts",
                ],
            )
        });
        let (Some(()), Some((ffmpeg, ffprobe))) = (made, tools()) else {
            return;
        };
        let (a, b) = (
            video_signature(&ffmpeg, &ffprobe, &a).unwrap(),
            video_signature(&ffmpeg, &ffprobe, &b).unwrap(),
        );
        assert!(b.frames.iter().all(Option::is_some), "{:?}", b.frames);
        assert!(video_durations_match(a.duration_ms, b.duration_ms));
        assert!(frames_match(&a.frames, &b.frames, 6));
    }

    #[test]
    fn test_audio_longer_than_the_picture_does_not_shift_frames() {
        // Phones often record a little more audio than video; a copy without the audio track
        // must sample the same moments. Matroska keeps the video's duration only in a tag.
        for container in ["mp4", "mkv"] {
            let dir = tempdir().unwrap();
            let (a, b) = (
                dir.path().join(format!("a.{container}")),
                dir.path().join("b.mp4"),
            );
            let s = |p: &Path| p.to_string_lossy().into_owned();
            let made = zoom(
                &a,
                0,
                10,
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=12",
                    "-map",
                    "0:v",
                    "-map",
                    "1:a",
                    "-c:v",
                    "mpeg4",
                    "-q:v",
                    "3",
                    "-c:a",
                    "aac",
                ],
            )
            .and_then(|()| ffmpeg(&["-i", &s(&a), "-an", "-c:v", "mpeg4", "-q:v", "8", &s(&b)]));
            let (Some(()), Some((ffmpeg, ffprobe))) = (made, tools()) else {
                return;
            };
            let (a, b) = (
                video_signature(&ffmpeg, &ffprobe, &a).unwrap(),
                video_signature(&ffmpeg, &ffprobe, &b).unwrap(),
            );
            assert!(
                (a.duration_ms - b.duration_ms).abs() < 100,
                "{}: {} {}",
                container,
                a.duration_ms,
                b.duration_ms
            );
            assert!(frames_match(&a.frames, &b.frames, 6), "{}", container);
        }
    }

    #[test]
    fn test_unopenable_video_is_transient() {
        let Some((ffmpeg, ffprobe)) = tools() else {
            return;
        };
        let missing = Path::new("does/not/exist.mp4");
        assert!(video_signature(&ffmpeg, &ffprobe, missing)
            .unwrap_err()
            .is_transient());
    }

    #[test]
    fn test_re_encoded_audio_matches() {
        let dir = tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a.wav"),
            dir.path().join("b.m4a"),
            dir.path().join("c.wav"),
        );
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let made = ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=30",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=d=30:c=pink:a=0.1",
            "-filter_complex",
            "amix=inputs=2",
            &s(&a),
        ])
        .and_then(|()| ffmpeg(&["-i", &s(&a), "-c:a", "aac", "-b:a", "96k", &s(&b)]))
        .and_then(|()| {
            ffmpeg(&[
                "-f",
                "lavfi",
                "-i",
                "anoisesrc=d=30:c=brown:a=0.3:seed=7",
                &s(&c),
            ])
        });
        let fpcalc = find_tool("fpcalc");
        let (Some(()), Some(fpcalc)) = (made, fpcalc) else {
            eprintln!("ffmpeg or fpcalc not available, skipping");
            return;
        };
        let print = |p: &Path| audio_fingerprint(&fpcalc, p).unwrap();
        let (a, b, c) = (print(&a), print(&b), print(&c));
        assert!(
            audio_match(&a.values, &b.values),
            "{}",
            bit_error_rate(&a.values, &b.values)
        );
        assert!(
            !audio_match(&a.values, &c.values),
            "{}",
            bit_error_rate(&a.values, &c.values)
        );
    }
}
