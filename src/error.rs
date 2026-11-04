//! Errors for PixRS.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::apply::{JournalError, MoveFault};
use crate::catalog::{display_root, SCHEMA_VERSION};
use crate::plan::PlanError;
use crate::template::TemplateError;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The exit code for a process stopped by Ctrl-C (128 + SIGINT), as shells report it.
pub const INTERRUPTED: i32 = 130;

/// Errors that end a command.
#[derive(Debug)]
pub enum Error {
    /// There is no data directory to put the default catalog in.
    NoDataDir,
    /// A folder argument is not a folder.
    NotADirectory(PathBuf),
    /// A path pixrs must store is not valid UTF-8; `what` names it, like "Library".
    NotUtf8 { what: &'static str, path: PathBuf },
    /// The catalog would be inside the library it catalogs.
    CatalogInLibrary { catalog: PathBuf, library: PathBuf },
    /// There is no catalog at the path given.
    NoCatalog(PathBuf),
    /// A library argument was never scanned.
    NotScanned(PathBuf),
    /// A library argument is a folder inside the library at `root`.
    PartOfLibrary { path: PathBuf, root: String },
    /// `relocate` was given a folder that is no library's root.
    UnknownLibrary { path: PathBuf, roots: Vec<String> },
    /// `relocate` was given the folder the library is at already.
    AlreadyThere { root: String },
    /// Too few of the sampled files are at the folder `relocate` was given.
    TooFewMatch {
        matching: usize,
        compared: usize,
        folder: PathBuf,
    },
    /// A scan found no media where the catalog lists `cataloged` files.
    NoMedia { folder: PathBuf, cataloged: usize },
    /// The thumbnail cache would be inside a library.
    CacheInLibrary { cache: PathBuf, library: String },
    /// The `--template` or `--layout` given is not valid.
    Template(TemplateError),
    /// A plan cannot be made, read or carried out.
    Plan(PlanError),
    /// A journal cannot be read.
    Journal(JournalError),
    /// The catalog a plan or journal names is not there.
    CatalogGone(PathBuf),
    /// The catalog file has a schema this version does not understand.
    UnknownSchema { path: PathBuf, version: i32 },
    /// Another `apply`, `undo` or `relocate` holds the lock file at this path.
    Locked(PathBuf),
    /// A file operation failed.
    Io(io::Error),
    /// A file operation on `path` failed.
    File { path: PathBuf, source: io::Error },
    /// A scan could not list the folder it was given, which has `cataloged` files.
    Unlistable { folder: PathBuf, cataloged: usize },
    /// The library went away during a scan.
    Disappeared(PathBuf),
    /// The viewer cannot listen on its port.
    Listen { port: u16, source: io::Error },
    /// A move failed halfway; only `undo` should touch the file now.
    Move(MoveFault),
    /// The catalog database failed.
    Catalog(rusqlite::Error),
    /// The command was stopped before it finished.
    Interrupted,
}

impl Error {
    /// The process exit code for this error, after the BSD `sysexits.h` conventions.
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::NoDataDir
            | Self::NotADirectory(_)
            | Self::NotUtf8 { .. }
            | Self::CatalogInLibrary { .. }
            | Self::NoCatalog(_)
            | Self::NotScanned(_)
            | Self::PartOfLibrary { .. }
            | Self::UnknownLibrary { .. }
            | Self::AlreadyThere { .. }
            | Self::TooFewMatch { .. }
            | Self::NoMedia { .. }
            | Self::CacheInLibrary { .. }
            | Self::Template(_) => exitcode::USAGE,
            Self::Plan(e) => e.exit_code(),
            Self::Journal(_) | Self::CatalogGone(_) | Self::UnknownSchema { .. } => {
                exitcode::DATAERR
            }
            Self::Locked(_) => exitcode::TEMPFAIL,
            Self::Io(_)
            | Self::File { .. }
            | Self::Unlistable { .. }
            | Self::Disappeared(_)
            | Self::Listen { .. }
            | Self::Move(_) => exitcode::IOERR,
            Self::Catalog(_) => exitcode::SOFTWARE,
            Self::Interrupted => INTERRUPTED,
        }
    }
}

