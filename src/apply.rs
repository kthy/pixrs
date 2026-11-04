//! Carrying out plans, with a journal of every step, and undoing them from the journal.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::catalog::{Catalog, CatalogRow, Stamp};
use crate::error::{AtPath, Error};
use crate::names::folded;
use crate::plan::{
    below, split_name, state_dir, Keeper, Move, MoveKind, Plan, PlanKind, PLAN_VERSION,
};
use crate::text::{printable, shown};
use log::debug;
use ring::digest::{Context, Digest, SHA256};
use same_file::is_same_file;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One line of a journal. Every step is written, and flushed to disk, before it is taken.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
enum Record {
    Start {
        /// `PLAN_VERSION`: journals of other versions are refused.
        version: u32,
        plan: PathBuf,
        catalog: PathBuf,
    },
    /// A folder about to be created for a target.
    Mkdir {
        path: PathBuf,
    },
    /// A file about to be moved.
    Intent(Intent),
    /// A temporary file pixrs created for a move, by `apply` or `undo`.
    Via {
        index: usize,
        path: PathBuf,
        kind: ViaKind,
        /// For a copy, the file it is copied from.
        source: Option<PathBuf>,
    },
    /// The catalog row of a file that left its library, so undo can put it back.
    Row {
        index: usize,
        row: CatalogRow,
    },
    Done {
        index: usize,
    },
    /// A file left where it was. Undo ignores its intent.
    Skipped {
        index: usize,
        reason: String,
    },
    /// A folder about to be removed because the moves left it empty.
    Rmdir {
        path: PathBuf,
    },
    Finished,
}

/// A move about to happen: the plan's move, and where it is in the plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Intent {
    index: usize,
    #[serde(flatten)]
    planned: Move,
}

/// The start of a journal's first line, read before the rest so that a journal of another
/// version is refused by its version, not by the first record that changed.
#[derive(Deserialize)]
struct Header {
    event: String,
    version: Option<u32>,
}

/// What a temporary file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ViaKind {
    /// The file itself, stopped on its way during a rename that only changes letter case or
    /// Unicode normalization. It may be the only copy: never removed.
    Stop,
    /// A copy being made across file systems. Redundant while its source exists.
    Copy,
}

/// An open journal file.
struct Journal {
    file: File,
}

impl Journal {
    fn write(&mut self, record: &Record) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()
    }

    /// Journal a temporary file of the move at `index`.
    fn via(
        &mut self,
        index: usize,
        path: &Path,
        kind: ViaKind,
        source: Option<&Path>,
    ) -> io::Result<()> {
        self.write(&Record::Via {
            index,
            path: path.to_path_buf(),
            kind,
            source: source.map(Path::to_path_buf),
        })
    }
}

/// The lock that keeps `apply`, `undo` and `relocate` runs on one catalog apart. Removed when
/// dropped, or by `release_lock` when the process is stopped.
pub(crate) struct Lock(PathBuf);

/// The lock this process holds, for `release_lock`.
static HELD: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Remove the lock this process holds, if any. For the Ctrl-C handler, which exits without
/// running destructors; an interrupted `apply` is reversed with `undo`, which needs the lock.
pub fn release_lock() {
    let held = HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(path) = held {
        let _ = fs::remove_file(path);
    }
}

impl Lock {
    pub(crate) fn take(catalog: &Path) -> Result<Self, Error> {
        let path = state_dir(catalog, "pixrs.lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                *HELD
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path.clone());
                Ok(Self(path))
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(Error::Locked(path)),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.as_ref() == Some(&self.0) {
            *held = None;
        }
        drop(held);
        let _ = fs::remove_file(&self.0);
    }
}

/// Test hooks for `apply`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ApplyOptions {
    /// Copy instead of renaming, as across file systems.
    pub(crate) force_copy: bool,
    /// Where to stop as if interrupted.
    pub(crate) stop: Option<Stop>,
}

/// Where a test stops `apply` as if it was interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// After this many files were moved.
    AfterMoves(usize),
    /// After the first move, before the catalog follows it.
    BeforeCatalog,
    /// Halfway through the first copy.
    InCopy,
    /// Halfway through the first rename that only changes letter case.
    InRename,
}

/// A file `apply` left where it was, and why.
#[derive(Debug)]
pub struct Skipped {
    pub file: PathBuf,
    pub reason: Skip,
}

/// What `apply` did.
#[derive(Debug, Default)]
pub struct ApplySummary {
    pub moved: usize,
    pub skipped: Vec<Skipped>,
    pub folders_removed: usize,
    pub journal: PathBuf,
}

/// What `undo` did.
#[derive(Debug, Default)]
pub struct UndoSummary {
    pub moved_back: usize,
    pub warnings: Vec<String>,
}

/// Why a move did not happen.
enum MoveError {
    /// Something is at the target; nothing is ever overwritten.
    Taken,
    /// It failed before anything changed.
    Failed(MoveFault),
    /// It failed halfway: the file may be at a temporary name, or in two places. Only undo
    /// should touch it now.
    Partial(MoveFault),
}

/// What went wrong in a move.
#[derive(Debug)]
pub enum MoveFault {
    /// A file operation failed.
    Io(io::Error),
    /// The `copy` of `source` across file systems does not read back the same.
    CopyMismatch { copy: PathBuf, source: PathBuf },
    /// The source changed while it was copied, so both are kept.
    ChangedWhileCopied(PathBuf),
    /// A file turned up at `target` during a rename that only changes the spelling; the moved
    /// file waits at `waiting`.
    TurnedUp { target: PathBuf, waiting: PathBuf },
    /// Someone replaced the temporary file at this path.
    Replaced(PathBuf),
    /// A symbolic link is on the way to this folder.
    ThroughLink(PathBuf),
    /// A test stopped the move halfway.
    Stopped,
}

impl fmt::Display for MoveFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::CopyMismatch { copy, source } => write!(
                f,
                "the copy {} does not match {}",
                copy.display(),
                source.display()
            ),
            Self::ChangedWhileCopied(source) => write!(
                f,
                "{} changed while it was copied; both are kept",
                source.display()
            ),
            Self::TurnedUp { target, waiting } => write!(
                f,
                "{} turned up during the rename; the file waits at {}",
                target.display(),
                waiting.display()
            ),
            Self::Replaced(temp) => write!(
                f,
                "the temporary file {} was replaced by something else; left there",
                temp.display()
            ),
            Self::ThroughLink(dir) => {
                write!(f, "{} is reached through a symbolic link", dir.display())
            }
            Self::Stopped => write!(f, "stopped as asked"),
        }
    }
}

