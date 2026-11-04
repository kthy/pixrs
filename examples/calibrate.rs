//! Measure how far copies and non-copies are apart on a real library, to choose how `pixrs dupes`
//! decides that images are similar. A development tool, not part of `pixrs` itself.
//!
//! Scan the library first, with the catalog outside it, then:
//!
//! ```text
//! cargo run --release --example calibrate -- --catalog CATALOG --out DIR
//! ```
//!
//! Candidates are pairs of different images whose perceptual hashes are close and whose shapes
//! match. For each, the tool measures how their fingerprints differ, in several ways, and does
//! the same for resized and recompressed copies it makes of a sample (in memory only). It
//! prints counts and statistics only, no file names.
//!
//! It reads the library and writes nothing but `DIR`: a review page (`DIR/index.html`) of
//! candidate pairs spread over the range of fingerprint differences, for a person to judge
//! which are the same picture. Paste the page's JSON into a file and pass it as `--labels` to
//! see how well each way of measuring separates the answers.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use clap::Parser;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::DynamicImage;
use pixrs::catalog::{library_path, Catalog, HashedImage};
use pixrs::decode::{decode_in_process, decode_with_ffmpeg, find_tool, image_decoder, Decoder};
use pixrs::dupes::aspects_match;
use pixrs::imagehash::{distance, fingerprint_differences, perceptual_hash};
use pixrs::scan::resolve;
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

#[derive(Debug, Parser)]
struct Opt {
    /// The catalog of the scanned library
    #[arg(long)]
    catalog: PathBuf,
    /// Folder for the review page; must be outside every cataloged library
    #[arg(long)]
    out: PathBuf,
    /// Number of images to make resized and recompressed variants of
    #[arg(long, default_value_t = 300)]
    sample: usize,
    /// Largest perceptual hash distance for a candidate pair
    #[arg(long, default_value_t = 8)]
    candidates: u32,
    /// Number of groups the review page splits the candidates into, by fingerprint difference
    #[arg(long, default_value_t = 16)]
    buckets: usize,
    /// Pairs per group on the review page
    #[arg(long, default_value_t = 12)]
    pairs: usize,
    /// Answers from the review page (its JSON), to measure each way of comparing against
    #[arg(long)]
    labels: Option<PathBuf>,
    /// With --labels: fingerprint sides to compare, decoding the answered pairs and the sample
    /// again (e.g. 32,64,128,256)
    #[arg(long, value_delimiter = ',')]
    sizes: Vec<u32>,
}

/// Transformations that should still count as the same picture.
const VARIANTS: &[&str] = &[
    "half size",
    "1024 px",
    "JPEG q85",
    "JPEG q60",
    "half + q60",
    "640 px + q50",
];

/// Ways of summing up fingerprint differences into one number.
const METRICS: &[&str] = &["mean", "p90", "p95", "p99", "max"];

/// The metric the review page spreads its pairs over.
const REVIEW_METRIC: usize = 3;

/// A candidate pair and how it differs.
struct Candidate {
    a: usize,
    b: usize,
    hash_distance: u32,
    metrics: [f64; 5],
}

/// One answer from the review page.
#[derive(Debug, Deserialize)]
struct Label {
    a: String,
    b: String,
    different: bool,
}

fn main() {
    let opt = Opt::parse();
    let catalog = Catalog::open_existing(&opt.catalog).unwrap_or_else(|e| die(&e.to_string()));
    let roots: Vec<String> = catalog
        .libraries()
        .unwrap_or_else(|e| die(&e.to_string()))
        .into_iter()
        .map(|l| l.root)
        .collect();
    check_outside(&opt.out, &roots);
    // Images with almost no detail have no perceptual hash and are never similar.
    let mut images: Vec<HashedImage> = catalog
        .hashed_images(true)
        .unwrap_or_else(|e| die(&e.to_string()))
        .into_iter()
        .filter(|image| image.visual.is_some())
        .collect();
    // A fixed order, so samples and saved answers stay the same between runs.
    images.sort_by(|a, b| (&a.file.root, &a.file.path).cmp(&(&b.file.root, &b.file.path)));
    if images.len() < 2 {
        die("The catalog has fewer than two images with fingerprints. Run `pixrs scan` (again).");
    }
    let ffmpeg = find_tool("ffmpeg");
    println!(
        "{} images with a perceptual hash and fingerprint",
        images.len()
    );

    let candidates = find_candidates(&images, opt.candidates);
    println!(
        "{} candidate pairs: different pixels, perceptual hashes at most {} bits apart, same shape",
        candidates.len(),
        opt.candidates
    );
    let variants = sample_variants(&opt, &images, &roots, ffmpeg.as_deref());

    print_variants(&variants);
    print_spread("Library candidates", candidates.iter().map(|c| c.metrics));
    if let Some(path) = &opt.labels {
        print_labels(path, &images, &candidates, &variants);
        if !opt.sizes.is_empty() {
            compare_sizes(&opt, path, &images, &roots, ffmpeg.as_deref());
        }
    }
    write_review(&opt, &images, &roots, ffmpeg.as_deref(), &candidates);
}

