//! Measure how video signatures and audio fingerprints behave on real files, to choose how
//! `pixrs dupes` matches them. A development tool, not part of `pixrs` itself.
//!
//! ```text
//! cargo run --release --example calibrate_av -- --catalog CATALOG --out DIR [--audio FOLDER]
//! ```
//!
//! Videos come from a scanned catalog. Each sampled video is re-encoded into `DIR` (never into
//! a library) in a few ways that should still count as copies, and the tool reports how many of
//! the 9 frames of each copy match the original, and how many pairs of different videos in the
//! catalog would match. Audio files come from `FOLDER`: each is re-encoded, and the bit error
//! rate of its fingerprint against the original is compared with that of unrelated pairs. It
//! prints counts and statistics only, no file names.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use clap::Parser;
use pixrs::avhash::{
    audio_durations_match, audio_fingerprint, bit_error_rate, frames_match, video_durations_match,
    video_signature, AudioFingerprint, Frame, FRAMES,
};
use pixrs::catalog::{library_path, Catalog, HashedAv};
use pixrs::decode::Tools;
use pixrs::dupes::{aspect, aspects_match, DEFAULT_THRESHOLD};
use pixrs::imagehash::distance;
use pixrs::scan::resolve;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};

#[derive(Debug, Parser)]
struct Opt {
    /// The catalog of the scanned libraries
    #[arg(long)]
    catalog: PathBuf,
    /// Folder for re-encoded copies; must be outside every cataloged library
    #[arg(long)]
    out: PathBuf,
    /// Number of videos to re-encode
    #[arg(long, default_value_t = 30)]
    sample: usize,
    /// A folder of audio files to calibrate audio on
    #[arg(long)]
    audio: Option<PathBuf>,
}

/// Re-encodings that should still count as the same video: name and `ffmpeg` output options.
const VIDEO_VARIANTS: &[(&str, &[&str])] = &[
    (
        "half, mpeg4",
        &[
            "-vf",
            "scale=trunc(iw/4)*2:-2",
            "-c:v",
            "mpeg4",
            "-q:v",
            "8",
        ],
    ),
    (
        "720p h264 crf 28",
        &[
            "-vf",
            "scale=-2:'min(720,ih)'",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "28",
        ],
    ),
    (
        "480p h264 crf 33",
        &[
            "-vf",
            "scale=-2:'min(480,ih)'",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "33",
        ],
    ),
];

/// Re-encodings that should still count as the same audio.
const AUDIO_VARIANTS: &[(&str, &str, &[&str])] = &[
    ("aac 96k", "m4a", &["-c:a", "aac", "-b:a", "96k"]),
    ("mp3 128k", "mp3", &["-c:a", "libmp3lame", "-b:a", "128k"]),
    ("opus 48k", "opus", &["-c:a", "libopus", "-b:a", "48k"]),
    (
        "mp2 64k mono",
        "mp2",
        &["-c:a", "mp2", "-b:a", "64k", "-ac", "1"],
    ),
];

fn die(msg: &str) -> ! {
    eprintln!("{}", msg);
    exit(1)
}