impl std::error::Error for MoveFault {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for MoveFault {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Why a move of the plan was skipped.
#[derive(Debug)]
pub enum Skip {
    /// The file is no longer there.
    Gone,
    /// The path is not a file.
    NotAFile,
    /// The file changed since the plan was made.
    Changed,
    /// It is a sidecar whose media file was not moved.
    MediaNotMoved,
    /// The kept copy at this path is gone or changed.
    KeeperChanged(PathBuf),
    /// It is the same file as the kept copy at this path.
    SameAsKeeper(PathBuf),
    /// A symbolic link is on the way to its folder (`target`: its target folder).
    ThroughLink { target: bool, dir: PathBuf },
    /// Whether a link is on the way to `dir` cannot be checked.
    Unchecked { dir: PathBuf, source: io::Error },
    /// Its target folder cannot be created.
    NoFolder(io::Error),
    /// Something is at the target.
    Taken(PathBuf),
    /// The move failed before anything changed.
    Failed(MoveFault),
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => write!(f, "it is no longer there"),
            Self::NotAFile => write!(f, "it is not a file"),
            Self::Changed => write!(f, "it changed since the plan was made"),
            Self::MediaNotMoved => write!(f, "its media file was not moved"),
            Self::KeeperChanged(path) => {
                write!(f, "the kept copy {} is gone or changed", path.display())
            }
            Self::SameAsKeeper(path) => {
                write!(f, "it is the same file as the kept copy {}", path.display())
            }
            Self::ThroughLink { target, dir } => write!(
                f,
                "{} {} is reached through a symbolic link",
                if *target {
                    "its target folder"
                } else {
                    "its folder"
                },
                dir.display()
            ),
            Self::Unchecked { dir, source } => {
                write!(f, "cannot check {}: {}", dir.display(), source)
            }
            Self::NoFolder(e) => write!(f, "cannot create its folder: {e}"),
            Self::Taken(target) => write!(f, "{} is taken", target.display()),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// Number for temporary file names, unique within the process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A new, empty temporary file in `dir`, so the name is pixrs's alone, open for reading and
/// writing. Write through the open file: anyone who can write to `dir` can replace the name,
/// with a symbolic link to another file for example, and `still_ours` tells whether they did.
pub(crate) fn reserve_temp(dir: &Path) -> io::Result<(PathBuf, File)> {
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = dir.join(format!(
            ".pixrs-{}-{}-{}.tmp",
            std::process::id(),
            nanos,
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let created = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path);
        match created {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

/// Whether the name `temp` still leads to `file`, the temporary file `reserve_temp` created
/// there, and not to something put in its place.
pub(crate) fn still_ours(temp: &Path, file: &File) -> bool {
    let regular = fs::symlink_metadata(temp).is_ok_and(|meta| meta.file_type().is_file());
    let ours = file.try_clone().and_then(same_file::Handle::from_file);
    let there = same_file::Handle::from_path(temp);
    regular && matches!((ours, there), (Ok(ours), Ok(there)) if ours == there)
}

/// Close the temporary file `file` and remove it from `temp`, if the name still leads to it:
/// anything else there was put in its place by someone else, and stays.
fn discard(temp: &Path, file: File) {
    let ours = still_ours(temp, &file);
    drop(file);
    if ours {
        let _ = fs::remove_file(temp);
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Whether `a` and `b` differ only in letter case or Unicode normalization, so they are the same
/// file on macOS (and, for letter case, on Windows).
fn spelling_only(a: &Path, b: &Path) -> bool {
    a != b && folded(&a.to_string_lossy()) == folded(&b.to_string_lossy())
}

/// Whether `a` and `b` are the same file (one name in another letter case, or a hard link).
fn same_file(a: &Path, b: &Path) -> bool {
    is_same_file(a, b).unwrap_or(false)
}

/// Whether a file named exactly `path` (letter case included) is in its folder. An error
/// reading the folder is passed on: callers must not take it as "not there".
fn exactly_there(path: &Path) -> io::Result<bool> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(false);
    };
    for entry in fs::read_dir(dir)? {
        if entry?.file_name() == name {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How far a copy's mtime may be from the original's: file systems store mtimes with different
/// precision, FAT and exFAT to 2 seconds.
const MTIME_SLACK_NS: i64 = 2_000_000_000;

/// Whether a file has the size of `stamp` and (within `MTIME_SLACK_NS`) its mtime.
fn matches(meta: &fs::Metadata, stamp: Stamp) -> bool {
    meta.is_file()
        && crate::scan::stamp(meta).is_ok_and(|s| {
            s.size == stamp.size && (s.mtime_ns - stamp.mtime_ns).abs() <= MTIME_SLACK_NS
        })
}

/// How much is read at once to copy and hash files: large reads keep the round trips to a
/// network share few.
const READ_SIZE: usize = 256 * 1024;

/// Hashes what is written to it.
struct Hasher(Context);

impl Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Writes to a file, and hashes what it writes.
struct HashingWriter<'a> {
    file: &'a mut File,
    hasher: Hasher,
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write_all(buf)?;
        self.hasher.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// SHA-256 of everything `reader` reads.
fn sha256_from(reader: impl Read) -> io::Result<Digest> {
    let mut hasher = Hasher(Context::new(&SHA256));
    io::copy(
        &mut BufReader::with_capacity(READ_SIZE, reader),
        &mut hasher,
    )?;
    Ok(hasher.0.finish())
}

/// SHA-256 of a file.
fn sha256_of(path: &Path) -> io::Result<Digest> {
    sha256_from(File::open(path)?)
}

fn same_digest(a: &Digest, b: &Digest) -> bool {
    a.as_ref() == b.as_ref()
}

/// Whether the file `part` is the start of (or all of) the file `whole`: a copy, stopped or
/// finished, of `whole`.
fn is_prefix_of(part: &Path, whole: &Path) -> io::Result<bool> {
    let (part, whole) = (File::open(part)?, File::open(whole)?);
    let len = part.metadata()?.len();
    if len > whole.metadata()?.len() {
        return Ok(false);
    }
    Ok(same_digest(
        &sha256_from(part)?,
        &sha256_from(whole.take(len))?,
    ))
}

/// A free name next to `path` to keep a file under that undo cannot put back:
/// `name.pixrs-recovered.ext`, then `name.pixrs-recovered-2.ext` and so on.
fn recovered_name(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = split_name(&name);
    let mut n = 1;
    loop {
        let number = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let candidate = path.with_file_name(format!("{stem}.pixrs-recovered{number}{ext}"));
        if !exists(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Remove a source file whose verified copy is in place. On Windows a read-only file cannot be
/// removed, so that flag goes first.
fn remove_source(path: &Path) -> io::Result<()> {
    if cfg!(windows) {
        let mut permissions = fs::metadata(path)?.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
        }
    }
    fs::remove_file(path)
}

/// An mtime in nanoseconds since the epoch as a `SystemTime`.
fn system_time(mtime_ns: i64) -> SystemTime {
    let magnitude = Duration::from_nanos(mtime_ns.unsigned_abs());
    if mtime_ns >= 0 {
        UNIX_EPOCH + magnitude
    } else {
        UNIX_EPOCH - magnitude
    }
}

/// Flush a folder's entries to disk, so a rename in it survives a power cut. Windows has no
/// such call for folders; its file systems journal metadata. Some network and FUSE file systems
/// cannot do it either and say so; that is not an error: the rename or new folder it follows is
/// in place already, and only its durability is not guaranteed.
fn sync_dir(dir: &Path) -> io::Result<()> {
    if !cfg!(unix) {
        return Ok(());
    }
    let synced = File::open(dir).and_then(|d| d.sync_all());
    #[cfg(test)]
    let synced = synced.and_then(|()| tests::folder_sync_result());
    match synced {
        Err(e) if cannot_sync_folders(&e) => {
            debug!("Cannot flush the folder {}: {}", shown(dir), e);
            Ok(())
        }
        other => other,
    }
}

/// `EIO` on Linux and macOS.
const EIO: i32 = 5;

/// Whether a failed folder flush means the file system cannot flush folders. Most say so with
/// `EINVAL` or `ENOTSUP`; an SMB share mounted in WSL answers every folder flush with `EIO`.
fn cannot_sync_folders(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
    ) || e.raw_os_error() == Some(EIO)
}

/// Copy `from`, which has `stamp`, into `copy`, the open temporary file reserved at `via`,
/// flushed to disk, with `from`'s mtime, check that the copy reads back the same, and return its
/// SHA-256. All of it goes through `copy`, never through the name. Permissions are copied later,
/// onto the file in its final place: a read-only temporary file could not be removed on Windows.
fn copy_verified(
    from: &Path,
    via: &Path,
    copy: &mut File,
    stamp: Stamp,
    stop_in_copy: bool,
) -> Result<Digest, MoveFault> {
    let mut source = BufReader::with_capacity(READ_SIZE, File::open(from)?);
    let mut writer = HashingWriter {
        file: &mut *copy,
        hasher: Hasher(Context::new(&SHA256)),
    };
    if stop_in_copy {
        io::copy(&mut source.take(stamp.size / 2), &mut writer)?;
        return Err(MoveFault::Stopped);
    }
    io::copy(&mut source, &mut writer)?;
    let hash = writer.hasher.0.finish();
    copy.sync_all()?;
    copy.set_modified(system_time(stamp.mtime_ns))?;
    copy.seek(io::SeekFrom::Start(0))?;
    if !same_digest(&sha256_from(&mut *copy)?, &hash) {
        return Err(MoveFault::CopyMismatch {
            copy: via.to_path_buf(),
            source: from.to_path_buf(),
        });
    }
    Ok(hash)
}

/// Move `from` (which has `stamp`) to `to`, never overwriting. `note_via` is called with each
/// temporary file before it is used.
///
/// Renames if it can; a rename that only changes letter case or Unicode normalization goes
/// through a temporary name (`rename_spelling`), and across file systems the file is copied
/// (`copy_across`). Otherwise `to` must not exist in any letter case.
fn move_file(
    from: &Path,
    to: &Path,
    stamp: Stamp,
    opts: &ApplyOptions,
    note_via: impl FnMut(&Path, ViaKind, Option<&Path>) -> io::Result<()>,
) -> Result<(), MoveError> {
    if spelling_only(from, to) {
        return rename_spelling(from, to, opts, note_via);
    }
    if exists(to) {
        return Err(MoveError::Taken);
    }
    if !opts.force_copy {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(MoveError::Failed(e.into())),
        }
    }
    copy_across(from, to, stamp, opts, note_via)
}

/// The folder a target goes in.
fn folder_of(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// Rename `from` to `to`, which only differs in letter case or Unicode normalization, through a
/// temporary name: refused if another file has exactly the new name, and undone if anything
/// turns up at `to` once `from` is out of the way.
fn rename_spelling(
    from: &Path,
    to: &Path,
    opts: &ApplyOptions,
    mut note_via: impl FnMut(&Path, ViaKind, Option<&Path>) -> io::Result<()>,
) -> Result<(), MoveError> {
    if exactly_there(to).unwrap_or(true) && !same_file(from, to) {
        return Err(MoveError::Taken);
    }
    let (via, reservation) =
        reserve_temp(folder_of(to)).map_err(|e| MoveError::Failed(e.into()))?;
    debug!(
        "Renaming only the spelling (letter case or Unicode normalization), through {}",
        shown(&via)
    );
    // The file itself goes to `via`, by a rename that replaces the reservation.
    drop(reservation);
    if let Err(e) = note_via(&via, ViaKind::Stop, None).and_then(|()| fs::rename(from, &via)) {
        let _ = fs::remove_file(&via);
        return Err(MoveError::Failed(e.into()));
    }
    if opts.stop == Some(Stop::InRename) {
        return Err(MoveError::Partial(MoveFault::Stopped));
    }
    // `from` is out of the way now, so anything at `to` is another file. Go back if the old
    // name is free; on Windows and macOS it is the same name as `to`, so the file waits at
    // `via` for undo instead of replacing the newcomer.
    if exists(to) {
        if exists(from) {
            return Err(MoveError::Partial(MoveFault::TurnedUp {
                target: to.to_path_buf(),
                waiting: via,
            }));
        }
        return match fs::rename(&via, from) {
            Ok(()) => Err(MoveError::Taken),
            Err(e) => Err(MoveError::Partial(e.into())),
        };
    }
    fs::rename(&via, to).map_err(|e| MoveError::Partial(e.into()))
}

/// Move `from` to `to` on another file system: copy it to a temporary file in the target
/// folder, check the copy, rename it into place, flush the folder, check that `from` did not
/// change meanwhile, and only then remove `from`.
fn copy_across(
    from: &Path,
    to: &Path,
    stamp: Stamp,
    opts: &ApplyOptions,
    mut note_via: impl FnMut(&Path, ViaKind, Option<&Path>) -> io::Result<()>,
) -> Result<(), MoveError> {
    let dir = folder_of(to);
    let (via, mut copy) = reserve_temp(dir).map_err(|e| MoveError::Failed(e.into()))?;
    debug!(
        "Copying {} to another file system, through {}",
        shown(from),
        shown(&via)
    );
    let stop_in_copy = opts.stop == Some(Stop::InCopy);
    let copied = note_via(&via, ViaKind::Copy, Some(from))
        .map_err(MoveFault::from)
        .and_then(|()| copy_verified(from, &via, &mut copy, stamp, stop_in_copy));
    let hash = match copied {
        Ok(hash) => hash,
        Err(e) if stop_in_copy => {
            // As if the process died here: the partial copy stays for undo to clean up.
            return Err(MoveError::Partial(e));
        }
        Err(e) => {
            discard(&via, copy);
            return Err(MoveError::Failed(e));
        }
    };
    if exists(to) {
        discard(&via, copy);
        return Err(MoveError::Taken);
    }
    // Only the verified copy goes into place, not what someone put under its name.
    if !still_ours(&via, &copy) {
        return Err(MoveError::Failed(MoveFault::Replaced(via)));
    }
    if let Err(e) = fs::rename(&via, to) {
        discard(&via, copy);
        return Err(MoveError::Failed(e.into()));
    }
    // Through the open file, which is the one at `to` now, whatever the name leads to.
    if let Ok(meta) = fs::metadata(from) {
        let _ = copy.set_permissions(meta.permissions());
    }
    drop(copy);
    sync_dir(dir).map_err(|e| MoveError::Partial(e.into()))?;
    // An edit to `from` while it was copied would be lost with it.
    let unchanged = fs::symlink_metadata(from).is_ok_and(|m| matches(&m, stamp))
        && sha256_of(from).is_ok_and(|h| same_digest(&h, &hash));
    if !unchanged {
        return Err(MoveError::Partial(MoveFault::ChangedWhileCopied(
            from.to_path_buf(),
        )));
    }
    remove_source(from).map_err(|e| MoveError::Partial(e.into()))
}

/// `Err` if the file at `path` should not be moved: gone, or not as `stamp` says.
fn unchanged(path: &Path, stamp: Stamp) -> Result<(), Skip> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Err(Skip::Gone);
    };
    if !meta.is_file() {
        return Err(Skip::NotAFile);
    }
    if crate::scan::stamp(&meta).is_ok_and(|s| s == stamp) {
        Ok(())
    } else {
        Err(Skip::Changed)
    }
}

/// Folders a run of `apply` or `undo` has found to be real: they exist, and no symbolic link is
/// on the way to them. Files mostly share folders with the files before them, and on a network
/// share each check is a round trip, so each folder is checked once a run. A folder replaced by
/// a link while the run goes on is not noticed; that is the same race as a file turning up at a
/// target between its check and the rename (#44).
#[derive(Default)]
struct KnownFolders(HashSet<PathBuf>);

/// Whether a symbolic link is on the way to `dir`, so that it is somewhere else than its path
/// says. With a `base` that `dir` is below, `base` must be where its path says (canonical), as
/// the roots plans are made from are, and the parts after it must not be links, as far as they
/// exist: the rest are created below them. Without one (`undo` has no plan), the nearest
/// existing ancestor of `dir` must be canonical. What is found real is remembered in `known`.
fn through_symlink(dir: &Path, base: Option<&Path>, known: &mut KnownFolders) -> io::Result<bool> {
    if known.0.contains(dir) {
        return Ok(false);
    }
    let below_base = base.and_then(|base| Some((base, dir.strip_prefix(base).ok()?)));
    let Some((base, rest)) = below_base else {
        let existing = dir.ancestors().find(|d| exists(d)).unwrap_or(dir);
        if known.0.contains(existing) {
            return Ok(false);
        }
        let real = fs::canonicalize(existing)? == existing;
        if real {
            known.0.insert(existing.to_path_buf());
        }
        return Ok(!real);
    };
    if through_symlink(base, None, known)? {
        return Ok(true);
    }
    let mut path = base.to_path_buf();
    for part in rest.components() {
        path.push(part);
        if known.0.contains(&path) {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(true),
            Ok(meta) if meta.is_dir() => {
                known.0.insert(path.clone());
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// `Err` for a move into `dir` if a symbolic link is on the way to it, or it cannot be checked.
fn link_free(dir: &Path, known: &mut KnownFolders) -> Result<(), MoveError> {
    match through_symlink(dir, None, known) {
        Ok(false) => Ok(()),
        Ok(true) => Err(MoveError::Failed(MoveFault::ThroughLink(dir.to_path_buf()))),
        Err(e) => Err(MoveError::Failed(e.into())),
    }
}

/// `Err` if a move must not happen: a symbolic link on the way to its source or its
/// target would make the file come from, or go, somewhere else than the plan says. A link
/// inside a library (from an extracted archive, say) is not in the catalog, since the scan
/// does not follow links, so this is where it is caught.
fn not_misplaced(m: &Move, plan: &Plan, known: &mut KnownFolders) -> Result<(), Skip> {
    // `Plan::check` has seen to it that the source is in one of the plan's libraries and the
    // target below its destination; the parts below those are what a link can hide in.
    let root = plan
        .libraries
        .iter()
        .map(|l| l.root.as_path())
        .find(|root| m.from.starts_with(root));
    let destination = Some(plan.destination.as_path());
    for (target, dir, base) in [
        (false, m.from.parent(), root),
        (true, m.to.parent(), destination),
    ] {
        let Some(dir) = dir else {
            continue;
        };
        match through_symlink(dir, base, known) {
            Ok(false) => {}
            Ok(true) => {
                return Err(Skip::ThroughLink {
                    target,
                    dir: dir.to_path_buf(),
                })
            }
            Err(source) => {
                return Err(Skip::Unchecked {
                    dir: dir.to_path_buf(),
                    source,
                })
            }
        }
    }
    Ok(())
}

/// `Err` if a move should not happen now: a sidecar's media file was not moved, a quarantined
/// copy's kept copy is gone or changed (or is the same file), or the file changed.
fn ready(m: &Move, done: &[bool]) -> Result<(), Skip> {
    match &m.kind {
        MoveKind::Sidecar { of } if !done.get(*of).copied().unwrap_or(false) => {
            return Err(Skip::MediaNotMoved);
        }
        MoveKind::Quarantine {
            keeper: Keeper { path, stamp },
            ..
        } => {
            if unchanged(path, *stamp).is_err() {
                return Err(Skip::KeeperChanged(path.clone()));
            }
            if same_file(path, &m.from) {
                return Err(Skip::SameAsKeeper(path.clone()));
            }
        }
        MoveKind::Organize { .. } | MoveKind::Sidecar { .. } => {}
    }
    unchanged(&m.from, m.stamp)
}

/// A new journal for a plan, next to the catalog.
fn new_journal(plan_path: &Path, catalog: &Path) -> Result<(Journal, PathBuf), Error> {
    let dir = state_dir(catalog, "journals");
    fs::create_dir_all(&dir)?;
    let stem = plan_path.file_stem().map_or_else(
        || String::from("plan"),
        |s| s.to_string_lossy().into_owned(),
    );
    let mut path = dir.join(format!("{stem}.jsonl"));
    let mut n = 2;
    while exists(&path) {
        path = dir.join(format!("{stem}-{n}.jsonl"));
        n += 1;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    sync_dir(&dir)?;
    Ok((Journal { file }, path))
}

/// Why `apply` stopped, and the journal to undo what it did, if it got that far.
#[derive(Debug)]
pub struct ApplyError {
    pub error: Error,
    pub journal: Option<PathBuf>,
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for ApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Carry out the plan in `plan_path`.
pub fn apply(plan_path: &Path, plan: &Plan) -> Result<ApplySummary, ApplyError> {
    apply_with(plan_path, plan, &ApplyOptions::default())
}

/// `apply`, with test hooks.
pub(crate) fn apply_with(
    plan_path: &Path,
    plan: &Plan,
    opts: &ApplyOptions,
) -> Result<ApplySummary, ApplyError> {
    let mut journal = None;
    apply_journaled(plan_path, plan, opts, &mut journal)
        .map_err(|error| ApplyError { error, journal })
}

/// `apply`, which tells the journal it started as soon as there is one.
fn apply_journaled(
    plan_path: &Path,
    plan: &Plan,
    opts: &ApplyOptions,
    journal_out: &mut Option<PathBuf>,
) -> Result<ApplySummary, Error> {
    // `read_plan` checks too; a plan built in code is checked here.
    plan.check()?;
    // The journal names the plan, and journals are UTF-8.
    crate::error::utf8(plan_path, "Plan")?;
    let catalog = Catalog::open_for_update(&plan.catalog)?;
    let _lock = Lock::take(&plan.catalog)?;
    let (mut journal, journal_path) = new_journal(plan_path, &plan.catalog)?;
    *journal_out = Some(journal_path.clone());
    journal.write(&Record::Start {
        version: PLAN_VERSION,
        plan: plan_path.to_path_buf(),
        catalog: plan.catalog.clone(),
    })?;
    let mut run = Run {
        plan,
        opts,
        catalog,
        journal,
        known: KnownFolders::default(),
        done: vec![false; plan.moves.len()],
        summary: ApplySummary {
            journal: journal_path,
            ..ApplySummary::default()
        },
    };
    for (index, m) in plan.moves.iter().enumerate() {
        run.carry_out(index, m)?;
    }
    if let PlanKind::Organize {
        prune_empty: true, ..
    } = plan.kind
    {
        run.prune_emptied()?;
    }
    run.journal.write(&Record::Finished)?;
    Ok(run.summary)
}

/// A run of `apply`: the plan, the open catalog and journal, and what has happened so far.
struct Run<'a> {
    plan: &'a Plan,
    opts: &'a ApplyOptions,
    catalog: Catalog,
    journal: Journal,
    known: KnownFolders,
    /// Which moves are done, by index.
    done: Vec<bool>,
    summary: ApplySummary,
}

impl Run<'_> {
    /// Carry out the move at `index`, or skip it. `Err` only for a failure that ends the run.
    fn carry_out(&mut self, index: usize, m: &Move) -> Result<(), Error> {
        let checked =
            ready(m, &self.done).and_then(|()| not_misplaced(m, self.plan, &mut self.known));
        if let Err(why) = checked {
            return self.skip(index, m, why);
        }
        if let Err(e) = create_folders(m.to.parent(), &mut self.journal, &mut self.known) {
            return self.skip(index, m, Skip::NoFolder(e));
        }
        self.journal.write(&Record::Intent(Intent {
            index,
            planned: m.clone(),
        }))?;
        let journal = &mut self.journal;
        let moved = move_file(&m.from, &m.to, m.stamp, self.opts, |via, kind, source| {
            journal.via(index, via, kind, source)
        });
        match moved {
            Ok(()) => {}
            Err(MoveError::Taken) => return self.skip(index, m, Skip::Taken(m.to.clone())),
            Err(MoveError::Failed(e)) => return self.skip(index, m, Skip::Failed(e)),
            Err(MoveError::Partial(e)) => return Err(e.into()),
        }
        self.journal.write(&Record::Done { index })?;
        debug!("Move {}: {} -> {}", index, shown(&m.from), shown(&m.to));
        if let Some(done) = self.done.get_mut(index) {
            *done = true;
        }
        self.summary.moved += 1;
        if self.opts.stop == Some(Stop::BeforeCatalog) {
            return Err(Error::Interrupted);
        }
        update_catalog(&self.catalog, &mut self.journal, index, m)?;
        if self.opts.stop == Some(Stop::AfterMoves(self.summary.moved)) {
            return Err(Error::Interrupted);
        }
        Ok(())
    }

    /// Journal and report that the move at `index` is skipped.
    fn skip(&mut self, index: usize, m: &Move, reason: Skip) -> Result<(), Error> {
        let text = reason.to_string();
        debug!(
            "Move {}: skipped {}: {}",
            index,
            shown(&m.from),
            printable(&text)
        );
        self.journal.write(&Record::Skipped {
            index,
            reason: text,
        })?;
        self.summary.skipped.push(Skipped {
            file: m.from.clone(),
            reason,
        });
        Ok(())
    }

    /// Remove the folders the moves left empty, deepest first, up to the library roots.
    fn prune_emptied(&mut self) -> Result<(), Error> {
        let plan = self.plan;
        let roots: Vec<&Path> = plan.libraries.iter().map(|l| l.root.as_path()).collect();
        let emptied: BTreeSet<&Path> = plan
            .moves
            .iter()
            .zip(&self.done)
            .filter(|(_, &done)| done)
            .filter_map(|(m, _)| m.from.parent())
            .collect();
        // Deepest first, so a folder's subfolders go before it.
        for dir in emptied.iter().rev() {
            let mut dir = dir.to_path_buf();
            while roots.iter().any(|r| below(&dir, r)) && is_empty_dir(&dir) {
                self.journal.write(&Record::Rmdir { path: dir.clone() })?;
                debug!("Removing the empty folder {}", shown(&dir));
                fs::remove_dir(&dir)?;
                self.summary.folders_removed += 1;
                if !dir.pop() {
                    break;
                }
            }
        }
        Ok(())
    }
}

fn is_empty_dir(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

/// Create the missing folders of `dir`, outermost first, each journaled first. Folders in
/// `known` exist; the ones made here join them.
fn create_folders(
    dir: Option<&Path>,
    journal: &mut Journal,
    known: &mut KnownFolders,
) -> io::Result<()> {
    let Some(dir) = dir else {
        return Ok(());
    };
    let missing: Vec<&Path> = dir
        .ancestors()
        .take_while(|d| !known.0.contains(*d) && !exists(d))
        .collect();
    for folder in missing.into_iter().rev() {
        journal.write(&Record::Mkdir {
            path: folder.to_path_buf(),
        })?;
        match fs::create_dir(folder) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        if let Some(parent) = folder.parent() {
            sync_dir(parent)?;
        }
        known.0.insert(folder.to_path_buf());
    }
    Ok(())
}

/// Bring the catalog in line with a move: the row follows the file, or is kept in the journal
/// if the file left its library.
fn update_catalog(
    catalog: &Catalog,
    journal: &mut Journal,
    index: usize,
    m: &Move,
) -> Result<(), Error> {
    match &m.kind {
        MoveKind::Organize {
            row,
            new_path: Some(new_path),
        } => {
            // A row at the target is stale: the file there was gone, or the move would not
            // have happened.
            catalog.rename_row(row.library_id, &row.path, row.library_id, new_path, true)?;
        }
        MoveKind::Organize {
            row,
            new_path: None,
        }
        | MoveKind::Quarantine { row, .. } => {
            // The row is only gone once the journal has it.
            let tx = catalog.transaction()?;
            if let Some(taken) = catalog.take_row(row.library_id, &row.path)? {
                journal.write(&Record::Row { index, row: taken })?;
            }
            tx.commit()?;
        }
        MoveKind::Sidecar { .. } => {}
    }
    Ok(())
}

/// Why a journal cannot be read.
#[derive(Debug)]
pub enum JournalError {
    /// A line other than the last is not a journal record.
    BadRecord {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    /// The file does not start like a journal.
    NotAJournal(PathBuf),
    /// The journal was written by another version of pixrs (`found`, if it says).
    Version { path: PathBuf, found: Option<u32> },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRecord { path, line, source } => write!(
                f,
                "{} line {} is not a journal record: {}",
                path.display(),
                line,
                source
            ),
            Self::NotAJournal(path) => {
                write!(f, "{} does not start like a pixrs journal", path.display())
            }
            Self::Version { path, found } => {
                write!(f, "{} has ", path.display())?;
                match found {
                    Some(version) => write!(f, "journal version {version}")?,
                    None => write!(f, "no journal version")?,
                }
                write!(
                    f,
                    ", but this version of pixrs only understands version {PLAN_VERSION}; undo \
                     it with the version of pixrs that wrote it"
                )
            }
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BadRecord { source, .. } => Some(source),
            Self::NotAJournal(_) | Self::Version { .. } => None,
        }
    }
}

/// `Err` unless `line`, the first of the journal at `path`, starts a journal of this version.
fn check_start(path: &Path, line: &str) -> Result<(), JournalError> {
    match serde_json::from_str::<Header>(line) {
        Ok(Header { event, version }) if event == "start" => {
            if version == Some(PLAN_VERSION) {
                Ok(())
            } else {
                Err(JournalError::Version {
                    path: path.to_path_buf(),
                    found: version,
                })
            }
        }
        _ => Err(JournalError::NotAJournal(path.to_path_buf())),
    }
}

/// The records of a journal of this version, which start with `Start`. A last line cut short by
/// a crash is ignored.
fn read_journal(path: &Path) -> Result<Vec<Record>, Error> {
    let file = File::open(path).at(path)?;
    let mut lines = BufReader::new(file).lines().enumerate().peekable();
    let mut records = vec![];
    while let Some((n, line)) = lines.next() {
        let line = line.at(path)?;
        if n == 0 {
            check_start(path, &line)?;
        }
        match serde_json::from_str(&line) {
            Ok(record) => records.push(record),
            Err(_) if lines.peek().is_none() => {}
            Err(source) => {
                return Err(JournalError::BadRecord {
                    path: path.to_path_buf(),
                    line: n + 1,
                    source,
                }
                .into())
            }
        }
    }
    Ok(records)
}

/// Where a moved file is now.
enum Found {
    /// Only at its target: it can go back.
    AtTarget,
    /// Back at its old place (or never moved).
    AtSource,
    /// At both, as two files with the same contents: a verified copy that got into place
    /// before the source was removed.
    Copied,
    /// Something else; leave it alone and say why.
    Unclear(String),
}

/// Where the file of a journaled move is, a file with `stamp`. Only a copy pixrs made itself (a
/// temporary copy is journaled for the move, and the two names are different files with the
/// same contents) counts as a copy that may be removed.
fn locate(from: &Path, to: &Path, stamp: Stamp, had_copy: bool) -> io::Result<Found> {
    let neither = || {
        Found::Unclear(format!(
            "{} is neither at {} nor back at its old place",
            from.display(),
            to.display()
        ))
    };
    let both = || {
        Found::Unclear(format!(
            "{} and {} both exist; left as they are",
            from.display(),
            to.display()
        ))
    };
    if spelling_only(from, to) {
        // One file under two spellings on Windows and macOS: look at the exact names.
        return Ok(match (exactly_there(to)?, exactly_there(from)?) {
            (true, false) if fs::metadata(to).is_ok_and(|m| matches(&m, stamp)) => Found::AtTarget,
            (true, false) => Found::Unclear(format!(
                "{} is not the file that was moved there; left there",
                to.display()
            )),
            // Two names for one file (Windows, macOS), or two different files (Linux).
            (true, true) if same_file(from, to) => Found::AtSource,
            (true, true) => both(),
            (false, true) => Found::AtSource,
            (false, false) => neither(),
        });
    }
    let at_target = fs::symlink_metadata(to).ok();
    let at_source = exists(from);
    Ok(match (at_target, at_source) {
        (None, true) => Found::AtSource,
        (None, false) => neither(),
        (Some(_), true) if same_file(from, to) => Found::AtSource,
        (Some(meta), true) => {
            // A copy pixrs made has the source's mtime too.
            if had_copy && matches(&meta, stamp) && same_digest(&sha256_of(to)?, &sha256_of(from)?)
            {
                Found::Copied
            } else {
                both()
            }
        }
        // Moves keep the mtime, so a file there with another one is not the moved file.
        (Some(meta), false) if matches(&meta, stamp) => Found::AtTarget,
        (Some(_), false) => Found::Unclear(format!(
            "{} is not the file that was moved there (it changed, or is another file); left \
             there",
            to.display()
        )),
    })
}

/// A temporary file and what it is, from a `Via` record.
struct ViaFile<'a> {
    path: &'a Path,
    kind: ViaKind,
    source: Option<&'a Path>,
}

/// Deal with a temporary file left by an interrupted move of a file with `stamp`, without ever
/// losing data.
///
/// An empty file is a reservation that was never used (unless the moved file is empty too) and
/// goes. A stopped rename is the file itself: it goes back to `from` if it is the moved file
/// and `from` is free. A copy is removed only if its source still has every byte of it.
/// Anything else is kept next to the temporary file, as `name.pixrs-recovered.ext`.
fn clean_up_via(via: &ViaFile, from: &Path, stamp: Stamp, warnings: &mut Vec<String>) {
    let Ok(meta) = fs::symlink_metadata(via.path) else {
        return;
    };
    let unused = meta.len() == 0 && stamp.size != 0;
    let redundant = via.kind == ViaKind::Copy
        && via
            .source
            .is_some_and(|source| is_prefix_of(via.path, source).unwrap_or(false));
    let result = if unused || redundant {
        fs::remove_file(via.path)
    } else if via.kind == ViaKind::Stop && matches(&meta, stamp) && !exists(from) {
        fs::rename(via.path, from)
    } else {
        // Next to the temporary file, so on its file system.
        let keep = recovered_name(
            &via.path
                .with_file_name(from.file_name().unwrap_or_default()),
        );
        fs::rename(via.path, &keep).map(|()| {
            warnings.push(format!(
                "Kept {} as {}: check it by hand",
                via.path.display(),
                keep.display()
            ));
        })
    };
    if let Err(e) = result {
        warnings.push(format!("Cannot clean up {}: {}", via.path.display(), e));
    }
}

/// Open a journal to add records after its last complete line. A line a crash cut short is cut
/// off: its record was never complete, and nothing happens before its record is. A last record
/// that is complete but lacks its newline (edited by hand) gets one instead.
fn append_to(path: &Path) -> io::Result<Journal> {
    let contents = fs::read(path)?;
    let complete = contents
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    // Not opened for appending: Windows does not allow cutting a file opened that way.
    let mut file = OpenOptions::new().write(true).open(path)?;
    if let Some(tail) = contents.get(complete..).filter(|tail| !tail.is_empty()) {
        if serde_json::from_slice::<Record>(tail).is_ok() {
            file.seek(io::SeekFrom::End(0))?;
            file.write_all(b"\n")?;
        } else {
            file.set_len(complete as u64)?;
        }
        file.sync_data()?;
    }
    file.seek(io::SeekFrom::End(0))?;
    Ok(Journal { file })
}

/// Whether `path` is named like a temporary file `reserve_temp` makes:
/// `.pixrs-<pid>-<nanos>-<n>.tmp`.
fn is_temp_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(numbers) = name
        .strip_prefix(".pixrs-")
        .and_then(|n| n.strip_suffix(".tmp"))
    else {
        return false;
    };
    let parts: Vec<&str> = numbers.split('-').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether a `Via` record describes a temporary file pixrs makes for the move `from` -> `to`:
/// named like one, in the folder of `to` (made by `apply`) or of `from` (made by `undo`), and
/// for a copy, copied from one of the two. Undo removes and renames the files such records
/// name, so a journal must not be able to point them at anything else.
fn via_belongs_to(via: &ViaFile, from: &Path, to: &Path) -> bool {
    let in_place = [to.parent(), from.parent()].contains(&via.path.parent());
    let source_ok = match (via.kind, via.source) {
        (ViaKind::Copy, Some(source)) => source == from || source == to,
        (ViaKind::Copy, None) => false,
        (ViaKind::Stop, _) => true,
    };
    is_temp_name(via.path) && in_place && source_ok
}

/// What a journal records, sorted out for undo.
#[derive(Default)]
struct JournalIndex<'a> {
    intents: Vec<&'a Intent>,
    vias: HashMap<usize, Vec<ViaFile<'a>>>,
    /// Catalog rows of files that left their library, by move.
    rows: HashMap<usize, &'a CatalogRow>,
    skipped: HashSet<usize>,
    /// Folders `apply` created, and removed, in that order.
    created: Vec<&'a Path>,
    removed: Vec<&'a Path>,
}

impl<'a> JournalIndex<'a> {
    fn new(records: &'a [Record]) -> Self {
        let mut index = Self::default();
        for record in records {
            match record {
                Record::Intent(intent) => index.intents.push(intent),
                Record::Via {
                    index: move_index,
                    path,
                    kind,
                    source,
                } => index.vias.entry(*move_index).or_default().push(ViaFile {
                    path,
                    kind: *kind,
                    source: source.as_deref(),
                }),
                Record::Row { index: i, row } => {
                    index.rows.insert(*i, row);
                }
                Record::Skipped { index: i, .. } => {
                    index.skipped.insert(*i);
                }
                Record::Mkdir { path } => index.created.push(path),
                Record::Rmdir { path } => index.removed.push(path),
                Record::Start { .. } | Record::Done { .. } | Record::Finished => {}
            }
        }
        index
    }
}

/// Reverse everything the journal at `path` records, as far as it got. Safe to run again, also
/// after a crash: it looks at where each file actually is, and journals its own steps.
pub fn undo(path: &Path) -> Result<UndoSummary, Error> {
    let records = read_journal(path)?;
    let Some(Record::Start { catalog, .. }) = records.first() else {
        return Err(JournalError::NotAJournal(path.to_path_buf()).into());
    };
    let mut undo = Undo {
        catalog: Catalog::open_for_update(catalog)?,
        _lock: Lock::take(catalog)?,
        journal: append_to(path)?,
        known: KnownFolders::default(),
        summary: UndoSummary::default(),
    };
    let index = JournalIndex::new(&records);
    undo.restore_folders(&index);
    for intent in index.intents.iter().rev() {
        undo.reverse(intent, &index)?;
    }
    for dir in index.created.iter().rev() {
        if is_empty_dir(dir) {
            let _ = fs::remove_dir(dir);
        }
    }
    Ok(undo.summary)
}

/// A run of `undo`.
struct Undo {
    catalog: Catalog,
    _lock: Lock,
    journal: Journal,
    known: KnownFolders,
    summary: UndoSummary,
}

impl Undo {
    /// Recreate the folders the moves emptied, so files can go back into them. Only folders a
    /// moved file came out of were ever removed, so only those come back.
    fn restore_folders(&mut self, index: &JournalIndex) {
        for dir in index.removed.iter().rev() {
            let came_from = index
                .intents
                .iter()
                .any(|intent| below(&intent.planned.from, dir));
            if !came_from {
                self.summary.warnings.push(format!(
                    "{} is not a folder a moved file came from; not recreated",
                    dir.display()
                ));
                continue;
            }
            if let Err(e) = fs::create_dir_all(dir) {
                self.summary
                    .warnings
                    .push(format!("Cannot recreate {}: {}", dir.display(), e));
            }
        }
    }

    /// Reverse one journaled move, as far as it got.
    fn reverse(&mut self, intent: &Intent, index: &JournalIndex) -> Result<(), Error> {
        let Intent { index: i, planned } = intent;
        let (from, to) = (planned.from.as_path(), planned.to.as_path());
        let mut vias: Vec<&ViaFile> = vec![];
        for via in index.vias.get(i).into_iter().flatten() {
            if via_belongs_to(via, from, to) {
                vias.push(via);
            } else {
                self.summary.warnings.push(format!(
                    "{} is not a temporary file of the move of {}; left alone",
                    via.path.display(),
                    from.display()
                ));
            }
        }
        for via in &vias {
            clean_up_via(via, from, planned.stamp, &mut self.summary.warnings);
        }
        if index.skipped.contains(i) {
            debug!("Move {}: skipped by apply; {} stays", i, shown(from));
            return Ok(());
        }
        let had_copy = vias.iter().any(|v| v.kind == ViaKind::Copy);
        let found = locate(from, to, planned.stamp, had_copy)
            .unwrap_or_else(|e| Found::Unclear(format!("Cannot check {}: {}", to.display(), e)));
        let at = match &found {
            Found::AtTarget => "at the target, moving it back",
            Found::Copied => "copied, removing the copy",
            Found::AtSource => "back at its place already",
            Found::Unclear(_) => "unclear, leaving it",
        };
        debug!("Move {}: {} -> {}: {}", i, shown(from), shown(to), at);
        // The catalog follows only a file that is back.
        if self.put_back(*i, planned, found) {
            self.restore_row(planned, index.rows.get(i).copied())?;
        }
        Ok(())
    }

    /// Put the file of a move back where it was found to belong; whether it is back.
    fn put_back(&mut self, index: usize, m: &Move, found: Found) -> bool {
        let (from, to) = (m.from.as_path(), m.to.as_path());
        match found {
            Found::AtTarget => {
                // Without a plan, the folder it goes back to must at least be where its path
                // says, as the roots plans are made from are.
                let journal = &mut self.journal;
                let moved = link_free(folder_of(from), &mut self.known).and_then(|()| {
                    move_file(
                        to,
                        from,
                        m.stamp,
                        &ApplyOptions::default(),
                        |via, kind, source| journal.via(index, via, kind, source),
                    )
                });
                match moved {
                    Ok(()) => {
                        self.summary.moved_back += 1;
                        true
                    }
                    Err(MoveError::Taken) => {
                        self.summary.warnings.push(format!(
                            "{} stays: {} is taken",
                            to.display(),
                            from.display()
                        ));
                        false
                    }
                    Err(MoveError::Failed(e) | MoveError::Partial(e)) => {
                        self.summary.warnings.push(format!(
                            "Cannot move {} back: {}",
                            to.display(),
                            e
                        ));
                        false
                    }
                }
            }
            Found::Copied => match remove_source(to) {
                Ok(()) => true,
                Err(e) => {
                    self.summary.warnings.push(format!(
                        "Cannot remove the copy {}: {}",
                        to.display(),
                        e
                    ));
                    false
                }
            },
            Found::AtSource => true,
            Found::Unclear(warning) => {
                self.summary.warnings.push(warning);
                false
            }
        }
    }

    /// Put the catalog row of a file that is back where it was: only where a row still points
    /// at the target, or none is at the file's old place, so running undo again changes nothing.
    fn restore_row(&self, m: &Move, saved: Option<&CatalogRow>) -> Result<(), Error> {
        match &m.kind {
            MoveKind::Organize {
                row,
                new_path: Some(new_path),
            } => {
                self.catalog.rename_row(
                    row.library_id,
                    new_path,
                    row.library_id,
                    &row.path,
                    false,
                )?;
            }
            MoveKind::Organize {
                row,
                new_path: None,
            }
            | MoveKind::Quarantine { row, .. } => {
                if let Some(saved) = saved {
                    if !self.catalog.has_row(row.library_id, &row.path)? {
                        self.catalog.put_row(saved)?;
                    }
                }
            }
            MoveKind::Sidecar { .. } => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::{library_path, FileRecord};
    use crate::plan::{plan_organize, plan_quarantine, write_plan, OrganizeOptions};
    use crate::template::Template;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;
    use tempfile::{tempdir, TempDir};
    use time::macros::datetime;

    struct Fixture {
        dir: TempDir,
        root: PathBuf,
        catalog_path: PathBuf,
    }

    /// A library of real files, scanned into a catalog next to it.
    fn fixture(files: &[(&str, &[u8])]) -> Fixture {
        let dir = tempdir().unwrap();
        let root = dir.path().join("lib");
        for (path, contents) in files {
            let full = library_path(&root, path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, contents).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let catalog_path = dir.path().join("catalog.sqlite");
        crate::scan::scan_with(
            &root,
            &catalog_path,
            crate::scan::ScanOptions::QUIET,
            &crate::decode::Tools::default(),
        )
        .unwrap();
        Fixture {
            dir,
            root,
            catalog_path,
        }
    }

    /// Every file and folder below `dir`, with file contents.
    fn tree(dir: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
        walkdir::WalkDir::new(dir)
            .min_depth(1)
            .into_iter()
            .map(|e| e.unwrap())
            .map(|e| {
                // `/` on every OS, so the keys read the same on Windows.
                let rel = e
                    .path()
                    .strip_prefix(dir)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let contents = e.file_type().is_file().then(|| fs::read(e.path()).unwrap());
                (rel, contents)
            })
            .collect()
    }

    fn rows(f: &Fixture) -> Vec<FileRecord> {
        let catalog = Catalog::open(&f.catalog_path).unwrap();
        let id = catalog.library_id(f.root.to_str().unwrap()).unwrap();
        catalog.files(id).unwrap()
    }

    fn organize_plan(f: &Fixture, template: &str, prune_empty: bool) -> (PathBuf, Plan) {
        let catalog = Catalog::open(&f.catalog_path).unwrap();
        let id = catalog.library_id(f.root.to_str().unwrap()).unwrap();
        let template = Template::parse(template).unwrap();
        let now = datetime!(2026-10-20 12:00 UTC);
        let opts = OrganizeOptions {
            template: &template,
            undated: "undated",
            suffix: "_{n}",
            trust_dates: false,
            to: None,
            prune_empty,
            now,
        };
        let (plan, _) = plan_organize(&catalog, &f.catalog_path, id, &f.root, &opts).unwrap();
        let path = write_plan(&plan, now).unwrap();
        (path, plan)
    }

    const FILES: &[(&str, &[u8])] = &[
        ("2017/2017-10-08 18.15.36.jpg", b"one"),
        ("2017/sub/IMG_20190501_143201.jpg", b"two"),
        ("2017/sub/IMG_20190501_143201.xmp", b"edits"),
        ("misc/DSC0047.mp4", b"three"),
        ("Ærø/Sommer på Ærø 2019-07-01.JPG", b"four"),
    ];

    #[test]
    fn test_round_trip() {
        for force_copy in [false, true] {
            let f = fixture(FILES);
            let (before, catalog_before) = (tree(&f.root), rows(&f));
            let (plan_path, plan) = organize_plan(&f, "{type}/{year}/{date}_{time}.{ext}", true);
            let opts = ApplyOptions {
                force_copy,
                ..ApplyOptions::default()
            };
            let summary = apply_with(&plan_path, &plan, &opts).unwrap();
            assert_eq!((summary.moved, summary.skipped.len()), (5, 0));
            let after = tree(&f.root);
            assert!(after.contains_key("image/2019/2019-05-01_143201.jpg"));
            assert!(after.contains_key("image/2019/2019-05-01_143201.xmp"));
            assert!(after.contains_key("video/undated/DSC0047.mp4"));
            assert!(after.contains_key("image/2019/Sommer på Ærø 2019-07-01.jpg"));
            // The emptied folders are pruned; the catalog follows the files.
            assert!(!after.contains_key("2017") && !after.contains_key("misc"));
            assert!(rows(&f)
                .iter()
                .any(|r| r.path == "video/undated/DSC0047.mp4"));

            let undone = undo(&summary.journal).unwrap();
            assert_eq!(undone.warnings, Vec::<String>::new());
            assert_eq!(tree(&f.root), before, "force_copy {}", force_copy);
            assert_eq!(rows(&f), catalog_before);
            // Running it again changes nothing.
            undo(&summary.journal).unwrap();
            assert_eq!(tree(&f.root), before);
        }
    }

    #[test]
    fn test_interrupted_apply_is_undone() {
        let f = fixture(FILES);
        let before = tree(&f.root);
        let (plan_path, plan) = organize_plan(&f, "{year}/{date}_{time}.{ext}", false);
        let journal = state_dir(&f.catalog_path, "journals").join(format!(
            "{}.jsonl",
            plan_path.file_stem().unwrap().to_string_lossy()
        ));
        let opts = ApplyOptions {
            stop: Some(Stop::AfterMoves(2)),
            ..ApplyOptions::default()
        };
        assert!(apply_with(&plan_path, &plan, &opts).is_err());
        assert_ne!(tree(&f.root), before);
        undo(&journal).unwrap();
        assert_eq!(tree(&f.root), before);

        // Stopped halfway through a copy across file systems.
        let opts = ApplyOptions {
            force_copy: true,
            stop: Some(Stop::InCopy),
        };
        assert!(apply_with(&plan_path, &plan, &opts).is_err());
        let journal = state_dir(&f.catalog_path, "journals").join(format!(
            "{}-2.jsonl",
            plan_path.file_stem().unwrap().to_string_lossy()
        ));
        undo(&journal).unwrap();
        assert_eq!(tree(&f.root), before);
    }

    #[test]
    fn test_changed_and_taken_files_are_skipped() {
        let f = fixture(FILES);
        let (plan_path, plan) = organize_plan(&f, "{type}/{name}.{ext}", false);
        // One file changes after planning; another's target gets taken.
        fs::write(f.root.join("misc/DSC0047.mp4"), b"changed!").unwrap();
        fs::create_dir_all(f.root.join("image")).unwrap();
        fs::write(f.root.join("image/2017-10-08 18.15.36.jpg"), b"intruder").unwrap();
        let summary = apply(&plan_path, &plan).unwrap();
        let skipped = &summary.skipped;
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped.iter().any(|s| matches!(s.reason, Skip::Changed)));
        assert!(skipped.iter().any(|s| matches!(s.reason, Skip::Taken(_))));
        assert_eq!(
            fs::read(f.root.join("image/2017-10-08 18.15.36.jpg")).unwrap(),
            b"intruder"
        );
        assert_eq!(
            fs::read(f.root.join("misc/DSC0047.mp4")).unwrap(),
            b"changed!"
        );
    }

    #[test]
    fn test_case_only_rename_round_trip() {
        let f = fixture(&[("Upper.JPG", b"x")]);
        let before = tree(&f.root);
        let (plan_path, plan) = organize_plan(&f, "{name}.{ext}", false);
        assert_eq!(plan.moves.len(), 1);
        let summary = apply(&plan_path, &plan).unwrap();
        assert!(exactly_there(&f.root.join("Upper.jpg")).unwrap());
        undo(&summary.journal).unwrap();
        assert_eq!(tree(&f.root), before);
    }

    #[test]
    fn test_quarantine_round_trip_restores_catalog_rows() {
        let f = fixture(&[
            ("a/photo.jpg", b"same bytes"),
            ("b/copy of photo.jpg", b"same bytes"),
            ("b/copy of photo.xmp", b"sidecar"),
        ]);
        let catalog = Catalog::open(&f.catalog_path).unwrap();
        let report =
            crate::dupes::find_dupes(&catalog, &[], crate::dupes::Similarity::default()).unwrap();
        assert_eq!(report.groups.len(), 1);
        // Resolved, as `pixrs dupes --quarantine` resolves it.
        let quarantine = f.dir.path().canonicalize().unwrap().join("quarantine");
        let now = datetime!(2026-10-20 12:00 UTC);
        let (plan, _) =
            plan_quarantine(&catalog, &f.catalog_path, &report, &[], &quarantine, now).unwrap();
        drop(catalog);
        let plan_path = write_plan(&plan, now).unwrap();
        let (before, catalog_before) = (tree(&f.root), rows(&f));

        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.moved, 2);
        // The keeper (oldest, then path order) stays; the copy and its sidecar move, below a
        // folder named after the library.
        let moved = tree(&quarantine);
        let moved_from = if tree(&f.root).contains_key("a/photo.jpg") {
            "b"
        } else {
            "a"
        };
        assert_eq!(
            moved
                .keys()
                .filter(|k| k.starts_with(&format!("lib/{moved_from}/")))
                .count(),
            if moved_from == "b" { 2 } else { 1 }
        );
        assert_eq!(rows(&f).len(), 1);

        undo(&summary.journal).unwrap();
        assert_eq!(tree(&f.root), before);
        assert_eq!(rows(&f), catalog_before);
    }

    thread_local! {
        /// The OS error every folder flush on this thread fails with, as on a file system that
        /// cannot flush folders.
        static FOLDER_SYNC_ERROR: std::cell::Cell<Option<i32>> =
            const { std::cell::Cell::new(None) };
    }

    pub(super) fn folder_sync_result() -> io::Result<()> {
        match FOLDER_SYNC_ERROR.with(std::cell::Cell::get) {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => Ok(()),
        }
    }

    #[test]
    fn test_which_folder_flush_errors_mean_it_cannot_be_done() {
        assert!(cannot_sync_folders(&io::Error::from_raw_os_error(EIO)));
        assert!(cannot_sync_folders(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        assert!(cannot_sync_folders(&io::Error::from(
            io::ErrorKind::InvalidInput
        )));
        // A full disk or a refused permission is a real failure.
        assert!(!cannot_sync_folders(&io::Error::from_raw_os_error(28)));
        assert!(!cannot_sync_folders(&io::Error::from_raw_os_error(13)));
    }

    #[cfg(unix)]
    #[test]
    fn test_apply_carries_on_where_folders_cannot_be_flushed() {
        for force_copy in [false, true] {
            let f = fixture(FILES);
            let (plan_path, plan) = organize_plan(&f, "{type}/{year}/{date}_{time}.{ext}", true);
            FOLDER_SYNC_ERROR.with(|e| e.set(Some(EIO)));
            let opts = ApplyOptions {
                force_copy,
                ..ApplyOptions::default()
            };
            let summary = apply_with(&plan_path, &plan, &opts);
            FOLDER_SYNC_ERROR.with(|e| e.set(None));
            let summary = summary.unwrap();
            assert!(summary.skipped.is_empty(), "force_copy {force_copy}");
            assert_eq!(summary.moved, plan.moves.len());
        }

        // Any other error is a failure: here the journal's folder cannot be flushed, so apply
        // stops before it moves anything.
        let f = fixture(FILES);
        let before = tree(&f.root);
        let (plan_path, plan) = organize_plan(&f, "{type}/{year}/{date}_{time}.{ext}", true);
        FOLDER_SYNC_ERROR.with(|e| e.set(Some(28)));
        let applied = apply(&plan_path, &plan);
        FOLDER_SYNC_ERROR.with(|e| e.set(None));
        assert!(matches!(applied.unwrap_err().error, Error::Io(_)));
        assert_eq!(tree(&f.root), before);
    }

    #[test]
    fn test_lock() {
        let dir = tempdir().unwrap();
        let catalog = dir.path().join("catalog.sqlite");
        let lock = Lock::take(&catalog).unwrap();
        assert!(matches!(Lock::take(&catalog), Err(Error::Locked(_))));
        drop(lock);
        assert!(Lock::take(&catalog).is_ok());
    }

    #[test]
    fn test_a_copy_goes_through_its_reserved_file() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("photo.jpg");
        fs::write(&from, b"the photo").unwrap();
        let (via, mut copy) = reserve_temp(dir.path()).unwrap();
        assert!(still_ours(&via, &copy));
        let stamp = Stamp {
            size: 9,
            mtime_ns: 0,
        };
        let hash = copy_verified(&from, &via, &mut copy, stamp, false).unwrap();
        assert!(same_digest(&hash, &sha256_of(&from).unwrap()));
        assert_eq!(fs::read(&via).unwrap(), b"the photo");
        discard(&via, copy);
        assert!(!exists(&via));

        // Another file put under the name of a temporary file is not it, and stays.
        let (via, copy) = reserve_temp(dir.path()).unwrap();
        fs::remove_file(&via).unwrap();
        fs::write(&via, b"someone else's").unwrap();
        assert!(!still_ours(&via, &copy));
        discard(&via, copy);
        assert_eq!(fs::read(&via).unwrap(), b"someone else's");
    }

    #[cfg(unix)]
    #[test]
    fn test_a_link_put_in_place_of_a_temporary_file_is_not_written_through() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let from = dir.path().join("photo.jpg");
        fs::write(&from, b"the photo").unwrap();
        let victim = dir.path().join("victim.txt");
        fs::write(&victim, b"precious").unwrap();
        let (via, mut copy) = reserve_temp(dir.path()).unwrap();
        // Someone who can write to the folder swaps the name for a link to one of your files.
        fs::remove_file(&via).unwrap();
        symlink(&victim, &via).unwrap();
        let stamp = Stamp {
            size: 9,
            mtime_ns: 0,
        };
        copy_verified(&from, &via, &mut copy, stamp, false).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert!(!still_ours(&via, &copy));
        discard(&via, copy);
        assert!(fs::symlink_metadata(&via).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
    }

    #[test]
    fn test_truncated_journal_line_is_ignored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let start = serde_json::to_string(&Record::Start {
            version: PLAN_VERSION,
            plan: PathBuf::from("p"),
            catalog: PathBuf::from("c"),
        })
        .unwrap();
        fs::write(&path, format!("{start}\n{{\"event\":\"int")).unwrap();
        assert_eq!(read_journal(&path).unwrap().len(), 1);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn test_names_differing_in_case_are_not_overwritten() {
        let f = fixture(&[("Photo.JPG", b"upper"), ("Photo.jpg", b"lower")]);
        let before = tree(&f.root);
        let (plan_path, plan) = organize_plan(&f, "{name}.{ext}", false);
        let summary = apply(&plan_path, &plan).unwrap();
        let after = tree(&f.root);
        let mut contents: Vec<&[u8]> = after.values().flatten().map(Vec::as_slice).collect();
        contents.sort();
        assert_eq!(contents, [b"lower".as_slice(), b"upper".as_slice()]);
        undo(&summary.journal).unwrap();
        assert_eq!(tree(&f.root), before);
    }

    #[test]
    fn test_folder_case_is_kept() {
        // The template says `image`; the folder on disk is `Image`. That is not a move.
        let f = fixture(&[("Image/x.jpg", b"x")]);
        let (_, plan) = organize_plan(&f, "{type}/{name}.{ext}", false);
        assert_eq!(plan.moves, vec![]);
    }

    #[test]
    fn test_undo_leaves_skipped_moves_and_rows_alone() {
        let f = fixture(FILES);
        let catalog_before = rows(&f);
        let (plan_path, plan) = organize_plan(&f, "{type}/{name}.{ext}", false);
        // An identical copy appears at one target after planning.
        fs::create_dir_all(f.root.join("video")).unwrap();
        fs::write(f.root.join("video/DSC0047.mp4"), b"three").unwrap();
        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.skipped.len(), 1);
        for _ in 0..2 {
            let undone = undo(&summary.journal).unwrap();
            assert_eq!(undone.warnings, Vec::<String>::new());
            assert_eq!(
                fs::read(f.root.join("video/DSC0047.mp4")).unwrap(),
                b"three"
            );
            assert_eq!(fs::read(f.root.join("misc/DSC0047.mp4")).unwrap(), b"three");
            assert_eq!(rows(&f), catalog_before);
        }
    }

    #[test]
    fn test_crash_before_the_catalog_follows() {
        let f = fixture(FILES);
        let (before, catalog_before) = (tree(&f.root), rows(&f));
        let (plan_path, plan) = organize_plan(&f, "{type}/{name}.{ext}", false);
        let opts = ApplyOptions {
            stop: Some(Stop::BeforeCatalog),
            ..ApplyOptions::default()
        };
        assert!(apply_with(&plan_path, &plan, &opts).is_err());
        let journal = state_dir(&f.catalog_path, "journals").join(format!(
            "{}.jsonl",
            plan_path.file_stem().unwrap().to_string_lossy()
        ));
        undo(&journal).unwrap();
        assert_eq!(tree(&f.root), before);
        assert_eq!(rows(&f), catalog_before);
    }

    #[test]
    fn test_quarantine_needs_the_kept_copy_and_leaves_shared_sidecars() {
        let f = fixture(&[
            ("a/IMG_1.jpg", b"same bytes"),
            ("a/IMG_1.jpeg", b"same bytes"),
            ("a/IMG_1.xmp", b"edits of the kept copy"),
        ]);
        let catalog = Catalog::open(&f.catalog_path).unwrap();
        let report =
            crate::dupes::find_dupes(&catalog, &[], crate::dupes::Similarity::default()).unwrap();
        // Resolved, as `pixrs dupes --quarantine` resolves it.
        let quarantine = f.dir.path().canonicalize().unwrap().join("quarantine");
        let now = datetime!(2026-10-20 12:00 UTC);
        let (plan, notes) =
            plan_quarantine(&catalog, &f.catalog_path, &report, &[], &quarantine, now).unwrap();
        drop(catalog);
        // Only the copy moves; the sidecar stays with the kept file.
        assert_eq!(plan.moves.len(), 1);
        assert_eq!(notes.warnings.len(), 1);
        let MoveKind::Quarantine { keeper, .. } = plan.moves[0].kind.clone() else {
            panic!("not a quarantine move: {:?}", plan.moves[0]);
        };
        let plan_path = write_plan(&plan, now).unwrap();
        // The kept copy is gone before the plan is applied: nothing moves.
        fs::rename(&keeper.path, f.dir.path().join("elsewhere.jpg")).unwrap();
        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.moved, 0);
        assert!(matches!(summary.skipped[0].reason, Skip::KeeperChanged(_)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_crash_in_a_case_only_rename_keeps_the_file() {
        let f = fixture(&[("Upper.JPG", b"original")]);
        let (plan_path, plan) = organize_plan(&f, "{name}.{ext}", false);
        let opts = ApplyOptions {
            stop: Some(Stop::InRename),
            ..ApplyOptions::default()
        };
        let error = apply_with(&plan_path, &plan, &opts).unwrap_err();
        // Meanwhile another file turns up at the new name.
        fs::write(f.root.join("Upper.jpg"), b"newcomer").unwrap();
        let undone = undo(&error.journal.unwrap()).unwrap();
        assert_eq!(fs::read(f.root.join("Upper.JPG")).unwrap(), b"original");
        assert_eq!(fs::read(f.root.join("Upper.jpg")).unwrap(), b"newcomer");
        assert_eq!(undone.moved_back, 0);
    }

    #[test]
    fn test_undo_after_a_cut_journal_line() {
        let f = fixture(&[("Upper.JPG", b"x"), ("other.jpg", b"y")]);
        let before = tree(&f.root);
        let (plan_path, plan) = organize_plan(&f, "{name}.{ext}", false);
        let opts = ApplyOptions {
            stop: Some(Stop::AfterMoves(1)),
            ..ApplyOptions::default()
        };
        let journal = apply_with(&plan_path, &plan, &opts)
            .unwrap_err()
            .journal
            .unwrap();
        // A crash cut the last record short.
        let mut file = OpenOptions::new().append(true).open(&journal).unwrap();
        file.write_all(b"{\"event\":\"int").unwrap();
        drop(file);
        undo(&journal).unwrap();
        assert_eq!(tree(&f.root), before);
        // Undo's own records start on a line of their own, so the journal still reads.
        read_journal(&journal).unwrap();
        undo(&journal).unwrap();
        assert_eq!(tree(&f.root), before);
    }

    #[test]
    fn test_a_partial_copy_without_its_source_is_kept_aside() {
        let dir = tempdir().unwrap();
        let (via, source, from) = (
            dir.path().join(".pixrs-1.tmp"),
            dir.path().join("gone.jpg"),
            dir.path().join("photo.jpg"),
        );
        fs::write(&via, b"half a pho").unwrap();
        let copy = ViaFile {
            path: &via,
            kind: ViaKind::Copy,
            source: Some(&source),
        };
        let stamp = Stamp {
            size: 20,
            mtime_ns: 0,
        };
        let mut warnings = vec![];
        clean_up_via(&copy, &from, stamp, &mut warnings);
        assert!(!from.exists());
        assert_eq!(
            fs::read(dir.path().join("photo.pixrs-recovered.jpg")).unwrap(),
            b"half a pho"
        );
        assert_eq!(warnings.len(), 1);

        // With its source still there, a partial copy is redundant.
        fs::write(&via, b"half").unwrap();
        fs::write(&source, b"half and the rest").unwrap();
        clean_up_via(&copy, &from, stamp, &mut warnings);
        assert!(!via.exists());

        // An unused, empty reservation for a stopped rename goes; it is not the file.
        fs::write(&via, b"").unwrap();
        let stop = ViaFile {
            path: &via,
            kind: ViaKind::Stop,
            source: None,
        };
        clean_up_via(&stop, &from, stamp, &mut warnings);
        assert!(!via.exists() && !from.exists());
    }

    #[test]
    fn test_quarantine_never_moves_the_kept_file_under_another_name() {
        let f = fixture(&[("a/photo.jpg", b"one inode")]);
        let (kept, other) = (f.root.join("a/photo.jpg"), f.root.join("b/photo.jpg"));
        fs::create_dir(other.parent().unwrap()).unwrap();
        fs::hard_link(&kept, &other).unwrap();
        let rescan = || {
            crate::scan::scan_with(
                &f.root,
                &f.catalog_path,
                crate::scan::ScanOptions::QUIET,
                &crate::decode::Tools::default(),
            )
            .unwrap();
        };
        rescan();
        let now = datetime!(2026-10-20 12:00 UTC);
        let quarantine = f.dir.path().join("quarantine");
        let plan_for = |f: &Fixture| {
            let catalog = Catalog::open(&f.catalog_path).unwrap();
            let report =
                crate::dupes::find_dupes(&catalog, &[], crate::dupes::Similarity::default())
                    .unwrap();
            assert_eq!(report.groups.len(), 1);
            plan_quarantine(&catalog, &f.catalog_path, &report, &[], &quarantine, now).unwrap()
        };
        let (plan, notes) = plan_for(&f);
        assert_eq!(plan.moves, vec![]);
        assert!(
            notes.warnings[0].contains("same file as the kept copy"),
            "{:?}",
            notes.warnings
        );

        // A real copy with the kept file's mtime is planned; it becomes the kept file's other
        // name before the plan is applied, so it still matches the plan, but does not move.
        fs::remove_file(&other).unwrap();
        fs::write(&other, b"one inode").unwrap();
        let kept_time = fs::metadata(&kept).unwrap().modified().unwrap();
        File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(kept_time)
            .unwrap();
        rescan();
        let (plan, _) = plan_for(&f);
        assert_eq!(plan.moves.len(), 1);
        assert_eq!(plan.moves[0].from, other);
        let plan_path = write_plan(&plan, now).unwrap();
        fs::remove_file(&other).unwrap();
        fs::hard_link(&kept, &other).unwrap();
        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.moved, 0);
        assert!(
            matches!(summary.skipped[0].reason, Skip::SameAsKeeper(_)),
            "{:?}",
            summary.skipped
        );
        assert!(kept.is_file() && other.is_file());
    }

    /// A journal file of the given records, next to the fixture's catalog.
    fn write_journal(f: &Fixture, name: &str, records: &[Record]) -> PathBuf {
        let path = f.dir.path().join(name);
        let lines: Vec<String> = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    fn start_of(catalog: &Path) -> Record {
        Record::Start {
            version: PLAN_VERSION,
            plan: PathBuf::from("crafted"),
            catalog: catalog.to_path_buf(),
        }
    }

    fn start(f: &Fixture) -> Record {
        start_of(&f.catalog_path)
    }

    /// An intent to move a file that is not cataloged.
    fn intent(index: usize, from: &Path, to: &Path, size: u64, mtime_ns: i64) -> Record {
        Record::Intent(Intent {
            index,
            planned: Move {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                stamp: Stamp { size, mtime_ns },
                kind: MoveKind::Sidecar { of: index },
            },
        })
    }

    fn via(index: usize, path: &Path, kind: ViaKind, source: Option<&Path>) -> Record {
        Record::Via {
            index,
            path: path.to_path_buf(),
            kind,
            source: source.map(Path::to_path_buf),
        }
    }

    #[test]
    fn test_is_temp_name() {
        assert!(is_temp_name(Path::new("/x/.pixrs-12-345-6.tmp")));
        assert!(!is_temp_name(Path::new("/x/.pixrs-12-345.tmp")));
        assert!(!is_temp_name(Path::new("/x/.pixrs-12-345-6.tmp.jpg")));
        assert!(!is_temp_name(Path::new("/x/pixrs-12-345-6.tmp")));
        assert!(!is_temp_name(Path::new("/x/.pixrs-1a-345-6.tmp")));
        assert!(!is_temp_name(Path::new("/x/photo.jpg")));
    }

    #[test]
    fn test_undo_only_touches_pixrs_temporary_files() {
        let f = fixture(&[("photo.jpg", b"x")]);
        let dir = f.dir.path().join("elsewhere");
        fs::create_dir(&dir).unwrap();
        let victim = dir.join("victim.txt");
        fs::write(&victim, b"precious").unwrap();
        let empty = dir.join("empty.txt");
        fs::write(&empty, b"").unwrap();
        // A temporary name in the wrong folder, and a copy of something else.
        let elsewhere = f.dir.path().join(".pixrs-1-2-3.tmp");
        fs::write(&elsewhere, b"").unwrap();
        let other = dir.join(".pixrs-1-2-4.tmp");
        fs::write(&other, b"half").unwrap();
        let (from, to) = (dir.join("from.jpg"), f.root.join("to.jpg"));
        let journal = write_journal(
            &f,
            "crafted.jsonl",
            &[
                start(&f),
                intent(0, &from, &to, 8, 0),
                via(0, &victim, ViaKind::Copy, Some(&victim)),
                via(0, &empty, ViaKind::Stop, None),
                via(0, &elsewhere, ViaKind::Stop, None),
                via(0, &other, ViaKind::Copy, Some(&victim)),
            ],
        );
        let summary = undo(&journal).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(fs::read(&empty).unwrap(), b"");
        assert_eq!(fs::read(&elsewhere).unwrap(), b"");
        assert_eq!(fs::read(&other).unwrap(), b"half");
        assert_eq!(summary.moved_back, 0);
        let left_alone = summary
            .warnings
            .iter()
            .filter(|w| w.ends_with("left alone"))
            .count();
        assert_eq!(left_alone, 4, "{:?}", summary.warnings);
    }

    #[test]
    fn test_undo_removes_only_a_copy_pixrs_made() {
        let f = fixture(&[("photo.jpg", b"x")]);
        let (from, to) = (f.root.join("a.jpg"), f.root.join("b").join("a.jpg"));
        fs::create_dir(to.parent().unwrap()).unwrap();
        fs::write(&from, b"the same bytes").unwrap();
        fs::write(&to, b"the same bytes").unwrap();
        let stamp = crate::scan::stamp(&fs::metadata(&from).unwrap()).unwrap();
        let temp = to.with_file_name(".pixrs-1-2-3.tmp");
        let journal = write_journal(
            &f,
            "copied.jsonl",
            &[
                start(&f),
                intent(0, &from, &to, stamp.size, stamp.mtime_ns),
                via(0, &temp, ViaKind::Copy, Some(&from)),
            ],
        );
        // Same bytes, but not the mtime pixrs gives a copy: another file. Both stay.
        let other_time = system_time(stamp.mtime_ns - 3_600_000_000_000);
        File::options()
            .write(true)
            .open(&to)
            .unwrap()
            .set_modified(other_time)
            .unwrap();
        let summary = undo(&journal).unwrap();
        assert!(to.exists() && from.exists());
        assert_eq!(summary.warnings.len(), 1, "{:?}", summary.warnings);
        // With the copy's mtime, it is the copy pixrs made, and goes.
        File::options()
            .write(true)
            .open(&to)
            .unwrap()
            .set_modified(system_time(stamp.mtime_ns))
            .unwrap();
        let summary = undo(&journal).unwrap();
        assert!(!to.exists() && from.exists());
        assert_eq!(summary.warnings, Vec::<String>::new());
    }

    #[test]
    fn test_undo_recreates_only_folders_a_moved_file_came_from() {
        let f = fixture(&[("photo.jpg", b"x")]);
        let from = f.root.join("gone").join("a.jpg");
        let unrelated = f.dir.path().join("unrelated");
        let journal = write_journal(
            &f,
            "rmdir.jsonl",
            &[
                start(&f),
                Record::Rmdir {
                    path: unrelated.clone(),
                },
                Record::Rmdir {
                    path: from.parent().unwrap().to_path_buf(),
                },
                intent(0, &from, &f.root.join("a.jpg"), 1, 0),
            ],
        );
        let summary = undo(&journal).unwrap();
        assert!(!unrelated.exists());
        assert!(from.parent().unwrap().is_dir());
        assert!(summary.warnings.iter().any(|w| w.contains("not recreated")));
    }

    #[test]
    fn test_apply_and_undo_need_an_existing_catalog() {
        let f = fixture(&[("photo.jpg", b"x")]);
        let missing = f.dir.path().join("nowhere").join("catalog.sqlite");
        let (plan_path, mut plan) = organize_plan(&f, "{type}/{name}.{ext}", false);
        plan.catalog.clone_from(&missing);
        assert!(matches!(
            apply(&plan_path, &plan).unwrap_err().error,
            Error::CatalogGone(_)
        ));
        assert!(!missing.parent().unwrap().exists());
        assert!(f.root.join("photo.jpg").exists());

        // A SQLite file that is not a catalog is left as it is.
        let foreign = f.dir.path().join("places.sqlite");
        let conn = rusqlite::Connection::open(&foreign).unwrap();
        conn.execute_batch(
            "CREATE TABLE moz_places (url TEXT); INSERT INTO moz_places VALUES ('x');",
        )
        .unwrap();
        drop(conn);
        let before = fs::read(&foreign).unwrap();
        let journal = write_journal(&f, "foreign.jsonl", &[start_of(&foreign)]);
        assert!(matches!(
            undo(&journal),
            Err(Error::UnknownSchema { version: 0, .. })
        ));
        assert_eq!(fs::read(&foreign).unwrap(), before);
        assert!(!f.dir.path().join("pixrs.lock").exists());
        let journal = write_journal(&f, "missing.jsonl", &[start_of(&missing)]);
        assert!(matches!(undo(&journal), Err(Error::CatalogGone(_))));
        assert!(!missing.parent().unwrap().exists());
    }

    #[test]
    fn test_a_faulty_plan_is_refused_before_anything_happens() {
        let f = fixture(FILES);
        let before = tree(&f.root);
        let (plan_path, mut plan) = organize_plan(&f, "{type}/{name}.{ext}", false);
        plan.moves[0].kind = MoveKind::Sidecar { of: 99 };
        let error = apply(&plan_path, &plan).unwrap_err();
        assert!(
            matches!(
                error.error,
                Error::Plan(crate::plan::PlanError::BadMove { .. })
            ),
            "{}",
            error.error
        );
        assert!(error.journal.is_none());
        assert_eq!(tree(&f.root), before);
        assert!(!state_dir(&f.catalog_path, "journals").exists());
        assert!(!state_dir(&f.catalog_path, "pixrs.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_a_destination_swapped_for_a_link_is_caught_once_a_run() {
        use std::os::unix::fs::symlink;
        let f = fixture(FILES);
        let sorted = f.dir.path().canonicalize().unwrap().join("sorted");
        let elsewhere = f.dir.path().join("elsewhere");
        fs::create_dir_all(&sorted).unwrap();
        fs::create_dir(&elsewhere).unwrap();
        let catalog = Catalog::open(&f.catalog_path).unwrap();
        let id = catalog.library_id(f.root.to_str().unwrap()).unwrap();
        let template = Template::parse("{year}/{name}.{ext}").unwrap();
        let now = datetime!(2026-10-20 12:00 UTC);
        let opts = OrganizeOptions {
            template: &template,
            undated: "undated",
            suffix: "_{n}",
            trust_dates: true,
            to: Some(sorted.clone()),
            prune_empty: false,
            now,
        };
        let (plan, _) = plan_organize(&catalog, &f.catalog_path, id, &f.root, &opts).unwrap();
        drop(catalog);
        let plan_path = write_plan(&plan, now).unwrap();
        // The folder the plan sorts into becomes a link before the plan is applied.
        fs::remove_dir(&sorted).unwrap();
        symlink(&elsewhere, &sorted).unwrap();
        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.moved, 0);
        assert!(
            summary
                .skipped
                .iter()
                .all(|s| matches!(s.reason, Skip::ThroughLink { .. } | Skip::MediaNotMoved)),
            "{:?}",
            summary.skipped
        );
        assert!(tree(&elsewhere).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_moves_through_a_symbolic_link_are_skipped() {
        use std::os::unix::fs::symlink;
        let f = fixture(&[("misc/IMG_20190501_143201.jpg", b"x")]);
        let outside = f.dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let (plan_path, plan) = organize_plan(&f, "{year}/{name}.{ext}", true);
        assert_eq!(plan.moves.len(), 1);
        // The target folder is a link by the time the plan is applied.
        symlink(&outside, f.root.join("2019")).unwrap();
        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.moved, 0);
        assert!(
            matches!(summary.skipped[0].reason, Skip::ThroughLink { .. }),
            "{:?}",
            summary.skipped
        );
        assert!(tree(&outside).is_empty());
        assert!(f.root.join("misc/IMG_20190501_143201.jpg").is_file());
        assert_eq!(undo(&summary.journal).unwrap().moved_back, 0);
        fs::remove_file(f.root.join("2019")).unwrap();

        // Applied for real, the folder the file came from is pruned; a link takes its place
        // before undo.
        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.moved, 1);
        assert!(!f.root.join("misc").exists());
        symlink(&outside, f.root.join("misc")).unwrap();
        let undone = undo(&summary.journal).unwrap();
        assert_eq!(undone.moved_back, 0);
        assert!(
            undone.warnings.iter().any(|w| w.contains("symbolic link")),
            "{:?}",
            undone.warnings
        );
        assert!(tree(&outside).is_empty());
        assert!(f.root.join("2019/IMG_20190501_143201.jpg").is_file());
    }

    #[test]
    fn test_a_journal_of_another_version_is_refused() {
        let f = fixture(&[("photo.jpg", b"x")]);
        let path = f.dir.path().join("old.jsonl");
        // A JSON string, so a Windows path keeps its backslashes escaped.
        let catalog = serde_json::to_string(&f.catalog_path).unwrap();
        for (start, found) in [
            (
                format!(r#"{{"event":"start","plan":"p","catalog":{catalog}}}"#),
                None,
            ),
            (
                format!(r#"{{"event":"start","version":2,"plan":"p","catalog":{catalog}}}"#),
                Some(2),
            ),
        ] {
            let intent = r#"{"event":"intent","index":0,"from":"a","to":"b","size":1}"#;
            fs::write(&path, format!("{start}\n{intent}\n")).unwrap();
            let error = undo(&path).unwrap_err();
            assert!(
                matches!(&error, Error::Journal(JournalError::Version { found: f, .. }) if *f == found),
                "{error}"
            );
            assert_eq!(error.exit_code(), exitcode::DATAERR);
        }
        fs::write(&path, "{\"event\":\"done\",\"index\":0}\n").unwrap();
        assert!(matches!(
            undo(&path),
            Err(Error::Journal(JournalError::NotAJournal(_)))
        ));
    }
}
