//! The SQLite catalog of scanned media files.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::avhash::{
    frames_from_bytes, frames_to_bytes, values_from_bytes, values_to_bytes, AudioFingerprint,
    Frame, VideoSignature,
};
use crate::error::Error;
use crate::imagehash::ImageHashes;
use crate::media::MediaType;
use directories::ProjectDirs;
use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, Row, Transaction,
};
use std::collections::{HashMap, HashSet};
use std::fs::create_dir_all;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Schema version, stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i32 = 1;

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
    /// Camera make and model, without the make twice when the model starts with it.
    pub fn camera(&self) -> Option<String> {
        match (&self.camera_make, &self.camera_model) {
            (Some(make), Some(model)) if model.starts_with(make.as_str()) => Some(model.clone()),
            (Some(make), Some(model)) => Some(format!("{} {}", make, model)),
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
    pub sha256: String,
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
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => conn.execute_batch(&format!(
                "BEGIN; {} PRAGMA user_version = {}; COMMIT;",
                SCHEMA, SCHEMA_VERSION
            ))?,
            SCHEMA_VERSION => {}
            v => {
                return Err(Error::Usage(format!(
                    "{} has catalog schema version {}, but this version of pixrs only \
                     understands version {}",
                    path.display(),
                    v,
                    SCHEMA_VERSION
                )))
            }
        }
        Ok(Catalog { conn })
    }

    /// Open the catalog at `path` read-only. It must exist and have the current schema.
    pub fn open_existing(path: &Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Err(Error::Usage(format!(
                "There is no catalog at {}. Run `pixrs scan <LIBRARY>` first.",
                path.display()
            )));
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(Error::Usage(format!(
                "{} is not a catalog this version of pixrs understands (schema version {}, \
                 expected {})",
                path.display(),
                version,
                SCHEMA_VERSION
            )));
        }
        Ok(Catalog { conn })
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

    /// ID and root of the outermost library whose root is `path` or contains it, if any.
    pub fn containing_library(&self, path: &Path) -> rusqlite::Result<Option<(i64, String)>> {
        let mut stmt = self.conn.prepare("SELECT id, root FROM libraries")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut outermost: Option<(i64, String)> = None;
        for row in rows {
            let (id, root): (i64, String) = row?;
            let shorter = outermost.as_ref().is_none_or(|(_, o)| root.len() < o.len());
            if path.starts_with(&root) && shorter {
                outermost = Some((id, root));
            }
        }
        Ok(outermost)
    }

    /// Fold every other library inside `root`, the root of `library_id`, into that library:
    /// its rows move there with the path from `root` to its root in front, keeping their hashes,
    /// and the library is dropped. Where both have a row for a file with the same size and mtime,
    /// hashes the outer row lacks are taken from the inner one; otherwise the outer row stays.
    /// Returns the roots of the folded libraries.
    pub fn fold_nested(&self, library_id: i64, root: &Path) -> rusqlite::Result<Vec<String>> {
        let mut inner: Vec<(i64, String, String)> = vec![];
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, root FROM libraries WHERE id != ?1 ORDER BY root")?;
            let rows = stmt.query_map([library_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            for row in rows {
                let (id, inner_root): (i64, String) = row?;
                if let Some(prefix) = rel_root(root, Path::new(&inner_root)) {
                    inner.push((id, inner_root, prefix));
                }
            }
        }
        if inner.is_empty() {
            return Ok(vec![]);
        }
        let hash_columns = HASH_COLUMNS
            .iter()
            .map(|c| {
                format!(
                    "{c} = CASE WHEN o.hash_version IS NULL THEN i.{c} ELSE o.{c} END",
                    c = c
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let tx = self.conn.unchecked_transaction()?;
        for (id, _, prefix) in &inner {
            self.conn.execute(
                &format!(
                    "UPDATE files AS o SET sha256 = coalesce(o.sha256, i.sha256), {}
                     FROM files AS i
                     WHERE o.library_id = ?1 AND i.library_id = ?2 AND o.path = ?3 || i.path
                       AND o.size = i.size AND o.mtime_ns = i.mtime_ns",
                    hash_columns
                ),
                params![library_id, id, prefix],
            )?;
            self.conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO files (library_id, path, {cols})
                     SELECT ?1, ?3 || path, {cols} FROM files WHERE library_id = ?2",
                    cols = MOVED_COLUMNS
                ),
                params![library_id, id, prefix],
            )?;
            self.conn
                .execute("DELETE FROM files WHERE library_id = ?1", [id])?;
            self.conn
                .execute("DELETE FROM libraries WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(inner.into_iter().map(|(_, root, _)| root).collect())
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
                    size: row.get::<_, i64>(1)? as u64,
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
                file.stamp.size as i64,
                file.stamp.mtime_ns,
                file.media_type.as_str(),
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
    ) -> rusqlite::Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size FROM files
             WHERE library_id = ?1 AND sha256 IS NULL
               AND size IN (SELECT size FROM files GROUP BY size HAVING count(*) > 1)
             ORDER BY path",
        )?;
        let rows = stmt.query_map([library_id], |row| {
            Ok((row.get(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        rows.collect()
    }

    /// Number of files in the given libraries (all if empty) that share their size with another
    /// file but have no hash yet, because their library has not been scanned since the size
    /// became shared.
    pub fn count_unhashed_with_shared_size(&self, libraries: &[i64]) -> rusqlite::Result<usize> {
        let in_libraries = if libraries.is_empty() {
            String::new()
        } else {
            format!(
                "AND library_id IN ({})",
                vec!["?"; libraries.len()].join(", ")
            )
        };
        self.conn.query_row(
            &format!(
                "SELECT count(*) FROM files
                 WHERE sha256 IS NULL
                   AND size IN (SELECT size FROM files GROUP BY size HAVING count(*) > 1)
                   {}",
                in_libraries
            ),
            params_from_iter(libraries),
            |row| row.get::<_, i64>(0).map(|n| n as usize),
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
        let rows = stmt.query_map([], |row| {
            Ok(HashedFile {
                library_id: row.get(0)?,
                root: row.get(1)?,
                path: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                mtime_ns: row.get(4)?,
                sha256: row.get(5)?,
            })
        })?;
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
            "SELECT {} FROM files WHERE library_id = ?1 ORDER BY path",
            FILE_COLUMNS
        ))?;
        let rows = stmt.query_map([library_id], file_record)?;
        rows.collect()
    }

    /// Every media file in the library with its type and the versions the hash pass decoded
    /// it with, if it has, ordered by path.
    #[allow(clippy::type_complexity)]
    pub fn hash_versions(
        &self,
        library_id: i64,
    ) -> rusqlite::Result<Vec<(String, MediaType, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, media_type, hash_version FROM files WHERE library_id = ?1
             ORDER BY path",
        )?;
        let rows = stmt.query_map([library_id], |row| {
            let media_type: String = row.get(1)?;
            let media_type = media_type.parse().map_err(|e: String| {
                rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, e.into())
            })?;
            Ok((row.get(0)?, media_type, row.get(2)?))
        })?;
        rows.collect()
    }

    /// Number of media files in the given libraries (all if empty) the hash pass has not
    /// decoded: not scanned since they changed, or no decoder for them (RAW, HEIC without
    /// `ffmpeg`, video without `ffmpeg`, audio without `fpcalc`).
    pub fn count_unhashed(&self, libraries: &[i64]) -> rusqlite::Result<usize> {
        let filter = Filter {
            libraries: libraries.to_vec(),
            media_type: None,
        };
        let (cond, values) = filter.condition();
        self.conn.query_row(
            &format!(
                "SELECT count(*) FROM files WHERE {} AND hash_version IS NULL",
                cond
            ),
            params_from_iter(values),
            |row| row.get::<_, i64>(0).map(|n| n as usize),
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
    pub fn hashed_av(&self) -> rusqlite::Result<Vec<HashedAv>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.library_id, l.root, f.path, f.size, f.mtime_ns, f.sha256, f.duration_ms,
                    coalesce(f.width, 0), coalesce(f.height, 0), f.frames, f.chromaprint
             FROM files f JOIN libraries l ON l.id = f.library_id
             WHERE f.duration_ms IS NOT NULL
               AND (f.frames IS NOT NULL OR f.chromaprint IS NOT NULL)",
        )?;
        let rows = stmt.query_map([], |row| {
            let frames: Option<Vec<u8>> = row.get(9)?;
            let chromaprint: Option<Vec<u8>> = row.get(10)?;
            Ok(HashedAv {
                file: HashedFile {
                    library_id: row.get(0)?,
                    root: row.get(1)?,
                    path: row.get(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    mtime_ns: row.get(4)?,
                    sha256: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                },
                duration_ms: row.get(6)?,
                width: row.get(7)?,
                height: row.get(8)?,
                frames: frames.as_deref().and_then(frames_from_bytes),
                chromaprint: chromaprint.as_deref().map(values_from_bytes),
            })
        })?;
        rows.collect()
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
                hashes.and_then(|h| h.perceptual).map(|p| p as i64),
                hashes.and_then(|h| h.fingerprint.as_deref()),
                hashes.map(|h| h.width),
                hashes.map(|h| h.height),
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
                    coalesce(f.width, 0), coalesce(f.height, 0), {}
             FROM files f JOIN libraries l ON l.id = f.library_id
             WHERE f.pixel_hash IS NOT NULL",
            visual_columns
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(HashedImage {
                file: HashedFile {
                    library_id: row.get(0)?,
                    root: row.get(1)?,
                    path: row.get(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    mtime_ns: row.get(4)?,
                    sha256: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                },
                pixel_hash: row.get(6)?,
                width: row.get(7)?,
                height: row.get(8)?,
                phash: row.get::<_, Option<i64>>(9)?.map(|p| p as u64),
                fingerprint: row.get(10)?,
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

    /// Move a file's row to a new path, possibly in another library, if there is a row at the
    /// old path. A row at the new path is dropped first with `replace` (when the file there is
    /// known to be gone); without it, the row stays where it is. Returns whether it moved.
    pub fn rename_row(
        &self,
        library_id: i64,
        path: &str,
        new_library_id: i64,
        new_path: &str,
        replace: bool,
    ) -> rusqlite::Result<bool> {
        if (library_id, path) == (new_library_id, new_path) {
            return Ok(false);
        }
        let tx = self.conn.unchecked_transaction()?;
        if !self.has_row(library_id, path)? {
            return Ok(false);
        }
        if self.has_row(new_library_id, new_path)? {
            if !replace {
                return Ok(false);
            }
            self.remove(new_library_id, new_path)?;
        }
        self.conn.execute(
            "UPDATE files SET library_id = ?3, path = ?4 WHERE library_id = ?1 AND path = ?2",
            params![library_id, path, new_library_id, new_path],
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
                    "SELECT {}, hash_version, pixel_hash, phash, fingerprint, duration_ms, frames,
                            chromaprint
                     FROM files WHERE library_id = ?1 AND path = ?2",
                    FILE_COLUMNS
                ),
                params![library_id, path],
                |row| {
                    Ok(CatalogRow {
                        library_id,
                        file: file_record(row)?,
                        hash_version: row.get(10)?,
                        pixel_hash: row.get(11)?,
                        phash: row.get(12)?,
                        fingerprint: hex(row.get(13)?),
                        duration_ms: row.get(14)?,
                        frames: hex(row.get(15)?),
                        chromaprint: hex(row.get(16)?),
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
                row.phash,
                unhex(&row.fingerprint),
                row.duration_ms,
                unhex(&row.frames),
                unhex(&row.chromaprint),
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
                files: row.get::<_, i64>(3)? as usize,
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
                &format!(
                    "SELECT {} FROM files WHERE library_id = ?1 AND path = ?2",
                    FILE_COLUMNS
                ),
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
        let (cond, mut values) = below(library_id, dir, media_type);
        values.insert(0, Value::from(dir_prefix_len(dir) + 1));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT substr(rest, 1, instr(rest, '/') - 1) AS name, count(*)
             FROM (SELECT substr(path, ?) AS rest FROM files WHERE {})
             WHERE instr(rest, '/') > 0
             GROUP BY name ORDER BY name",
            cond
        ))?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get::<_, i64>(1)? as usize))
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
        let (cond, mut values) = in_folder(library_id, dir, media_type);
        values.extend([Value::from(page.limit), Value::from(page.offset)]);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM files WHERE {} ORDER BY path LIMIT ? OFFSET ?",
            FILE_COLUMNS, cond
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
                        "SELECT path FROM files WHERE {} AND path {} ? ORDER BY path {} LIMIT 1",
                        cond, op, order
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
            "SELECT substr({}, 1, 7) AS month, count(*) FROM files WHERE {}
             GROUP BY month ORDER BY month DESC",
            DATE_KEY, cond
        ))?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get::<_, i64>(1)? as usize))
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
        values.extend([
            Value::from(String::from(month)),
            Value::from(page.limit),
            Value::from(page.offset),
        ]);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}, library_id, {} AS date FROM files
             WHERE {} AND substr(date, 1, 7) = ?
             ORDER BY date DESC, library_id DESC, path DESC LIMIT ? OFFSET ?",
            FILE_COLUMNS, DATE_KEY, cond
        ))?;
        let rows = stmt.query_map(params_from_iter(values), dated_file)?;
        rows.collect()
    }

    /// When a file was taken, or its mtime in local time, as `YYYY-MM-DDTHH:MM:SS`.
    pub fn date(&self, library_id: i64, path: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {} FROM files WHERE library_id = ?1 AND path = ?2",
                    DATE_KEY
                ),
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
                         WHERE {} AND ({}, library_id, path) {} (?, ?, ?)
                         ORDER BY {} {o}, library_id {o}, path {o} LIMIT 1",
                        cond,
                        DATE_KEY,
                        op,
                        DATE_KEY,
                        o = order
                    ),
                    params_from_iter(values),
                    |row| Ok((row.get(0)?, row.get(1)?)),
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
        let rows = stmt.query_map(params![size as i64, sha256], |row| {
            Ok(HashedFile {
                library_id: row.get(0)?,
                root: row.get(1)?,
                path: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                mtime_ns: row.get(4)?,
                sha256: row.get(5)?,
            })
        })?;
        let mut copies: Vec<HashedFile> = rows.collect::<rusqlite::Result<_>>()?;
        if let Some(keep) = one_row_per_file(&copies) {
            copies.retain(|f| keep.contains(&(f.library_id, f.path.clone())));
        }
        Ok(copies)
    }
}

