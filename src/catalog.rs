//! The SQLite catalog of scanned media files.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::avhash::{
    frames_from_bytes, frames_to_bytes, values_from_bytes, values_to_bytes, AudioFingerprint,
    Frame, VideoSignature,
};
use crate::dates::{from_taken_at, Taken};
use crate::error::Error;
use crate::imagehash::{ImageHashes, Visual};
use crate::media::{Dimensions, MediaType};
use crate::names::folded;
use directories::ProjectDirs;
use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, Row, Transaction,
    TransactionBehavior,
};
use same_file::is_same_file;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::create_dir_all;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Schema version, stored in `PRAGMA user_version`.
pub(crate) const SCHEMA_VERSION: i32 = 2;

/// SQL that takes a catalog from one schema version to the next: the first entry takes version 1
/// to 2, and so on.
const MIGRATIONS: [&str; (SCHEMA_VERSION - 1) as usize] = [
    // Video and audio dates that `nom-exif` read from a QuickTime header holding Unix time, which
    // come out before 1970 in UTC: read as `dates::quicktime_creation` reads them, as a scan of
    // the file would (2082844800 is `dates::QUICKTIME_EPOCH_OFFSET`). 1904-01-01 is a header
    // without a time.
    "UPDATE files
     SET taken_at = CASE
         WHEN taken_at = '1904-01-01T00:00:00+00:00' THEN NULL
         ELSE strftime('%Y-%m-%dT%H:%M:%S+00:00', substr(taken_at, 1, 19), '+2082844800 seconds')
     END
     WHERE media_type IN ('video', 'audio')
         AND length(taken_at) = 25
         AND taken_at LIKE '%+00:00'
         AND taken_at >= '1904-01-01T00:00:00'
         AND taken_at < '1970-01-01T00:00:00';",
];

/// Schema for an empty catalog.
const SCHEMA: &str = "
CREATE TABLE libraries (
    id INTEGER PRIMARY KEY,
    root TEXT NOT NULL UNIQUE,
    scanned_at TEXT
);
CREATE TABLE files (
    library_id INTEGER NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    media_type TEXT NOT NULL,
    sha256 TEXT,
    taken_at TEXT,
    camera_make TEXT,
    camera_model TEXT,
    width INTEGER,
    height INTEGER,
    -- The hash and decoder versions the hash pass decoded (or failed to decode) the file with;
    -- NULL if it has not. A file decoded with other versions is decoded again.
    hash_version TEXT,
    pixel_hash TEXT,
    -- Perceptual hash: 64 bits stored as a signed integer.
    phash INTEGER,
    -- 32 x 32 grayscale pixels, to confirm that images with close perceptual hashes are copies.
    fingerprint BLOB,
    -- Video and audio: duration in milliseconds, the video's sampled frames
    -- (`avhash::frames_to_bytes`), the audio's Chromaprint values (32-bit little-endian).
    duration_ms INTEGER,
    frames BLOB,
    chromaprint BLOB,
    PRIMARY KEY (library_id, path)
);
CREATE INDEX files_size ON files (size);
CREATE INDEX files_sha256 ON files (sha256);
";

/// What a scan compares to decide whether a file changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stamp {
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
}

/// Metadata read from inside a media file. Every field is optional.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MediaMeta {
    /// When the photo or video was taken: RFC 3339 if the file records a timezone,
    /// otherwise `YYYY-MM-DDTHH:MM:SS` in the camera's local time.
    pub taken_at: Option<String>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl MediaMeta {
    /// When the photo or video was taken, if `taken_at` is a valid date and time.
    pub fn taken(&self) -> Option<Taken> {
        self.taken_at.as_deref().and_then(from_taken_at)
    }

    /// Camera make and model, without the make twice when the model starts with it.
    pub fn camera(&self) -> Option<String> {
        match (&self.camera_make, &self.camera_model) {
            (Some(make), Some(model)) if model.starts_with(make.as_str()) => Some(model.clone()),
            (Some(make), Some(model)) => Some(format!("{make} {model}")),
            (Some(one), None) | (None, Some(one)) => Some(one.clone()),
            (None, None) => None,
        }
    }
}

/// One media file in the catalog.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileRecord {
    /// Path relative to the library root, with `/` as separator on every platform.
    pub path: String,
    pub stamp: Stamp,
    pub media_type: MediaType,
    /// SHA-256 of the contents as lowercase hex. Only computed for files that share their size
    /// with another cataloged file, since only those can be exact duplicates.
    pub sha256: Option<String>,
    pub meta: MediaMeta,
}

/// A hashed file, with the root of the library it is in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HashedFile {
    pub library_id: i64,
    pub root: String,
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// `None` if the file has no content hash: its size is not shared with another file.
    pub sha256: Option<String>,
}