/// Exit with a message.
fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    exit(1)
}

/// Refuse an output folder inside a library, with symlinks and `..` resolved.
fn check_outside(out: &Path, roots: &[String]) {
    let out = resolve(out).unwrap_or_else(|e| die(&e.to_string()));
    for root in roots {
        if out.starts_with(root) {
            die(&format!("--out must be outside the library {root}"));
        }
    }
}

/// A stable ID for an image, used in file names and the review answers.
fn image_id(image: &HashedImage) -> String {
    let f = &image.file;
    let id = format!("{}\0{}\0{}", f.root, f.path, f.mtime_ns);
    let digest = ring::digest::digest(&ring::digest::SHA256, id.as_bytes());
    data_encoding::HEXLOWER.encode(&digest.as_ref()[..12])
}

/// Mean, 90th, 95th and 99th percentile, and largest of sorted differences.
#[allow(
    clippy::cast_precision_loss,
    reason = "statistics over counts far below 2^52"
)]
fn metrics(diffs: &[u8]) -> [f64; 5] {
    if diffs.is_empty() {
        return [255.0; 5];
    }
    let at = |p: usize| f64::from(diffs[(diffs.len() * p / 100).min(diffs.len() - 1)]);
    let mean = diffs.iter().map(|&d| f64::from(d)).sum::<f64>() / diffs.len() as f64;
    [
        mean,
        at(90),
        at(95),
        at(99),
        f64::from(diffs[diffs.len() - 1]),
    ]
}

fn find_candidates(images: &[HashedImage], max_distance: u32) -> Vec<Candidate> {
    let mut candidates = vec![];
    for a in 0..images.len() {
        for b in a + 1..images.len() {
            let (x, y) = (&images[a], &images[b]);
            if x.pixel_hash == y.pixel_hash || !aspects_match(x.dimensions, y.dimensions) {
                continue;
            }
            let (Some(xv), Some(yv)) = (&x.visual, &y.visual) else {
                continue;
            };
            let hash_distance = distance(xv.phash, yv.phash);
            if hash_distance <= max_distance {
                let diffs = fingerprint_differences(&xv.fingerprint, &yv.fingerprint);
                candidates.push(Candidate {
                    a,
                    b,
                    hash_distance,
                    metrics: metrics(&diffs),
                });
            }
        }
    }
    candidates
}

/// Where an image is on disk.
fn full_path(image: &HashedImage, roots: &[String]) -> PathBuf {
    let root = roots
        .iter()
        .find(|r| **r == image.file.root)
        .expect("image roots are cataloged libraries");
    library_path(Path::new(root), &image.file.path)
}

/// Decode an image upright, as the scan does for its perceptual hash.
fn decode(image: &HashedImage, roots: &[String], ffmpeg: Option<&Path>) -> Option<DynamicImage> {
    let path = full_path(image, roots);
    match image_decoder(&image.file.path)? {
        Decoder::InProcess => {
            let (mut img, orientation) = decode_in_process(&path).ok()?;
            img.apply_orientation(orientation);
            Some(img)
        }
        Decoder::Ffmpeg => decode_with_ffmpeg(ffmpeg?, &path).ok(),
    }
}

/// Shrink to fit `max` pixels, if larger.
fn fit(img: &DynamicImage, max: u32) -> DynamicImage {
    if img.width() > max || img.height() > max {
        img.resize(max, max, FilterType::Triangle)
    } else {
        img.clone()
    }
}