/// Run `ffmpeg` with input `input` and the given output options into `output`.
fn encode(ffmpeg: &Path, input: &Path, options: &[&str], output: &Path) -> bool {
    Command::new(ffmpeg)
        .args(["-nostdin", "-y", "-loglevel", "error", "-i"])
        .arg(input)
        .args(options)
        .args(
            ["-an"]
                .iter()
                .filter(|_| options.iter().any(|o| o.starts_with("-c:v"))),
        )
        .arg(output)
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// How many frame pairs have hashes at most `threshold` bits apart, and of how many have a
/// hash in both.
fn frame_counts(a: &[Frame], b: &[Frame], threshold: u32) -> (usize, usize) {
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
    (matching, both)
}

fn main() {
    let opt = Opt::parse();
    let tools = Tools::find();
    let (Some((ffmpeg, _)), Some(ffprobe)) = (&tools.ffmpeg, &tools.ffprobe) else {
        die("ffmpeg and ffprobe must be on PATH");
    };
    let catalog = Catalog::open_existing(&opt.catalog).unwrap_or_else(|e| die(&e.to_string()));
    let out = resolve(&opt.out).unwrap_or_else(|e| die(&e.to_string()));
    for library in catalog.libraries().unwrap_or_else(|e| die(&e.to_string())) {
        if out.starts_with(&library.root) {
            die("--out must be outside every library");
        }
    }
    if let Some(folder) = &opt.audio {
        let folder = resolve(folder).unwrap_or_else(|e| die(&e.to_string()));
        if out.starts_with(&folder) || folder.starts_with(&out) {
            die("--out and --audio must not contain each other");
        }
    }
    fs::create_dir_all(&out).unwrap_or_else(|e| die(&e.to_string()));

    let mut videos: Vec<HashedAv> = catalog
        .hashed_av()
        .unwrap_or_else(|e| die(&e.to_string()))
        .into_iter()
        .filter(|v| v.frames.is_some())
        .collect();
    videos.sort_by(|a, b| (&a.file.root, &a.file.path).cmp(&(&b.file.root, &b.file.path)));
    println!("{} videos with signatures", videos.len());
    let with_detail: Vec<usize> = videos
        .iter()
        .map(|v| v.frames.as_ref().map_or(0, |f| f.iter().flatten().count()))
        .collect();
    println!(
        "Frames with detail per video: {:?} (of {})",
        histogram(&with_detail, FRAMES),
        FRAMES
    );

    // Copies that should match.
    println!();
    println!("Re-encoded copies: matching frames (of frames with a hash in both), per copy:");
    let step = (videos.len() / opt.sample.max(1)).max(1);
    for (name, options) in VIDEO_VARIANTS {
        let mut results = vec![];
        for (n, video) in videos.iter().step_by(step).take(opt.sample).enumerate() {
            let source = library_path(Path::new(&video.file.root), &video.file.path);
            let copy = out.join(format!("variant-{}.mp4", n));
            if !encode(ffmpeg, &source, options, &copy) {
                continue;
            }
            if let Ok(sig) = video_signature(ffmpeg, ffprobe, &copy) {
                let original = video.frames.as_deref().unwrap_or_default();
                let (matching, both) = frame_counts(original, &sig.frames, DEFAULT_THRESHOLD);
                let matched = video_durations_match(video.duration_ms, sig.duration_ms)
                    && frames_match(original, &sig.frames, DEFAULT_THRESHOLD);
                results.push((matching, both, matched));
            }
            let _ = fs::remove_file(&copy);
        }
        let matched = results.iter().filter(|r| r.2).count();
        let pairs: Vec<String> = results
            .iter()
            .map(|(m, b, _)| format!("{}/{}", m, b))
            .collect();
        println!(
            "  {:<18} {} of {} match by the rule; {}",
            name,
            matched,
            results.len(),
            pairs.join(" ")
        );
    }

    // Pairs of different videos in the catalog.
    let mut close = 0;
    let mut by_matching = vec![0usize; FRAMES + 1];
    let mut close_matched = 0;
    let mut all_pairs = 0;
    let mut any_matched = 0;
    for (i, a) in videos.iter().enumerate() {
        for b in &videos[i + 1..] {
            let (fa, fb) = (
                a.frames.as_deref().unwrap_or_default(),
                b.frames.as_deref().unwrap_or_default(),
            );
            let matched = frames_match(fa, fb, DEFAULT_THRESHOLD);
            all_pairs += 1;
            any_matched += usize::from(matched);
            if !video_durations_match(a.duration_ms, b.duration_ms)
                || !aspects_match(aspect(a.width, a.height), aspect(b.width, b.height))
            {
                continue;
            }
            close += 1;
            by_matching[frame_counts(fa, fb, DEFAULT_THRESHOLD).0] += 1;
            close_matched += usize::from(matched);
        }
    }
    println!();
    println!(
        "Pairs of different videos with close durations and the same shape: {}; by matching \
         frames 0..{}: {:?}; matching by the rule: {}",
        close, FRAMES, by_matching, close_matched
    );
    // All pairs, whatever their durations: how often would the frames alone match?
    println!(
        "All {} pairs of different videos, ignoring durations: {} match by frames",
        all_pairs, any_matched
    );

    if let Some(folder) = &opt.audio {
        calibrate_audio(&tools, folder, &out);
    }
}

/// Counts of `values` from 0 to `max`.
fn histogram(values: &[usize], max: usize) -> Vec<usize> {
    let mut counts = vec![0; max + 1];
    for &v in values {
        counts[v.min(max)] += 1;
    }
    counts
}

fn calibrate_audio(tools: &Tools, folder: &Path, out: &Path) {
    let (Some((ffmpeg, _)), Some((fpcalc, _))) = (&tools.ffmpeg, &tools.fpcalc) else {
        die("ffmpeg and fpcalc must be on PATH for audio");
    };
    let files: Vec<PathBuf> = walkdir::WalkDir::new(folder)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| pixrs::media::MediaType::from_path(p) == Some(pixrs::media::MediaType::Audio))
        .collect();
    let prints: Vec<AudioFingerprint> = files
        .iter()
        .filter_map(|f| audio_fingerprint(fpcalc, f).ok())
        .collect();
    println!();
    println!("{} audio files fingerprinted", prints.len());
    for (name, ext, options) in AUDIO_VARIANTS {
        let mut rates = vec![];
        for (n, file) in files.iter().enumerate() {
            let copy = out.join(format!("audio-{}.{}", n, ext));
            if !encode(ffmpeg, file, options, &copy) {
                continue;
            }
            if let (Ok(a), Ok(b)) = (
                audio_fingerprint(fpcalc, file),
                audio_fingerprint(fpcalc, &copy),
            ) {
                rates.push(bit_error_rate(&a.values, &b.values));
            }
            let _ = fs::remove_file(&copy);
        }
        rates.sort_by(f64::total_cmp);
        println!(
            "  {:<14} bit error rate of copies: min {:.3}, median {:.3}, max {:.3} ({} files)",
            name,
            rates.first().copied().unwrap_or(f64::NAN),
            rates.get(rates.len() / 2).copied().unwrap_or(f64::NAN),
            rates.last().copied().unwrap_or(f64::NAN),
            rates.len()
        );
    }
    let mut unrelated = vec![];
    for (i, a) in prints.iter().enumerate() {
        for b in &prints[i + 1..] {
            unrelated.push((
                bit_error_rate(&a.values, &b.values),
                audio_durations_match(a.duration_ms, b.duration_ms),
            ));
        }
    }
    unrelated.sort_by(|a, b| a.0.total_cmp(&b.0));
    let close = unrelated.iter().filter(|u| u.1).count();
    println!(
        "  unrelated pairs: {} ({} with close durations); bit error rate min {:.3}, median {:.3}",
        unrelated.len(),
        close,
        unrelated.first().map_or(f64::NAN, |u| u.0),
        unrelated.get(unrelated.len() / 2).map_or(f64::NAN, |u| u.0)
    );
}
