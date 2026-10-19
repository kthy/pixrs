//! Scan a media library into the catalog.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::catalog::{library_path, Catalog, FileRecord, MediaMeta, Stamp};
use crate::error::Error;
use crate::media::MediaType;
use indicatif::{ProgressBar, ProgressStyle};
use log::debug;
use nom_exif::{EntryValue, ExifDateTime, ExifTag, Metadata, TrackInfoTag};
use ring::digest::{Context, SHA256};
use std::collections::HashSet;
use std::fmt::Display;
use std::fs::{File, Metadata as FsMetadata};
use std::io::{self, Read};
use std::path::{absolute, Component, Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

/// Commit to the catalog after this many files, so an interrupted scan keeps its progress.
const COMMIT_EVERY: usize = 256;

/// What a scan did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanSummary {
    /// Files new to the catalog.
    pub added: usize,
    /// Cataloged files whose size or mtime changed, and were read again.
    pub updated: usize,
    /// Cataloged files that no longer exist.
    pub removed: usize,
    /// Cataloged files with the same size and mtime as last time.
    pub unchanged: usize,
    /// Files hashed because another cataloged file has the same size.
    pub hashed: usize,
    /// Files or folders that could not be read, or whose names are not valid UTF-8.
    pub skipped: usize,
}

/// A media file that is new or changed since the last scan.
struct Pending {
    rel: String,
    path: PathBuf,
    stamp: Stamp,
    media_type: MediaType,
    new: bool,
}

/// Scan the media files below `library` into the catalog at `catalog_path`.
///
/// The scan runs in two passes. The first records every file's size, mtime and metadata, reading
/// only files that are new or changed since the last scan, and removes files that disappeared.
/// The second hashes the files that could be exact duplicates: those whose size matches another
/// cataloged file, in any library. Files with a unique size are never read in full.
///
/// The library itself is only read.
pub fn scan(library: &Path, catalog_path: &Path, quiet: bool) -> Result<ScanSummary, Error> {
    let root = library
        .canonicalize()
        .ok()
        .filter(|root| root.is_dir())
        .ok_or_else(|| Error::Usage(format!("{} is not a directory", library.display())))?;
    let root_str = root
        .to_str()
        .ok_or_else(|| Error::Usage(format!("{} is not valid UTF-8", root.display())))?;
    let catalog_path = resolve(catalog_path)?;
    if catalog_path.starts_with(&root) {
        return Err(Error::Usage(format!(
            "The catalog {} must be outside the library {}",
            catalog_path.display(),
            root.display()
        )));
    }

    let catalog = Catalog::open(&catalog_path)?;
    let library_id = catalog.library_id(root_str)?;
    let known = catalog.stamps(library_id)?;

    let mut summary = ScanSummary::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut unreadable: Vec<String> = vec![];
    let mut pending: Vec<Pending> = vec![];

    for entry in WalkDir::new(&root) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                let path = e.path().unwrap_or(&root).to_path_buf();
                warn(&path, &e);
                if let Some(rel) = relative_path(&root, &path) {
                    unreadable.push(rel);
                }
                summary.skipped += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(media_type) = MediaType::from_path(entry.path()) else {
            continue;
        };
        let Some(rel) = relative_path(&root, entry.path()) else {
            warn(entry.path(), "file name is not valid UTF-8");
            summary.skipped += 1;
            continue;
        };
        let stamp = match entry
            .metadata()
            .map_err(io::Error::from)
            .and_then(|m| stamp(&m))
        {
            Ok(stamp) => stamp,
            Err(e) => {
                warn(entry.path(), &e);
                summary.skipped += 1;
                seen.insert(rel);
                continue;
            }
        };
        match known.get(&rel) {
            Some(known_stamp) if *known_stamp == stamp => summary.unchanged += 1,
            known_stamp => pending.push(Pending {
                rel: rel.clone(),
                path: entry.into_path(),
                stamp,
                media_type,
                new: known_stamp.is_none(),
            }),
        }
        seen.insert(rel);
    }

    if seen.is_empty() && !known.is_empty() {
        return Err(Error::Usage(format!(
            "Found no media files in {}, but the catalog lists {} for it. If it is on a drive, \
             is the drive mounted? The catalog was not changed.",
            root.display(),
            known.len()
        )));
    }

    // Pass 1: record new and changed files. A changed file loses its hash until pass 2.
    let pb = progress_bar(pending.len() as u64, "Reading", false, quiet);
    let mut tx = catalog.transaction()?;
    for (i, file) in pending.into_iter().enumerate() {
        if i > 0 && i % COMMIT_EVERY == 0 {
            tx.commit()?;
            tx = catalog.transaction()?;
        }
        pb.set_message(file.rel.clone());
        let meta = match read_meta(&file.path) {
            Ok(meta) => meta,
            Err(e) => {
                pb.suspend(|| warn(&file.path, &e));
                summary.skipped += 1;
                pb.inc(1);
                continue;
            }
        };
        let record = FileRecord {
            path: file.rel,
            stamp: file.stamp,
            media_type: file.media_type,
            sha256: None,
            meta,
        };
        catalog.upsert(library_id, &record)?;
        if file.new {
            summary.added += 1;
        } else {
            summary.updated += 1;
        }
        pb.inc(1);
    }
    pb.finish_and_clear();

    for rel in known.keys() {
        if !seen.contains(rel) && !unreadable.iter().any(|dir| is_within(rel, dir)) {
            catalog.remove(library_id, rel)?;
            summary.removed += 1;
        }
    }
    tx.commit()?;

    // Pass 2: hash files that share their size with another file and have no hash yet.
    let to_hash = catalog.unhashed_with_shared_size(library_id)?;
    let pb = progress_bar(
        to_hash.iter().map(|(_, size)| size).sum(),
        "Hashing",
        true,
        quiet,
    );
    let mut tx = catalog.transaction()?;
    for (i, (rel, _)) in to_hash.into_iter().enumerate() {
        if i > 0 && i % COMMIT_EVERY == 0 {
            tx.commit()?;
            tx = catalog.transaction()?;
        }
        pb.set_message(rel.clone());
        let path = library_path(&root, &rel);
        match hash_file(&path, &pb) {
            Ok(sha256) => {
                catalog.set_sha256(library_id, &rel, &sha256)?;
                summary.hashed += 1;
            }
            Err(e) => {
                pb.suspend(|| warn(&path, &e));
                summary.skipped += 1;
            }
        }
    }
    pb.finish_and_clear();
    catalog.mark_scanned(library_id)?;
    tx.commit()?;

    Ok(summary)
}

