//! Code for handling audio files.

use crate::shared;
use indicatif::ProgressBar;
use log::{debug, info, trace};
use maud::{html, Markup};
use std::path::PathBuf;

pub(crate) fn into_html<F>(glob_pattern: &str, on_err: F) -> Markup
where
    F: Fn(i32),
{
    trace!("process({:?}, _)", glob_pattern);

    let mut snippets: Vec<Markup> = vec![];

    let file_extensions = vec!["mp3", "ogg", "wav"];
    let files: Vec<PathBuf> = shared::find_files(file_extensions, glob_pattern, on_err);
    let num_files = files.len();
    info!("Found {} audio files", num_files);
    let pb = ProgressBar::new(num_files as u64);

    for file in files {
        debug!("Processing audio: {:?}", file);
        let path = format!("file://{}", file.display());
        snippets.push(html! {
            figure {
                figcaption { (path) }
                audio controls preload="metadata" src=(path) {
                    "Your browser does not support the "
                    code { "audio" }
                    " element."
                }
            }
        });
        pb.inc(1);
    }

    pb.finish_with_message("done");

    html! {
        ul {
            @for snippet in snippets {
                li { (snippet) }
            }
        }
    }
}
