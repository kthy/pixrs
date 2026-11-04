//! Progress bars for the steps of long commands.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use indicatif::{HumanDuration, ProgressBar, ProgressState, ProgressStyle};
use std::fmt;
use std::time::Duration;

/// What a progress bar counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unit {
    Files,
    Bytes,
}

/// A progress bar of `len` files or bytes. The message is cut to fit the terminal: a line that
/// wraps is not cleared when the bar is drawn again.
#[expect(
    clippy::expect_used,
    reason = "the templates are constants; test_progress_templates_parse checks them"
)]
pub(crate) fn progress_bar(
    len: u64,
    prefix: &'static str,
    unit: Unit,
    quiet: bool,
) -> ProgressBar {
    if quiet {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(len).with_prefix(prefix);
    let t = match unit {
        Unit::Bytes => {
            "{prefix} [{elapsed}, ETA {eta}] {bar:15.white/grey} {bytes:>10}/{total_bytes} \
             {binary_bytes_per_sec:>11} {wide_msg:.dim}"
        }
        Unit::Files => {
            "{prefix} [{elapsed}, ETA {eta}] {bar:15.white/grey} {human_pos:>7}/{human_len:7} \
             {rate:>8} {wide_msg:.dim}"
        }
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
pub(crate) fn finish_step(pb: &ProgressBar, done: &str) {
    pb.finish_and_clear();
    // Not `pb.println`, which pads the line to the terminal width instead of ending it.
    if !pb.is_hidden() {
        eprintln!("{} ({})", done, HumanDuration(pb.elapsed()));
    }
}

/// A spinner counting media files, with the current folder as its message.
#[expect(
    clippy::expect_used,
    reason = "the templates are constants; test_progress_templates_parse checks them"
)]
pub(crate) fn spinner(prefix: &'static str, quiet: bool) -> ProgressBar {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_templates_parse() {
        for unit in [Unit::Bytes, Unit::Files] {
            progress_bar(1, "test", unit, false).finish_and_clear();
        }
        spinner("test", false).finish_and_clear();
    }
}