/// An open catalog.
pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// The default catalog location in the OS data directory, if there is one.
    pub fn default_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "pixrs").map(|dirs| dirs.data_dir().join("catalog.sqlite"))
    }

    /// Open the catalog at `path`, creating it if it does not exist.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        configure(&conn)?;
        if schema_version(&conn)? == 0 {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?;
        } else {
            check_schema(&conn, path, || migrate(&conn))?;
        }
        Ok(Self { conn })
    }

    /// Open the catalog at `path` for writing. It must exist and have the current schema, or an
    /// older one, which is migrated: unlike `open`, nothing is created, so a path from a plan or
    /// journal file cannot make a catalog, or put tables into another SQLite file.
    pub fn open_for_update(path: &Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Err(Error::CatalogGone(path.to_path_buf()));
        }
        let conn = connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        // Before anything is written to the file, such as the journal mode.
        check_schema(&conn, path, || migrate(&conn))?;
        configure(&conn)?;
        Ok(Self { conn })
    }

    /// Open the catalog at `path` read-only. It must exist and have the current schema; an older
    /// one is migrated first.
    pub fn open_existing(path: &Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Err(Error::NoCatalog(path.to_path_buf()));
        }
        let conn = connect(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // A catalog of an older version is brought up to date once, through a connection that
        // may write.
        check_schema(&conn, path, || {
            migrate(&connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?)
        })?;
        Ok(Self { conn })
    }

    /// Start a transaction. Catalog writes made while it is alive are part of it.
    pub fn transaction(&self) -> rusqlite::Result<Transaction<'_>> {
        self.conn.unchecked_transaction()
    }

    /// The ID of the library at `root`, registering it if it is new.
    pub fn library_id(&self, root: &str) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO libraries (root) VALUES (?1) ON CONFLICT (root) DO NOTHING",
            [root],
        )?;
        self.conn
            .query_row("SELECT id FROM libraries WHERE root = ?1", [root], |row| {
                row.get(0)
            })
    }

    /// The outermost library whose root is `path` or contains it, if any.
    pub fn containing_library(&self, path: &Path) -> rusqlite::Result<Option<LibraryRoot>> {
        let mut outermost: Option<LibraryRoot> = None;
        for library in self.roots(None)? {
            let shorter = outermost
                .as_ref()
                .is_none_or(|o| library.root.len() < o.root.len());
            if path.starts_with(&library.root) && shorter {
                outermost = Some(library);
            }
        }
        Ok(outermost)
    }

    /// Every library but `except`, ordered by root.
    fn roots(&self, except: Option<i64>) -> rusqlite::Result<Vec<LibraryRoot>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, root FROM libraries WHERE id IS NOT ?1 ORDER BY root")?;
        let rows = stmt.query_map([except], |row| {
            Ok(LibraryRoot {
                id: row.get(0)?,
                root: row.get(1)?,
            })
        })?;
        rows.collect()
    }

    /// Fold every other library inside `root`, the root of `library_id`, into that library:
    /// its rows move there with the path from `root` to its root in front, keeping their hashes,
    /// and the library is dropped. Where both have a row for a file with the same size and mtime,
    /// hashes the outer row lacks are taken from the inner one; otherwise the outer row stays.
    /// Returns the roots of the folded libraries.
    pub fn fold_nested(&self, library_id: i64, root: &Path) -> rusqlite::Result<Vec<String>> {
        let inner = self.nested(library_id, root)?;
        if inner.is_empty() {
            return Ok(vec![]);
        }
        let tx = self.conn.unchecked_transaction()?;
        for nested in &inner {
            self.merge_library(nested.library.id, library_id, &nested.prefix)?;
        }
        tx.commit()?;
        Ok(inner
            .into_iter()
            .map(|nested| nested.library.root)
            .collect())
    }

    /// Point the library `library_id` at `new_root`, where its files are now, keeping its rows
    /// and their hashes. If `new_root` is another library's root or inside it, the rows move
    /// into that library instead, as `fold_nested` moves them; libraries inside `new_root` are
    /// folded in.
    pub fn relocate(&self, library_id: i64, new_root: &str) -> rusqlite::Result<Relocated> {
        let new = Path::new(new_root);
        let tx = self.conn.unchecked_transaction()?;
        // The outermost other library that `new_root` is in, with the path to `new_root` in it.
        let mut outer: Option<Nested> = None;
        for library in self.roots(Some(library_id))? {
            let shorter = outer
                .as_ref()
                .is_none_or(|o| library.root.len() < o.library.root.len());
            if let Some(prefix) = path_below(Path::new(&library.root), new).filter(|_| shorter) {
                outer = Some(Nested { library, prefix });
            }
        }
        let library = if let Some(Nested { library, prefix }) = outer {
            self.merge_library(library_id, library.id, &prefix)?;
            library
        } else {
            self.conn.execute(
                "UPDATE libraries SET root = ?2 WHERE id = ?1",
                params![library_id, new_root],
            )?;
            LibraryRoot {
                id: library_id,
                root: String::from(new_root),
            }
        };
        let inner = self.nested(library.id, Path::new(&library.root))?;
        for nested in &inner {
            self.merge_library(nested.library.id, library.id, &nested.prefix)?;
        }
        tx.commit()?;
        Ok(Relocated {
            root: library.root,
            folded: inner
                .into_iter()
                .map(|nested| nested.library.root)
                .collect(),
        })
    }

    /// Every library other than `library_id` inside `root`.
    fn nested(&self, library_id: i64, root: &Path) -> rusqlite::Result<Vec<Nested>> {
        Ok(self
            .roots(Some(library_id))?
            .into_iter()
            .filter_map(|library| {
                let prefix = path_below(root, Path::new(&library.root))?;
                Some(Nested { library, prefix })
            })
            .collect())
    }

    /// Move the rows of library `from` into library `into`, with `prefix` in front of their
    /// paths, and drop `from`. Where `into` has a row for a file with the same size and mtime,
    /// it takes the hashes it lacks; otherwise it stays. Run it in a transaction.
    fn merge_library(&self, from: i64, into: i64, prefix: &str) -> rusqlite::Result<()> {
        let hash_columns = HASH_COLUMNS
            .iter()
            .map(|c| format!("{c} = CASE WHEN o.hash_version IS NULL THEN i.{c} ELSE o.{c} END"))
            .collect::<Vec<_>>()
            .join(", ");
        self.conn.execute(
            &format!(
                "UPDATE files AS o SET sha256 = coalesce(o.sha256, i.sha256), {hash_columns}
                 FROM files AS i
                 WHERE o.library_id = ?1 AND i.library_id = ?2 AND o.path = ?3 || i.path
                   AND o.size = i.size AND o.mtime_ns = i.mtime_ns"
            ),
            params![into, from, prefix],
        )?;
        self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO files (library_id, path, {MOVED_COLUMNS})
                 SELECT ?1, ?3 || path, {MOVED_COLUMNS} FROM files WHERE library_id = ?2"
            ),
            params![into, from, prefix],
        )?;
        self.conn
            .execute("DELETE FROM files WHERE library_id = ?1", [from])?;
        self.conn
            .execute("DELETE FROM libraries WHERE id = ?1", [from])?;
        Ok(())
    }

    /// The ID of the library at `root`, if it has been scanned.
    pub fn find_library(&self, root: &str) -> rusqlite::Result<Option<i64>> {
        self.conn
            .query_row("SELECT id FROM libraries WHERE root = ?1", [root], |row| {
                row.get(0)
            })
            .optional()
    }

    /// Record that the library was just scanned.
    pub fn mark_scanned(&self, library_id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE libraries SET scanned_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') \
             WHERE id = ?1",
            [library_id],
        )?;
        Ok(())
    }

    /// Size and mtime of every cataloged file in the library, keyed by relative path.
    pub fn stamps(&self, library_id: i64) -> rusqlite::Result<HashMap<String, Stamp>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, mtime_ns FROM files WHERE library_id = ?1")?;
        let rows = stmt.query_map([library_id], |row| {
            Ok((
                row.get(0)?,
                Stamp {
                    size: row.get(1)?,
                    mtime_ns: row.get(2)?,
                },
            ))
        })?;
        rows.collect()
    }

    /// Insert or replace a file. A replaced file loses its image hashes until it is hashed again.
    pub fn upsert(&self, library_id: i64, file: &FileRecord) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO files (library_id, path, size, mtime_ns, media_type, sha256, taken_at,
                                camera_make, camera_model, width, height)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (library_id, path) DO UPDATE SET
                size = excluded.size,
                mtime_ns = excluded.mtime_ns,
                media_type = excluded.media_type,
                sha256 = excluded.sha256,
                taken_at = excluded.taken_at,
                camera_make = excluded.camera_make,
                camera_model = excluded.camera_model,
                width = excluded.width,
                height = excluded.height,
                hash_version = NULL,
                duration_ms = NULL,
                frames = NULL,
                chromaprint = NULL,
                pixel_hash = NULL,
                phash = NULL,
                fingerprint = NULL",
            params![
                library_id,
                file.path,
                file.stamp.size,
                file.stamp.mtime_ns,
                file.media_type,
                file.sha256,
                file.meta.taken_at,
                file.meta.camera_make,
                file.meta.camera_model,
                file.meta.width,
                file.meta.height,
            ],
        )?;
        Ok(())
    }

    /// Give a file's row `new`, the name the file has on disk, which is its cataloged name `old`
    /// in another Unicode normalization. Everything else in the row stays. There must be no row
    /// at `new`.
    pub fn respell(&self, library_id: i64, old: &str, new: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET path = ?3 WHERE library_id = ?1 AND path = ?2",
            params![library_id, old, new],
        )?;
        Ok(())
    }

    /// Set the hash of a file.
    pub fn set_sha256(&self, library_id: i64, path: &str, sha256: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET sha256 = ?3 WHERE library_id = ?1 AND path = ?2",
            params![library_id, path, sha256],
        )?;
        Ok(())
    }

    /// Path and size of the library's files that have no hash yet and share their size with
    /// another cataloged file in any library, ordered by path.
    pub fn unhashed_with_shared_size(
        &self,
        library_id: i64,
    ) -> rusqlite::Result<Vec<UnhashedFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size FROM files
             WHERE library_id = ?1 AND sha256 IS NULL
               AND size IN (SELECT size FROM files GROUP BY size HAVING count(*) > 1)
             ORDER BY path",
        )?;
        let rows = stmt.query_map([library_id], |row| {
            Ok(UnhashedFile {
                path: row.get(0)?,
                size: row.get(1)?,
            })
        })?;
        rows.collect()
    }

    /// Number of files in the given libraries (all if empty) that share their size with another
    /// file but have no hash yet, because their library has not been scanned since the size
    /// became shared.
    pub fn count_unhashed_with_shared_size(&self, libraries: &[i64]) -> rusqlite::Result<usize> {
        self.conn.query_row(
            &format!(
                "SELECT count(*) FROM files
                 WHERE sha256 IS NULL
                   AND size IN (SELECT size FROM files GROUP BY size HAVING count(*) > 1)
                   AND {}",
                in_libraries(libraries)
            ),
            params_from_iter(libraries),
            |row| row.get(0),
        )
    }

    /// Every file, in any library, whose size and hash match another file's.
    pub fn duplicate_candidates(&self) -> rusqlite::Result<Vec<HashedFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.library_id, l.root, f.path, f.size, f.mtime_ns, f.sha256
             FROM files f JOIN libraries l ON l.id = f.library_id
             WHERE (f.size, f.sha256) IN (
                 SELECT size, sha256 FROM files WHERE sha256 IS NOT NULL
                 GROUP BY size, sha256 HAVING count(*) > 1
             )",
        )?;
        let rows = stmt.query_map([], hashed_file)?;
        rows.collect()
    }

    /// Remove a file from the catalog.
    pub fn remove(&self, library_id: i64, path: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM files WHERE library_id = ?1 AND path = ?2",
            params![library_id, path],
        )?;
        Ok(())
    }

    /// Every cataloged file in the library, ordered by path.
    pub fn files(&self, library_id: i64) -> rusqlite::Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE library_id = ?1 ORDER BY path"
        ))?;
        let rows = stmt.query_map([library_id], file_record)?;
        rows.collect()
    }

    /// Every media file in the library with its type and the versions the hash pass decoded
    /// it with, if it has, ordered by path.
    pub fn hash_versions(&self, library_id: i64) -> rusqlite::Result<Vec<HashVersion>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, media_type, hash_version FROM files WHERE library_id = ?1
             ORDER BY path",
        )?;
        let rows = stmt.query_map([library_id], |row| {
            Ok(HashVersion {
                path: row.get(0)?,
                media_type: row.get(1)?,
                version: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Number of media files in the given libraries (all if empty) the hash pass has not
    /// decoded: not scanned since they changed, or no decoder for them (RAW, HEIC without
    /// `ffmpeg`, video without `ffmpeg`, audio without `fpcalc`).
    pub fn count_unhashed(&self, libraries: &[i64]) -> rusqlite::Result<usize> {
        self.conn.query_row(
            &format!(
                "SELECT count(*) FROM files WHERE {} AND hash_version IS NULL",
                in_libraries(libraries)
            ),
            params_from_iter(libraries),
            |row| row.get(0),
        )
    }

    /// Record that a video was decoded with `version`, with its signature, or without if
    /// decoding failed.
    pub fn set_video_signature(
        &self,
        library_id: i64,
        path: &str,
        version: &str,
        signature: Option<&VideoSignature>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET hash_version = ?3, duration_ms = ?4, frames = ?5
             WHERE library_id = ?1 AND path = ?2",
            params![
                library_id,
                path,
                version,
                signature.map(|s| s.duration_ms),
                signature.map(|s| frames_to_bytes(&s.frames)),
            ],
        )?;
        Ok(())
    }

    /// Record that audio was fingerprinted with `version`, with its fingerprint, or without if
    /// that failed.
    pub fn set_audio_fingerprint(
        &self,
        library_id: i64,
        path: &str,
        version: &str,
        fingerprint: Option<&AudioFingerprint>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET hash_version = ?3, duration_ms = ?4, chromaprint = ?5
             WHERE library_id = ?1 AND path = ?2",
            params![
                library_id,
                path,
                version,
                fingerprint.map(|f| f.duration_ms),
                fingerprint.map(|f| values_to_bytes(&f.values)),
            ],
        )?;
        Ok(())
    }

    /// Every video with a signature and every audio file with a fingerprint, in any library.
    /// A file whose stored signature cannot be read is left out.
    pub fn hashed_av(&self) -> rusqlite::Result<Vec<HashedAv>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.library_id, l.root, f.path, f.size, f.mtime_ns, f.sha256, f.duration_ms,
                    f.width, f.height, f.frames, f.chromaprint
             FROM files f JOIN libraries l ON l.id = f.library_id
             WHERE f.duration_ms IS NOT NULL
               AND (f.frames IS NOT NULL OR f.chromaprint IS NOT NULL)",
        )?;
        let rows = stmt.query_map([], |row| {
            let duration_ms = row.get(6)?;
            let frames: Option<Vec<u8>> = row.get(9)?;
            let signature = if let Some(frames) = frames {
                frames_from_bytes(&frames).map(|frames| {
                    AvSignature::Video(VideoSignature {
                        duration_ms,
                        frames,
                    })
                })
            } else {
                row.get::<_, Option<Vec<u8>>>(10)?.and_then(|values| {
                    let values = values_from_bytes(&values)?;
                    Some(AvSignature::Audio(AudioFingerprint {
                        duration_ms,
                        values,
                    }))
                })
            };
            let Some(signature) = signature else {
                return Ok(None);
            };
            Ok(Some(HashedAv {
                file: hashed_file(row)?,
                dimensions: Dimensions::new(row.get(7)?, row.get(8)?),
                signature,
            }))
        })?;
        rows.filter_map(Result::transpose).collect()
    }

    /// Record that an image was decoded with `version`, with its hashes and decoded
    /// dimensions, or without if decoding failed.
    pub fn set_image_hashes(
        &self,
        library_id: i64,
        path: &str,
        version: &str,
        hashes: Option<&ImageHashes>,
    ) -> rusqlite::Result<()> {
        let visual = hashes.and_then(|h| h.visual.as_ref());
        self.conn.execute(
            "UPDATE files SET hash_version = ?3, pixel_hash = ?4, phash = ?5,
                              fingerprint = ?6,
                              width = coalesce(?7, width), height = coalesce(?8, height)
             WHERE library_id = ?1 AND path = ?2",
            params![
                library_id,
                path,
                version,
                hashes.map(|h| h.pixels.as_str()),
                visual.map(|v| v.phash.cast_signed()),
                visual.map(|v| v.fingerprint.as_slice()),
                hashes.map(|h| h.dimensions.width),
                hashes.map(|h| h.dimensions.height),
            ],
        )?;
        Ok(())
    }

    /// Every image, in any library, that has image hashes. Without `visual`, perceptual hashes
    /// and fingerprints are left out (`None`), which saves reading about 1 KB per image when
    /// similar images are not looked for.
    pub fn hashed_images(&self, visual: bool) -> rusqlite::Result<Vec<HashedImage>> {
        let visual_columns = if visual {
            "f.phash, f.fingerprint"
        } else {
            "NULL, NULL"
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT f.library_id, l.root, f.path, f.size, f.mtime_ns, f.sha256, f.pixel_hash,
                    f.width, f.height, {visual_columns}
             FROM files f JOIN libraries l ON l.id = f.library_id
             WHERE f.pixel_hash IS NOT NULL"
        ))?;
        let rows = stmt.query_map([], |row| {
            let phash: Option<i64> = row.get(9)?;
            let fingerprint: Option<Vec<u8>> = row.get(10)?;
            Ok(HashedImage {
                file: hashed_file(row)?,
                pixel_hash: row.get(6)?,
                dimensions: Dimensions::new(row.get(7)?, row.get(8)?),
                visual: phash.zip(fingerprint).map(|(phash, fingerprint)| Visual {
                    phash: phash.cast_unsigned(),
                    fingerprint,
                }),
            })
        })?;
        rows.collect()
    }

    /// A number that changes whenever another connection commits to the catalog, e.g. a scan.
    pub fn data_version(&self) -> rusqlite::Result<i64> {
        self.conn
            .pragma_query_value(None, "data_version", |row| row.get(0))
    }

    /// Whether the catalog has a row for the file.
    pub fn has_row(&self, library_id: i64, path: &str) -> rusqlite::Result<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM files WHERE library_id = ?1 AND path = ?2",
                params![library_id, path],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
    }

    /// Move a file's row `from` one path `to` another, possibly in another library, if there is
    /// a row at `from`. A row at `to` is dropped first with `replace` (when the file there is
    /// known to be gone); without it, the row stays where it is. Returns whether it moved.
    pub fn rename_row(
        &self,
        from: &FileRef,
        to: &FileRef,
        replace: bool,
    ) -> rusqlite::Result<bool> {
        if from == to {
            return Ok(false);
        }
        let tx = self.conn.unchecked_transaction()?;
        if !self.has_row(from.library_id, &from.path)? {
            return Ok(false);
        }
        if self.has_row(to.library_id, &to.path)? {
            if !replace {
                return Ok(false);
            }
            self.remove(to.library_id, &to.path)?;
        }
        self.conn.execute(
            "UPDATE files SET library_id = ?3, path = ?4 WHERE library_id = ?1 AND path = ?2",
            params![from.library_id, from.path, to.library_id, to.path],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Remove a file's row and return everything in it, so `put_row` can restore it.
    pub fn take_row(&self, library_id: i64, path: &str) -> rusqlite::Result<Option<CatalogRow>> {
        let row = self
            .conn
            .query_row(
                &format!(
                    "SELECT {FILE_COLUMNS}, hash_version, pixel_hash, phash, fingerprint, duration_ms, frames,
                            chromaprint
                     FROM files WHERE library_id = ?1 AND path = ?2"
                ),
                params![library_id, path],
                |row| {
                    Ok(CatalogRow {
                        library_id,
                        file: file_record(row)?,
                        hash_version: row.get(10)?,
                        pixel_hash: row.get(11)?,
                        phash: row.get::<_, Option<i64>>(12)?.map(i64::cast_unsigned),
                        fingerprint: row.get::<_, Option<Vec<u8>>>(13)?.as_deref().map(hex),
                        duration_ms: row.get(14)?,
                        frames: row.get::<_, Option<Vec<u8>>>(15)?.as_deref().map(hex),
                        chromaprint: row.get::<_, Option<Vec<u8>>>(16)?.as_deref().map(hex),
                    })
                },
            )
            .optional()?;
        if row.is_some() {
            self.remove(library_id, path)?;
        }
        Ok(row)
    }

    /// Put back a row from `take_row`, replacing any row at its path.
    pub fn put_row(&self, row: &CatalogRow) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.upsert(row.library_id, &row.file)?;
        self.conn.execute(
            "UPDATE files SET hash_version = ?3, pixel_hash = ?4, phash = ?5,
                              fingerprint = ?6, duration_ms = ?7, frames = ?8, chromaprint = ?9
             WHERE library_id = ?1 AND path = ?2",
            params![
                row.library_id,
                row.file.path,
                row.hash_version,
                row.pixel_hash,
                row.phash.map(u64::cast_signed),
                unhex(row.fingerprint.as_deref()),
                row.duration_ms,
                unhex(row.frames.as_deref()),
                unhex(row.chromaprint.as_deref()),
            ],
        )?;
        tx.commit()
    }

    /// Every library with its file count, ordered by root.
    pub fn libraries(&self) -> rusqlite::Result<Vec<Library>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.id, l.root, l.scanned_at, count(f.path)
             FROM libraries l LEFT JOIN files f ON f.library_id = l.id
             GROUP BY l.id ORDER BY l.root",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Library {
                id: row.get(0)?,
                root: row.get(1)?,
                scanned_at: row.get(2)?,
                files: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// The root of a library, if it is in the catalog.
    pub fn library_root(&self, library_id: i64) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT root FROM libraries WHERE id = ?1",
                [library_id],
                |row| row.get(0),
            )
            .optional()
    }

    /// One cataloged file, if it is in the catalog.
    pub fn file(&self, library_id: i64, path: &str) -> rusqlite::Result<Option<FileRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {FILE_COLUMNS} FROM files WHERE library_id = ?1 AND path = ?2"),
                params![library_id, path],
                file_record,
            )
            .optional()
    }

    /// The folders directly inside the folder `dir` of a library (`""` is the root), with the
    /// number of files below each, ordered by name.
    pub fn subfolders(
        &self,
        library_id: i64,
        dir: &str,
        media_type: Option<MediaType>,
    ) -> rusqlite::Result<Vec<(String, usize)>> {
        let (cond, values) = below(library_id, dir, media_type);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT substr(rest, 1, instr(rest, '/') - 1) AS name, count(*)
             FROM (SELECT substr(path, {}) AS rest FROM files WHERE {cond})
             WHERE instr(rest, '/') > 0
             GROUP BY name ORDER BY name",
            dir_prefix_len(dir) + 1
        ))?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    /// The files directly inside the folder `dir` of a library, ordered by path.
    pub fn folder_files(
        &self,
        library_id: i64,
        dir: &str,
        media_type: Option<MediaType>,
        page: Page,
    ) -> rusqlite::Result<Vec<FileRecord>> {
        let (cond, values) = in_folder(library_id, dir, media_type);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE {cond} ORDER BY path {}",
            page.sql()
        ))?;
        let rows = stmt.query_map(params_from_iter(values), file_record)?;
        rows.collect()
    }

    /// The paths before and after `path` among the files in its folder, in folder order.
    pub fn folder_neighbors(
        &self,
        library_id: i64,
        path: &str,
        media_type: Option<MediaType>,
    ) -> rusqlite::Result<(Option<String>, Option<String>)> {
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        let (cond, values) = in_folder(library_id, dir, media_type);
        let neighbor = |op: &str, order: &str| -> rusqlite::Result<Option<String>> {
            let mut values = values.clone();
            values.push(Value::from(String::from(path)));
            self.conn
                .query_row(
                    &format!(
                        "SELECT path FROM files WHERE {cond} AND path {op} ? ORDER BY path {order} LIMIT 1"
                    ),
                    params_from_iter(values),
                    |row| row.get(0),
                )
                .optional()
        };
        Ok((neighbor("<", "DESC")?, neighbor(">", "ASC")?))
    }

    /// Months (`YYYY-MM`) that have files, newest first, with the number of files in each.
    /// A file's date is when it was taken, or its mtime in local time if that is unknown.
    pub fn months(&self, filter: &Filter) -> rusqlite::Result<Vec<(String, usize)>> {
        let (cond, values) = filter.condition();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT substr({DATE_KEY}, 1, 7) AS month, count(*) FROM files WHERE {cond}
             GROUP BY month ORDER BY month DESC"
        ))?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    /// The files dated in `month` (`YYYY-MM`), newest first.
    pub fn month_files(
        &self,
        filter: &Filter,
        month: &str,
        page: Page,
    ) -> rusqlite::Result<Vec<DatedFile>> {
        let (cond, mut values) = filter.condition();
        values.push(Value::from(String::from(month)));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS}, library_id, {DATE_KEY} AS date FROM files
             WHERE {cond} AND substr(date, 1, 7) = ?
             ORDER BY date DESC, library_id DESC, path DESC {}",
            page.sql()
        ))?;
        let rows = stmt.query_map(params_from_iter(values), dated_file)?;
        rows.collect()
    }

    /// When a file was taken, or its mtime in local time, as `YYYY-MM-DDTHH:MM:SS`.
    pub fn date(&self, library_id: i64, path: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                &format!("SELECT {DATE_KEY} FROM files WHERE library_id = ?1 AND path = ?2"),
                params![library_id, path],
                |row| row.get(0),
            )
            .optional()
    }

    /// The files before and after a file in date order (newest first), as library ID and path.
    pub fn date_neighbors(
        &self,
        filter: &Filter,
        library_id: i64,
        path: &str,
    ) -> rusqlite::Result<(Option<FileRef>, Option<FileRef>)> {
        let Some(date) = self.date(library_id, path)? else {
            return Ok((None, None));
        };
        let (cond, values) = filter.condition();
        let neighbor = |op: &str, order: &str| -> rusqlite::Result<Option<FileRef>> {
            let mut values = values.clone();
            values.extend([
                Value::from(date.clone()),
                Value::from(library_id),
                Value::from(String::from(path)),
            ]);
            self.conn
                .query_row(
                    &format!(
                        "SELECT library_id, path FROM files
                         WHERE {cond} AND ({DATE_KEY}, library_id, path) {op} (?, ?, ?)
                         ORDER BY {DATE_KEY} {order}, library_id {order}, path {order} LIMIT 1"
                    ),
                    params_from_iter(values),
                    |row| {
                        Ok(FileRef {
                            library_id: row.get(0)?,
                            path: row.get(1)?,
                        })
                    },
                )
                .optional()
        };
        Ok((neighbor(">", "ASC")?, neighbor("<", "DESC")?))
    }

    /// Every file, in any library, with the given size and hash.
    pub fn copies(&self, size: u64, sha256: &str) -> rusqlite::Result<Vec<HashedFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.library_id, l.root, f.path, f.size, f.mtime_ns, f.sha256
             FROM files f JOIN libraries l ON l.id = f.library_id
             WHERE f.size = ?1 AND f.sha256 = ?2",
        )?;
        let rows = stmt.query_map(params![size, sha256], hashed_file)?;
        let mut copies: Vec<HashedFile> = rows.collect::<rusqlite::Result<_>>()?;
        if let Some(keep) = one_row_per_file(&copies) {
            let mut keep = keep.into_iter();
            copies.retain(|_| keep.next().unwrap_or(true));
        }
        Ok(copies)
    }
}

