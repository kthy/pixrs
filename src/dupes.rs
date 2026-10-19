//! Find duplicates in the catalog, exact and visual, and report them.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::catalog::{display_root, library_path, Catalog, HashedFile, HashedImage};
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
    /// Images the image hash pass has not decoded (new or changed since the last scan, or no
    /// decoder for them), so they may have same-pixel or similar copies this report does not
    /// show. Images the decoder rejected, and images with too little detail for a perceptual
    /// hash, are not counted.
    pub unhashed_images: usize,
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

/// What `find_dupes` reads from the catalog. Loading it is quick; grouping it compares every
/// pair of images and can take a while, so the viewer does that without holding the catalog.
pub struct DupeInput {
    exact: Vec<HashedFile>,
    images: Vec<HashedImage>,
    unhashed_candidates: usize,
    unhashed_images: usize,
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

/// Pairs of indices into `images` that are similar: perceptual hashes at most `threshold` bits
/// apart, matching aspect ratios, and matching fingerprints. Compares every pair, on all cores.
fn similar_pairs(images: &[Visual], threshold: u32) -> Vec<(usize, usize)> {
    let workers = available_parallelism().map_or(1, |n| n.get());
    thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|w| {
                scope.spawn(move || {
                    let mut pairs = vec![];
                    // Rows round-robin: row i has n - i - 1 comparisons, so this balances.
                    for i in (w..images.len()).step_by(workers) {
                        let (hash, aspect, fingerprint) = images[i];
                        for (j, &(other, other_aspect, other_fingerprint)) in
                            images.iter().enumerate().skip(i + 1)
                        {
                            // Cheapest test first: most pairs fail on the hash.
                            if distance(hash, other) <= threshold
                                && aspects_match(aspect, other_aspect)
                                && fingerprints_match(fingerprint, other_fingerprint)
                            {
                                pairs.push((i, j));
                            }
                        }
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

/// Read what `group_dupes` needs from the catalog. Perceptual hashes and fingerprints are only
/// read when `similar` is set.
pub fn load_dupes(
    catalog: &Catalog,
    libraries: &[i64],
    similar: bool,
) -> rusqlite::Result<DupeInput> {
    Ok(DupeInput {
        exact: catalog.duplicate_candidates()?,
        images: catalog.hashed_images(similar)?,
        unhashed_candidates: catalog.count_unhashed_with_shared_size(libraries)?,
        unhashed_images: catalog.count_unhashed_images(libraries)?,
    })
}

/// Find duplicates. Files are linked when they have the same size and SHA-256 (exact), or the
/// same pixel hash (same pixels). With `similar` set, images are also linked when their
/// perceptual hashes are at most that many bits apart, their aspect ratios match and their
/// fingerprints match (similar). A group is every file linked to another, directly or through
/// others, and has the weakest tier among its files: a group with an exact pair and a similar
/// third file is similar.
///
/// Similar matching is opt-in because on libraries with burst shots most similar pairs are
/// shots taken moments apart, not copies: on the owner's library, which has few resized copies,
/// the fingerprint check rejected about 95% of burst pairs, and those left still outnumbered
/// real copies.
///
/// With `libraries` empty, every group is reported. Otherwise only groups with at least one file
/// in the given libraries are, but still with all their files, so `pixrs dupes /old-disk` shows
/// what on the old disk also exists elsewhere.
///
/// In each group the kept file is the one with the earliest mtime. Ties are broken by path order:
/// library root first, then the path inside the library. In similar groups the largest image
/// (pixels, then bytes) is kept instead, since similar copies are often scaled down; the oldest
/// wins ties. The keeper is chosen from all files, so it does not depend on `libraries`.
///
/// The report reflects the catalog as of each library's last scan; files are not checked.
pub fn find_dupes(
    catalog: &Catalog,
    libraries: &[i64],
    similar: Option<u32>,
) -> rusqlite::Result<DupeReport> {
    let input = load_dupes(catalog, libraries, similar.is_some())?;
    Ok(group_dupes(input, libraries, similar))
}

/// The grouping half of `find_dupes`, for input from `load_dupes`.
pub fn group_dupes(input: DupeInput, libraries: &[i64], similar: Option<u32>) -> DupeReport {
    let mut nodes: Vec<Node> = vec![];
    let mut index: HashMap<(i64, String), usize> = HashMap::new();
    let mut add = |file: HashedFile| -> usize {
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
    };
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

    let mut sets = Sets((0..nodes.len()).collect());
    let mut exact_sets: HashMap<(u64, String), Vec<usize>> = HashMap::new();
    for &i in &exact {
        let key = (nodes[i].file.size, nodes[i].file.sha256.clone());
        exact_sets.entry(key).or_default().push(i);
    }
    for members in exact_sets.values() {
        // Byte-identical files have the same pixels and size, even if only one was decoded.
        let decoded = members
            .iter()
            .copied()
            .find(|&i| nodes[i].pixel_hash.is_some());
        let (pixel_hash, pixels, aspect) = match decoded {
            Some(i) => (
                nodes[i].pixel_hash.clone(),
                nodes[i].pixels,
                nodes[i].aspect,
            ),
            None => (None, 0, 0.0),
        };
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
    let pairs = match similar {
        Some(threshold) => similar_pairs(&visuals, threshold),
        None => vec![],
    };
    drop(visuals);
    for (a, b) in pairs {
        sets.union(hashed[a], hashed[b]);
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
            group.sort_by(|&a, &b| {
                let (a, b) = (&nodes[a], &nodes[b]);
                let oldest = || {
                    (a.file.mtime_ns, &a.file.root, &a.file.path).cmp(&(
                        b.file.mtime_ns,
                        &b.file.root,
                        &b.file.path,
                    ))
                };
                if tier == Tier::Similar {
                    // Similar copies are often smaller: keep the largest.
                    (b.pixels, b.file.size)
                        .cmp(&(a.pixels, a.file.size))
                        .then_with(oldest)
                } else {
                    oldest()
                }
            });
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
        unhashed_images: input.unhashed_images,
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
            _ => writeln!(out, "{} similar images:", n)?,
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

/// Quote a CSV field if it needs it (RFC 4180).
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
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
            find_dupes(&self.catalog, &[], Some(DEFAULT_THRESHOLD)).unwrap()
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
                unhashed_images: 3,
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
        let report = find_dupes(&fx.catalog, &[old_disk], Some(DEFAULT_THRESHOLD)).unwrap();
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
        let report = find_dupes(&fx.catalog, &[lib], Some(DEFAULT_THRESHOLD)).unwrap();
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
                    "unhashed_images": 2,
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
        crate::scan::scan(&lib, &catalog_path, true).unwrap();

        let catalog = Catalog::open_existing(&catalog_path).unwrap();
        let report = find_dupes(&catalog, &[], Some(DEFAULT_THRESHOLD)).unwrap();
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
        assert_eq!(report.summary.unhashed_images, 0);

        // Similar images are only looked for when asked.
        let report = find_dupes(&catalog, &[], None).unwrap();
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

        let report = find_dupes(&fx.catalog, &[], Some(3)).unwrap();
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
        let report = find_dupes(&fx.catalog, &[], Some(3)).unwrap();
        assert_eq!(kept_and_others(&report.groups[0])[0], (true, "c.jpg"));

        // Without the similar link the group is exact, and the oldest copy is kept.
        let report = find_dupes(&fx.catalog, &[], Some(2)).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].tier, Tier::Exact);
        assert_eq!(kept_and_others(&report.groups[0])[0], (true, "a.jpg"));
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

        let report = find_dupes(&fx.catalog, &[], Some(DEFAULT_THRESHOLD)).unwrap();
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
        let report = find_dupes(&fx.catalog, &[], Some(DEFAULT_THRESHOLD)).unwrap();
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
        let report = find_dupes(&fx.catalog, &[], Some(0)).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].tier, Tier::SamePixels);
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
        crate::scan::scan(&lib, &catalog_path, true).unwrap();

        let catalog = Catalog::open_existing(&catalog_path).unwrap();
        let report = find_dupes(&catalog, &[], Some(DEFAULT_THRESHOLD)).unwrap();
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
