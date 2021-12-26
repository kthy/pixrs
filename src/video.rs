use indicatif::ProgressBar;
use log::trace;
use maud::{html, Markup};
use std::path::PathBuf;

pub(crate) fn process<F>(files: Vec<PathBuf>, pb: &ProgressBar, _on_err: F) -> Markup
where
    F: Fn(i32),
{
    for file in &files {
        trace!("Processing video: {:?}", file);
        pb.inc(1);
    }

    html! {
        ul {
            @for file in &files {
                li { (file.display()) }
            }
        }
    }
}