/// An image with its pixel and perceptual hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedImage {
    pub file: HashedFile,
    pub pixel_hash: String,
    /// Size of the upright image, if known.
    pub dimensions: Option<Dimensions>,
    /// `None` for images with almost no detail, and when not read.
    pub visual: Option<Visual>,
}

/// Everything in a file's catalog row, kept in a journal while the file is outside its library.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogRow {
    pub library_id: i64,
    pub file: FileRecord,
    pub hash_version: Option<String>,
    pub pixel_hash: Option<String>,
    pub phash: Option<u64>,
    /// BLOBs as lowercase hex.
    pub fingerprint: Option<String>,
    pub duration_ms: Option<i64>,
    pub frames: Option<String>,
    pub chromaprint: Option<String>,
}

/// A BLOB as lowercase hex, for `CatalogRow`.
fn hex(blob: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(blob)
}

/// Lowercase hex from `hex` as a BLOB.
fn unhex(text: Option<&str>) -> Option<Vec<u8>> {
    text.and_then(|t| data_encoding::HEXLOWER.decode(t.as_bytes()).ok())
}

/// A video or audio file with its signature or fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedAv {
    pub file: HashedFile,
    /// Size of the video, if known; `None` for audio.
    pub dimensions: Option<Dimensions>,
    pub signature: AvSignature,
}