/// Encode as JPEG and decode again.
fn recompress(img: &DynamicImage, quality: u8) -> DynamicImage {
    let mut jpeg = vec![];
    JpegEncoder::new_with_quality(&mut jpeg, quality)
        .encode_image(&img.to_rgb8())
        .expect("encoding to memory");
    image::load_from_memory(&jpeg).expect("decoding our own JPEG")
}

fn make_variant(img: &DynamicImage, name: &str) -> DynamicImage {
    let half = |img: &DynamicImage| {
        img.resize_exact(
            (img.width() / 2).max(1),
            (img.height() / 2).max(1),
            FilterType::Triangle,
        )
    };
    match name {
        "half size" => half(img),
        "1024 px" => fit(img, 1024),
        "JPEG q85" => recompress(img, 85),
        "JPEG q60" => recompress(img, 60),
        "half + q60" => recompress(&half(img), 60),
        "640 px + q50" => recompress(&fit(img, 640), 50),
        _ => unreachable!("unknown variant {}", name),
    }
}

/// For each variant: the perceptual hash distance and fingerprint metrics of each sampled
/// image's variant to the image. Split across threads; decoding full-size photos is slow.
fn sample_variants(
    opt: &Opt,
    images: &[HashedImage],
    roots: &[String],
    ffmpeg: Option<&Path>,
) -> Vec<Vec<(u32, [f64; 5])>> {
    let step = (images.len() / opt.sample.max(1)).max(1);
    let sample: Vec<&HashedImage> = images.iter().step_by(step).take(opt.sample).collect();
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(8);
    let chunk = sample.len().div_ceil(workers).max(1);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = sample
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    let mut results = vec![vec![]; VARIANTS.len()];
                    for image in part {
                        let Some(upright) = decode(image, roots, ffmpeg) else {
                            continue;
                        };
                        let Some(visual) = &image.visual else {
                            continue;
                        };
                        let (original, fingerprint) =
                            (visual.phash, visual.fingerprint.as_slice());
                        for (v, name) in VARIANTS.iter().enumerate() {
                            // A variant without detail counts as missed.
                            let measured = match perceptual_hash(&make_variant(&upright, name)) {
                                Some(variant) => (
                                    distance(original, variant.phash),
                                    metrics(&fingerprint_differences(
                                        fingerprint,
                                        &variant.fingerprint,
                                    )),
                                ),
                                None => (64, [255.0; 5]),
                            };
                            results[v].push(measured);
                        }
                    }
                    results
                })
            })
            .collect();
        let mut all = vec![vec![]; VARIANTS.len()];
        for handle in handles {
            for (v, part) in handle
                .join()
                .expect("sampling thread")
                .into_iter()
                .enumerate()
            {
                all[v].extend(part);
            }
        }
        all
    });
    eprintln!("{} images sampled for variants", results[0].len());
    results
}

/// The `p`th percentile of unsorted values.
fn percentile(values: &mut [f64], p: usize) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    values[(values.len() * p / 100).min(values.len() - 1)]
}

fn print_variants(variants: &[Vec<(u32, [f64; 5])>]) {
    println!();
    println!(
        "Resized and recompressed copies of the sample (these should all match). Columns: \
         perceptual hash distance at the 99th percentile and worst; each fingerprint metric at \
         the 99th percentile and worst of the sample."
    );
    let mut header = format!("{:<14} {:>9} {:>9}", "variant", "hash p99", "hash max");
    for m in METRICS {
        let _ = write!(
            header,
            " {:>9} {:>9}",
            format!("{} p99", m),
            format!("{} max", m)
        );
    }
    println!("{header}");
    for (v, name) in VARIANTS.iter().enumerate() {
        let mut hashes: Vec<f64> = variants[v].iter().map(|(d, _)| f64::from(*d)).collect();
        let mut line = format!(
            "{:<14} {:>9.0} {:>9.0}",
            name,
            percentile(&mut hashes, 99),
            percentile(&mut hashes, 100)
        );
        for m in 0..METRICS.len() {
            let mut values: Vec<f64> = variants[v].iter().map(|(_, ms)| ms[m]).collect();
            let _ = write!(
                line,
                " {:>9.1} {:>9.1}",
                percentile(&mut values, 99),
                percentile(&mut values, 100)
            );
        }
        println!("{line}");
    }
}