/// An image with its pixel and perceptual hashes. `file.sha256` is empty if the file has no
/// content hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedImage {
    pub file: HashedFile,
    pub pixel_hash: String,
    /// `None` for images with almost no detail.
    pub phash: Option<u64>,
    /// Size of the upright image; 0 if unknown.
    pub width: u32,
    pub height: u32,
    /// `None` when `phash` is.
    pub fingerprint: Option<Vec<u8>>,
}

/// Everything in a file's catalog row, kept in a journal while the file is outside its library.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogRow {
    pub library_id: i64,
    pub file: FileRecord,
    pub hash_version: Option<String>,
    pub pixel_hash: Option<String>,
    pub phash: Option<i64>,
    /// BLOBs as lowercase hex.
    pub fingerprint: Option<String>,
    pub duration_ms: Option<i64>,
    pub frames: Option<String>,
    pub chromaprint: Option<String>,
}

/// A BLOB as lowercase hex, for `CatalogRow`.
fn hex(blob: Option<Vec<u8>>) -> Option<String> {
    blob.map(|b| data_encoding::HEXLOWER.encode(&b))
}

/// Lowercase hex from `hex` as a BLOB.
fn unhex(text: &Option<String>) -> Option<Vec<u8>> {
    text.as_deref()
        .and_then(|t| data_encoding::HEXLOWER.decode(t.as_bytes()).ok())
}