/// `path` made absolute, with its longest existing prefix canonicalized so that symlinks and `..`
/// cannot hide where it really is. The parts that do not exist yet are applied lexically.
pub(crate) fn resolve(path: &Path) -> io::Result<PathBuf> {
    let path = absolute(path)?;
    for ancestor in path.ancestors() {
        if let Ok(mut resolved) = ancestor.canonicalize() {
            let rest = path
                .strip_prefix(ancestor)
                .expect("an ancestor is a prefix");
            for part in rest.components() {
                match part {
                    Component::ParentDir => {
                        resolved.pop();
                    }
                    Component::Normal(name) => resolved.push(name),
                    _ => {}
                }
            }
            return Ok(resolved);
        }
    }
    Ok(path)
}

/// Print a warning about a file that is skipped.
fn warn(path: &Path, reason: impl Display) {
    eprintln!("Skipping {}: {}", path.display(), reason);
}

/// Whether the relative path `rel` is `dir` or inside it. The empty string is the library root.
fn is_within(rel: &str, dir: &str) -> bool {
    dir.is_empty()
        || rel == dir
        || rel
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `path` relative to `root` with `/` separators, or `None` if any part is not valid UTF-8.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts = rel
        .components()
        .map(|c| match c {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<&str>>>()?;
    Some(parts.join("/"))
}

/// Size and modification time of a file.
fn stamp(meta: &FsMetadata) -> io::Result<Stamp> {
    let mtime_ns = match meta.modified()?.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as i64,
        Err(before) => -(before.duration().as_nanos() as i64),
    };
    Ok(Stamp {
        size: meta.len(),
        mtime_ns,
    })
}

/// A progress bar of `len` files, or bytes if `bytes` is set.
fn progress_bar(len: u64, prefix: &'static str, bytes: bool, quiet: bool) -> ProgressBar {
    if quiet {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(len).with_prefix(prefix);
    let t = if bytes {
        "{prefix} [{elapsed}/{duration}] {wide_bar:.white/grey} {bytes:>10}/{total_bytes:10} \
         {msg:.dim}"
    } else {
        "{prefix} [{elapsed}/{duration}] {wide_bar:.white/grey} {pos:>6}/{len:6} {msg:.dim}"
    };
    pb.set_style(
        ProgressStyle::default_bar()
            .template(t)
            .expect("Expected a ProgressStyle!")
            .progress_chars("##-"),
    );
    pb
}

/// SHA-256 of the file at `path` as lowercase hex, advancing `pb` by the bytes read.
fn hash_file(path: &Path, pb: &ProgressBar) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut context = Context::new(&SHA256);
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        context.update(&buffer[..count]);
        pb.inc(count as u64);
    }
    Ok(data_encoding::HEXLOWER.encode(context.finish().as_ref()))
}

