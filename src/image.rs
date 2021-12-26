use indicatif::ProgressBar;
use log::trace;
use maud::{html, Markup};
use std::path::PathBuf;

pub(crate) fn process<F>(files: Vec<PathBuf>, pb: &ProgressBar, _on_err: F) -> Markup
where
    F: Fn(i32),
{
    for file in &files {
        trace!("Processing image: {:?}", file);
        pb.inc(1);
    }

    html! {
        #images {
            @for file in &files {
                @let path = format!("file://{}", file.display());
                a href=(path) { img src=(path); }
            }
        }
    }
}