/// A video or audio file with its signature or fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedAv {
    pub file: HashedFile,
    pub duration_ms: i64,
    /// Size of the video; 0 if unknown or audio.
    pub width: u32,
    pub height: u32,
    /// For videos.
    pub frames: Option<Vec<Frame>>,
    /// For audio.
    pub chromaprint: Option<Vec<u32>>,
}

/// A library ID and a path in that library.
pub type FileRef = (i64, String);

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
        let mut cond = String::from("1");
        let mut values: Vec<Value> = vec![];
        if !self.libraries.is_empty() {
            cond.push_str(&format!(
                " AND library_id IN ({})",
                vec!["?"; self.libraries.len()].join(", ")
            ));
            values.extend(self.libraries.iter().map(|&id| Value::from(id)));
        }
        if let Some(media_type) = self.media_type {
            cond.push_str(" AND media_type = ?");
            values.push(Value::from(String::from(media_type.as_str())));
        }
        (cond, values)
    }
}

/// A slice of a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub limit: i64,
    pub offset: i64,
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

/// The path from `outer` to `inner` with `/` separators and a trailing `/`, if `inner` is
/// strictly inside `outer`.
fn rel_root(outer: &Path, inner: &Path) -> Option<String> {
    let rel = inner.strip_prefix(outer).ok()?;
    let mut prefix = String::new();
    for c in rel.components() {
        match c {
            Component::Normal(part) => {
                prefix.push_str(part.to_str()?);
                prefix.push('/');
            }
            _ => return None,
        }
    }
    (!prefix.is_empty()).then_some(prefix)
}

