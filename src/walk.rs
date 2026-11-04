//! Walking a library on several threads, for `scan`.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::catalog::Stamp;
use crate::media::MediaType;
use crate::scan::stamp;
use std::fs::{self, DirEntry};
use std::io;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

/// Files a worker stats in one job. A folder holding thousands of files, as phones and cameras
/// leave them, is stat'ed by several workers at once instead of by the one that listed it.
/// Each batch holds its folder open (`DirEntry` stats relative to it, which on a network file
/// system saves looking up every folder of the path again), so workers take batches before
/// folders to list, and only a few folders are open at a time.
const STAT_BATCH: usize = 64;

/// A media file the walk found.
#[derive(Debug)]
pub(crate) struct MediaFile {
    pub path: PathBuf,
    pub media_type: MediaType,
    /// Its size and modification time, or why they could not be read.
    pub stamp: io::Result<Stamp>,
}

/// What the walk found.
#[derive(Debug)]
pub(crate) enum Found {
    Media(MediaFile),
    /// A folder that could not be listed, or not completely, or an entry whose type could not be
    /// read.
    Unreadable {
        path: PathBuf,
        error: io::Error,
    },
}

/// Walk the folder `start` on up to `workers` threads, without following symbolic links. Calls
/// `listing` with each folder as a worker starts listing it (on that worker's thread), and passes
/// each media file and each failure to `found` on this thread, in no particular order.
pub(crate) fn walk(
    start: &Path,
    workers: usize,
    listing: impl Fn(&Path) + Sync,
    mut found: impl FnMut(Found),
) {
    let queue = Queue::new(start.to_path_buf());
    let (tx, rx) = mpsc::sync_channel(1024);
    thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            let (tx, queue, listing) = (tx.clone(), &queue, &listing);
            scope.spawn(move || {
                while let Some(job) = queue.next() {
                    let _busy = Busy(queue);
                    match job {
                        Job::List(dir) => {
                            listing(&dir);
                            list(&dir, queue, &tx);
                        }
                        Job::Stat(files) => {
                            for (entry, media_type) in files {
                                let stamp = entry.metadata().and_then(|m| stamp(&m));
                                let path = entry.path();
                                let file = MediaFile {
                                    path,
                                    media_type,
                                    stamp,
                                };
                                send(&tx, Found::Media(file));
                            }
                        }
                    }
                }
            });
        }
        drop(tx);
        for item in rx {
            found(item);
        }
    });
}

/// List the folder `dir`: queue its subfolders, and its media files in batches to be stat'ed.
fn list(dir: &Path, queue: &Queue, tx: &mpsc::SyncSender<Found>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            let path = dir.to_path_buf();
            return send(tx, Found::Unreadable { path, error });
        }
    };
    let mut batch = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // The rest of the folder cannot be listed; its cataloged files stay.
                let path = dir.to_path_buf();
                send(tx, Found::Unreadable { path, error });
                break;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                let path = entry.path();
                send(tx, Found::Unreadable { path, error });
                continue;
            }
        };
        // Symbolic links are not followed, so they are neither folders nor files here.
        if file_type.is_dir() {
            queue.push(Job::List(entry.path()));
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Some(media_type) = MediaType::from_path(Path::new(&entry.file_name())) else {
            continue;
        };
        batch.push((entry, media_type));
        if batch.len() == STAT_BATCH {
            queue.push(Job::Stat(mem::take(&mut batch)));
        }
    }
    if !batch.is_empty() {
        queue.push(Job::Stat(batch));
    }
}

/// Pass `found` to the walk's thread. An error means it has stopped receiving, which only
/// happens when it panicked; the workers then finish their queue without sending.
fn send(tx: &mpsc::SyncSender<Found>, found: Found) {
    let _ = tx.send(found);
}

/// Work for the walk's threads.
enum Job {
    /// A folder to list.
    List(PathBuf),
    /// Media files to stat, all in one folder.
    Stat(Vec<(DirEntry, MediaType)>),
}