/// Read date taken, camera and dimensions from EXIF or video/audio track metadata.
///
/// Files without readable metadata get empty metadata. Only failing to read the file is an error.
fn read_meta(path: &Path) -> io::Result<MediaMeta> {
    let meta = match nom_exif::read_metadata(path) {
        Ok(Metadata::Exif(exif)) => MediaMeta {
            taken_at: exif
                .get(ExifTag::DateTimeOriginal)
                .or_else(|| exif.get(ExifTag::CreateDate))
                .and_then(datetime),
            camera_make: exif.get(ExifTag::Make).and_then(text),
            camera_model: exif.get(ExifTag::Model).and_then(text),
            width: exif
                .get(ExifTag::ExifImageWidth)
                .or_else(|| exif.get(ExifTag::ImageWidth))
                .and_then(dimension),
            height: exif
                .get(ExifTag::ExifImageHeight)
                .or_else(|| exif.get(ExifTag::ImageHeight))
                .and_then(dimension),
        },
        Ok(Metadata::Track(track)) => MediaMeta {
            taken_at: track.get(TrackInfoTag::CreateDate).and_then(datetime),
            camera_make: track.get(TrackInfoTag::Make).and_then(text),
            camera_model: track.get(TrackInfoTag::Model).and_then(text),
            width: track.get(TrackInfoTag::Width).and_then(dimension),
            height: track.get(TrackInfoTag::Height).and_then(dimension),
        },
        Err(nom_exif::Error::Io(e)) if e.kind() != io::ErrorKind::UnexpectedEof => return Err(e),
        Err(e) => {
            debug!("No metadata in {}: {}", path.display(), e);
            MediaMeta::default()
        }
    };
    Ok(meta)
}

/// A date and time as RFC 3339 if it has a timezone, else as `YYYY-MM-DDTHH:MM:SS`.
fn datetime(value: &EntryValue) -> Option<String> {
    value.as_datetime().map(|dt| match dt {
        ExifDateTime::Aware(dt) => dt.to_rfc3339(),
        ExifDateTime::Naive(dt) => dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
    })
}

