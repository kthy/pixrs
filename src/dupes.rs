//! Find exact duplicates in the catalog and report them.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::catalog::{display_root, library_path, Catalog, HashedFile};
use indicatif::HumanBytes;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::OffsetDateTime;

/// A file in a duplicate group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DupeFile {
    #[serde(skip)]
    pub library_id: i64,
    /// Root of the library the file is in.
    pub library: String,
    /// Path relative to the library root, with `/` separators.
    pub path: String,
    /// Modification time, RFC 3339 in UTC.
    pub mtime: String,
    /// Modification time in nanoseconds since the Unix epoch.
    #[serde(skip)]
    pub mtime_ns: i64,
    /// Whether this is the copy that would be kept.
    pub keep: bool,
}

/// Files with identical contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DupeGroup {
    pub sha256: String,
    /// Size of each file in bytes.
    pub size: u64,
    /// The kept file first, then the others from oldest to newest.
    pub files: Vec<DupeFile>,
}

impl DupeGroup {
    /// Bytes freed by removing every copy but the kept one.
    pub fn reclaimable_bytes(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
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
}

/// All duplicate groups, largest reclaimable size first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DupeReport {
    pub groups: Vec<DupeGroup>,
    pub summary: Summary,
}

/// Find exact duplicates: files with the same size and SHA-256.
///
/// With `libraries` empty, every group is reported. Otherwise only groups with at least one file
/// in the given libraries are, but still with all their copies, so `pixrs dupes /old-disk` shows
/// what on the old disk also exists elsewhere.
///
/// In each group the kept file is the one with the earliest mtime. Ties are broken by path order:
/// library root first, then the path inside the library. The keeper is chosen from all copies,
/// so it does not depend on `libraries`.
///
/// The report reflects the catalog as of each library's last scan; files are not checked.
pub fn find_dupes(catalog: &Catalog, libraries: &[i64]) -> rusqlite::Result<DupeReport> {
    let mut by_content: BTreeMap<(u64, String), Vec<HashedFile>> = BTreeMap::new();
    for file in catalog.duplicate_candidates()? {
        by_content
            .entry((file.size, file.sha256.clone()))
            .or_default()
            .push(file);
    }

    let mut groups: Vec<DupeGroup> = by_content
        .into_iter()
        .filter(|(_, files)| {
            files.len() > 1
                && (libraries.is_empty()
                    || files.iter().any(|f| libraries.contains(&f.library_id)))
        })
        .map(|((size, sha256), mut files)| {
            files.sort_by(|a, b| {
                (a.mtime_ns, &a.root, &a.path).cmp(&(b.mtime_ns, &b.root, &b.path))
            });
            let files = files
                .into_iter()
                .enumerate()
                .map(|(i, f)| DupeFile {
                    library_id: f.library_id,
                    library: display_root(&f.root),
                    path: f.path,
                    mtime: format_mtime(f.mtime_ns),
                    mtime_ns: f.mtime_ns,
                    keep: i == 0,
                })
                .collect();
            DupeGroup {
                sha256,
                size,
                files,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.reclaimable_bytes()
            .cmp(&a.reclaimable_bytes())
            .then_with(|| a.sha256.cmp(&b.sha256))
    });

    let summary = Summary {
        groups: groups.len(),
        redundant_files: groups.iter().map(|g| g.files.len() - 1).sum(),
        reclaimable_bytes: groups.iter().map(DupeGroup::reclaimable_bytes).sum(),
        unhashed_candidates: catalog.count_unhashed_with_shared_size(libraries)?,
    };
    Ok(DupeReport { groups, summary })
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
        writeln!(
            out,
            "{} copies of {} (sha256 {}):",
            group.files.len(),
            HumanBytes(group.size),
            group.sha256.get(..12).unwrap_or(&group.sha256)
        )?;
        for file in &group.files {
            writeln!(
                out,
                "  {}  {}  {}",
                if file.keep { "keep" } else { "    " },
                library_path(Path::new(&file.library), &file.path).display(),
                human_mtime(file.mtime_ns)
            )?;
        }
        writeln!(out)?;
    }
    let s = &report.summary;
    if s.groups == 0 {
        writeln!(out, "No exact duplicates found.")
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

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
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

/// Write the report as CSV with a header row and one row per file.
pub fn write_csv(report: &DupeReport, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "group,sha256,size,keep,library,path,mtime")?;
    for (i, group) in report.groups.iter().enumerate() {
        for file in &group.files {
            writeln!(
                out,
                "{},{},{},{},{},{},{}",
                i + 1,
                group.sha256,
                group.size,
                file.keep,
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
            find_dupes(&self.catalog, &[]).unwrap()
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
        let report = find_dupes(&fx.catalog, &[old_disk]).unwrap();
        let shas: Vec<&str> = report.groups.iter().map(|g| g.sha256.as_str()).collect();
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
        let order: Vec<&str> = report.groups.iter().map(|g| g.sha256.as_str()).collect();
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
        let report = find_dupes(&fx.catalog, &[lib]).unwrap();
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
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "No exact duplicates found.\n"
        );
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
                    "sha256": "aaa",
                    "size": 100,
                    "files": [
                        {"library": "/lib", "path": "a.jpg", "mtime": "1970-01-01T00:00:00Z",
                         "keep": true},
                        {"library": "/lib", "path": "b.jpg", "mtime": "1970-01-01T00:00:01Z",
                         "keep": false},
                    ],
                }],
                "summary": {
                    "groups": 1,
                    "redundant_files": 1,
                    "reclaimable_bytes": 100,
                    "unhashed_candidates": 0,
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
            "group,sha256,size,keep,library,path,mtime\n\
             1,aaa,100,true,/lib,plain.jpg,1970-01-01T00:00:00Z\n\
             1,aaa,100,false,/lib,\"with, comma \"\"and quotes\"\".jpg\",1970-01-01T00:00:01Z\n"
        );
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
        let report = find_dupes(&catalog, &[]).unwrap();
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