/// The jobs not yet taken, and how many are being worked on: the walk is done when there are
/// none of either.
struct Queue {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    lists: Vec<PathBuf>,
    stats: Vec<Vec<(DirEntry, MediaType)>>,
    busy: usize,
}

impl State {
    /// The next job: files to stat before folders to list, the newest first, which walks depth
    /// first and keeps the queue short.
    fn pop(&mut self) -> Option<Job> {
        self.stats
            .pop()
            .map(Job::Stat)
            .or_else(|| self.lists.pop().map(Job::List))
    }

    const fn is_empty(&self) -> bool {
        self.lists.is_empty() && self.stats.is_empty()
    }
}

impl Queue {
    fn new(start: PathBuf) -> Self {
        Self {
            state: Mutex::new(State {
                lists: vec![start],
                stats: vec![],
                busy: 0,
            }),
            changed: Condvar::new(),
        }
    }

    /// The state, also after a worker panicked while holding it: the counts stay right, since
    /// `Busy` still drops.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, job: Job) {
        let mut state = self.lock();
        match job {
            Job::List(dir) => state.lists.push(dir),
            Job::Stat(files) => state.stats.push(files),
        }
        drop(state);
        self.changed.notify_one();
    }

    /// The next job, waiting while other workers may still queue some. `None` once all are done.
    fn next(&self) -> Option<Job> {
        let mut state = self.lock();
        loop {
            if let Some(job) = state.pop() {
                state.busy += 1;
                return Some(job);
            }
            if state.busy == 0 {
                self.changed.notify_all();
                return None;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// A job being worked on; done when dropped, also if the work panics.
struct Busy<'q>(&'q Queue);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.busy -= 1;
        if state.busy == 0 && state.is_empty() {
            self.0.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;

    fn found(start: &Path, workers: usize) -> (BTreeSet<PathBuf>, Vec<PathBuf>) {
        let (mut media, mut unreadable) = (BTreeSet::new(), vec![]);
        walk(
            start,
            workers,
            |_| {},
            |found| match found {
                Found::Media(file) => {
                    file.stamp.unwrap();
                    assert!(media.insert(file.path), "found twice");
                }
                Found::Unreadable { path, .. } => unreadable.push(path),
            },
        );
        (media, unreadable)
    }

    #[test]
    fn test_finds_every_media_file_once_with_any_number_of_workers() {
        let dir = tempfile::tempdir().unwrap();
        let mut expected = BTreeSet::new();
        // A deep chain, a wide folder of several stat batches, and empty folders.
        let mut deep = dir.path().to_path_buf();
        for depth in 0..20 {
            deep.push(format!("d{depth}"));
            fs::create_dir_all(&deep).unwrap();
            let file = deep.join(format!("{depth}.jpg"));
            fs::write(&file, b"x").unwrap();
            expected.insert(file);
        }
        let wide = dir.path().join("wide");
        fs::create_dir(&wide).unwrap();
        for i in 0..STAT_BATCH * 3 + 5 {
            let file = wide.join(format!("{i}.mp4"));
            fs::write(&file, b"x").unwrap();
            expected.insert(file);
            fs::write(wide.join(format!("{i}.txt")), b"not media").unwrap();
        }
        fs::create_dir_all(dir.path().join("empty/inside")).unwrap();
        for workers in [1, 2, 7, 32] {
            let (media, unreadable) = found(dir.path(), workers);
            assert_eq!(media, expected, "{workers} workers");
            assert_eq!(unreadable, Vec::<PathBuf>::new());
        }
    }

    #[test]
    fn test_a_missing_start_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let start = dir.path().join("gone");
        let (media, unreadable) = found(&start, 4);
        assert!(media.is_empty());
        assert_eq!(unreadable, vec![start]);
    }

    #[cfg(unix)]
    #[test]
    fn test_symbolic_links_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("a.jpg"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, lib.join("folder")).unwrap();
        std::os::unix::fs::symlink(outside.join("a.jpg"), lib.join("b.jpg")).unwrap();
        let (media, unreadable) = found(&lib, 4);
        assert!(media.is_empty(), "{media:?}");
        assert_eq!(unreadable, Vec::<PathBuf>::new());
    }
}