/// Print how a set of measurements is spread, per metric.
fn print_spread(title: &str, all: impl Iterator<Item = [f64; 5]>) {
    let all: Vec<[f64; 5]> = all.collect();
    println!();
    println!(
        "{} ({}), fingerprint metrics by percentile:",
        title,
        all.len()
    );
    println!(
        "{:<6} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "metric", "p1", "p5", "p10", "p25", "p50", "p75", "p90"
    );
    for (m, name) in METRICS.iter().enumerate() {
        let mut values: Vec<f64> = all.iter().map(|ms| ms[m]).collect();
        let mut line = format!("{name:<6}");
        for p in [1, 5, 10, 25, 50, 75, 90] {
            let _ = write!(line, " {:>7.1}", percentile(&mut values, p));
        }
        println!("{line}");
    }
}

/// Measure each metric against the answers from the review page: for each limit, the share of
/// synthetic copies kept, and how many answered pairs land on each side.
#[allow(
    clippy::cast_precision_loss,
    reason = "statistics over counts far below 2^52"
)]
fn print_labels(
    path: &Path,
    images: &[HashedImage],
    candidates: &[Candidate],
    variants: &[Vec<(u32, [f64; 5])>],
) {
    let text = fs::read_to_string(path).unwrap_or_else(|e| die(&e.to_string()));
    let labels: Vec<Label> = serde_json::from_str(&text).unwrap_or_else(|e| die(&e.to_string()));
    let ids: Vec<String> = images.iter().map(image_id).collect();
    let by_pair: HashMap<(&str, &str), &Candidate> = candidates
        .iter()
        .map(|c| ((ids[c.a].as_str(), ids[c.b].as_str()), c))
        .collect();
    let answered: Vec<(&Candidate, bool)> = labels
        .iter()
        .filter_map(|l| {
            by_pair
                .get(&(l.a.as_str(), l.b.as_str()))
                .or_else(|| by_pair.get(&(l.b.as_str(), l.a.as_str())))
                .map(|c| (*c, l.different))
        })
        .collect();
    let same = answered.iter().filter(|(_, d)| !d).count();
    println!();
    println!(
        "Answers: {} pairs matched ({} the same picture, {} different).",
        answered.len(),
        same,
        answered.len() - same
    );
    println!(
        "For each metric and limit: copies kept is the share of synthetic copies within the \
         limit; same kept and different let in count answered pairs within it."
    );
    println!(
        "{:<6} {:>6} {:>12} {:>10} {:>16}",
        "metric", "limit", "copies kept", "same kept", "different let in"
    );
    for (m, name) in METRICS.iter().enumerate() {
        for limit in [
            2.0, 3.0, 4.0, 6.0, 8.0, 10.0, 12.0, 16.0, 20.0, 24.0, 32.0, 48.0,
        ] {
            let copies: Vec<bool> = variants
                .iter()
                .flatten()
                .map(|(_, ms)| ms[m] <= limit)
                .collect();
            let kept = copies.iter().filter(|&&k| k).count() as f64 / copies.len().max(1) as f64;
            let same_kept = answered
                .iter()
                .filter(|(c, d)| !d && c.metrics[m] <= limit)
                .count();
            let let_in = answered
                .iter()
                .filter(|(c, d)| *d && c.metrics[m] <= limit)
                .count();
            println!(
                "{:<6} {:>6.0} {:>11.1}% {:>6} of {:<3} {:>8} of {:<3}",
                name,
                limit,
                100.0 * kept,
                same_kept,
                same,
                let_in,
                answered.len() - same
            );
        }
    }
    let mut hash_of_same: Vec<f64> = answered
        .iter()
        .filter(|(_, d)| !d)
        .map(|(c, _)| f64::from(c.hash_distance))
        .collect();
    println!(
        "Largest perceptual hash distance among pairs answered the same: {}",
        percentile(&mut hash_of_same, 100)
    );
}

/// Fingerprint of an upright image at `side` x `side` pixels.
fn fingerprint_at(upright: &DynamicImage, side: u32) -> Vec<u8> {
    upright
        .resize_exact(side, side, FilterType::Triangle)
        .to_luma8()
        .into_raw()
}