/// What similar videos and audio are found by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AvSignature {
    Video(VideoSignature),
    Audio(AudioFingerprint),
}

impl AvSignature {
    /// Duration of the video or audio in milliseconds.
    pub const fn duration_ms(&self) -> i64 {
        match self {
            Self::Video(video) => video.duration_ms,
            Self::Audio(audio) => audio.duration_ms,
        }
    }

    /// The sampled frames of a video.
    pub fn frames(&self) -> Option<&[Frame]> {
        match self {
            Self::Video(video) => Some(&video.frames),
            Self::Audio(_) => None,
        }
    }

    /// The Chromaprint values of audio.
    pub fn chromaprint(&self) -> Option<&[u32]> {
        match self {
            Self::Video(_) => None,
            Self::Audio(audio) => Some(&audio.values),
        }
    }
}

/// A catalog row's key: a library ID and a path in that library, with `/` separators.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileRef {
    pub library_id: i64,
    pub path: String,
}

impl FileRef {
    /// The row at `path` in the same library.
    #[must_use]
    pub fn with_path(&self, path: &str) -> Self {
        Self {
            library_id: self.library_id,
            path: String::from(path),
        }
    }
}

/// A library's ID and root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryRoot {
    pub id: i64,
    /// Canonical root path, as stored.
    pub root: String,
}