/// The rows to keep so that each file on disk has one, or `None` if it already has.
/// Overlapping libraries, which catalogs scanned before pixrs folded nested libraries can have,
/// list a file once in each; the row from the outermost library is kept, so a file is never its
/// own duplicate.
pub fn one_row_per_file<'a>(
    files: impl IntoIterator<Item = &'a HashedFile>,
) -> Option<HashSet<(i64, String)>> {
    let mut best: HashMap<PathBuf, &HashedFile> = HashMap::new();
    let mut rows: HashSet<(i64, &str)> = HashSet::new();
    for f in files {
        rows.insert((f.library_id, &f.path));
        best.entry(library_path(Path::new(&f.root), &f.path))
            .and_modify(|b| {
                if f.root.len() < b.root.len() {
                    *b = f;
                }
            })
            .or_insert(f);
    }
    let keep: HashSet<(i64, String)> = best
        .into_values()
        .map(|f| (f.library_id, f.path.clone()))
        .collect();
    (keep.len() < rows.len()).then_some(keep)
}

/// SQL for a file's date: the first 19 characters of `taken_at` (the camera's local time in both
/// stored formats), or else the mtime in local time.
const DATE_KEY: &str = "coalesce(substr(taken_at, 1, 19), \
     strftime('%Y-%m-%dT%H:%M:%S', mtime_ns / 1000000000, 'unixepoch', 'localtime'))";

