//! Scan a media library into the catalog.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::avhash::{
    audio_fingerprint, audio_version, video_signature, video_version, AudioFingerprint,
    VideoSignature,
};
use crate::catalog::{
    cataloged_spelling, display_root, library_path, Catalog, FileRecord, MediaMeta, Stamp,
};
use crate::dates::quicktime_creation;
use crate::decode::{ffmpeg_handles_heic, image_decoder, DecodeError, Decoder, Tools};
use crate::error::Error;
use crate::imagehash::{hash_image, hash_version, ImageHashes};
use crate::media::MediaType;
use crate::names::nfc;
use crate::text::{printable, shown};
use indicatif::{
    HumanBytes, HumanCount, HumanDuration, ProgressBar, ProgressState, ProgressStyle,
};
use log::debug;
use nom_exif::{EntryValue, ExifDateTime, ExifTag, Metadata, TrackInfoTag};
use ring::digest::{Context, SHA256};
use same_file::is_same_file;
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display};
use std::fs::{symlink_metadata, File, Metadata as FsMetadata};
use std::io::{self, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{absolute, Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{self, available_parallelism};
use std::time::{Duration, UNIX_EPOCH};
use time::macros::format_description;
use time::OffsetDateTime;
use walkdir::WalkDir;

/// Commit to the catalog after this many files, so an interrupted scan keeps its progress.
const COMMIT_EVERY: usize = 256;

/// Most images decoded at once. Each can take a few hundred MB while it is decoded.
const MAX_DECODERS: usize = 8;

/// Most files hashed at once. A few reads at once keep an SSD or a network share busy (each
/// read on a share waits for round trips); many would make a spinning disk seek between them.
const MAX_HASHERS: usize = 4;

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
    /// Cataloged files found under another Unicode normalization of their name (composed or
    /// decomposed accents), whose rows now have the name as it is on disk, with their hashes.
    /// Also counted as updated or unchanged.
    pub respelled: usize,
    /// Files hashed because another cataloged file has the same size.
    pub hashed: usize,
    /// Files or folders that could not be read, or whose names are not valid UTF-8.
    pub skipped: usize,
    /// Media files decoded for their hashes, signatures or fingerprints: new, changed, or made
    /// with other decoder versions.
    pub media_hashed: usize,
    /// Media files that failed to decode. They are not tried again until they or the decoders
    /// change.
    pub media_undecodable: usize,
    /// Media files with no decoder and no hashes: RAW formats, HEIC and AVIF without `ffmpeg`
    /// 7.1 or later, video without `ffmpeg` and `ffprobe`, audio without `fpcalc`. They are tried
    /// again on every scan.
    pub media_without_decoder: usize,
    /// The root of the library the scanned folder is part of, if it is not that library's root.
    /// Only that part of the library was scanned.
    pub part_of: Option<String>,
    /// Roots of the libraries inside the scanned one that were folded into it.
    pub folded: Vec<String>,
}

/// How a scan runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Hide the progress bars.
    pub quiet: bool,
    /// Accept a walk that finds no media files where the catalog lists some, and remove those
    /// rows. Without it such a scan is refused, since an unmounted drive looks the same.
    pub allow_empty: bool,
}

