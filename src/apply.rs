//! Carrying out plans, with a journal of every step, and undoing them from the journal.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::catalog::{Catalog, CatalogRow};
use crate::error::Error;
use crate::plan::{state_dir, Keeper, Move, Plan};
use log::debug;
use ring::digest::{Context, SHA256};
use same_file::is_same_file;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
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
        plan: String,
        catalog: String,
    },
    /// A folder about to be created for a target.
    Mkdir {
        path: String,
    },
    /// A file about to be moved.
    Intent {
        index: usize,
        from: String,
        to: String,
        size: u64,
        mtime_ns: i64,
        cataloged: Option<(i64, String)>,
        cataloged_to: Option<(i64, String)>,
    },
    /// A temporary file pixrs created for a move, by `apply` or `undo`.
    Via {
        index: usize,
        path: String,
        kind: ViaKind,
        /// For a copy, the file it is copied from.
        source: Option<String>,
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
        path: String,
    },
    Finished,
}

/// What a temporary file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ViaKind {
    /// The file itself, stopped on its way during a rename that only changes letter case. It may
    /// be the only copy: never removed.
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
}

/// The lock that keeps `apply`, `undo` and `relocate` runs on one catalog apart. Removed when
/// dropped, or by `release_lock` when the process is stopped.
pub(crate) struct Lock(PathBuf);

/// The lock this process holds, for `release_lock`.
static HELD: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Remove the lock this process holds, if any. For the Ctrl-C handler, which exits without
/// running destructors; an interrupted `apply` is reversed with `undo`, which needs the lock.
pub fn release_lock() {
    if let Some(path) = HELD.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = fs::remove_file(path);
    }
}

impl Lock {
    pub(crate) fn take(catalog: &Path) -> Result<Lock, Error> {
        let path = state_dir(catalog, "pixrs.lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                *HELD.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.clone());
                Ok(Lock(path))
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(Error::Usage(format!(
                "{} exists: another `pixrs apply`, `undo` or `relocate` is running on this \
                 catalog. If none is (it crashed), delete the file and try again.",
                path.display()
            ))),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        if held.as_ref() == Some(&self.0) {
            held.take();
        }
        let _ = fs::remove_file(&self.0);
    }
}

/// Test hooks for `apply`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApplyOptions {
    /// Stop as if interrupted after this many files were moved.
    pub stop_after: Option<usize>,
    /// Stop as if interrupted after the first move, before the catalog follows it.
    pub stop_before_catalog: bool,
    /// Copy instead of renaming, as across file systems.
    pub force_copy: bool,
    /// Stop as if interrupted halfway through the first copy.
    pub stop_in_copy: bool,
    /// Stop as if interrupted halfway through the first rename that only changes letter case.
    pub stop_in_rename: bool,
}

