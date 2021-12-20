use indicatif::ProgressBar;
use log::trace;
use std::path::PathBuf;

pub(crate) fn process<F>(files: Vec<PathBuf>, pb: &ProgressBar, on_err: F) -> () where F: Fn(i32) -> () {
    for file in files {
        trace!("Processing {:?}", file);
        pb.inc(1);
    }
}