/// A library inside another library's root.
struct Nested {
    library: LibraryRoot,
    /// The path from the outer root to this library's root, ending in `/`, or empty for the same
    /// folder under another spelling.
    prefix: String,
}

/// Where `Catalog::relocate` put a library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relocated {
    /// The root of the library the rows are in now: the new root, or the library it is inside.
    pub root: String,
    /// The roots of the libraries inside it that were folded in.
    pub folded: Vec<String>,
}

/// A cataloged file with no content hash yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnhashedFile {
    pub path: String,
    pub size: u64,
}

/// A media file with the versions the hash pass decoded it with, if it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashVersion {
    pub path: String,
    pub media_type: MediaType,
    pub version: Option<String>,
}

/// A scanned library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Library {
    pub id: i64,
    /// Canonical root path, as stored.
    pub root: String,
    /// When the last scan finished, RFC 3339 in UTC.
    pub scanned_at: Option<String>,
    pub files: usize,
}

/// A file with the library it is in and its date (see `Catalog::months`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatedFile {
    pub library_id: i64,
    pub file: FileRecord,
    pub date: String,
}

/// Which files a listing shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only files in these libraries; all if empty.
    pub libraries: Vec<i64>,
    pub media_type: Option<MediaType>,
}

impl Filter {
    /// The SQL condition selecting the files, with its parameters.
    fn condition(&self) -> (String, Vec<Value>) {
        let mut cond = in_libraries(&self.libraries);
        let mut values: Vec<Value> = self.libraries.iter().map(|&id| Value::from(id)).collect();
        if let Some(media_type) = self.media_type {
            cond.push_str(" AND media_type = ?");
            values.push(Value::from(media_type));
        }
        (cond, values)
    }
}

/// The SQL condition selecting files in `libraries`, all if empty, with one `?` per library.
fn in_libraries(libraries: &[i64]) -> String {
    if libraries.is_empty() {
        String::from("1")
    } else {
        format!("library_id IN ({})", vec!["?"; libraries.len()].join(", "))
    }
}

/// A slice of a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub limit: usize,
    pub offset: usize,
}

impl Page {
    /// The `LIMIT` and `OFFSET` clause.
    fn sql(self) -> String {
        format!("LIMIT {} OFFSET {}", self.limit, self.offset)
    }
}

/// Columns read by `file_record`, in its order.
const FILE_COLUMNS: &str =
    "path, size, mtime_ns, media_type, sha256, taken_at, camera_make, camera_model, width, height";

/// Columns the hash pass fills in.
const HASH_COLUMNS: [&str; 7] = [
    "hash_version",
    "pixel_hash",
    "phash",
    "fingerprint",
    "duration_ms",
    "frames",
    "chromaprint",
];

/// Columns `fold_nested` copies, besides the library and path.
const MOVED_COLUMNS: &str = "size, mtime_ns, media_type, sha256, taken_at, camera_make, \
     camera_model, width, height, hash_version, pixel_hash, phash, fingerprint, duration_ms, \
     frames, chromaprint";

/// The path from `outer` to `inner` with `/` separators and a trailing `/` if `inner` is inside
/// `outer`, or empty if they are the same folder: by spelling, or else by file identity when
/// both exist. On a file system that ignores letter case (or Unicode normalization, as macOS
/// does) one folder has many spellings, and on Linux `canonicalize` keeps the one the user typed
/// (`/mnt/c/photos` for `/mnt/c/Photos`), so two spellings must not become two libraries. Found
/// by identity, the path is spelled as it is on disk, since the catalog needs one spelling and
/// that is the one a scan sees.
fn path_below(outer: &Path, inner: &Path) -> Option<String> {
    let parts = |from: &Path| -> Option<Vec<&OsStr>> {
        inner
            .strip_prefix(from)
            .ok()?
            .components()
            .map(|c| match c {
                Component::Normal(part) => Some(part),
                _ => None,
            })
            .collect()
    };
    let join = |parts: Vec<OsString>| -> Option<String> {
        let mut prefix = String::new();
        for part in parts {
            prefix.push_str(part.to_str()?);
            prefix.push('/');
        }
        Some(prefix)
    };
    if let Some(parts) = parts(outer) {
        return join(parts.into_iter().map(OsStr::to_os_string).collect());
    }
    let ancestor = inner
        .ancestors()
        .find(|ancestor| is_same_file(ancestor, outer).unwrap_or(false))?;
    let mut dir = outer.to_path_buf();
    let mut on_disk = vec![];
    for part in parts(ancestor)? {
        let name = on_disk_name(&dir, part);
        dir.push(&name);
        on_disk.push(name);
    }
    join(on_disk)
}

/// The name in `dir` that is `name` in some letter case or Unicode normalization: `name` itself
/// if it is there, else the one the file system reads it as (it ignores case or normalization
/// here), else `name` as given.
fn on_disk_name(dir: &Path, name: &OsStr) -> OsString {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return name.to_os_string();
    };
    let wanted = folded(&name.to_string_lossy());
    let mut same_name = None;
    for entry in entries.flatten() {
        let found = entry.file_name();
        if found == name {
            return found;
        }
        if same_name.is_none() && folded(&found.to_string_lossy()) == wanted {
            same_name = Some(found);
        }
    }
    same_name.unwrap_or_else(|| name.to_os_string())
}

/// `path` spelled as the catalog spells the library it is in, if it is in one under another
/// spelling (see `path_below`); otherwise unchanged. Call it on a canonical path before looking
/// the path up by its spelling.
pub fn cataloged_spelling(libraries: &[Library], path: &Path) -> PathBuf {
    if libraries.iter().any(|l| path.starts_with(&l.root)) {
        return path.to_path_buf();
    }
    for library in libraries {
        let root = Path::new(&library.root);
        if let Some(prefix) = path_below(root, path) {
            let mut spelled = root.to_path_buf();
            spelled.extend(prefix.split('/').filter(|part| !part.is_empty()));
            return spelled;
        }
    }
    path.to_path_buf()
}

/// Whether to keep each of `files`, in order, so that each file on disk has one row, or `None`
/// if it already has. Overlapping libraries, which catalogs scanned before pixrs folded nested
/// libraries can have, list a file once in each; the row from the outermost library is kept, so
/// a file is never its own duplicate. A row given more than once is kept or dropped each time.
pub fn one_row_per_file<'a>(files: impl IntoIterator<Item = &'a HashedFile>) -> Option<Vec<bool>> {
    let files: Vec<&HashedFile> = files.into_iter().collect();
    let mut best: HashMap<PathBuf, &HashedFile> = HashMap::new();
    let mut rows: HashSet<(i64, &str)> = HashSet::new();
    for &f in &files {
        rows.insert((f.library_id, &f.path));
        best.entry(library_path(Path::new(&f.root), &f.path))
            .and_modify(|b| {
                if f.root.len() < b.root.len() {
                    *b = f;
                }
            })
            .or_insert(f);
    }
    let keep: HashSet<(i64, &str)> = best
        .into_values()
        .map(|f| (f.library_id, f.path.as_str()))
        .collect();
    (keep.len() < rows.len()).then(|| {
        files
            .iter()
            .map(|f| keep.contains(&(f.library_id, f.path.as_str())))
            .collect()
    })
}

/// How long a connection waits for another one's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// An existing catalog file, opened with `flags`.
fn connect(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Set the journal mode and the pragmas every writing connection uses.
fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")
}

/// The schema version of the catalog, 0 for an empty database.
fn schema_version(conn: &Connection) -> rusqlite::Result<i32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Accept the catalog at `path` if it has the current schema, run `migrate` if it has an older
/// one, and refuse any other database.
fn check_schema(
    conn: &Connection,
    path: &Path,
    migrate: impl FnOnce() -> rusqlite::Result<()>,
) -> Result<(), Error> {
    match schema_version(conn)? {
        SCHEMA_VERSION => Ok(()),
        older if (1..SCHEMA_VERSION).contains(&older) => Ok(migrate()?),
        version => Err(Error::UnknownSchema {
            path: path.to_path_buf(),
            version,
        }),
    }
}

