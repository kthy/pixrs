//! Code for handling audio files.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::html::html_index_dupe;
use crate::statics::SERVER_URL;
use crate::structs::{InventoryItem, PixrsOpt};
use crate::{create_inventory_item, create_progress_bar, find_files, sha256_digest};
use log::{debug, info};
use maud::{html, Markup, PreEscaped};
use std::collections::HashMap;
use std::fs::{create_dir_all, remove_dir_all, File};
use std::io::Write;
use std::path::PathBuf;
use uuid::Uuid;

pub(crate) fn into_html<F>(opt: &PixrsOpt, glob_pattern: &str, on_err: F) -> Markup
where
    F: Fn(i32),
{
    // Prepare output directory
    let base_snippet_path = format!("{}/audio", opt.out_path.display());
    let _ = remove_dir_all(&base_snippet_path);
    create_dir_all(&base_snippet_path).expect("Unable to create audio output directory!");

    // Collections of HTML snippets for audio files
    let mut items: Vec<Markup> = vec![];
    let mut dupes: Vec<Markup> = vec![];

    let file_extensions = vec!["mp3", "ogg", "wav"];
    let files: Vec<PathBuf> = find_files(file_extensions, glob_pattern, on_err);
    let num_files = files.len();
    info!("Found {} audio files", num_files);
    let pb = create_progress_bar(
        opt,
        num_files as u64,
        String::from("Processing audio files …"),
    );
    let mut map_digest_to_item = HashMap::with_capacity(num_files);
    // let mut map_uuid_to_item = HashMap::with_capacity(num_files);

    for file in files {
        debug!("Processing audio: {:?}", file);

        let digest = sha256_digest(&file);

        // Check if we've seen that digest before
        // (HTML generation is running single-threaded, so we're not worried about a race here)
        #[allow(clippy::map_entry)]
        if !map_digest_to_item.contains_key(&digest) {
            // Insert new inventory item
            let (inv_item, uuid) = create_inventory_item(&digest, &file, glob_pattern);
            map_digest_to_item.insert(digest, inv_item.clone());
            // map_uuid_to_item.insert(uuid, inv_item.clone());

            // Write HTML snippet to local file
            write_snippet(&base_snippet_path, &uuid, html_for_htmx(&inv_item));

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
        h2 { "Audio" }
        @if items.is_empty() {
            p .clear-both { "No audio files found." }
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

/// Generate HTML for a single file to load into index.html.
fn html_for_htmx(inv_item: &InventoryItem) -> Markup {
    let src = format!("{}{}", SERVER_URL, inv_item.local_path);
    html! {
        #(inv_item.uuid) {
            figure {
                audio controls preload="metadata" src=(src) {
                    "Your browser does not support embedded audio."
                }
                figcaption { (inv_item.path) }
            }
        }
    }
}

/// Generate HTML for a single file to place in index.html.
fn html_for_index(inv_item: &InventoryItem) -> Markup {
    let hx_get_url = format!("/audio/{}.html", inv_item.uuid);
    let text = &inv_item
        .path
        .split(std::path::MAIN_SEPARATOR)
        .next_back()
        .unwrap();
    html! {
        #(inv_item.uuid) .thumbnail {
            span .clickable hx-get=(hx_get_url) hx-swap="outerHTML" title=(inv_item.path) {
                code { (PreEscaped("&nbsp;&#9654;&nbsp;"))(text) }
            }
        }
    }
}

/// Write HTML snippet to a file.
fn write_snippet(base_snippet_path: &str, uuid: &Uuid, markup: Markup) {
    let snippet_path = format!("{}/{}.html", base_snippet_path, uuid);
    let mut snippet_file =
        File::create(snippet_path).expect("Couldn't create audio file HTML snippet!");
    write!(snippet_file, "{}", markup.into_string())
        .expect("Couldn't write audio file HTML snippet!");
}