/// Number of characters before the names of a folder's children: `dir/`, or none for the root.
/// SQLite counts characters, not bytes, in `substr` on text.
fn dir_prefix_len(dir: &str) -> i64 {
    if dir.is_empty() {
        0
    } else {
        dir.chars().count() as i64 + 1
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
            Value::from(format!("{}/", dir)),
            Value::from(format!("{}0", dir)),
        ]);
    }
    if let Some(media_type) = media_type {
        cond.push_str(" AND media_type = ?");
        values.push(Value::from(String::from(media_type.as_str())));
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
        format!(r"\\{}", unc)
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

/// Build a `FileRecord` from a row selected in `FILE_COLUMNS` order.
fn file_record(row: &Row) -> rusqlite::Result<FileRecord> {
    let media_type: String = row.get(3)?;
    Ok(FileRecord {
        path: row.get(0)?,
        stamp: Stamp {
            size: row.get::<_, i64>(1)? as u64,
            mtime_ns: row.get(2)?,
        },
        media_type: media_type.parse().map_err(|e: String| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into())
        })?,
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
            .map(|(_, _, version)| version)
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
        assert_eq!(
            found("/lib/sub/deeper"),
            Some((outer, String::from("/lib")))
        );
        assert_eq!(found("/lib"), Some((outer, String::from("/lib"))));
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
            Err(Error::Usage(_))
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
            Err(Error::Usage(_))
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
        assert!(matches!(Catalog::open(&path), Err(Error::Usage(_))));
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
            vec![(String::from("c.jpg"), 2)]
        );

        catalog.remove(id, "a.jpg").unwrap();
        catalog.remove(id, "c.jpg").unwrap();
        assert!(catalog.files(id).unwrap().is_empty());
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
            if path.ends_with(".mp4") {
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

        let neighbors = |path| catalog.date_neighbors(&mine, id, path).unwrap();
        assert_eq!(
            neighbors("a.jpg"),
            (
                Some((id, String::from("x/1.jpg"))),
                Some((id, String::from("b.mp4")))
            )
        );
        assert_eq!(neighbors("x/1.jpg").0, None);
        // Files with the same date are ordered by library, then path, both descending.
        assert_eq!(
            neighbors("x/2.jpg"),
            (
                Some((id, String::from("x/y/3.jpg"))),
                Some((id, String::from("x.jpg")))
            )
        );
        let everything = |path| {
            catalog
                .date_neighbors(&Filter::default(), id, path)
                .unwrap()
        };
        assert_eq!(
            everything("ø/z/5.jpg").0,
            Some((other, String::from("elsewhere.jpg")))
        );
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
                .map(|(path, _, v)| format!("{} {}", path, v.unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        assert_eq!(versions(&catalog), ["a.jpg ", "b.jpg "]);

        let hashes = ImageHashes {
            pixels: String::from("ppp"),
            perceptual: Some(u64::MAX),
            fingerprint: Some(vec![7; 4]),
            width: 30,
            height: 20,
        };
        catalog
            .set_image_hashes(id, "a.jpg", "v1", Some(&hashes))
            .unwrap();
        catalog.set_image_hashes(id, "b.jpg", "v1", None).unwrap();
        assert_eq!(versions(&catalog), ["a.jpg v1", "b.jpg v1"]);
        let images = catalog.hashed_images(true).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(
            (
                images[0].pixel_hash.as_str(),
                images[0].phash,
                images[0].width,
                images[0].height
            ),
            ("ppp", Some(u64::MAX), 30, 20)
        );
        assert_eq!(catalog.count_unhashed(&[]).unwrap(), 0);

        catalog.upsert(id, &record("a.jpg", 2)).unwrap();
        assert_eq!(versions(&catalog), ["a.jpg ", "b.jpg v1"]);
        assert!(catalog.hashed_images(true).unwrap().is_empty());
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
        assert!(catalog.copies(2, "abc").unwrap().is_empty());
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
}
