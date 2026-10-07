//! The SQLite catalog of scanned media files.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::error::Error;
use crate::media::MediaType;
use directories::ProjectDirs;
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, Row, Transaction,
};
use std::collections::HashMap;
use std::fs::create_dir_all;
use std::path::{Path, PathBuf};
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
    PRIMARY KEY (library_id, path)
);
CREATE INDEX files_size ON files (size);
CREATE INDEX files_sha256 ON files (sha256);
";

/// What a scan compares to decide whether a file changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
}

/// Metadata read from inside a media file. Every field is optional.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaMeta {
    /// When the photo or video was taken: RFC 3339 if the file records a timezone,
    /// otherwise `YYYY-MM-DDTHH:MM:SS` in the camera's local time.
    pub taken_at: Option<String>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// One media file in the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// Insert or replace a file.
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
                height = excluded.height",
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
        let mut stmt = self.conn.prepare(
            "SELECT path, size, mtime_ns, media_type, sha256, taken_at, camera_make,
                    camera_model, width, height
             FROM files WHERE library_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([library_id], file_record)?;
        rows.collect()
    }
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

/// Build a `FileRecord` from a row selected in `Catalog::files` column order.
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
}