/// What `apply` did.
#[derive(Debug, Default)]
pub struct ApplySummary {
    pub moved: usize,
    /// Files left where they were, and why.
    pub skipped: Vec<(String, String)>,
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
    Failed(io::Error),
    /// It failed halfway: the file may be at a temporary name, or in two places. Only undo
    /// should touch it now.
    Partial(io::Error),
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
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
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

/// Whether `a` and `b` differ only in letter case, so they are the same file on Windows and
/// macOS.
fn case_only(a: &Path, b: &Path) -> bool {
    a != b && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
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

/// Whether a file has `size` and (within `MTIME_SLACK_NS`) `mtime_ns`.
fn matches(meta: &fs::Metadata, size: u64, mtime_ns: i64) -> bool {
    meta.is_file()
        && crate::scan::stamp(meta)
            .is_ok_and(|s| s.size == size && (s.mtime_ns - mtime_ns).abs() <= MTIME_SLACK_NS)
}

/// Whether the file `part` is the start of (or all of) the file `whole`: a copy, stopped or
/// finished, of `whole`.
fn is_prefix_of(part: &Path, whole: &Path) -> io::Result<bool> {
    let (mut a, mut b) = (File::open(part)?, File::open(whole)?);
    if a.metadata()?.len() > b.metadata()?.len() {
        return Ok(false);
    }
    let (mut x, mut y) = (vec![0; 64 * 1024], vec![0; 64 * 1024]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

/// A free name next to `path` to keep a file under that undo cannot put back:
/// `name.pixrs-recovered.ext`, then `name.pixrs-recovered-2.ext` and so on.
fn recovered_name(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{}", ext)),
        _ => (name.to_string(), String::new()),
    };
    let mut n = 1;
    loop {
        let number = if n == 1 {
            String::new()
        } else {
            format!("-{}", n)
        };
        let candidate = path.with_file_name(format!("{}.pixrs-recovered{}{}", stem, number, ext));
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

/// SHA-256 of a file.
fn sha256_of(path: &Path) -> io::Result<Vec<u8>> {
    sha256_from(File::open(path)?)
}

/// SHA-256 of everything `reader` reads.
fn sha256_from(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut context = Context::new(&SHA256);
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => context.update(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(context.finish().as_ref().to_vec())
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
            debug!("Cannot flush the folder {}: {}", dir.display(), e);
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

/// Copy `from` into `copy`, the open temporary file reserved at `via`, flushed to disk, with
/// `from`'s mtime, check that the copy reads back the same, and return its SHA-256. All of it
/// goes through `copy`, never through the name. Permissions are copied later, onto the file in
/// its final place: a read-only temporary file could not be removed on Windows.
fn copy_verified(
    from: &Path,
    via: &Path,
    copy: &mut File,
    mtime: i64,
    stop_in_copy: bool,
) -> io::Result<Vec<u8>> {
    let mut source = File::open(from)?;
    let mut context = Context::new(&SHA256);
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let n = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        context.update(&buffer[..n]);
        copy.write_all(&buffer[..n])?;
        if stop_in_copy {
            return Err(io::Error::other("stopped in the middle of a copy"));
        }
    }
    copy.sync_all()?;
    copy.set_modified(system_time(mtime))?;
    let hash = context.finish().as_ref().to_vec();
    copy.seek(io::SeekFrom::Start(0))?;
    if sha256_from(copy)? != hash {
        return Err(io::Error::other(format!(
            "the copy {} does not match {}",
            via.display(),
            from.display()
        )));
    }
    Ok(hash)
}

/// Move `from` (which has `size` and `mtime`) to `to`, never overwriting.
///
/// A rename that only changes letter case goes through a temporary name: refused if another
/// file has exactly the new name, and undone if anything turns up at `to` once `from` is out of
/// the way. Otherwise `to` must not exist in any letter case. Renames if it can; across file
/// systems it copies to a temporary file in the target folder, checks the copy, renames it into
/// place, flushes the folder, checks that `from` did not change meanwhile, and only then
/// removes `from`. `note_via` is called with each temporary file before it is used.
fn move_file(
    from: &Path,
    to: &Path,
    size: u64,
    mtime: i64,
    opts: &ApplyOptions,
    mut note_via: impl FnMut(&Path, ViaKind, Option<&Path>) -> io::Result<()>,
) -> Result<(), MoveError> {
    let dir = to.parent().unwrap_or(Path::new("."));
    if case_only(from, to) {
        if exactly_there(to).unwrap_or(true) && !same_file(from, to) {
            return Err(MoveError::Taken);
        }
        let (via, reservation) = reserve_temp(dir).map_err(MoveError::Failed)?;
        // The file itself goes to `via`, by a rename that replaces the reservation.
        drop(reservation);
        if let Err(e) = note_via(&via, ViaKind::Stop, None).and_then(|()| fs::rename(from, &via)) {
            let _ = fs::remove_file(&via);
            return Err(MoveError::Failed(e));
        }
        if opts.stop_in_rename {
            return Err(MoveError::Partial(io::Error::other("stopped in a rename")));
        }
        // `from` is out of the way now, so anything at `to` is another file. Go back if the old
        // name is free; on Windows and macOS it is the same name as `to`, so the file waits at
        // `via` for undo instead of replacing the newcomer.
        if exists(to) {
            if exists(from) {
                return Err(MoveError::Partial(io::Error::other(format!(
                    "{} turned up during the rename; the file waits at {}",
                    to.display(),
                    via.display()
                ))));
            }
            return match fs::rename(&via, from) {
                Ok(()) => Err(MoveError::Taken),
                Err(e) => Err(MoveError::Partial(e)),
            };
        }
        return fs::rename(&via, to).map_err(MoveError::Partial);
    }
    if exists(to) {
        return Err(MoveError::Taken);
    }
    if !opts.force_copy {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(MoveError::Failed(e)),
        }
    }
    let (via, mut copy) = reserve_temp(dir).map_err(MoveError::Failed)?;
    let copied = note_via(&via, ViaKind::Copy, Some(from))
        .and_then(|()| copy_verified(from, &via, &mut copy, mtime, opts.stop_in_copy));
    let hash = match copied {
        Ok(hash) => hash,
        Err(e) if opts.stop_in_copy => {
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
        return Err(MoveError::Failed(io::Error::other(format!(
            "the temporary file {} was replaced by something else; left there",
            via.display()
        ))));
    }
    if let Err(e) = fs::rename(&via, to) {
        discard(&via, copy);
        return Err(MoveError::Failed(e));
    }
    // Through the open file, which is the one at `to` now, whatever the name leads to.
    if let Ok(meta) = fs::metadata(from) {
        let _ = copy.set_permissions(meta.permissions());
    }
    drop(copy);
    sync_dir(dir).map_err(MoveError::Partial)?;
    // An edit to `from` while it was copied would be lost with it.
    let unchanged = fs::symlink_metadata(from).is_ok_and(|m| matches(&m, size, mtime))
        && sha256_of(from).is_ok_and(|h| h == hash);
    if !unchanged {
        return Err(MoveError::Partial(io::Error::other(format!(
            "{} changed while it was copied; both are kept",
            from.display()
        ))));
    }
    remove_source(from).map_err(MoveError::Partial)
}

/// Why the file at `path` should not be moved, if it should not: gone, or not as expected.
fn changed(path: &Path, size: u64, mtime_ns: i64) -> Option<String> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Some(String::from("it is no longer there"));
    };
    if !meta.is_file() {
        return Some(String::from("it is not a file"));
    }
    let same = crate::scan::stamp(&meta).is_ok_and(|s| s.size == size && s.mtime_ns == mtime_ns);
    (!same).then(|| String::from("it changed since the plan was made"))
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
    let Some(base) = base.filter(|base| dir.starts_with(base)) else {
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
    for part in dir.strip_prefix(base).expect("a prefix").components() {
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
        Ok(true) => Err(MoveError::Failed(io::Error::other(format!(
            "{} is reached through a symbolic link",
            dir.display()
        )))),
        Err(e) => Err(MoveError::Failed(e)),
    }
}

/// Why a move must not happen, if it must not: a symbolic link on the way to its source or its
/// target would make the file come from, or go, somewhere else than the plan says. A link
/// inside a library (from an extracted archive, say) is not in the catalog, since the scan
/// does not follow links, so this is where it is caught.
fn misplaced(m: &Move, plan: &Plan, known: &mut KnownFolders) -> Option<String> {
    let (from, to) = (Path::new(&m.from), Path::new(&m.to));
    // `Plan::check` has seen to it that the source is in one of the plan's libraries and the
    // target below its destination; the parts below those are what a link can hide in.
    let root = plan
        .libraries
        .iter()
        .map(|l| Path::new(l.root.as_str()))
        .find(|root| from.starts_with(root));
    let destination = Some(Path::new(plan.destination.as_str()));
    for (what, dir, base) in [
        ("its folder", from.parent(), root),
        ("its target folder", to.parent(), destination),
    ] {
        let Some(dir) = dir else {
            continue;
        };
        match through_symlink(dir, base, known) {
            Ok(false) => {}
            Ok(true) => {
                return Some(format!(
                    "{} {} is reached through a symbolic link",
                    what,
                    dir.display()
                ))
            }
            Err(e) => return Some(format!("cannot check {}: {}", dir.display(), e)),
        }
    }
    None
}

/// Why a move should not happen now, if it should not.
fn problem(m: &Move, done: &[bool]) -> Option<String> {
    if let Some(media) = m.sidecar_of {
        if !done[media] {
            return Some(String::from("its media file was not moved"));
        }
    }
    if let Some(Keeper {
        path,
        size,
        mtime_ns,
    }) = &m.keeper
    {
        if changed(Path::new(path), *size, *mtime_ns).is_some() {
            return Some(format!("the kept copy {} is gone or changed", path));
        }
        if same_file(Path::new(path), Path::new(&m.from)) {
            return Some(format!("it is the same file as the kept copy {}", path));
        }
    }
    changed(Path::new(&m.from), m.size, m.mtime_ns)
}

/// A new journal for a plan, next to the catalog.
fn new_journal(plan_path: &Path, catalog: &Path) -> Result<(Journal, PathBuf), Error> {
    let dir = state_dir(catalog, "journals");
    fs::create_dir_all(&dir)?;
    let stem = plan_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("plan"));
    let mut path = dir.join(format!("{}.jsonl", stem));
    let mut n = 2;
    while exists(&path) {
        path = dir.join(format!("{}-{}.jsonl", stem, n));
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

/// Carry out the plan in `plan_path`.
pub fn apply(plan_path: &Path, plan: &Plan) -> Result<ApplySummary, ApplyError> {
    apply_with(plan_path, plan, &ApplyOptions::default())
}

/// `apply`, with test hooks.
pub fn apply_with(
    plan_path: &Path,
    plan: &Plan,
    opts: &ApplyOptions,
) -> Result<ApplySummary, ApplyError> {
    let mut journal = None;
    apply_journaled(plan_path, plan, opts, &mut journal)
        .map_err(|error| ApplyError { error, journal })
}

fn apply_journaled(
    plan_path: &Path,
    plan: &Plan,
    opts: &ApplyOptions,
    journal_out: &mut Option<PathBuf>,
) -> Result<ApplySummary, Error> {
    // `read_plan` checks too; a plan built in code is checked here.
    plan.check()?;
    let catalog_path = Path::new(&plan.catalog);
    let catalog = Catalog::open_for_update(catalog_path)?;
    let _lock = Lock::take(catalog_path)?;
    let (mut journal, journal_path) = new_journal(plan_path, catalog_path)?;
    *journal_out = Some(journal_path.clone());
    let mut summary = ApplySummary {
        journal: journal_path,
        ..ApplySummary::default()
    };
    journal.write(&Record::Start {
        plan: plan_path.to_string_lossy().into_owned(),
        catalog: plan.catalog.clone(),
    })?;

    let mut done = vec![false; plan.moves.len()];
    let mut known = KnownFolders::default();
    for (index, m) in plan.moves.iter().enumerate() {
        let (from, to) = (Path::new(&m.from), Path::new(&m.to));
        let mut skip = |journal: &mut Journal, reason: String| -> Result<(), Error> {
            journal.write(&Record::Skipped {
                index,
                reason: reason.clone(),
            })?;
            summary.skipped.push((m.from.clone(), reason));
            Ok(())
        };
        if let Some(reason) = problem(m, &done) {
            skip(&mut journal, reason)?;
            continue;
        }
        if let Some(reason) = misplaced(m, plan, &mut known) {
            skip(&mut journal, reason)?;
            continue;
        }
        if let Err(e) = create_folders(to.parent(), &mut journal, &mut known) {
            skip(&mut journal, format!("cannot create its folder: {}", e))?;
            continue;
        }
        journal.write(&Record::Intent {
            index,
            from: m.from.clone(),
            to: m.to.clone(),
            size: m.size,
            mtime_ns: m.mtime_ns,
            cataloged: m.cataloged.clone(),
            cataloged_to: m.cataloged_to.clone(),
        })?;
        let moved = move_file(from, to, m.size, m.mtime_ns, opts, |via, kind, source| {
            journal.write(&Record::Via {
                index,
                path: via.to_string_lossy().into_owned(),
                kind,
                source: source.map(|s| s.to_string_lossy().into_owned()),
            })
        });
        match moved {
            Ok(()) => {}
            Err(MoveError::Taken) => {
                skip(&mut journal, format!("{} is taken", m.to))?;
                continue;
            }
            Err(MoveError::Failed(e)) => {
                skip(&mut journal, e.to_string())?;
                continue;
            }
            Err(MoveError::Partial(e)) => return Err(e.into()),
        }
        journal.write(&Record::Done { index })?;
        done[index] = true;
        summary.moved += 1;
        if opts.stop_before_catalog {
            return Err(Error::Usage(String::from("stopped as asked")));
        }
        update_catalog(&catalog, &mut journal, index, m)?;
        if opts.stop_after == Some(summary.moved) {
            return Err(Error::Usage(String::from("stopped as asked")));
        }
    }

    if plan.prune_empty {
        let roots: Vec<PathBuf> = plan
            .libraries
            .iter()
            .map(|l| PathBuf::from(&l.root))
            .collect();
        let emptied: BTreeSet<PathBuf> = plan
            .moves
            .iter()
            .zip(&done)
            .filter(|(_, &d)| d)
            .filter_map(|(m, _)| Path::new(&m.from).parent().map(Path::to_path_buf))
            .collect();
        // Deepest first, so a folder's subfolders go before it.
        for dir in emptied.iter().rev() {
            let mut dir = dir.clone();
            while roots.iter().any(|r| dir.starts_with(r) && dir != *r) && is_empty_dir(&dir) {
                journal.write(&Record::Rmdir {
                    path: dir.to_string_lossy().into_owned(),
                })?;
                fs::remove_dir(&dir)?;
                summary.folders_removed += 1;
                if !dir.pop() {
                    break;
                }
            }
        }
    }
    journal.write(&Record::Finished)?;
    Ok(summary)
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
            path: folder.to_string_lossy().into_owned(),
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
    let Some((library_id, path)) = &m.cataloged else {
        return Ok(());
    };
    match &m.cataloged_to {
        // A row at the target is stale: the file there was gone, or the move would not have
        // happened.
        Some((new_library_id, new_path)) => {
            catalog.rename_row(*library_id, path, *new_library_id, new_path, true)?;
        }
        None => {
            // The row is only gone once the journal has it.
            let tx = catalog.transaction()?;
            if let Some(row) = catalog.take_row(*library_id, path)? {
                journal.write(&Record::Row { index, row })?;
            }
            tx.commit()?;
        }
    }
    Ok(())
}

/// The records of a journal. A last line cut short by a crash is ignored.
fn read_journal(path: &Path) -> Result<Vec<Record>, Error> {
    let file = File::open(path)?;
    let lines: Vec<String> = BufReader::new(file).lines().collect::<Result<_, _>>()?;
    let mut records = vec![];
    for (n, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if n + 1 == lines.len() => {}
            Err(e) => {
                return Err(Error::Usage(format!(
                    "{} line {} is not a journal record: {}",
                    path.display(),
                    n + 1,
                    e
                )))
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

/// Where the file of a journaled move is. Only a copy pixrs made itself (a temporary copy is
/// journaled for the move, and the two names are different files with the same contents)
/// counts as a copy that may be removed.
fn locate(from: &Path, to: &Path, size: u64, mtime_ns: i64, had_copy: bool) -> io::Result<Found> {
    let neither = || {
        Found::Unclear(format!(
            "{} is neither at {} nor back at its old place",
            from.display(),
            to.display()
        ))
    };
    if case_only(from, to) {
        // One file under two spellings on Windows and macOS: look at the exact names.
        return Ok(match (exactly_there(to)?, exactly_there(from)?) {
            (true, false) if fs::metadata(to).is_ok_and(|m| matches(&m, size, mtime_ns)) => {
                Found::AtTarget
            }
            (true, false) => Found::Unclear(format!(
                "{} is not the file that was moved there; left there",
                to.display()
            )),
            // Two names for one file (Windows, macOS), or two different files (Linux).
            (true, true) if same_file(from, to) => Found::AtSource,
            (true, true) => Found::Unclear(format!(
                "{} and {} both exist; left as they are",
                from.display(),
                to.display()
            )),
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
            if had_copy && matches(&meta, size, mtime_ns) && sha256_of(to)? == sha256_of(from)? {
                Found::Copied
            } else {
                Found::Unclear(format!(
                    "{} and {} both exist; left as they are",
                    from.display(),
                    to.display()
                ))
            }
        }
        // Moves keep the mtime, so a file there with another one is not the moved file.
        (Some(meta), false) if matches(&meta, size, mtime_ns) => Found::AtTarget,
        (Some(_), false) => Found::Unclear(format!(
            "{} is not the file that was moved there (it changed, or is another file); left \
             there",
            to.display()
        )),
    })
}

/// Deal with a temporary file left by an interrupted move of a file of `size` and `mtime`,
/// without ever losing data.
///
/// An empty file is a reservation that was never used (unless the moved file is empty too) and
/// goes. A stopped rename is the file itself: it goes back to `from` if it is the moved file
/// and `from` is free. A copy is removed only if its source still has every byte of it.
/// Anything else is kept next to the temporary file, as `name.pixrs-recovered.ext`.
fn clean_up_via(via: &ViaFile, from: &Path, size: u64, mtime: i64, warnings: &mut Vec<String>) {
    let Ok(meta) = fs::symlink_metadata(&via.path) else {
        return;
    };
    let unused = meta.len() == 0 && size != 0;
    let redundant = via.kind == ViaKind::Copy
        && via
            .source
            .as_deref()
            .is_some_and(|source| is_prefix_of(&via.path, source).unwrap_or(false));
    let result = if unused || redundant {
        fs::remove_file(&via.path)
    } else if via.kind == ViaKind::Stop && matches(&meta, size, mtime) && !exists(from) {
        fs::rename(&via.path, from)
    } else {
        // Next to the temporary file, so on its file system.
        let keep = recovered_name(
            &via.path
                .with_file_name(from.file_name().unwrap_or_default()),
        );
        fs::rename(&via.path, &keep).map(|()| {
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
    if complete < contents.len() {
        if serde_json::from_slice::<Record>(&contents[complete..]).is_ok() {
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

/// A temporary file and what it is, from a `Via` record.
struct ViaFile {
    path: PathBuf,
    kind: ViaKind,
    source: Option<PathBuf>,
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
    let source_ok = match (via.kind, via.source.as_deref()) {
        (ViaKind::Copy, Some(source)) => source == from || source == to,
        (ViaKind::Copy, None) => false,
        (ViaKind::Stop, _) => true,
    };
    is_temp_name(&via.path) && in_place && source_ok
}

/// Reverse everything the journal at `path` records, as far as it got. Safe to run again, also
/// after a crash: it looks at where each file actually is, and journals its own steps.
pub fn undo(path: &Path) -> Result<UndoSummary, Error> {
    let records = read_journal(path)?;
    let Some(Record::Start { catalog, .. }) = records.first() else {
        return Err(Error::Usage(format!(
            "{} does not start like a pixrs journal",
            path.display()
        )));
    };
    let catalog_path = PathBuf::from(catalog);
    let catalog = Catalog::open_for_update(&catalog_path)?;
    let _lock = Lock::take(&catalog_path)?;
    let mut journal = append_to(path)?;
    let mut summary = UndoSummary::default();

    let mut intents: Vec<&Record> = vec![];
    let mut vias: HashMap<usize, Vec<ViaFile>> = HashMap::new();
    let mut rows: HashMap<usize, &CatalogRow> = HashMap::new();
    let mut skipped: HashSet<usize> = HashSet::new();
    let mut created: Vec<PathBuf> = vec![];
    let mut removed: Vec<PathBuf> = vec![];
    for record in &records {
        match record {
            Record::Intent { .. } => intents.push(record),
            Record::Via {
                index,
                path,
                kind,
                source,
            } => {
                vias.entry(*index).or_default().push(ViaFile {
                    path: PathBuf::from(path),
                    kind: *kind,
                    source: source.as_deref().map(PathBuf::from),
                });
            }
            Record::Row { index, row } => {
                rows.insert(*index, row);
            }
            Record::Skipped { index, .. } => {
                skipped.insert(*index);
            }
            Record::Mkdir { path } => created.push(PathBuf::from(path)),
            Record::Rmdir { path } => removed.push(PathBuf::from(path)),
            _ => {}
        }
    }

    // Folders the moves emptied come back first, so files can go back into them. Only folders
    // a moved file came out of were ever removed, so only those come back.
    let sources: Vec<&Path> = intents
        .iter()
        .filter_map(|record| match record {
            Record::Intent { from, .. } => Some(Path::new(from)),
            _ => None,
        })
        .collect();
    for dir in removed.iter().rev() {
        if !sources
            .iter()
            .any(|from| from.starts_with(dir) && *from != dir)
        {
            summary.warnings.push(format!(
                "{} is not a folder a moved file came from; not recreated",
                dir.display()
            ));
            continue;
        }
        if let Err(e) = fs::create_dir_all(dir) {
            summary
                .warnings
                .push(format!("Cannot recreate {}: {}", dir.display(), e));
        }
    }
    let opts = ApplyOptions::default();
    let mut known = KnownFolders::default();
    for record in intents.iter().rev() {
        let Record::Intent {
            index,
            from,
            to,
            size,
            mtime_ns,
            cataloged,
            cataloged_to,
        } = record
        else {
            continue;
        };
        let (from, to) = (Path::new(from), Path::new(to));
        let mut index_vias = vias.remove(index).unwrap_or_default();
        index_vias.retain(|via| {
            let ours = via_belongs_to(via, from, to);
            if !ours {
                summary.warnings.push(format!(
                    "{} is not a temporary file of the move of {}; left alone",
                    via.path.display(),
                    from.display()
                ));
            }
            ours
        });
        for via in &index_vias {
            clean_up_via(via, from, *size, *mtime_ns, &mut summary.warnings);
        }
        if skipped.contains(index) {
            continue;
        }
        let had_copy = index_vias.iter().any(|v| v.kind == ViaKind::Copy);
        let found = match locate(from, to, *size, *mtime_ns, had_copy) {
            Ok(found) => found,
            Err(e) => Found::Unclear(format!("Cannot check {}: {}", to.display(), e)),
        };
        let back = match found {
            Found::AtTarget => {
                // Without a plan, the folder it goes back to must at least be where its path
                // says, as the roots plans are made from are.
                let back_to = from.parent().unwrap_or(Path::new("."));
                let moved = link_free(back_to, &mut known).and_then(|()| {
                    move_file(to, from, *size, *mtime_ns, &opts, |via, kind, source| {
                        journal.write(&Record::Via {
                            index: *index,
                            path: via.to_string_lossy().into_owned(),
                            kind,
                            source: source.map(|s| s.to_string_lossy().into_owned()),
                        })
                    })
                });
                match moved {
                    Ok(()) => {
                        summary.moved_back += 1;
                        true
                    }
                    Err(MoveError::Taken) => {
                        summary.warnings.push(format!(
                            "{} stays: {} is taken",
                            to.display(),
                            from.display()
                        ));
                        false
                    }
                    Err(MoveError::Failed(e) | MoveError::Partial(e)) => {
                        summary
                            .warnings
                            .push(format!("Cannot move {} back: {}", to.display(), e));
                        false
                    }
                }
            }
            Found::Copied => match remove_source(to) {
                Ok(()) => true,
                Err(e) => {
                    summary.warnings.push(format!(
                        "Cannot remove the copy {}: {}",
                        to.display(),
                        e
                    ));
                    false
                }
            },
            Found::AtSource => true,
            Found::Unclear(warning) => {
                summary.warnings.push(warning);
                false
            }
        };
        // The catalog follows only a file that is back, and only where a row still points at
        // the target, so running undo again changes nothing.
        if let (true, Some((library_id, path))) = (back, cataloged) {
            match (cataloged_to, rows.get(index)) {
                (Some((new_library_id, new_path)), _) => {
                    catalog.rename_row(*new_library_id, new_path, *library_id, path, false)?;
                }
                (None, Some(row)) if !catalog.has_row(*library_id, path)? => {
                    catalog.put_row(row)?;
                }
                _ => {}
            }
        }
    }
    for dir in created.iter().rev() {
        if is_empty_dir(dir) {
            let _ = fs::remove_dir(dir);
        }
    }
    Ok(summary)
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
            stop_after: Some(2),
            ..ApplyOptions::default()
        };
        assert!(apply_with(&plan_path, &plan, &opts).is_err());
        assert_ne!(tree(&f.root), before);
        undo(&journal).unwrap();
        assert_eq!(tree(&f.root), before);

        // Stopped halfway through a copy across file systems.
        let opts = ApplyOptions {
            force_copy: true,
            stop_in_copy: true,
            ..ApplyOptions::default()
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
        let skipped: Vec<&str> = summary.skipped.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(skipped.len(), 2, "{:?}", skipped);
        assert!(skipped.contains(&"it changed since the plan was made"));
        assert!(skipped.iter().any(|r| r.ends_with("is taken")));
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
                .filter(|k| k.starts_with(&format!("lib/{}/", moved_from)))
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
            assert_eq!(summary.skipped, vec![], "force_copy {}", force_copy);
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
        assert!(matches!(Lock::take(&catalog), Err(Error::Usage(_))));
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
        let hash = copy_verified(&from, &via, &mut copy, 0, false).unwrap();
        assert_eq!(hash, sha256_of(&from).unwrap());
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
        copy_verified(&from, &via, &mut copy, 0, false).unwrap();
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
            plan: String::from("p"),
            catalog: String::from("c"),
        })
        .unwrap();
        fs::write(&path, format!("{}\n{{\"event\":\"int", start)).unwrap();
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
            stop_before_catalog: true,
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
        let keeper = plan.moves[0].keeper.clone().unwrap();
        let plan_path = write_plan(&plan, now).unwrap();
        // The kept copy is gone before the plan is applied: nothing moves.
        fs::rename(&keeper.path, f.dir.path().join("elsewhere.jpg")).unwrap();
        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.moved, 0);
        assert!(summary.skipped[0].1.contains("kept copy"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_crash_in_a_case_only_rename_keeps_the_file() {
        let f = fixture(&[("Upper.JPG", b"original")]);
        let (plan_path, plan) = organize_plan(&f, "{name}.{ext}", false);
        let opts = ApplyOptions {
            stop_in_rename: true,
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
            stop_after: Some(1),
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
            path: via.clone(),
            kind: ViaKind::Copy,
            source: Some(source.clone()),
        };
        let mut warnings = vec![];
        clean_up_via(&copy, &from, 20, 0, &mut warnings);
        assert!(!from.exists());
        assert_eq!(
            fs::read(dir.path().join("photo.pixrs-recovered.jpg")).unwrap(),
            b"half a pho"
        );
        assert_eq!(warnings.len(), 1);

        // With its source still there, a partial copy is redundant.
        fs::write(&via, b"half").unwrap();
        fs::write(&source, b"half and the rest").unwrap();
        clean_up_via(&copy, &from, 20, 0, &mut warnings);
        assert!(!via.exists());

        // An unused, empty reservation for a stopped rename goes; it is not the file.
        fs::write(&via, b"").unwrap();
        let stop = ViaFile {
            path: via.clone(),
            kind: ViaKind::Stop,
            source: None,
        };
        clean_up_via(&stop, &from, 20, 0, &mut warnings);
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
        assert_eq!(plan.moves[0].from, other.to_str().unwrap());
        let plan_path = write_plan(&plan, now).unwrap();
        fs::remove_file(&other).unwrap();
        fs::hard_link(&kept, &other).unwrap();
        let summary = apply(&plan_path, &plan).unwrap();
        assert_eq!(summary.moved, 0);
        assert!(
            summary.skipped[0].1.contains("same file as the kept copy"),
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

    fn start(f: &Fixture) -> Record {
        Record::Start {
            plan: String::from("crafted"),
            catalog: f.catalog_path.to_string_lossy().into_owned(),
        }
    }

    fn intent(index: usize, from: &Path, to: &Path, size: u64, mtime_ns: i64) -> Record {
        Record::Intent {
            index,
            from: from.to_string_lossy().into_owned(),
            to: to.to_string_lossy().into_owned(),
            size,
            mtime_ns,
            cataloged: None,
            cataloged_to: None,
        }
    }

    fn via(index: usize, path: &Path, kind: ViaKind, source: Option<&Path>) -> Record {
        Record::Via {
            index,
            path: path.to_string_lossy().into_owned(),
            kind,
            source: source.map(|s| s.to_string_lossy().into_owned()),
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
                    path: unrelated.to_string_lossy().into_owned(),
                },
                Record::Rmdir {
                    path: from.parent().unwrap().to_string_lossy().into_owned(),
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
        plan.catalog = missing.to_string_lossy().into_owned();
        assert!(matches!(
            apply(&plan_path, &plan).unwrap_err().error,
            Error::Usage(_)
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
        let journal = write_journal(
            &f,
            "foreign.jsonl",
            &[Record::Start {
                plan: String::from("crafted"),
                catalog: foreign.to_string_lossy().into_owned(),
            }],
        );
        assert!(matches!(undo(&journal), Err(Error::Usage(_))));
        assert_eq!(fs::read(&foreign).unwrap(), before);
        assert!(!f.dir.path().join("pixrs.lock").exists());
        let journal = write_journal(
            &f,
            "missing.jsonl",
            &[Record::Start {
                plan: String::from("crafted"),
                catalog: missing.to_string_lossy().into_owned(),
            }],
        );
        assert!(matches!(undo(&journal), Err(Error::Usage(_))));
        assert!(!missing.parent().unwrap().exists());
    }

    #[test]
    fn test_a_faulty_plan_is_refused_before_anything_happens() {
        let f = fixture(FILES);
        let before = tree(&f.root);
        let (plan_path, mut plan) = organize_plan(&f, "{type}/{name}.{ext}", false);
        plan.moves[0].sidecar_of = Some(99);
        let error = apply(&plan_path, &plan).unwrap_err();
        assert!(matches!(error.error, Error::Usage(_)), "{}", error.error);
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
        let reasons: Vec<&str> = summary.skipped.iter().map(|(_, r)| r.as_str()).collect();
        assert!(
            reasons
                .iter()
                .all(|r| r.contains("symbolic link") || *r == "its media file was not moved"),
            "{:?}",
            reasons
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
            summary.skipped[0].1.contains("symbolic link"),
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
}
