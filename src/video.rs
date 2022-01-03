//! Code for handling video files.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
 *************************************************************************************************/

use indicatif::ProgressBar;
use log::{debug, info, trace};
use maud::{html, Markup};
use std::path::{Path, PathBuf};

pub(crate) fn into_html<F>(_path: &Path, glob_pattern: &str, on_err: F) -> Markup
where
    F: Fn(i32),
{
    trace!("process({:?}, _)", glob_pattern);

    let mut snippets: Vec<Markup> = vec![];

    let file_extensions = vec!["avi", "m4v", "mp4", "mpeg", "mpg", "webm"];
    let files: Vec<PathBuf> = crate::find_files(file_extensions, glob_pattern, on_err);
    let num_files = files.len();
    info!("Found {} video files", num_files);
    let pb = ProgressBar::new(num_files as u64);

    for file in &files {
        debug!("Processing image: {:?}", file);
        let snippet_path = format!("file://{}", file.display());
        snippets.push(html! {
            a href=(snippet_path) { img src=(snippet_path); }
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