/// For each fingerprint size: how well each metric separates the answered pairs, with synthetic
/// copies standing in for "same picture" answers (there are few of those).
#[allow(
    clippy::too_many_lines,
    reason = "a calibration run, read top to bottom"
)]
fn compare_sizes(
    opt: &Opt,
    path: &Path,
    images: &[HashedImage],
    roots: &[String],
    ffmpeg: Option<&Path>,
) {
    let text = fs::read_to_string(path).unwrap_or_else(|e| die(&e.to_string()));
    let labels: Vec<Label> = serde_json::from_str(&text).unwrap_or_else(|e| die(&e.to_string()));
    let index: HashMap<String, usize> = images
        .iter()
        .enumerate()
        .map(|(i, image)| (image_id(image), i))
        .collect();
    let sizes = &opt.sizes;
    // Fingerprints at every size of every image in an answered pair, decoded once.
    let mut wanted: Vec<usize> = labels
        .iter()
        .flat_map(|l| [index.get(&l.a), index.get(&l.b)])
        .flatten()
        .copied()
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    let decode_all = |list: &[usize]| -> HashMap<usize, DynamicImage> {
        std::thread::scope(|scope| {
            let workers = std::thread::available_parallelism()
                .map_or(1, std::num::NonZero::get)
                .min(8);
            let chunk = list.len().div_ceil(workers).max(1);
            #[allow(
                clippy::needless_collect,
                reason = "every thread must start before any is joined"
            )]
            let handles: Vec<_> = list
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .filter_map(|&i| {
                                // Shrunk once; the largest fingerprint size is enough.
                                decode(&images[i], roots, ffmpeg).map(|img| (i, fit(&img, 1024)))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("decoding thread"))
                .collect()
        })
    };
    let decoded = decode_all(&wanted);
    eprintln!("{} answered images decoded", decoded.len());
    let step = (images.len() / opt.sample.max(1)).max(1);
    let sample: Vec<usize> = (0..images.len()).step_by(step).take(opt.sample).collect();
    let originals = decode_all(&sample);
    eprintln!("{} sample images decoded", originals.len());

    println!();
    println!(
        "Fingerprint size experiment. For each size and metric, the limit that keeps 99.5% of \
         synthetic copies, and how many answered pairs it lets in."
    );
    println!(
        "{:>5} {:<6} {:>7} {:>12} {:>16}",
        "side", "metric", "limit", "same kept", "different let in"
    );
    for &side in sizes {
        // Synthetic copies at this size.
        let mut copies: Vec<[f64; 5]> = vec![];
        for upright in originals.values() {
            let original = fingerprint_at(upright, side);
            for name in VARIANTS {
                let variant = fingerprint_at(&make_variant(upright, name), side);
                copies.push(metrics(&fingerprint_differences(&original, &variant)));
            }
        }
        let answered: Vec<([f64; 5], bool)> = labels
            .iter()
            .filter_map(|l| {
                let (a, b) = (
                    decoded.get(index.get(&l.a)?)?,
                    decoded.get(index.get(&l.b)?)?,
                );
                let diffs =
                    fingerprint_differences(&fingerprint_at(a, side), &fingerprint_at(b, side));
                Some((metrics(&diffs), l.different))
            })
            .collect();
        let same = answered.iter().filter(|(_, d)| !d).count();
        for (m, name) in METRICS.iter().enumerate() {
            let mut values: Vec<f64> = copies.iter().map(|ms| ms[m]).collect();
            if values.is_empty() {
                die("No sample image could be decoded.");
            }
            values.sort_by(f64::total_cmp);
            let limit = values[(values.len() * 995 / 1000).min(values.len() - 1)];
            let same_kept = answered
                .iter()
                .filter(|(ms, d)| !d && ms[m] <= limit)
                .count();
            let let_in = answered
                .iter()
                .filter(|(ms, d)| *d && ms[m] <= limit)
                .count();
            println!(
                "{:>5} {:<6} {:>7.1} {:>7} of {:<3} {:>8} of {:<3}",
                side,
                name,
                limit,
                same_kept,
                same,
                let_in,
                answered.len() - same
            );
        }
    }
}