impl fmt::Display for Error {
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per variant keeps every message of the program in one place"
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataDir => {
                write!(
                    f,
                    "Unable to find a data directory for the catalog; pass --catalog"
                )
            }
            Self::NotADirectory(path) => write!(f, "{} is not a directory", path.display()),
            Self::NotUtf8 { what, path } => {
                write!(f, "{} {} is not valid UTF-8", what, path.display())
            }
            Self::CatalogInLibrary { catalog, library } => write!(
                f,
                "The catalog {} must be outside the library {}",
                catalog.display(),
                library.display()
            ),
            Self::NoCatalog(path) => write!(
                f,
                "There is no catalog at {}. Run `pixrs scan <LIBRARY>` first.",
                path.display()
            ),
            Self::NotScanned(path) => write!(
                f,
                "{} has not been scanned. Run `pixrs scan {}` first.",
                path.display(),
                path.display()
            ),
            Self::PartOfLibrary { path, root } => write!(
                f,
                "{} is part of the library {}. Pass the library instead.",
                path.display(),
                display_root(root)
            ),
            Self::UnknownLibrary { path, roots } => {
                write!(
                    f,
                    "{} is not a library in the catalog. The libraries are:",
                    path.display()
                )?;
                for root in roots {
                    write!(f, "\n  {}", display_root(root))?;
                }
                Ok(())
            }
            Self::AlreadyThere { root } => {
                write!(f, "The library is already at {}", display_root(root))
            }
            Self::TooFewMatch {
                matching,
                compared,
                folder,
            } => write!(
                f,
                "Only {} of {} cataloged files compared are in {} with the same size and \
                 modification time. Is it the right folder? With --force the library is moved \
                 anyway, and the next scan reads the files that differ again.",
                matching,
                compared,
                folder.display()
            ),
            Self::NoMedia { folder, cataloged } => write!(
                f,
                "Found no media files in {}, but the catalog lists {} for it. If it is on a \
                 drive, is the drive mounted? Nothing was removed from the catalog. If the \
                 folder really is empty now, scan it with --allow-empty to remove them.",
                folder.display(),
                cataloged
            ),
            Self::CacheInLibrary { cache, library } => write!(
                f,
                "The thumbnail cache {} must be outside the library {}; pass --cache",
                cache.display(),
                display_root(library)
            ),
            Self::Template(e) => write!(f, "{e}"),
            Self::Plan(e) => write!(f, "{e}"),
            Self::Journal(e) => write!(f, "{e}"),
            Self::CatalogGone(path) => write!(f, "There is no catalog at {}", path.display()),
            Self::UnknownSchema { path, version } => write!(
                f,
                "{} is not a catalog this version of pixrs understands (schema version {}, \
                 expected {})",
                path.display(),
                version,
                SCHEMA_VERSION
            ),
            Self::Locked(path) => write!(
                f,
                "{} exists: another `pixrs apply`, `undo` or `relocate` is running on this \
                 catalog. If none is (it crashed), delete the file and try again.",
                path.display()
            ),
            Self::Io(e) => write!(f, "{e}"),
            Self::File { path, source } => write!(f, "{}: {}", path.display(), source),
            Self::Unlistable { folder, cataloged } => write!(
                f,
                "Cannot list the files in {}, which the catalog lists {} files for. Nothing was \
                 removed from the catalog.",
                folder.display(),
                cataloged
            ),
            Self::Disappeared(root) => write!(
                f,
                "{} disappeared during the scan. What was scanned so far is kept; scan again \
                 when it is back.",
                root.display()
            ),
            Self::Listen { port, source } => {
                write!(f, "Unable to listen on 127.0.0.1:{port}: {source}")
            }
            Self::Move(e) => write!(f, "{e}"),
            Self::Catalog(e) => write!(f, "catalog error: {e}"),
            Self::Interrupted => write!(f, "stopped"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Template(e) => Some(e),
            Self::Plan(e) => Some(e),
            Self::Journal(e) => Some(e),
            Self::Io(e) | Self::File { source: e, .. } | Self::Listen { source: e, .. } => Some(e),
            Self::Move(e) => Some(e),
            Self::Catalog(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Catalog(e)
    }
}

impl From<TemplateError> for Error {
    fn from(e: TemplateError) -> Self {
        Self::Template(e)
    }
}

impl From<PlanError> for Error {
    fn from(e: PlanError) -> Self {
        Self::Plan(e)
    }
}

impl From<JournalError> for Error {
    fn from(e: JournalError) -> Self {
        Self::Journal(e)
    }
}

impl From<MoveFault> for Error {
    fn from(e: MoveFault) -> Self {
        match e {
            MoveFault::Io(e) => Self::Io(e),
            MoveFault::Stopped => Self::Interrupted,
            e => Self::Move(e),
        }
    }
}

/// Attach the path an I/O operation was on to its error.
pub trait AtPath<T> {
    /// The result, with an error that names `path`.
    fn at(self, path: &Path) -> Result<T, Error>;
}

impl<T> AtPath<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T, Error> {
        self.map_err(|source| Error::File {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// `path` as UTF-8, or an error naming `what` it is, like "Library".
pub fn utf8<'a>(path: &'a Path, what: &'static str) -> Result<&'a str, Error> {
    path.to_str().ok_or_else(|| Error::NotUtf8 {
        what,
        path: path.to_path_buf(),
    })
}
