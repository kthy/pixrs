//! Code for handling image files.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
 *************************************************************************************************/

use crate::structs::PixrsOpt;
use crate::{create_progress_bar, find_files};
use log::{debug, info, trace};
use maud::{html, Markup};
use std::path::PathBuf;

pub(crate) fn into_html<F>(opt: &PixrsOpt, glob_pattern: &str, on_err: F) -> Markup
where
    F: Fn(i32),
{
    trace!("process({:?}, _)", glob_pattern);

    let mut snippets: Vec<Markup> = vec![];

    let file_extensions = vec!["gif", "jpg", "jpeg", "png"];
    let files: Vec<PathBuf> = find_files(file_extensions, glob_pattern, on_err);
    let num_files = files.len();
    info!("Found {} image files", num_files);
    let pb = create_progress_bar(
        opt,
        num_files as u64,
        String::from("Processing image files …"),
    );

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
