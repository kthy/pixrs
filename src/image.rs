//! Code for handling image files.

/*************************************************************************************************
 * Copyright © 2022, 2023, 2024 Kristian Thy (<k@pyxy.dk>)                                       *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
 *************************************************************************************************/

use crate::html::html_index_dupe;
use crate::statics::SERVER_URL;
use crate::structs::{InventoryItem, PixrsOpt};
use crate::{create_inventory_item, create_progress_bar, find_files, sha256_digest};
use log::{debug, info};
use maud::{html, Markup};
use std::collections::HashMap;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::PathBuf;

pub(crate) fn into_html<F>(opt: &PixrsOpt, glob_pattern: &str, on_err: F) -> Markup
where
    F: Fn(i32),
{
    // Prepare output directory
    let base_snippet_path = format!("{}/images", opt.out_path.display());
    let _ = remove_dir_all(&base_snippet_path);
    create_dir_all(&base_snippet_path).expect("Unable to create image output directory!");

    // Collection of HTML snippets for video files
    let mut items: Vec<Markup> = vec![];
    let mut dupes: Vec<Markup> = vec![];

    let file_extensions = vec!["gif", "jpg", "jpeg", "png"];
    let files: Vec<PathBuf> = find_files(file_extensions, glob_pattern, on_err);
    let num_files = files.len();
    info!("Found {} image files", num_files);
    let pb = create_progress_bar(
        opt,
        num_files as u64,
        String::from("Processing image files …"),
    );
    let mut map_digest_to_item = HashMap::with_capacity(num_files);

    for file in files {
        debug!("Processing image: {:?}", file);

        let digest = sha256_digest(&file);

        // Check if we've seen that digest before
        // (HTML generation is running single-threaded, so we're not worried about a race here)
        #[allow(clippy::map_entry)]
        if !map_digest_to_item.contains_key(&digest) {
            // Insert new inventory item
            let (inv_item, _) = create_inventory_item(&digest, &file, glob_pattern);
            map_digest_to_item.insert(digest, inv_item.clone());

            // Push initial HTML for main page to list of snippets
            items.push(html_for_index(&inv_item));
        } else {
            // Push duplicate warning to list of snippets
            dupes.push(html_index_dupe(
                &file.display().to_string(),
                &map_digest_to_item[&digest].path,
            ));
        }
        pb.inc(1);
    }

    pb.finish_with_message("done");

    html! {
        h2 { "Images" }
        @if items.is_empty() {
            p .clear-both { "No image files found." }
        } @else {
            @for item in items { (item) }
            @if !dupes.is_empty() {
                ul {
                    @for item in dupes { li { (item) } }
                }
            }
        }
    }
}

/// Generate HTML for a single file to place in index.html.
fn html_for_index(inv_item: &InventoryItem) -> Markup {
    let src = format!("{}{}", SERVER_URL, &inv_item.local_path);
    let caption = &inv_item
        .path
        .split(std::path::MAIN_SEPARATOR)
        .next_back()
        .unwrap();
    let t = &inv_item.path;
    html! {
        #(inv_item.uuid) .thumbnail {
            figure {
                a href=(src) data-lightbox="pixrs" data-alt=(t) data-title=(t) {
                    img src=(src) alt=(t) title=(t) height="128";
                }
                figcaption{ (caption) }
            }
        }
    }
}