/// Write `DIR/index.html`, `DIR/thumbs/` and `DIR/large/` with candidate pairs, grouped by the
/// review metric so the page covers its whole range.
#[allow(
    clippy::cast_possible_truncation,
    reason = "statistics over counts far below 2^52"
)]
fn write_review(
    opt: &Opt,
    images: &[HashedImage],
    roots: &[String],
    ffmpeg: Option<&Path>,
    candidates: &[Candidate],
) {
    let (thumbs, large) = (opt.out.join("thumbs"), opt.out.join("large"));
    for dir in [&thumbs, &large] {
        fs::create_dir_all(dir).unwrap_or_else(|e| die(&e.to_string()));
    }
    // A 320 px thumbnail and a 1600 px version to compare side by side.
    let renditions = |i: usize| -> (String, String) {
        let name = format!("{}.jpg", image_id(&images[i]));
        let (small_path, large_path) = (thumbs.join(&name), large.join(&name));
        if !small_path.exists() || !large_path.exists() {
            if let Some(img) = decode(&images[i], roots, ffmpeg) {
                for (path, size, quality) in [(&small_path, 320, 80), (&large_path, 1600, 90)] {
                    let mut jpeg = vec![];
                    if JpegEncoder::new_with_quality(&mut jpeg, quality)
                        .encode_image(&fit(&img, size).to_rgb8())
                        .is_ok()
                    {
                        let _ = fs::write(path, jpeg);
                    }
                }
            }
        }
        (format!("thumbs/{name}"), format!("large/{name}"))
    };

    let mut order: Vec<&Candidate> = candidates.iter().collect();
    order.sort_by(|x, y| x.metrics[REVIEW_METRIC].total_cmp(&y.metrics[REVIEW_METRIC]));
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut html = String::from(REVIEW_HEAD);
    let bucket = order.len().div_ceil(opt.buckets.max(1)).max(1);
    for (n, part) in order.chunks(bucket).enumerate() {
        // A random sample of the bucket, in a repeatable order.
        let mut part: Vec<&Candidate> = part.to_vec();
        for k in (1..part.len()).rev() {
            part.swap(k, rng.below(k as u64 + 1) as usize);
        }
        part.truncate(opt.pairs);
        let (low, high) = (
            order[n * bucket].metrics[REVIEW_METRIC],
            order[((n + 1) * bucket).min(order.len()) - 1].metrics[REVIEW_METRIC],
        );
        let _ = write!(
            html,
            "<section><h2>Group {n}: {metric} {low:.0} to {high:.0}</h2>",
            n = n + 1,
            metric = METRICS[REVIEW_METRIC],
            low = low,
            high = high
        );
        for c in part {
            let _ = write!(
                html,
                "<div class=\"pair\" data-a=\"{}\" data-b=\"{}\">",
                image_id(&images[c.a]),
                image_id(&images[c.b])
            );
            for k in [c.a, c.b] {
                let f = &images[k].file;
                let (small, big) = renditions(k);
                let (width, height) = images[k].dimensions.map_or((0, 0), |d| (d.width, d.height));
                let _ = write!(
                    html,
                    "<figure><img loading=\"lazy\" src=\"{}\" data-large=\"{}\">\
                     <figcaption>{}<br>{} KB, {} x {}</figcaption></figure>",
                    small,
                    big,
                    escape(&f.path),
                    f.size / 1000,
                    width,
                    height
                );
            }
            html.push_str("<label><input type=\"checkbox\"> different pictures</label></div>");
        }
        html.push_str("</section>");
        eprintln!("Review page: group {} done", n + 1);
    }
    html.push_str(REVIEW_TAIL);
    let index = opt.out.join("index.html");
    fs::write(&index, html).unwrap_or_else(|e| die(&e.to_string()));
    println!();
    println!("Review page: {}", index.display());
}

/// Escape text for HTML.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A small deterministic random number generator (xorshift), so runs are repeatable.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }
}

const REVIEW_HEAD: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>PixRS similarity review</title>
<style>
body { font: 14px/1.4 system-ui, sans-serif; margin: 1rem; }
#summary { position: sticky; top: 0; background: #fff; padding: 0.5rem 0;
           border-bottom: 1px solid #ccc; z-index: 5; }
#json { max-height: 4rem; overflow: auto; white-space: pre-wrap; word-break: break-all;
        font-size: 11px; }
.pair { display: inline-flex; gap: 0.5rem; align-items: flex-end; margin: 0 1.5rem 1.5rem 0;
        padding: 0.5rem; border: 1px solid #ccc; border-radius: 6px; }
.pair.different { background: #fde2e2; }
figure { margin: 0; width: 240px; }
figure img { width: 240px; height: 240px; object-fit: contain; background: #eee;
             cursor: zoom-in; }
figcaption { font-size: 12px; overflow-wrap: anywhere; }
#overlay { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.92); color: #eee;
           display: flex; flex-direction: column; z-index: 10; }
#overlay[hidden] { display: none; }
#overlay .bar { display: flex; gap: 1.5rem; align-items: center; padding: 0.5rem 1rem; }
#overlay .bar label { font-size: 16px; }
#overlay .images { flex: 1; display: flex; gap: 0.5rem; padding: 0 0.5rem 0.5rem;
                   min-height: 0; }