/// A text value without the NUL and space padding cameras like to add.
fn text(value: &EntryValue) -> Option<String> {
    value
        .as_str()
        .map(|s| s.trim_matches(|c: char| c == '\0' || c.is_whitespace()))
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// A pixel dimension, whichever integer type the file stored it as.
fn dimension(value: &EntryValue) -> Option<u32> {
    value
        .as_u32()
        .or_else(|| value.as_u16().map(u32::from))
        .or_else(|| value.as_u64().and_then(|n| u32::try_from(n).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use std::fs::{create_dir_all, remove_file, write};
    use std::time::Duration;
    use tempfile::{tempdir, TempDir};

    /// A temp dir with an empty `lib` folder and a catalog path next to it.
    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            create_dir_all(dir.path().join("lib")).unwrap();
            Fixture { dir }
        }

        fn lib(&self) -> PathBuf {
            self.dir.path().join("lib")
        }

        fn catalog(&self) -> PathBuf {
            self.dir.path().join("catalog.sqlite")
        }

        fn write(&self, rel: &str, contents: &[u8]) {
            let path = self.lib().join(rel);
            create_dir_all(path.parent().unwrap()).unwrap();
            write(path, contents).unwrap();
        }

        fn scan(&self) -> ScanSummary {
            scan(&self.lib(), &self.catalog(), true).unwrap()
        }

        fn files(&self) -> Vec<FileRecord> {
            let catalog = Catalog::open(&self.catalog()).unwrap();
            let root = self.lib().canonicalize().unwrap();
            let id = catalog.library_id(root.to_str().unwrap()).unwrap();
            catalog.files(id).unwrap()
        }
    }

    /// A minimal JPEG whose only content is an EXIF segment with make, model and date taken.
    fn jpeg_with_exif(make: &str, model: &str, taken: &str) -> Vec<u8> {
        fn ascii(s: &str) -> Vec<u8> {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            bytes
        }
        fn entry(tiff: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32) {
            tiff.extend(tag.to_le_bytes());
            tiff.extend(kind.to_le_bytes());
            tiff.extend(count.to_le_bytes());
            tiff.extend(value.to_le_bytes());
        }
        const ASCII: u16 = 2;
        const LONG: u16 = 4;

        let (make, model, taken) = (ascii(make), ascii(model), ascii(taken));
        // Layout: header (8), IFD0 with 3 entries (42), Exif IFD with 1 entry (18), then data.
        let make_at = 8 + 42 + 18;
        let model_at = make_at + make.len();
        let taken_at = model_at + model.len();

        let mut tiff = b"II*\0".to_vec();
        tiff.extend(8u32.to_le_bytes());
        tiff.extend(3u16.to_le_bytes());
        entry(&mut tiff, 0x010f, ASCII, make.len() as u32, make_at as u32);
        entry(
            &mut tiff,
            0x0110,
            ASCII,
            model.len() as u32,
            model_at as u32,
        );
        entry(&mut tiff, 0x8769, LONG, 1, 8 + 42);
        tiff.extend(0u32.to_le_bytes());
        tiff.extend(1u16.to_le_bytes());
        entry(
            &mut tiff,
            0x9003,
            ASCII,
            taken.len() as u32,
            taken_at as u32,
        );
        tiff.extend(0u32.to_le_bytes());
        tiff.extend(make);
        tiff.extend(model);
        tiff.extend(taken);

        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
        jpeg.extend(((2 + 6 + tiff.len()) as u16).to_be_bytes());
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(tiff);
        // Start of scan, which ends the metadata segments, then some image data and end of
        // image. nom-exif needs JPEGs to be longer than the 128 bytes it sniffs the type from.
        jpeg.extend([0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00]);
        jpeg.extend([0; 256]);
        jpeg.extend([0xff, 0xd9]);
        jpeg
    }

    fn paths(files: &[FileRecord]) -> Vec<&str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn test_scan_catalogs_media_only() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"image a");
        fx.write("sub/b.MP4", b"video b");
        fx.write("sub/deeper/c.flac", b"audio c");
        fx.write("notes.txt", b"not media");

        assert_eq!(
            fx.scan(),
            ScanSummary {
                added: 3,
                hashed: 3,
                ..Default::default()
            }
        );
        let files = fx.files();
        assert_eq!(
            paths(&files),
            vec!["a.jpg", "sub/b.MP4", "sub/deeper/c.flac"]
        );
        assert_eq!(files[1].media_type, MediaType::Video);
        assert_eq!(files[0].stamp.size, 7);
        // SHA-256 of "image a"
        assert_eq!(
            files[0].sha256.as_deref(),
            Some("a854f481d4cb2479ff869f3a33763e4c3e8d6b5727f909d774fdd5f37690baeb")
        );
    }

    #[test]
    fn test_unique_sizes_are_not_hashed() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"a");
        fx.write("b.jpg", b"bb");

        assert_eq!(fx.scan().hashed, 0);
        assert!(fx.files().iter().all(|f| f.sha256.is_none()));
    }

    #[test]
    fn test_hashes_once_a_size_is_shared() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"12345");
        assert_eq!(fx.scan().hashed, 0);

        fx.write("b.jpg", b"abcde");
        assert_eq!(
            fx.scan(),
            ScanSummary {
                added: 1,
                unchanged: 1,
                hashed: 2,
                ..Default::default()
            }
        );
        assert!(fx.files().iter().all(|f| f.sha256.is_some()));
    }

    #[test]
    fn test_rescan_of_unchanged_library_reads_nothing() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"image a");
        fx.write("b.jpg", b"image b");
        assert_eq!(fx.scan().hashed, 2);

        assert_eq!(
            fx.scan(),
            ScanSummary {
                unchanged: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_rescan_picks_up_added_changed_and_removed_files() {
        let fx = Fixture::new();
        fx.write("keep.jpg", b"k");
        fx.write("change.jpg", b"cc");
        fx.write("gone.jpg", b"ggg");
        fx.scan();

        fx.write("change.jpg", b"changed, and longer");
        fx.write("new.jpg", b"nnnn");
        remove_file(fx.lib().join("gone.jpg")).unwrap();

        assert_eq!(
            fx.scan(),
            ScanSummary {
                added: 1,
                updated: 1,
                removed: 1,
                unchanged: 1,
                hashed: 0,
                skipped: 0,
            }
        );
        let after = fx.files();
        assert_eq!(paths(&after), vec!["change.jpg", "keep.jpg", "new.jpg"]);
        assert_eq!(after[0].stamp.size, 19);
    }

    #[test]
    fn test_changed_file_is_hashed_again() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"aaaa");
        fx.write("b.jpg", b"bbbb");
        fx.scan();
        let before = fx.files()[0].sha256.clone();

        // Same size, new contents and mtime.
        fx.write("a.jpg", b"cccc");
        let file = File::options()
            .write(true)
            .open(fx.lib().join("a.jpg"))
            .unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000_000))
            .unwrap();

        let summary = fx.scan();
        assert_eq!((summary.updated, summary.hashed), (1, 1));
        let after = fx.files()[0].sha256.clone();
        assert!(after.is_some());
        assert_ne!(after, before);
    }

    #[test]
    fn test_changed_file_without_shared_size_loses_its_hash() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"aaaa");
        fx.write("b.jpg", b"bbbb");
        fx.scan();

        fx.write("a.jpg", b"aaaaa");
        assert_eq!(fx.scan().hashed, 0);
        assert_eq!(fx.files()[0].sha256, None);
    }

    #[test]
    fn test_identical_files_get_identical_hashes() {
        let fx = Fixture::new();
        fx.write("a/photo.jpg", b"same bytes");
        fx.write("b/copy of photo.jpg", b"same bytes");
        fx.scan();
        let files = fx.files();
        assert!(files[0].sha256.is_some());
        assert_eq!(files[0].sha256, files[1].sha256);
    }

    #[test]
    fn test_shared_size_across_libraries() {
        let dir = tempdir().unwrap();
        let catalog = dir.path().join("catalog.sqlite");
        let (one, two) = (dir.path().join("one"), dir.path().join("two"));
        create_dir_all(&one).unwrap();
        create_dir_all(&two).unwrap();
        write(one.join("x.jpg"), b"same").unwrap();
        write(two.join("y.jpg"), b"size").unwrap();

        assert_eq!(scan(&one, &catalog, true).unwrap().hashed, 0);
        // Only files in the library being scanned are hashed; the other library may be offline.
        assert_eq!(scan(&two, &catalog, true).unwrap().hashed, 1);
        assert_eq!(scan(&one, &catalog, true).unwrap().hashed, 1);
    }

    #[test]
    fn test_reads_exif() {
        let fx = Fixture::new();
        fx.write(
            "photo.jpg",
            &jpeg_with_exif("Canon", "EOS 5D", "2019:07:14 12:34:56"),
        );
        fx.scan();
        let meta = &fx.files()[0].meta;
        assert_eq!(meta.taken_at.as_deref(), Some("2019-07-14T12:34:56"));
        assert_eq!(meta.camera_make.as_deref(), Some("Canon"));
        assert_eq!(meta.camera_model.as_deref(), Some("EOS 5D"));
    }

    #[test]
    fn test_unreadable_metadata_is_not_an_error() {
        let fx = Fixture::new();
        fx.write("broken.jpg", b"not really a jpeg");
        fx.scan();
        assert_eq!(fx.files()[0].meta, MediaMeta::default());
    }

    #[test]
    fn test_unicode_file_names() {
        let fx = Fixture::new();
        fx.write("Æblegrød/blå bær ワイフ.jpg", b"unicode");
        fx.scan();
        assert_eq!(paths(&fx.files()), vec!["Æblegrød/blå bær ワイフ.jpg"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_non_utf8_file_names_are_skipped() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let fx = Fixture::new();
        fx.write("ok.jpg", b"ok");
        write(
            fx.lib().join(OsStr::from_bytes(b"bl\xe5.jpg")),
            b"latin-1 name",
        )
        .unwrap();

        let summary = fx.scan();
        assert_eq!((summary.added, summary.skipped), (1, 1));
        assert_eq!(paths(&fx.files()), vec!["ok.jpg"]);
    }

    #[test]
    fn test_empty_walk_does_not_wipe_catalog() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"a");
        fx.scan();
        remove_file(fx.lib().join("a.jpg")).unwrap();

        let result = scan(&fx.lib(), &fx.catalog(), true);
        assert!(matches!(result, Err(Error::Usage(_))));
        assert_eq!(paths(&fx.files()), vec!["a.jpg"]);
    }

    #[test]
    fn test_catalog_inside_library_via_dotdot_is_refused() {
        let fx = Fixture::new();
        create_dir_all(fx.lib().join("sub")).unwrap();
        // The library is given as `lib/sub/..`, the catalog as `lib/catalog.sqlite`: lexically
        // the catalog is not below the library, but it really is.
        let result = scan(
            &fx.lib().join("sub").join(".."),
            &fx.lib().join("catalog.sqlite"),
            true,
        );
        assert!(matches!(result, Err(Error::Usage(_))));
        assert!(!fx.lib().join("catalog.sqlite").exists());
    }

    #[test]
    fn test_catalog_in_missing_dirs_inside_library_is_refused() {
        let fx = Fixture::new();
        let nested = fx
            .lib()
            .join("new")
            .join("..")
            .join("deeper")
            .join("catalog.sqlite");
        let result = scan(&fx.lib(), &nested, true);
        assert!(matches!(result, Err(Error::Usage(_))));
        assert!(!fx.lib().join("deeper").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_catalog_symlinked_into_library_is_refused() {
        let fx = Fixture::new();
        let link = fx.dir.path().join("link");
        std::os::unix::fs::symlink(fx.lib(), &link).unwrap();
        let result = scan(&fx.lib(), &link.join("catalog.sqlite"), true);
        assert!(matches!(result, Err(Error::Usage(_))));
        assert!(!fx.lib().join("catalog.sqlite").exists());
    }

    #[test]
    fn test_catalog_inside_library_is_refused() {
        let fx = Fixture::new();
        let result = scan(&fx.lib(), &fx.lib().join("catalog.sqlite"), true);
        assert!(matches!(result, Err(Error::Usage(_))));
        assert!(!fx.lib().join("catalog.sqlite").exists());
    }

    #[test]
    fn test_missing_library_is_refused() {
        let fx = Fixture::new();
        let result = scan(&fx.lib().join("nope"), &fx.catalog(), true);
        assert!(matches!(result, Err(Error::Usage(_))));
    }

    #[test]
    fn test_is_within() {
        assert!(is_within("a/b.jpg", "a"));
        assert!(is_within("a", "a"));
        assert!(is_within("anything", ""));
        assert!(!is_within("ab/c.jpg", "a"));
    }
}
