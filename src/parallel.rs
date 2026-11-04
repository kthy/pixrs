//! Work spread over threads, with the results handled on the calling thread.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{self, available_parallelism};

/// Up to `max` threads, but no more than there are cores: for work that keeps a core busy.
pub(crate) fn per_core(max: usize) -> usize {
    available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(max)
}

/// `work` on each of `items` on up to `workers` threads, passing each item with its result to
/// `record` on this thread, in the order they finish. `record` is where the catalog is written:
/// it has one connection, and one writer. Stops at the first error from `record`.
pub(crate) fn in_parallel<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    work: impl Fn(&T) -> R + Sync,
    mut record: impl FnMut(&T, R) -> Result<(), Error>,
) -> Result<(), Error> {
    let workers = workers.clamp(1, items.len().max(1));
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
                if tx.send((item, work(item))).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        // Leaving early drops `rx`, which makes the workers stop at their next send.
        for (item, result) in rx {
            record(item, result)?;
        }
        Ok(())
    })
}
