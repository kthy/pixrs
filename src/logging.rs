//! Log lines on stderr, written around the progress bar on screen so that they do not break it.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use indicatif::{ProgressBar, WeakProgressBar};
use log::{Log, Metadata, Record};
use std::sync::{Mutex, PoisonError};

/// The progress bar last put on screen. Weak, so a dropped bar is not kept alive by it.
static BAR: Mutex<Option<WeakProgressBar>> = Mutex::new(None);

/// What is logged without `RUST_LOG`: pixrs's own errors, or with `debug` (`--debug`) its debug
/// output too. Dependencies log errors pixrs already reports (nom-exif on truncated files), so
/// they are left out. `--debug` only counts in debug builds.
pub fn default_filter(debug: bool) -> &'static str {
    if debug && cfg!(debug_assertions) {
        "pixrs=debug"
    } else {
        "pixrs=error"
    }
}

/// Log to stderr, filtered by `RUST_LOG` or else by `default_filter(debug)`. Only the first call
/// sets the logger.
pub fn init(debug: bool) {
    let env = env_logger::Env::default().default_filter_or(default_filter(debug));
    let logger = env_logger::Builder::from_env(env).build();
    let max_level = logger.filter();
    if log::set_boxed_logger(Box::new(AroundBar(logger))).is_ok() {
        log::set_max_level(max_level);
    }
}

/// Write log lines around `bar` while it is on screen: it is cleared for each line and drawn
/// again below it. Nothing may log inside `bar.suspend`, since logging suspends it too.
pub(crate) fn log_around(bar: &ProgressBar) {
    *BAR.lock().unwrap_or_else(PoisonError::into_inner) = Some(bar.downgrade());
}

/// The progress bar on screen, if there is one.
fn on_screen() -> Option<ProgressBar> {
    let bar = BAR
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()?
        .upgrade()?;
    (!bar.is_finished() && !bar.is_hidden()).then_some(bar)
}

/// `env_logger`, suspending the progress bar on screen for each line it writes.
struct AroundBar(env_logger::Logger);

impl Log for AroundBar {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.0.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.0.matches(record) {
            return;
        }
        match on_screen() {
            Some(bar) => bar.suspend(|| self.0.log(record)),
            None => self.0.log(record),
        }
    }

    fn flush(&self) {
        self.0.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn test_debug_filter_only_in_debug_builds() {
        assert_eq!(default_filter(false), "pixrs=error");
        let debug = if cfg!(debug_assertions) {
            "pixrs=debug"
        } else {
            "pixrs=error"
        };
        assert_eq!(default_filter(true), debug);
    }
}