impl ScanOptions {
    /// No progress bars, and an empty walk is refused.
    pub const QUIET: Self = Self {
        quiet: true,
        allow_empty: false,
    };
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
/// The scan runs in three passes. The first records every file's size, mtime and metadata,
/// reading only files that are new or changed since the last scan, and removes files that
/// disappeared. The second hashes the files that could be exact duplicates: those whose size
/// matches another cataloged file, in any library. Files with a unique size are never read in
/// full by it. The third decodes media files, in parallel: images for their pixel and perceptual
/// hashes and fingerprints, videos for their frame signatures (with `ffmpeg`), audio for its
/// fingerprint (with `fpcalc`). Only files that are new or changed, or were hashed with other
/// decoder versions, are decoded; files a decoder rejected are not tried again until they or
/// the decoders change.
///
/// A folder inside a cataloged library is scanned as that part of the library. Libraries inside
/// the scanned one are folded into it first, keeping their hashes.
///
/// A walk that finds no media files where the catalog lists some is refused, unless
/// `options.allow_empty` is set: then those rows are removed, and the library keeps its entry
/// with no files. The scanned folder must exist and be readable either way.
///
/// Names are matched to cataloged paths as spelled, or else in Unicode NFC: a file whose name
/// is a cataloged one in another normalization (macOS may decompose accents where Linux and
/// Windows compose them) keeps its row and hashes, and the row takes the name on disk.
///
/// The library itself is only read.
pub fn scan(
    library: &Path,
    catalog_path: &Path,
    options: ScanOptions,
) -> Result<ScanSummary, Error> {
    scan_with(library, catalog_path, options, &Tools::find())
}

/// `scan`, with the external tools given instead of looked up on `PATH`.
#[allow(clippy::too_many_lines, reason = "to be split by phase")]
pub fn scan_with(
    library: &Path,
    catalog_path: &Path,
    options: ScanOptions,
    tools: &Tools,
) -> Result<ScanSummary, Error> {
    let quiet = options.quiet;
    let start = library
        .canonicalize()
        .ok()
        .filter(|start| start.is_dir())
        .ok_or_else(|| Error::Usage(format!("{} is not a directory", library.display())))?;
    let catalog_path = resolve(catalog_path)?;
    let outside = |root: &Path| {
        if catalog_path.starts_with(root) {
            return Err(Error::Usage(format!(
                "The catalog {} must be outside the library {}",
                catalog_path.display(),
                root.display()
            )));
        }
        Ok(())
    };
    outside(&start)?;

    let catalog = Catalog::open(&catalog_path)?;
    // One folder under another letter case is the same library.
    let start = cataloged_spelling(&catalog.libraries()?, &start);
    let start_str = start
        .to_str()
        .ok_or_else(|| Error::Usage(format!("{} is not valid UTF-8", start.display())))?;
    let mut summary = ScanSummary::default();
    // `start` may be part of a library; `root` is the library's root, and paths are relative to
    // it. `part` is the scanned folder relative to `root`, empty for the whole library.
    let (library_id, root) = match catalog.containing_library(&start)? {
        Some((id, root)) => (id, PathBuf::from(root)),
        None => (catalog.library_id(start_str)?, start.clone()),
    };
    outside(&root)?;
    let part = relative_path(&root, &start)
        .ok_or_else(|| Error::Usage(format!("{} is not valid UTF-8", start.display())))?;
    if !part.is_empty() {
        summary.part_of = Some(display_root(&root.to_string_lossy()));
    }
    summary.folded = catalog
        .fold_nested(library_id, &root)?
        .iter()
        .map(|r| display_root(r))
        .collect();
    let mut known = catalog.stamps(library_id)?;
    let respellable = respellable(&known, &part);
    known.retain(|rel, _| is_within(rel, &part));

    let mut seen: HashSet<String> = HashSet::new();
    // Rows found under another Unicode normalization, with the name on disk.
    let mut respelled: Vec<(String, String)> = vec![];
    let mut unreadable: Vec<String> = vec![];
    let mut pending: Vec<Pending> = vec![];

    // Walk: find the media files and pick out the new and changed ones. On a network share this
    // alone can take hours, so it shows the folder it is in.
    let walk = spinner("[1/4] Walking", quiet);
    for entry in WalkDir::new(&start) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                let path = e.path().unwrap_or(&start).to_path_buf();
                walk.suspend(|| warn(&path, &e));
                if let Some(rel) = relative_path(&root, &path) {
                    unreadable.push(rel);
                }
                summary.skipped += 1;
                continue;
            }
        };
        if entry.file_type().is_dir() {
            let dir = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or_else(|_| entry.path());
            walk.set_message(shown(dir).to_string());
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(media_type) = MediaType::from_path(entry.path()) else {
            continue;
        };
        walk.inc(1);
        let Some(rel) = relative_path(&root, entry.path()) else {
            walk.suspend(|| warn(entry.path(), "file name is not valid UTF-8"));
            summary.skipped += 1;
            continue;
        };
        let mut known_stamp = known.get(&rel).copied();
        if known_stamp.is_none() {
            let found = respelling(&rel, entry.path(), &root, &known, &respellable, &seen);
            if let Some((old, old_stamp)) = found {
                seen.insert(old.clone());
                respelled.push((old, rel.clone()));
                known_stamp = Some(old_stamp);
            }
        }
        let stamp = match entry
            .metadata()
            .map_err(io::Error::from)
            .and_then(|m| stamp(&m))
        {
            Ok(stamp) => stamp,
            Err(e) => {
                walk.suspend(|| warn(entry.path(), &e));
                summary.skipped += 1;
                seen.insert(rel);
                continue;
            }
        };
        match known_stamp {
            Some(known_stamp) if known_stamp == stamp => summary.unchanged += 1,
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
    walk.finish_and_clear();
    // Not `walk.println`, which pads the line to the terminal width instead of ending it.
    if !walk.is_hidden() {
        eprintln!(
            "Found {} media files, {} new or changed ({})",
            HumanCount(walk.position()),
            HumanCount(pending.len() as u64),
            HumanDuration(walk.elapsed())
        );
    }

    // An unmounted drive or share often leaves an empty folder behind, so an empty walk only
    // removes rows when the user says the folder really is empty, and never when it could not
    // be listed.
    if seen.is_empty() && !known.is_empty() {
        if unreadable.contains(&part) {
            return Err(Error::Usage(format!(
                "Cannot list the files in {}, which the catalog lists {} files for. Nothing was \
                 removed from the catalog.",
                start.display(),
                known.len()
            )));
        }
        if !options.allow_empty {
            return Err(Error::Usage(format!(
                "Found no media files in {}, but the catalog lists {} for it. If it is on a \
                 drive, is the drive mounted? Nothing was removed from the catalog. If the \
                 folder really is empty now, scan it with --allow-empty to remove them.",
                start.display(),
                known.len()
            )));
        }
    }

    // Pass 1: record new and changed files. A changed file loses its hash until pass 2.
    let pb = progress_bar(pending.len() as u64, "[2/4] Reading", false, quiet);
    let mut tx = catalog.transaction()?;
    for (old, new) in &respelled {
        catalog.respell(library_id, old, new)?;
    }
    summary.respelled = respelled.len();
    for (i, file) in pending.into_iter().enumerate() {
        if i > 0 && i % COMMIT_EVERY == 0 {
            tx.commit()?;
            tx = catalog.transaction()?;
        }
        pb.set_message(printable(&file.rel).into_owned());
        // A parser bug on one broken file should not end the scan, or keep the file out of the
        // catalog: it is cataloged without metadata, like a file whose metadata cannot be parsed.
        let read = catch_unwind(AssertUnwindSafe(|| read_meta(&file.path)));
        let meta = match read {
            Ok(Ok(meta)) => meta,
            Ok(Err(e)) => {
                pb.suspend(|| warn(&file.path, &e));
                if !root.is_dir() {
                    pb.finish_and_clear();
                    tx.commit()?;
                    return Err(disappeared(&root));
                }
                summary.skipped += 1;
                pb.inc(1);
                continue;
            }
            Err(_) => {
                pb.suspend(|| {
                    eprintln!(
                        "Cataloging {} without metadata: the metadata parser crashed on it",
                        shown(&file.path)
                    );
                });
                MediaMeta::default()
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
    finish_step(&pb, &format!("Read {} files", HumanCount(pb.position())));

    for rel in known.keys() {
        if !seen.contains(rel) && !unreadable.iter().any(|dir| is_within(rel, dir)) {
            catalog.remove(library_id, rel)?;
            summary.removed += 1;
        }
    }
    tx.commit()?;

    // Pass 2: hash files that share their size with another file and have no hash yet.
    let mut to_hash = catalog.unhashed_with_shared_size(library_id)?;
    to_hash.retain(|(rel, _)| is_within(rel, &part));
    let pb = progress_bar(
        to_hash.iter().map(|(_, size)| size).sum(),
        "[3/4] Hashing",
        true,
        quiet,
    );
    let mut tx = Some(catalog.transaction()?);
    let mut done = 0;
    let hash = |(rel, _): &(String, u64)| {
        pb.set_message(printable(rel).into_owned());
        hash_file(&library_path(&root, rel), &pb)
    };
    in_parallel(&to_hash, MAX_HASHERS, hash, |i, hashed| {
        let rel = &to_hash[i].0;
        if done > 0 && done % COMMIT_EVERY == 0 {
            tx.take().expect("a transaction is open").commit()?;
            tx = Some(catalog.transaction()?);
        }
        done += 1;
        match hashed {
            Ok(sha256) => {
                catalog.set_sha256(library_id, rel, &sha256)?;
                summary.hashed += 1;
            }
            Err(e) => {
                pb.suspend(|| warn(&library_path(&root, rel), &e));
                if !root.is_dir() {
                    pb.finish_and_clear();
                    tx.take().expect("a transaction is open").commit()?;
                    return Err(disappeared(&root));
                }
                summary.skipped += 1;
            }
        }
        Ok(())
    })?;
    finish_step(
        &pb,
        &format!(
            "Hashed {} files, {}",
            HumanCount(done as u64),
            HumanBytes(pb.position())
        ),
    );
    tx.expect("a transaction is open").commit()?;

    // Pass 3: image hashes, video signatures and audio fingerprints of files that have none
    // yet, or were made with other decoder versions.
    let ffmpeg_version = tools.ffmpeg.as_ref().map(|(_, v)| v.as_str());
    let heic_ok = ffmpeg_version.is_some_and(ffmpeg_handles_heic);
    let mut warned_heic = false;
    let mut to_decode: Vec<(String, MediaType, String)> = vec![];
    for (rel, media_type, stored) in catalog.hash_versions(library_id)? {
        if !is_within(&rel, &part) {
            continue;
        }
        let current = match media_type {
            MediaType::Image => match (image_decoder(&rel), ffmpeg_version) {
                (Some(Decoder::InProcess), _) => hash_version(&rel, ""),
                (Some(Decoder::Ffmpeg), Some(version)) if heic_ok => hash_version(&rel, version),
                (Some(Decoder::Ffmpeg), Some(version)) => {
                    // An old `ffmpeg` may decode only part of a HEIC photo; better no hashes
                    // than wrong ones.
                    if !warned_heic {
                        eprintln!(
                            "ffmpeg {version} is older than 7.1 and may decode HEIC photos only in \
                             part, so HEIC and AVIF images are not hashed."
                        );
                        warned_heic = true;
                    }
                    None
                }
                _ => None,
            },
            MediaType::Video => match (ffmpeg_version, &tools.ffprobe) {
                (Some(version), Some(_)) => Some(video_version(version)),
                _ => None,
            },
            MediaType::Audio => tools.fpcalc.as_ref().map(|(_, v)| audio_version(v)),
        };
        match current {
            Some(current) if stored.as_ref() != Some(&current) => {
                to_decode.push((rel, media_type, current));
            }
            // Hashes made earlier, e.g. with an `ffmpeg` that is gone now, are kept.
            None if stored.is_none() => summary.media_without_decoder += 1,
            Some(_) | None => {}
        }
    }
    let pb = progress_bar(to_decode.len() as u64, "[4/4] Decoding", false, quiet);
    let mut tx = Some(catalog.transaction()?);
    let mut done = 0;
    let files: Vec<(&str, MediaType)> = to_decode
        .iter()
        .map(|(rel, media_type, _)| (rel.as_str(), *media_type))
        .collect();
    hash_files(&root, &files, tools, |i, result| {
        let ((rel, media_type), version) = (files[i], to_decode[i].2.as_str());
        if done > 0 && done % COMMIT_EVERY == 0 {
            tx.take().expect("a transaction is open").commit()?;
            tx = Some(catalog.transaction()?);
        }
        done += 1;
        pb.set_message(printable(rel).into_owned());
        pb.inc(1);
        match result {
            Ok(Some(hashed)) => {
                record_hashes(
                    &catalog,
                    library_id,
                    rel,
                    version,
                    Some(&hashed),
                    media_type,
                )?;
                summary.media_hashed += 1;
            }
            Ok(None) => summary.media_without_decoder += 1,
            Err(e) => {
                let path = library_path(&root, rel);
                // The reason can quote the file, or come from `ffmpeg`.
                pb.suspend(|| {
                    eprintln!(
                        "Cannot decode {}: {}",
                        shown(&path),
                        printable(&e.to_string())
                    );
                });
                if !root.is_dir() {
                    tx.take().expect("a transaction is open").commit()?;
                    return Err(disappeared(&root));
                }
                // A file that is gone or unreadable now is tried again on the next scan; only
                // one the decoder rejects is recorded as undecodable.
                if e.is_transient() || !path.is_file() {
                    summary.skipped += 1;
                } else {
                    record_hashes(&catalog, library_id, rel, version, None, media_type)?;
                    summary.media_undecodable += 1;
                }
            }
        }
        Ok(())
    })?;
    finish_step(&pb, &format!("Decoded {} files", HumanCount(pb.position())));
    if part.is_empty() {
        catalog.mark_scanned(library_id)?;
    }
    tx.expect("a transaction is open").commit()?;

    Ok(summary)
}

/// What the hash pass made of a file.
enum Hashed {
    Image(ImageHashes),
    Video(VideoSignature),
    Audio(AudioFingerprint),
}

/// Store a file's hashes, or that decoding it failed (`None`), with `version`.
fn record_hashes(
    catalog: &Catalog,
    library_id: i64,
    rel: &str,
    version: &str,
    hashed: Option<&Hashed>,
    media_type: MediaType,
) -> rusqlite::Result<()> {
    match (hashed, media_type) {
        (Some(Hashed::Image(h)), _) => catalog.set_image_hashes(library_id, rel, version, Some(h)),
        (Some(Hashed::Video(s)), _) => {
            catalog.set_video_signature(library_id, rel, version, Some(s))
        }
        (Some(Hashed::Audio(f)), _) => {
            catalog.set_audio_fingerprint(library_id, rel, version, Some(f))
        }
        (None, MediaType::Image) => catalog.set_image_hashes(library_id, rel, version, None),
        (None, MediaType::Video) => catalog.set_video_signature(library_id, rel, version, None),
        (None, MediaType::Audio) => catalog.set_audio_fingerprint(library_id, rel, version, None),
    }
}

/// Hash one file by its media type. `Ok(None)` if there is no tool for it.
fn hash_media(
    path: &Path,
    rel: &str,
    media_type: MediaType,
    tools: &Tools,
) -> Result<Option<Hashed>, DecodeError> {
    let ffmpeg = tools.ffmpeg.as_ref().map(|(f, _)| f.as_path());
    match media_type {
        MediaType::Image => Ok(hash_image(path, rel, ffmpeg)?.map(Hashed::Image)),
        MediaType::Video => match (ffmpeg, &tools.ffprobe) {
            (Some(ffmpeg), Some(ffprobe)) => {
                Ok(Some(Hashed::Video(video_signature(ffmpeg, ffprobe, path)?)))
            }
            _ => Ok(None),
        },
        MediaType::Audio => match &tools.fpcalc {
            Some((fpcalc, _)) => Ok(Some(Hashed::Audio(audio_fingerprint(fpcalc, path)?))),
            None => Ok(None),
        },
    }
}

/// Hash the files at the relative paths in `files` below `root` on several threads, and pass
/// each result with its index in `files` to `record` on this thread, in the order they finish.
/// Stops at the first error from `record`.
fn hash_files(
    root: &Path,
    files: &[(&str, MediaType)],
    tools: &Tools,
    record: impl FnMut(usize, Result<Option<Hashed>, DecodeError>) -> Result<(), Error>,
) -> Result<(), Error> {
    let work = |&(rel, media_type): &(&str, MediaType)| {
        let path = library_path(root, rel);
        // A decoder bug on one broken file should not end the scan.
        catch_unwind(AssertUnwindSafe(|| {
            hash_media(&path, rel, media_type, tools)
        }))
        .unwrap_or_else(|_| Err(DecodeError::Invalid(String::from("the decoder crashed"))))
    };
    in_parallel(files, MAX_DECODERS, work, record)
}

/// `work` on each of `items` on up to `max_workers` threads (no more than there are cores),
/// passing each result with its index in `items` to `record` on this thread, in the order they
/// finish. `record` is where the catalog is written: it has one connection, and one writer.
/// Stops at the first error from `record`.
fn in_parallel<T: Sync, R: Send>(
    items: &[T],
    max_workers: usize,
    work: impl Fn(&T) -> R + Sync,
    mut record: impl FnMut(usize, R) -> Result<(), Error>,
) -> Result<(), Error> {
    let workers = available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(max_workers)
        .min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::sync_channel(workers * 2);
    thread::scope(|scope| {
        for _ in 0..workers {
            let (tx, next, work) = (tx.clone(), &next, &work);
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else {
                    break;
                };
                if tx.send((i, work(item))).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        // Leaving early drops `rx`, which makes the workers stop at their next send.
        for (i, result) in rx {
            record(i, result)?;
        }
        Ok(())
    })
}

/// `path` made absolute, with its longest existing prefix canonicalized so that symlinks and `..`
/// cannot hide where it really is. The parts that do not exist yet are applied lexically.
pub fn resolve(path: &Path) -> io::Result<PathBuf> {
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

/// The error a scan stops with when the library is gone (an unplugged drive, a dropped network
/// share), instead of failing on every remaining file. What it did so far is committed first.
fn disappeared(root: &Path) -> Error {
    Error::Usage(format!(
        "{} disappeared during the scan. What was scanned so far is kept; scan again when it \
         is back.",
        root.display()
    ))
}

/// Print a warning about a file that is skipped.
fn warn(path: &Path, reason: impl Display) {
    eprintln!(
        "Skipping {}: {}",
        shown(path),
        printable(&reason.to_string())
    );
}

/// Whether the relative path `rel` is `dir` or inside it. The empty string is the library root.
fn is_within(rel: &str, dir: &str) -> bool {
    dir.is_empty()
        || rel == dir
        || rel
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The rows of `known` that a file in `part` can be under another Unicode normalization of its
/// name and that a lookup of the file's name in NFC in `known` does not find, by their path in
/// NFC: rows not spelled in NFC, and rows in `part` only in NFC. With every row in NFC and a
/// whole library scanned, as usual, there are none.
fn respellable(known: &HashMap<String, Stamp>, part: &str) -> HashMap<String, (String, Stamp)> {
    let part_nfc = nfc(part);
    let mut rows = HashMap::new();
    for (rel, stamp) in known {
        let rel_nfc = nfc(rel);
        let looked_up = is_within(rel, part) && rel_nfc == rel.as_str();
        if !looked_up && is_within(&rel_nfc, &part_nfc) {
            rows.insert(rel_nfc.into_owned(), (rel.clone(), *stamp));
        }
    }
    rows
}

/// The cataloged row, with its stamp, that is the file at `path` (`rel` in the library at
/// `root`) under another Unicode normalization of its name: a row in `known` whose path is
/// `rel` in NFC, or one in `respellable`. Not if `seen` has it, as the file of another name, or
/// if its path leads to another file (on Linux, which keeps names as given, both spellings can
/// be files of their own).
fn respelling(
    rel: &str,
    path: &Path,
    root: &Path,
    known: &HashMap<String, Stamp>,
    respellable: &HashMap<String, (String, Stamp)>,
    seen: &HashSet<String>,
) -> Option<(String, Stamp)> {
    let rel_nfc = nfc(rel);
    let (old, stamp) = match respellable.get(rel_nfc.as_ref()) {
        Some((old, stamp)) => (old.clone(), *stamp),
        None if rel_nfc != rel => {
            let stamp = *known.get(rel_nfc.as_ref())?;
            (rel_nfc.into_owned(), stamp)
        }
        None => return None,
    };
    if seen.contains(&old) {
        return None;
    }
    let old_path = library_path(root, &old);
    let elsewhere =
        symlink_metadata(&old_path).is_ok() && !is_same_file(&old_path, path).unwrap_or(false);
    (!elsewhere).then_some((old, stamp))
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
#[allow(
    clippy::cast_possible_truncation,
    reason = "nanoseconds since 1970 fit in i64 until 2262"
)]
pub(crate) fn stamp(meta: &FsMetadata) -> io::Result<Stamp> {
    let mtime_ns = match meta.modified()?.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as i64,
        Err(before) => -(before.duration().as_nanos() as i64),
    };
    Ok(Stamp {
        size: meta.len(),
        mtime_ns,
    })
}

/// A progress bar of `len` files, or bytes if `bytes` is set. The message is cut to fit the
/// terminal: a line that wraps is not cleared when the bar is drawn again.
pub(crate) fn progress_bar(
    len: u64,
    prefix: &'static str,
    bytes: bool,
    quiet: bool,
) -> ProgressBar {
    if quiet {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(len).with_prefix(prefix);
    let t = if bytes {
        "{prefix} [{elapsed}, ETA {eta}] {bar:15.white/grey} {bytes:>10}/{total_bytes} \
         {binary_bytes_per_sec:>11} {wide_msg:.dim}"
    } else {
        "{prefix} [{elapsed}, ETA {eta}] {bar:15.white/grey} {human_pos:>7}/{human_len:7} \
         {rate:>8} {wide_msg:.dim}"
    };
    pb.set_style(
        ProgressStyle::default_bar()
            .template(t)
            .expect("Expected a ProgressStyle!")
            .with_key("rate", |state: &ProgressState, w: &mut dyn fmt::Write| {
                let _ = write!(w, "{:.1}/s", state.per_sec());
            })
            .progress_chars("##-"),
    );
    crate::logging::log_around(&pb);
    pb
}

/// Clear a finished step's progress bar and print what the step did and how long it took, as
/// the walk does, unless the bars are hidden.
fn finish_step(pb: &ProgressBar, done: &str) {
    pb.finish_and_clear();
    // Not `pb.println`, which pads the line to the terminal width instead of ending it.
    if !pb.is_hidden() {
        eprintln!("{} ({})", done, HumanDuration(pb.elapsed()));
    }
}

/// A spinner counting media files, with the current folder as its message.
fn spinner(prefix: &'static str, quiet: bool) -> ProgressBar {
    if quiet {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new_spinner().with_prefix(prefix);
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{prefix} [{elapsed}] {spinner} {human_pos} media files {wide_msg:.dim}")
            .expect("Expected a ProgressStyle!"),
    );
    // Tick on a timer, so the spinner moves while a slow folder listing blocks the walk.
    pb.enable_steady_tick(Duration::from_millis(100));
    crate::logging::log_around(&pb);
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
    #[cfg(test)]
    tests::crash_if_asked(path);
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
            taken_at: track.get(TrackInfoTag::CreateDate).and_then(track_date),
            camera_make: track.get(TrackInfoTag::Make).and_then(text),
            camera_model: track.get(TrackInfoTag::Model).and_then(text),
            width: track.get(TrackInfoTag::Width).and_then(dimension),
            height: track.get(TrackInfoTag::Height).and_then(dimension),
        },
        Err(nom_exif::Error::Io(e)) if e.kind() != io::ErrorKind::UnexpectedEof => return Err(e),
        Err(e) => {
            debug!(
                "No metadata in {}: {}",
                shown(path),
                printable(&e.to_string())
            );
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

/// A track's creation date like `datetime`, with a QuickTime header's time read as ffmpeg reads
/// it (`quicktime_creation`). Only a date before 1970 in UTC is taken for such a time: `nom-exif`
/// gives header times in UTC, Matroska dates count from 2001, and phones write QuickTime's
/// `creationdate` key with their local offset.
fn track_date(value: &EntryValue) -> Option<String> {
    match value.as_datetime() {
        Some(ExifDateTime::Aware(dt))
            if dt.offset().local_minus_utc() == 0 && dt.timestamp() < 0 =>
        {
            let unix = quicktime_creation(dt.timestamp())?;
            let format =
                format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]+00:00");
            OffsetDateTime::from_unix_timestamp(unix)
                .ok()?
                .format(&format)
                .ok()
        }
        _ => datetime(value),
    }
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

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test fixtures with small values"
)]
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
            Self { dir }
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
            scan_with(
                &self.lib(),
                &self.catalog(),
                ScanOptions::QUIET,
                &Tools::default(),
            )
            .unwrap()
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

    /// A minimal MP4: a file type box and a movie box with only a movie header, created at
    /// `created` (seconds since 1904, or since 1970 as some encoders write it).
    fn mp4_created_at(created: u32) -> Vec<u8> {
        fn mp4_box(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
            let mut b = ((8 + body.len()) as u32).to_be_bytes().to_vec();
            b.extend(kind);
            b.extend(body);
            b
        }
        let mut mvhd = vec![0; 4]; // Version 0, no flags.
        mvhd.extend(created.to_be_bytes());
        mvhd.extend(created.to_be_bytes()); // Modified.
        mvhd.extend(1000u32.to_be_bytes()); // Time scale.
        mvhd.extend(1000u32.to_be_bytes()); // Duration.
        mvhd.extend([0; 76]); // Rate, volume, matrix and so on.
        mvhd.extend(2u32.to_be_bytes()); // Next track ID.
        let mut mp4 = mp4_box(*b"ftyp", b"isom\0\0\x02\0isomiso2mp41");
        mp4.extend(mp4_box(*b"moov", &mp4_box(*b"mvhd", &mvhd)));
        // nom-exif sniffs the type from the first 128 bytes.
        mp4.extend(mp4_box(*b"free", &[0; 128]));
        mp4
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
                media_undecodable: 1,
                // The video and the audio file, without ffmpeg and fpcalc.
                media_without_decoder: 2,
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
    fn test_images_are_decoded_once() {
        use crate::imagehash::tests::{jpeg, picture, with_exif};
        let fx = Fixture::new();
        let photo = jpeg(&picture(3, 300, 200), 90);
        fx.write("photo.jpg", &photo);
        fx.write("copy with exif.jpg", &with_exif(&photo));
        fx.write("raw.nef", b"not decodable here");
        fx.write("clip.mp4", b"not an image");

        let first = fx.scan();
        // The RAW file, and the video without ffmpeg.
        assert_eq!((first.media_hashed, first.media_without_decoder), (2, 2));
        let lib = fx.lib().canonicalize().unwrap();
        let catalog = Catalog::open(&fx.catalog()).unwrap();
        let id = catalog.library_id(lib.to_str().unwrap()).unwrap();
        let images = catalog.hashed_images(true).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].pixel_hash, images[1].pixel_hash);
        assert_ne!(images[0].file.size, images[1].file.size);
        drop(catalog);

        // Unchanged images are not decoded again; the RAW file is tried again.
        let second = fx.scan();
        assert_eq!((second.media_hashed, second.media_without_decoder), (0, 2));
        let catalog = Catalog::open(&fx.catalog()).unwrap();
        let unhashed: Vec<String> = catalog
            .hash_versions(id)
            .unwrap()
            .into_iter()
            .filter(|(_, _, version)| version.is_none())
            .map(|(path, _, _)| path)
            .collect();
        // Without tools, the video has no decoder either.
        assert_eq!(unhashed, ["clip.mp4", "raw.nef"]);

        // An image hashed with other decoder versions is decoded again.
        catalog
            .set_image_hashes(id, "photo.jpg", "an older version", None)
            .unwrap();
        drop(catalog);
        assert_eq!(fx.scan().media_hashed, 1);
        let catalog = Catalog::open(&fx.catalog()).unwrap();
        assert_eq!(catalog.hashed_images(true).unwrap().len(), 2);
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
                media_undecodable: 1,
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
                // The test files are not real JPEGs. Only the new and changed ones are tried.
                media_hashed: 0,
                media_undecodable: 2,
                media_without_decoder: 0,
                ..Default::default()
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
    fn test_many_files_are_hashed_in_parallel_each_with_its_own_hash() {
        let fx = Fixture::new();
        // All the same size, all different, so all are hashed; more than one commit's worth.
        let n = COMMIT_EVERY + 44;
        for i in 0..n {
            fx.write(
                &format!("d{}/{:05}.jpg", i % 7, i),
                format!("{i:05}").as_bytes(),
            );
        }
        let summary = fx.scan();
        assert_eq!((summary.hashed, summary.skipped), (n, 0));
        for file in fx.files() {
            let name = file.path.rsplit('/').next().unwrap();
            let expected = ring::digest::digest(&SHA256, name.trim_end_matches(".jpg").as_bytes());
            assert_eq!(
                file.sha256.as_deref(),
                Some(data_encoding::HEXLOWER.encode(expected.as_ref()).as_str()),
                "{}",
                file.path
            );
        }
    }

    #[test]
    fn test_in_parallel_does_each_item_once_and_stops_at_an_error() {
        let items: Vec<usize> = (0..1000).collect();
        let mut seen = vec![0; items.len()];
        in_parallel(
            &items,
            4,
            |&i| i * 2,
            |i, doubled| {
                assert_eq!(doubled, i * 2);
                seen[i] += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(seen.iter().all(|&n| n == 1));

        let mut recorded = 0;
        let stopped = in_parallel(
            &items,
            4,
            |&i| i,
            |_, _| {
                recorded += 1;
                if recorded == 10 {
                    return Err(Error::Usage(String::from("stop")));
                }
                Ok(())
            },
        );
        assert!(matches!(stopped, Err(Error::Usage(_))));
        assert_eq!(recorded, 10);
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

        assert_eq!(scan(&one, &catalog, ScanOptions::QUIET).unwrap().hashed, 0);
        // Only files in the library being scanned are hashed; the other library may be offline.
        assert_eq!(scan(&two, &catalog, ScanOptions::QUIET).unwrap().hashed, 1);
        assert_eq!(scan(&one, &catalog, ScanOptions::QUIET).unwrap().hashed, 1);
    }

    #[test]
    fn test_scan_of_folder_inside_library_scans_that_part() {
        let fx = Fixture::new();
        fx.write("a/one.jpg", b"1");
        fx.write("b/two.jpg", b"22");
        fx.scan();
        fx.write("a/one.jpg", b"changed");
        fx.write("a/new.jpg", b"new");
        remove_file(fx.lib().join("b/two.jpg")).unwrap();

        let part = scan_with(
            &fx.lib().join("a"),
            &fx.catalog(),
            ScanOptions::QUIET,
            &Tools::default(),
        );
        let summary = part.unwrap();
        assert_eq!((summary.added, summary.updated, summary.removed), (1, 1, 0));
        let root = fx.lib().canonicalize().unwrap();
        assert_eq!(summary.part_of, Some(display_root(root.to_str().unwrap())));
        // No new library, and the rest of the library is left as it was.
        let catalog = Catalog::open(&fx.catalog()).unwrap();
        assert_eq!(catalog.libraries().unwrap().len(), 1);
        assert_eq!(
            paths(&fx.files()),
            vec!["a/new.jpg", "a/one.jpg", "b/two.jpg"]
        );

        assert_eq!(fx.scan().removed, 1);
    }

    #[test]
    fn test_scan_of_parent_folds_inner_library() {
        let fx = Fixture::new();
        fx.write("sub/a.jpg", b"same");
        fx.write("sub/b.jpg", b"size");
        fx.write("c.jpg", b"c");
        let sub = scan_with(
            &fx.lib().join("sub"),
            &fx.catalog(),
            ScanOptions::QUIET,
            &Tools::default(),
        );
        assert_eq!(sub.unwrap().hashed, 2);

        let summary = fx.scan();
        let inner = fx.lib().join("sub").canonicalize().unwrap();
        assert_eq!(summary.folded, vec![display_root(inner.to_str().unwrap())]);
        // The inner library's files are not read or hashed again.
        assert_eq!(
            (summary.added, summary.unchanged, summary.hashed),
            (1, 2, 0)
        );
        let catalog = Catalog::open(&fx.catalog()).unwrap();
        assert_eq!(catalog.libraries().unwrap().len(), 1);
        let files = fx.files();
        assert_eq!(paths(&files), vec!["c.jpg", "sub/a.jpg", "sub/b.jpg"]);
        assert!(files[1].sha256.is_some() && files[2].sha256.is_some());
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
    fn test_reads_quicktime_creation_time_as_ffmpeg_does() {
        use crate::dates::QUICKTIME_EPOCH_OFFSET;
        let unix = time::macros::datetime!(2014-05-12 18:18:27 UTC).unix_timestamp();
        let fx = Fixture::new();
        fx.write("unix.mp4", &mp4_created_at(unix as u32));
        fx.write(
            "quicktime.mp4",
            &mp4_created_at((unix + QUICKTIME_EPOCH_OFFSET) as u32),
        );
        fx.write("unset.mp4", &mp4_created_at(0));
        fx.scan();
        let taken: Vec<_> = fx
            .files()
            .into_iter()
            .map(|f| (f.path, f.meta.taken_at))
            .collect();
        let date = Some(String::from("2014-05-12T18:18:27+00:00"));
        assert_eq!(
            taken,
            vec![
                (String::from("quicktime.mp4"), date.clone()),
                (String::from("unix.mp4"), date),
                (String::from("unset.mp4"), None),
            ]
        );
    }

    #[test]
    fn test_unreadable_metadata_is_not_an_error() {
        let fx = Fixture::new();
        fx.write("broken.jpg", b"not really a jpeg");
        fx.scan();
        assert_eq!(fx.files()[0].meta, MediaMeta::default());
    }

    /// The name of a file `read_meta` panics on, as a metadata parser with a bug can on a
    /// broken file.
    const CRASHES_THE_PARSER: &str = "crashes the parser.jpg";

    pub(super) fn crash_if_asked(path: &Path) {
        if path
            .file_name()
            .is_some_and(|name| name == CRASHES_THE_PARSER)
        {
            panic!("a metadata parser bug");
        }
    }

    #[test]
    fn test_a_crashing_metadata_parser_does_not_end_the_scan() {
        let fx = Fixture::new();
        let jpeg = jpeg_with_exif("Canon", "EOS 5D", "2019:07:14 12:34:56");
        fx.write(CRASHES_THE_PARSER, &jpeg);
        fx.write("fine.jpg", &jpeg);
        let summary = fx.scan();
        assert_eq!((summary.added, summary.skipped), (2, 0));
        let files = fx.files();
        let meta = |name: &str| &files.iter().find(|f| f.path == name).unwrap().meta;
        assert_eq!(*meta(CRASHES_THE_PARSER), MediaMeta::default());
        assert_eq!(meta("fine.jpg").camera_make.as_deref(), Some("Canon"));
        // Cataloged, so a rescan does not read it again.
        assert_eq!(fx.scan().unchanged, 2);
    }

    #[test]
    fn test_unicode_file_names() {
        let fx = Fixture::new();
        fx.write("Æblegrød/blå bær ワイフ.jpg", b"unicode");
        fx.scan();
        assert_eq!(paths(&fx.files()), vec!["Æblegrød/blå bær ワイフ.jpg"]);
    }

    /// `name` with the accent of `é` composed (NFC) and decomposed (NFD).
    fn spellings(name: &str) -> (String, String) {
        use crate::names::tests::{NFC, NFD};
        (name.replace("{}", NFC), name.replace("{}", NFD))
    }

    impl Fixture {
        /// Give the row of the file `on_disk` the path `cataloged`, as if another system had
        /// spelled its name so.
        fn respell_row(&self, on_disk: &str, cataloged: &str) {
            let catalog = Catalog::open(&self.catalog()).unwrap();
            let root = self.lib().canonicalize().unwrap();
            let id = catalog.library_id(root.to_str().unwrap()).unwrap();
            catalog.respell(id, on_disk, cataloged).unwrap();
        }
    }

    #[test]
    fn test_file_under_another_normalization_keeps_its_row() {
        let fx = Fixture::new();
        let (nfc, nfd) = spellings("{}/{}.jpg");
        let (other_nfc, other_nfd) = spellings("{}/other {}.jpg");
        fx.write(&nfd, b"same size");
        fx.write(&other_nfd, b"same SIZE");
        fx.write("plain.jpg", b"plain");
        assert_eq!(fx.scan().hashed, 2);
        // Cataloged in NFC, as on Linux; the files on disk are in NFD, as macOS may write them.
        fx.respell_row(&nfd, &nfc);
        fx.respell_row(&other_nfd, &other_nfc);
        fx.write(&other_nfd, b"changed, and longer");

        assert_eq!(
            fx.scan(),
            ScanSummary {
                updated: 1,
                unchanged: 2,
                respelled: 2,
                media_undecodable: 1,
                ..Default::default()
            }
        );
        let files = fx.files();
        let mut expected = vec![nfd.as_str(), other_nfd.as_str(), "plain.jpg"];
        expected.sort_unstable();
        assert_eq!(paths(&files), expected);
        let row = |path: &str| files.iter().find(|f| f.path == path).unwrap();
        // The unchanged file keeps its hash; the changed one is read again.
        assert!(row(&nfd).sha256.is_some());
        assert_eq!(row(&other_nfd).stamp.size, 19);
        assert_eq!(fx.scan().unchanged, 3);
    }

    #[test]
    fn test_part_of_library_under_another_normalization() {
        let fx = Fixture::new();
        let (nfc, nfd) = spellings("{}/{}.jpg");
        fx.write(&nfd, b"in the part");
        fx.write("elsewhere.jpg", b"elsewhere");
        fx.scan();
        fx.respell_row(&nfd, &nfc);

        let (_, folder) = spellings("{}");
        let part = scan_with(
            &fx.lib().join(folder),
            &fx.catalog(),
            ScanOptions::QUIET,
            &Tools::default(),
        );
        let summary = part.unwrap();
        assert_eq!(
            (summary.added, summary.removed, summary.respelled),
            (0, 0, 1)
        );
        let mut expected = vec![nfd.as_str(), "elsewhere.jpg"];
        expected.sort_unstable();
        assert_eq!(paths(&fx.files()), expected);
    }

    /// Linux keeps names as given, so both spellings can be files of their own there.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_both_normalizations_on_disk_are_two_files() {
        let fx = Fixture::new();
        let (nfc, nfd) = spellings("{}.jpg");
        fx.write(&nfc, b"composed");
        fx.write(&nfd, b"decomposed");
        assert_eq!(fx.scan().added, 2);
        assert_eq!(fx.scan().unchanged, 2);

        // A new file is not taken for a cataloged one whose name is still there.
        let catalog = Catalog::open(&fx.catalog()).unwrap();
        let root = fx.lib().canonicalize().unwrap();
        let id = catalog.library_id(root.to_str().unwrap()).unwrap();
        catalog.remove(id, &nfd).unwrap();
        drop(catalog);
        let summary = fx.scan();
        assert_eq!(
            (summary.added, summary.unchanged, summary.respelled),
            (1, 1, 0)
        );
        let mut expected = vec![nfc.as_str(), nfd.as_str()];
        expected.sort_unstable();
        assert_eq!(paths(&fx.files()), expected);
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

        let result = scan(&fx.lib(), &fx.catalog(), ScanOptions::QUIET);
        assert!(
            matches!(&result, Err(Error::Usage(m)) if m.contains("--allow-empty")),
            "{result:?}"
        );
        assert_eq!(paths(&fx.files()), vec!["a.jpg"]);
    }

    const ALLOW_EMPTY: ScanOptions = ScanOptions {
        quiet: true,
        allow_empty: true,
    };

    #[test]
    fn test_allow_empty_prunes_an_emptied_library() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"a");
        fx.write("b/c.mp4", b"c");
        fx.write("notes.txt", b"not media");
        fx.scan();
        remove_file(fx.lib().join("a.jpg")).unwrap();
        remove_file(fx.lib().join("b/c.mp4")).unwrap();

        let summary = scan_with(&fx.lib(), &fx.catalog(), ALLOW_EMPTY, &Tools::default());
        assert_eq!(summary.unwrap().removed, 2);
        assert_eq!(fx.files(), vec![]);
        // The library stays, with no files, as a library scanned while empty does.
        let catalog = Catalog::open(&fx.catalog()).unwrap();
        let libraries = catalog.libraries().unwrap();
        assert_eq!(libraries.len(), 1);
        assert_eq!(libraries[0].files, 0);
        assert!(libraries[0].scanned_at.is_some());
        // Nothing is left to protect, so the next scan needs no flag.
        assert_eq!(fx.scan(), ScanSummary::default());
    }

    #[test]
    fn test_allow_empty_prunes_only_the_scanned_part() {
        let fx = Fixture::new();
        fx.write("a/one.jpg", b"1");
        fx.write("b/two.jpg", b"22");
        fx.scan();
        remove_file(fx.lib().join("a/one.jpg")).unwrap();
        let part = fx.lib().join("a");

        let refused = scan_with(&part, &fx.catalog(), ScanOptions::QUIET, &Tools::default());
        assert!(matches!(refused, Err(Error::Usage(_))));
        assert_eq!(paths(&fx.files()), vec!["a/one.jpg", "b/two.jpg"]);

        let summary = scan_with(&part, &fx.catalog(), ALLOW_EMPTY, &Tools::default()).unwrap();
        assert_eq!(summary.removed, 1);
        let root = fx.lib().canonicalize().unwrap();
        assert_eq!(summary.part_of, Some(display_root(root.to_str().unwrap())));
        assert_eq!(paths(&fx.files()), vec!["b/two.jpg"]);
    }

    #[test]
    fn test_allow_empty_changes_nothing_when_media_is_found() {
        let fx = Fixture::new();
        fx.write("a.jpg", b"a");
        fx.write("b.jpg", b"b");
        fx.scan();
        remove_file(fx.lib().join("a.jpg")).unwrap();

        let summary = scan_with(&fx.lib(), &fx.catalog(), ALLOW_EMPTY, &Tools::default());
        let summary = summary.unwrap();
        assert_eq!((summary.removed, summary.unchanged), (1, 1));
        assert_eq!(paths(&fx.files()), vec!["b.jpg"]);
    }

    #[test]
    fn test_allow_empty_refuses_a_missing_folder() {
        let fx = Fixture::new();
        fx.write("a/one.jpg", b"1");
        fx.scan();
        remove_file(fx.lib().join("a/one.jpg")).unwrap();
        std::fs::remove_dir(fx.lib().join("a")).unwrap();

        let result = scan(&fx.lib().join("a"), &fx.catalog(), ALLOW_EMPTY);
        assert!(matches!(result, Err(Error::Usage(_))));
        assert_eq!(paths(&fx.files()), vec!["a/one.jpg"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_allow_empty_refuses_an_unreadable_folder() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new();
        fx.write("a.jpg", b"a");
        fx.scan();
        let lib = fx.lib();
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o300)).unwrap();
        if std::fs::read_dir(&lib).is_ok() {
            // Running as root: permissions do not apply.
            std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let result = scan(&lib, &fx.catalog(), ALLOW_EMPTY);
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(&result, Err(Error::Usage(m)) if m.starts_with("Cannot list")),
            "{result:?}"
        );
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
            ScanOptions::QUIET,
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
        let result = scan(&fx.lib(), &nested, ScanOptions::QUIET);
        assert!(matches!(result, Err(Error::Usage(_))));
        assert!(!fx.lib().join("deeper").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_catalog_symlinked_into_library_is_refused() {
        let fx = Fixture::new();
        let link = fx.dir.path().join("link");
        std::os::unix::fs::symlink(fx.lib(), &link).unwrap();
        let result = scan(&fx.lib(), &link.join("catalog.sqlite"), ScanOptions::QUIET);
        assert!(matches!(result, Err(Error::Usage(_))));
        assert!(!fx.lib().join("catalog.sqlite").exists());
    }

    #[test]
    fn test_catalog_inside_library_is_refused() {
        let fx = Fixture::new();
        let result = scan(
            &fx.lib(),
            &fx.lib().join("catalog.sqlite"),
            ScanOptions::QUIET,
        );
        assert!(matches!(result, Err(Error::Usage(_))));
        assert!(!fx.lib().join("catalog.sqlite").exists());
    }

    #[test]
    fn test_missing_library_is_refused() {
        let fx = Fixture::new();
        let result = scan(&fx.lib().join("nope"), &fx.catalog(), ScanOptions::QUIET);
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