/// Bring a catalog with an older schema version up to `SCHEMA_VERSION`, in one transaction that
/// holds the write lock, so two processes opening it at once migrate it once.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Read again under the lock: another process may have migrated it meanwhile.
    let version = schema_version(&tx)?;
    let pending = usize::try_from(version - 1)
        .ok()
        .and_then(|done| MIGRATIONS.get(done..))
        .filter(|_| version < SCHEMA_VERSION);
    let Some(pending) = pending else {
        return Ok(());
    };
    for sql in pending {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

/// SQL for a file's date: the first 19 characters of `taken_at` (the camera's local time in both
/// stored formats), or else the mtime in local time.
const DATE_KEY: &str = "coalesce(substr(taken_at, 1, 19), \
     strftime('%Y-%m-%dT%H:%M:%S', mtime_ns / 1000000000, 'unixepoch', 'localtime'))";

/// Number of characters before the names of a folder's children: `dir/`, or none for the root.
/// SQLite counts characters, not bytes, in `substr` on text.
fn dir_prefix_len(dir: &str) -> usize {
    if dir.is_empty() {
        0
    } else {
        dir.chars().count() + 1
    }
}

/// The condition selecting a library's files below the folder `dir`, with its parameters.
/// Everything below `a/b` sorts between `a/b/` and `a/b0`, so this is a range on the primary key.
fn below(library_id: i64, dir: &str, media_type: Option<MediaType>) -> (String, Vec<Value>) {
    let mut cond = String::from("library_id = ?");
    let mut values = vec![Value::from(library_id)];
    if !dir.is_empty() {
        cond.push_str(" AND path >= ? AND path < ?");
        values.extend([
            Value::from(format!("{dir}/")),
            Value::from(format!("{dir}0")),
        ]);
    }
    if let Some(media_type) = media_type {
        cond.push_str(" AND media_type = ?");
        values.push(Value::from(media_type));
    }
    (cond, values)
}

/// The condition selecting a library's files directly inside the folder `dir`.
fn in_folder(library_id: i64, dir: &str, media_type: Option<MediaType>) -> (String, Vec<Value>) {
    let (cond, values) = below(library_id, dir, media_type);
    (
        format!(
            "{} AND instr(substr(path, {}), '/') = 0",
            cond,
            dir_prefix_len(dir) + 1
        ),
        values,
    )
}

/// The file at the `/`-separated relative path `rel` below `root`. Joins part by part, because
/// Windows does not translate `/` in verbatim (`\\?\`) paths like a canonical root.
pub fn library_path(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    path.extend(rel.split('/'));
    path
}

/// A library root as users know it: without the `\\?\` prefix Windows puts on canonical paths.
pub fn display_root(root: &str) -> String {
    if let Some(unc) = root.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = root.strip_prefix(r"\\?\") {
        String::from(local)
    } else {
        String::from(root)
    }
}

/// Build a `DatedFile` from a row selected as `FILE_COLUMNS, library_id, date`.
fn dated_file(row: &Row) -> rusqlite::Result<DatedFile> {
    Ok(DatedFile {
        file: file_record(row)?,
        library_id: row.get(10)?,
        date: row.get(11)?,
    })
}

/// Build a `HashedFile` from a row that starts with `library_id, root, path, size, mtime_ns,
/// sha256`.
fn hashed_file(row: &Row) -> rusqlite::Result<HashedFile> {
    Ok(HashedFile {
        library_id: row.get(0)?,
        root: row.get(1)?,
        path: row.get(2)?,
        size: row.get(3)?,
        mtime_ns: row.get(4)?,
        sha256: row.get(5)?,
    })
}

/// Build a `FileRecord` from a row selected in `FILE_COLUMNS` order.
fn file_record(row: &Row) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        path: row.get(0)?,
        stamp: Stamp {
            size: row.get(1)?,
            mtime_ns: row.get(2)?,
        },
        media_type: row.get(3)?,
        sha256: row.get(4)?,
        meta: MediaMeta {
            taken_at: row.get(5)?,
            camera_make: row.get(6)?,
            camera_model: row.get(7)?,
            width: row.get(8)?,
            height: row.get(9)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    fn record(path: &str, size: u64) -> FileRecord {
        FileRecord {
            path: String::from(path),
            stamp: Stamp { size, mtime_ns: 1 },
            media_type: MediaType::Image,
            sha256: None,
            meta: MediaMeta::default(),
        }
    }

    #[test]
    fn test_fold_nested_moves_rows_and_keeps_hashes() {
        let dir = tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
        let outer = catalog.library_id("/lib").unwrap();
        let inner = catalog.library_id("/lib/sub").unwrap();
        // Its root starts with the same characters, but it is not inside `/lib`.
        let other = catalog.library_id("/library").unwrap();
        let hashed = |path: &str, size: u64, sha256: &str| FileRecord {
            sha256: Some(String::from(sha256)),
            ..record(path, size)
        };
        catalog.upsert(inner, &hashed("a.jpg", 1, "aaa")).unwrap();
        catalog.upsert(inner, &hashed("b.jpg", 1, "bbb")).unwrap();
        catalog
            .set_image_hashes(inner, "b.jpg", "v1", None)
            .unwrap();
        catalog.upsert(inner, &hashed("c.jpg", 2, "ccc")).unwrap();
        // The outer library lists b.jpg too, unhashed but unchanged, and c.jpg with another size.
        catalog.upsert(outer, &record("sub/b.jpg", 1)).unwrap();
        catalog.upsert(outer, &record("sub/c.jpg", 3)).unwrap();
        catalog.upsert(outer, &record("x.jpg", 1)).unwrap();
        catalog.upsert(other, &record("y.jpg", 1)).unwrap();

        let folded = catalog.fold_nested(outer, Path::new("/lib")).unwrap();
        assert_eq!(folded, vec![String::from("/lib/sub")]);
        assert_eq!(
            catalog.files(outer).unwrap(),
            vec![
                hashed("sub/a.jpg", 1, "aaa"),
                hashed("sub/b.jpg", 1, "bbb"),
                record("sub/c.jpg", 3),
                record("x.jpg", 1),
            ]
        );
        let versions: Vec<Option<String>> = catalog
            .hash_versions(outer)
            .unwrap()
            .into_iter()
            .map(|file| file.version)
            .collect();
        assert_eq!(versions, vec![None, Some(String::from("v1")), None, None]);
        let roots: Vec<String> = catalog
            .libraries()
            .unwrap()
            .into_iter()
            .map(|l| l.root)
            .collect();
        assert_eq!(roots, vec!["/lib", "/library"]);
        assert_eq!(
            catalog.fold_nested(outer, Path::new("/lib")).unwrap().len(),
            0
        );
    }

    #[test]
    fn test_containing_library_is_the_outermost() {
        let dir = tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
        let outer = catalog.library_id("/lib").unwrap();
        catalog.library_id("/lib/sub").unwrap();
        catalog.library_id("/library").unwrap();
        let found = |path: &str| catalog.containing_library(Path::new(path)).unwrap();
        let lib = Some(LibraryRoot {
            id: outer,
            root: String::from("/lib"),
        });
        assert_eq!(found("/lib/sub/deeper"), lib);
        assert_eq!(found("/lib"), lib);
        assert_eq!(found("/elsewhere"), None);
    }

    #[test]
    fn test_open_creates_schema_once() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sub/catalog.sqlite");
        let catalog = Catalog::open(&path).unwrap();
        let id = catalog.library_id("/lib").unwrap();
        catalog.upsert(id, &record("a.jpg", 1)).unwrap();
        drop(catalog);

        let catalog = Catalog::open(&path).unwrap();
        let id = catalog.library_id("/lib").unwrap();
        assert_eq!(catalog.files(id).unwrap(), vec![record("a.jpg", 1)]);
    }

    #[test]
    fn test_open_existing_requires_a_catalog() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("catalog.sqlite");
        assert!(matches!(
            Catalog::open_existing(&path),
            Err(Error::NoCatalog(_))
        ));
        assert!(!path.exists());
    }

    #[test]
    fn test_open_existing_refuses_empty_file_without_writing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("catalog.sqlite");
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(
            Catalog::open_existing(&path),
            Err(Error::UnknownSchema { version: 0, .. })
        ));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn test_display_root() {
        assert_eq!(display_root(r"\\?\C:\Photos"), r"C:\Photos");
        assert_eq!(display_root(r"\\?\UNC\nas\share"), r"\\nas\share");
        assert_eq!(display_root("/home/me/Photos"), "/home/me/Photos");
    }

    #[test]
    fn test_rejects_unknown_schema_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("catalog.sqlite");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(matches!(
            Catalog::open(&path),
            Err(Error::UnknownSchema { version: 99, .. })
        ));
    }

    /// A catalog at `path` with schema version 1 and one library holding `files`: path, media
    /// type and `taken_at`.
    fn catalog_v1(path: &Path, files: &[(&str, MediaType, Option<&str>)]) {
        let catalog = Catalog::open(path).unwrap();
        let id = catalog.library_id("/lib").unwrap();
        for &(name, media_type, taken_at) in files {
            let mut file = record(name, 1);
            file.media_type = media_type;
            file.meta.taken_at = taken_at.map(String::from);
            catalog.upsert(id, &file).unwrap();
        }
        catalog.conn.pragma_update(None, "user_version", 1).unwrap();
    }

    #[test]
    fn test_migration_reads_quicktime_unix_dates() {
        use crate::dates::quicktime_creation;
        use time::macros::datetime;

        let dir = tempdir().unwrap();
        let path = dir.path().join("catalog.sqlite");
        let files = [
            (
                "unix.mp4",
                MediaType::Video,
                Some("1948-05-11T18:18:27+00:00"),
            ),
            (
                "late.m4a",
                MediaType::Audio,
                Some("1969-12-31T23:59:59+00:00"),
            ),
            (
                "unset.mov",
                MediaType::Video,
                Some("1904-01-01T00:00:00+00:00"),
            ),
            // Not from a QuickTime header: with a local offset, in an image, or after 1970.
            (
                "keys.mov",
                MediaType::Video,
                Some("1965-03-01T10:00:00+01:00"),
            ),
            (
                "scan.jpg",
                MediaType::Image,
                Some("1948-05-11T18:18:27+00:00"),
            ),
            ("naive.mp4", MediaType::Video, Some("1948-05-11T18:18:27")),
            (
                "fine.mp4",
                MediaType::Video,
                Some("2019-07-14T12:34:56+00:00"),
            ),
            ("none.mp4", MediaType::Video, None),
        ];
        let migrated = [
            ("fine.mp4", Some("2019-07-14T12:34:56+00:00")),
            ("keys.mov", Some("1965-03-01T10:00:00+01:00")),
            ("late.m4a", Some("2036-01-01T23:59:59+00:00")),
            ("naive.mp4", Some("1948-05-11T18:18:27")),
            ("none.mp4", None),
            ("scan.jpg", Some("1948-05-11T18:18:27+00:00")),
            ("unix.mp4", Some("2014-05-12T18:18:27+00:00")),
            ("unset.mov", None),
        ];
        // The same as a scan reads them.
        let unix = datetime!(1948-05-11 18:18:27 UTC).unix_timestamp();
        assert_eq!(
            quicktime_creation(unix),
            Some(datetime!(2014-05-12 18:18:27 UTC).unix_timestamp())
        );
        assert_eq!(
            quicktime_creation(-1),
            Some(datetime!(2036-01-01 23:59:59 UTC).unix_timestamp())
        );

        let taken = |catalog: &Catalog| {
            let id = catalog.find_library("/lib").unwrap().unwrap();
            catalog
                .files(id)
                .unwrap()
                .into_iter()
                .map(|f| (f.path, f.meta.taken_at))
                .collect::<Vec<_>>()
        };
        let expected = migrated
            .iter()
            .map(|&(p, t)| (String::from(p), t.map(String::from)))
            .collect::<Vec<_>>();
        let version = |path: &Path| -> i32 {
            Connection::open(path)
                .unwrap()
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap()
        };

        catalog_v1(&path, &files);
        assert_eq!(taken(&Catalog::open_existing(&path).unwrap()), expected);
        assert_eq!(version(&path), SCHEMA_VERSION);

        let path = dir.path().join("for-update.sqlite");
        catalog_v1(&path, &files);
        assert_eq!(taken(&Catalog::open_for_update(&path).unwrap()), expected);

        let path = dir.path().join("open.sqlite");
        catalog_v1(&path, &files);
        assert_eq!(taken(&Catalog::open(&path).unwrap()), expected);
        // Migrated once: opening it again changes nothing.
        assert_eq!(taken(&Catalog::open(&path).unwrap()), expected);
    }

    #[test]
    fn test_upsert_and_remove() {
        let dir = tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
        let id = catalog.library_id("/lib").unwrap();
        let other = catalog.library_id("/other").unwrap();
        assert_ne!(id, other);

        catalog.upsert(id, &record("a.jpg", 1)).unwrap();
        catalog.upsert(id, &record("a.jpg", 2)).unwrap();
        catalog.upsert(other, &record("a.jpg", 3)).unwrap();
        assert_eq!(catalog.files(id).unwrap(), vec![record("a.jpg", 2)]);
        assert_eq!(catalog.stamps(id).unwrap()["a.jpg"].size, 2);

        catalog.set_sha256(id, "a.jpg", "abc").unwrap();
        assert_eq!(catalog.files(id).unwrap()[0].sha256.as_deref(), Some("abc"));
        // Same size in another library counts as shared.
        catalog.upsert(other, &record("b.jpg", 2)).unwrap();
        catalog.upsert(id, &record("c.jpg", 2)).unwrap();
        assert_eq!(
            catalog.unhashed_with_shared_size(id).unwrap(),
            vec![UnhashedFile {
                path: String::from("c.jpg"),
                size: 2
            }]
        );

        catalog.remove(id, "a.jpg").unwrap();
        catalog.remove(id, "c.jpg").unwrap();
        assert_eq!(catalog.files(id).unwrap(), Vec::<FileRecord>::new());
        assert_eq!(catalog.files(other).unwrap().len(), 2);
    }

    /// 2020-06-15T12:00:00Z, in mid-month so the local-time month is June in every timezone.
    const JUNE_2020_NS: i64 = 1_592_222_400_000_000_000;

    /// A catalog with one library of nested folders, and another library with one file.
    fn browse_fixture() -> (tempfile::TempDir, Catalog, i64, i64) {
        let dir = tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
        let id = catalog.library_id("/lib").unwrap();
        let other = catalog.library_id("/other").unwrap();
        let taken = [
            ("a.jpg", Some("2024-05-01T10:00:00+02:00")),
            ("b.mp4", Some("2023-01-01T00:00:00")),
            ("x.jpg", None),
            ("x0.jpg", None),
            ("x/1.jpg", Some("2024-05-03T09:00:00")),
            ("x/2.jpg", None),
            ("x/y/3.jpg", None),
            ("ø/4.jpg", None),
            ("ø/z/5.jpg", None),
        ];
        for (path, taken_at) in taken {
            let mut file = record(path, 1);
            file.stamp.mtime_ns = JUNE_2020_NS;
            file.meta.taken_at = taken_at.map(String::from);
            if Path::new(path).extension().is_some_and(|e| e == "mp4") {
                file.media_type = MediaType::Video;
            }
            catalog.upsert(id, &file).unwrap();
        }
        let mut file = record("elsewhere.jpg", 1);
        file.stamp.mtime_ns = JUNE_2020_NS;
        catalog.upsert(other, &file).unwrap();
        (dir, catalog, id, other)
    }

    fn paths(files: Vec<FileRecord>) -> Vec<String> {
        files.into_iter().map(|f| f.path).collect()
    }

    fn all() -> Page {
        Page {
            limit: 100,
            offset: 0,
        }
    }

    #[test]
    fn test_folders() {
        let (_dir, catalog, id, _) = browse_fixture();
        let names = |dir| catalog.subfolders(id, dir, None).unwrap();
        assert_eq!(
            names(""),
            vec![(String::from("x"), 3), (String::from("ø"), 2)]
        );
        assert_eq!(names("x"), vec![(String::from("y"), 1)]);
        assert_eq!(names("ø"), vec![(String::from("z"), 1)]);
        assert_eq!(names("ø/z"), vec![]);
        assert_eq!(
            catalog.subfolders(id, "", Some(MediaType::Video)).unwrap(),
            vec![]
        );

        let files = |dir, media_type, page| {
            paths(catalog.folder_files(id, dir, media_type, page).unwrap())
        };
        assert_eq!(
            files("", None, all()),
            ["a.jpg", "b.mp4", "x.jpg", "x0.jpg"]
        );
        assert_eq!(files("x", None, all()), ["x/1.jpg", "x/2.jpg"]);
        assert_eq!(files("ø", None, all()), ["ø/4.jpg"]);
        assert_eq!(files("", Some(MediaType::Video), all()), ["b.mp4"]);
        let page = Page {
            limit: 2,
            offset: 1,
        };
        assert_eq!(files("", None, page), ["b.mp4", "x.jpg"]);

        let neighbors = |path| catalog.folder_neighbors(id, path, None).unwrap();
        assert_eq!(neighbors("x/1.jpg"), (None, Some(String::from("x/2.jpg"))));
        assert_eq!(
            neighbors("b.mp4"),
            (Some(String::from("a.jpg")), Some(String::from("x.jpg")))
        );
    }

    #[test]
    fn test_dates() {
        let (_dir, catalog, id, other) = browse_fixture();
        let mine = Filter {
            libraries: vec![id],
            media_type: None,
        };
        assert_eq!(
            catalog.months(&mine).unwrap(),
            vec![
                (String::from("2024-05"), 2),
                (String::from("2023-01"), 1),
                (String::from("2020-06"), 6),
            ]
        );
        assert_eq!(catalog.months(&Filter::default()).unwrap()[2].1, 7);
        let videos = Filter {
            libraries: vec![],
            media_type: Some(MediaType::Video),
        };
        assert_eq!(
            catalog.months(&videos).unwrap(),
            vec![(String::from("2023-01"), 1)]
        );

        let may: Vec<String> = catalog
            .month_files(&mine, "2024-05", all())
            .unwrap()
            .into_iter()
            .map(|f| format!("{} {}", f.date, f.file.path))
            .collect();
        assert_eq!(
            may,
            ["2024-05-03T09:00:00 x/1.jpg", "2024-05-01T10:00:00 a.jpg"]
        );

        let at = |library_id, path: &str| FileRef {
            library_id,
            path: String::from(path),
        };
        let neighbors = |path| catalog.date_neighbors(&mine, id, path).unwrap();
        assert_eq!(
            neighbors("a.jpg"),
            (Some(at(id, "x/1.jpg")), Some(at(id, "b.mp4")))
        );
        assert_eq!(neighbors("x/1.jpg").0, None);
        // Files with the same date are ordered by library, then path, both descending.
        assert_eq!(
            neighbors("x/2.jpg"),
            (Some(at(id, "x/y/3.jpg")), Some(at(id, "x.jpg")))
        );
        let everything = |path| {
            catalog
                .date_neighbors(&Filter::default(), id, path)
                .unwrap()
        };
        assert_eq!(everything("ø/z/5.jpg").0, Some(at(other, "elsewhere.jpg")));
        assert_eq!(catalog.date(id, "missing.jpg").unwrap(), None);
    }

    #[test]
    fn test_image_hashes_are_reset_when_a_file_changes() {
        let dir = tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
        let id = catalog.library_id("/lib").unwrap();
        catalog.upsert(id, &record("a.jpg", 1)).unwrap();
        catalog.upsert(id, &record("b.jpg", 1)).unwrap();
        let versions = |catalog: &Catalog| {
            catalog
                .hash_versions(id)
                .unwrap()
                .into_iter()
                .map(|f| format!("{} {}", f.path, f.version.unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        assert_eq!(versions(&catalog), ["a.jpg ", "b.jpg "]);

        let hashes = ImageHashes {
            pixels: String::from("ppp"),
            visual: Some(Visual {
                phash: u64::MAX,
                fingerprint: vec![7; 4],
            }),
            dimensions: Dimensions {
                width: 30,
                height: 20,
            },
        };
        catalog
            .set_image_hashes(id, "a.jpg", "v1", Some(&hashes))
            .unwrap();
        catalog.set_image_hashes(id, "b.jpg", "v1", None).unwrap();
        assert_eq!(versions(&catalog), ["a.jpg v1", "b.jpg v1"]);
        let images = catalog.hashed_images(true).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].pixel_hash, "ppp");
        assert_eq!(images[0].visual, hashes.visual);
        assert_eq!(images[0].dimensions, Some(hashes.dimensions));
        assert_eq!(catalog.count_unhashed(&[]).unwrap(), 0);

        catalog.upsert(id, &record("a.jpg", 2)).unwrap();
        assert_eq!(versions(&catalog), ["a.jpg ", "b.jpg v1"]);
        assert_eq!(
            catalog.hashed_images(true).unwrap(),
            Vec::<HashedImage>::new()
        );
        assert_eq!(catalog.count_unhashed(&[id]).unwrap(), 1);
    }

    #[test]
    fn test_libraries_and_copies() {
        let (_dir, catalog, id, other) = browse_fixture();
        let libraries = catalog.libraries().unwrap();
        assert_eq!(
            libraries
                .iter()
                .map(|l| (l.id, l.files))
                .collect::<Vec<_>>(),
            vec![(id, 9), (other, 1)]
        );
        catalog.set_sha256(id, "a.jpg", "abc").unwrap();
        catalog.set_sha256(other, "elsewhere.jpg", "abc").unwrap();
        let copies = catalog.copies(1, "abc").unwrap();
        assert_eq!(copies.len(), 2);
        assert_eq!(catalog.copies(2, "abc").unwrap(), Vec::<HashedFile>::new());
        assert_eq!(
            catalog.file(id, "x/y/3.jpg").unwrap().unwrap().path,
            "x/y/3.jpg"
        );
        assert_eq!(catalog.file(other, "x/y/3.jpg").unwrap(), None);
        assert_eq!(
            catalog.library_root(other).unwrap().as_deref(),
            Some("/other")
        );
        assert_eq!(catalog.library_root(other + 1).unwrap(), None);
    }

    /// A symbolic link stands in for another spelling of the same folder, which only a
    /// case-insensitive file system can give.
    #[cfg(unix)]
    #[test]
    fn test_one_folder_under_two_names_is_one_library() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(lib.join("sub")).unwrap();
        let link = dir.path().join("link");
        symlink(&lib, &link).unwrap();

        assert_eq!(path_below(&lib, &link), Some(String::new()));
        assert_eq!(
            path_below(&lib, &link.join("sub")),
            Some(String::from("sub/"))
        );
        assert_eq!(
            path_below(&lib, &lib.join("sub/x")),
            Some(String::from("sub/x/"))
        );
        assert_eq!(path_below(&lib, dir.path()), None);
        // Found by identity, the parts below are spelled as on disk.
        assert_eq!(
            path_below(&lib, &link.join("SUB")),
            Some(String::from("sub/"))
        );
        assert_eq!(
            path_below(&link, &lib.join("sub")),
            Some(String::from("sub/"))
        );
        assert_eq!(path_below(Path::new("/a"), Path::new("/ab")), None);

        let libraries = vec![Library {
            id: 1,
            root: lib.to_str().unwrap().to_string(),
            scanned_at: None,
            files: 0,
        }];
        assert_eq!(
            cataloged_spelling(&libraries, &link.join("sub")),
            lib.join("sub")
        );
        assert_eq!(cataloged_spelling(&libraries, &link), lib);
        assert_eq!(
            cataloged_spelling(&libraries, &link.join("SUB")),
            lib.join("sub")
        );
        assert_eq!(
            cataloged_spelling(&libraries, &lib.join("x")),
            lib.join("x")
        );
        assert_eq!(cataloged_spelling(&libraries, dir.path()), dir.path());

        // A library cataloged under the other name is folded in, with its hashes.
        let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
        let a = catalog.library_id(lib.to_str().unwrap()).unwrap();
        let b = catalog.library_id(link.to_str().unwrap()).unwrap();
        catalog.upsert(a, &record("sub/x.jpg", 1)).unwrap();
        catalog
            .upsert(
                b,
                &FileRecord {
                    sha256: Some(String::from("xxx")),
                    ..record("sub/x.jpg", 1)
                },
            )
            .unwrap();
        let folded = catalog.fold_nested(a, &lib).unwrap();
        assert_eq!(folded, vec![link.to_str().unwrap().to_string()]);
        assert_eq!(catalog.files(a).unwrap()[0].sha256.as_deref(), Some("xxx"));
        assert_eq!(catalog.libraries().unwrap().len(), 1);
    }

    #[test]
    fn test_one_row_per_file_keeps_the_outer_row_each_time_it_is_given() {
        let file = |library_id: i64, root: &str, path: &str| HashedFile {
            library_id,
            root: String::from(root),
            path: String::from(path),
            ..HashedFile::default()
        };
        let outer = file(1, "/lib", "sub/a.jpg");
        let inner = file(2, "/lib/sub", "a.jpg");
        let other = file(1, "/lib", "b.jpg");
        assert_eq!(one_row_per_file([&outer, &other]), None);
        // The outer row twice, as when a file is both an exact copy and a decoded image.
        assert_eq!(
            one_row_per_file([&inner, &outer, &other, &outer, &inner]),
            Some(vec![false, true, true, true, false])
        );
    }
}