#overlay .images figure { flex: 1; width: auto; margin: 0; display: flex;
                          flex-direction: column; min-width: 0; }
#overlay .images img { flex: 1; min-height: 0; width: 100%; height: auto; object-fit: contain;
                       background: #111; cursor: default; }
#overlay.flip .images figure:not(.shown) { display: none; }
</style></head><body>
<div id="overlay" hidden><div class="bar"><b id="o-title"></b>
<span>Space: flip between the two in one place. Left/right: previous/next pair. D: tick
"different". Esc: close.</span>
<label><input type="checkbox" id="o-different"> different pictures</label></div>
<div class="images"><figure id="o-a"><img><figcaption></figcaption></figure>
<figure id="o-b"><img><figcaption></figcaption></figure></div></div>
<div id="summary"><p>Tick the pairs that are <b>not</b> the same picture (count burst shots
as different). Click a thumbnail to compare the pair large. Then copy the JSON below and paste
it back.</p>
<table id="table"></table><button id="copy">Copy results</button>
<pre id="json"></pre></div>
"#;

const REVIEW_TAIL: &str = r#"
<script>
const sections = [...document.querySelectorAll("section")];
const pairs = [...document.querySelectorAll(".pair")];
function results() {
  return pairs.map(p => ({ a: p.dataset.a, b: p.dataset.b,
                           different: p.querySelector("input").checked }));
}
function update() {
  const rows = sections.map(s => {
    const ticked = s.querySelectorAll("input:checked").length;
    const n = s.querySelectorAll(".pair").length;
    return `<tr><td>${s.querySelector("h2").textContent}</td><td>${ticked} of ${n} different</td></tr>`;
  });
  document.getElementById("table").innerHTML = rows.join("");
  pairs.forEach(p => p.classList.toggle("different", p.querySelector("input").checked));
  // Clipboard access can be refused for local files; the JSON is also shown to copy by hand.
  document.getElementById("json").textContent = JSON.stringify(results());
}
document.addEventListener("change", update);
document.getElementById("copy").addEventListener("click", () =>
  navigator.clipboard.writeText(JSON.stringify(results())));

// Click a thumbnail to compare its pair large.
const overlay = document.getElementById("overlay");
const different = document.getElementById("o-different");
let current = -1;
function show(n) {
  if (n < 0 || n >= pairs.length) return;
  current = n;
  const pair = pairs[n];
  const figures = pair.querySelectorAll("figure");
  ["o-a", "o-b"].forEach((id, k) => {
    const target = document.getElementById(id);
    target.querySelector("img").src = figures[k].querySelector("img").dataset.large;
    target.querySelector("figcaption").innerHTML = figures[k].querySelector("figcaption").innerHTML;
    target.classList.toggle("shown", k === 0);
  });
  document.getElementById("o-title").textContent =
    `${pair.closest("section").querySelector("h2").textContent}, pair ${n + 1} of ${pairs.length}`;
  different.checked = pair.querySelector("input").checked;
  overlay.hidden = false;
}
pairs.forEach((pair, n) =>
  pair.querySelectorAll("figure img").forEach(img => img.addEventListener("click", () => show(n))));
different.addEventListener("change", () => {
  pairs[current].querySelector("input").checked = different.checked;
  update();
});
document.addEventListener("keydown", e => {
  if (overlay.hidden) return;
  if (e.key === "Escape") { overlay.hidden = true; overlay.classList.remove("flip"); }
  else if (e.key === " ") {
    e.preventDefault();
    if (!overlay.classList.contains("flip")) overlay.classList.add("flip");
    else overlay.querySelectorAll(".images figure").forEach(f => f.classList.toggle("shown"));
  }
  else if (e.key === "ArrowRight") show(current + 1);
  else if (e.key === "ArrowLeft") show(current - 1);
  else if (e.key === "d" || e.key === "D") { different.checked = !different.checked;
    different.dispatchEvent(new Event("change")); }
});
update();
</script></body></html>
"#;
