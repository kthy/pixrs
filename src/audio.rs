//! Code for handling audio files.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
 *************************************************************************************************/

use crate::statics::SERVER_URL;
use crate::structs::InventoryItem;
use crate::{find_files, html::html_index_dupe, sha256_digest};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, info, trace};
use maud::{html, Markup, PreEscaped};
use std::collections::HashMap;
use std::fs::{create_dir_all, remove_dir_all, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

pub(crate) fn into_html<F>(out_path: &Path, glob_pattern: &str, on_err: F) -> Markup
where
    F: Fn(i32),
{
    trace!("into_html({:?}, {:?}, _)", out_path, glob_pattern);

    // Prepare output directory
    let base_snippet_path = format!("{}/audio", out_path.display());
    let _ = remove_dir_all(&base_snippet_path);
    create_dir_all(&base_snippet_path).expect("Unable to create audio output directory!");

    // Collection of HTML snippets for audio files
    let mut items: Vec<Markup> = vec![];

    let file_extensions = vec!["mp3", "ogg", "wav"];
    let files: Vec<PathBuf> = find_files(file_extensions, glob_pattern, on_err);
    let num_files = files.len();
    info!("Found {} audio files", num_files);
    let pb = ProgressBar::new(num_files as u64).with_prefix("Processing audio files …");
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{prefix} [{elapsed}/{duration}] {wide_bar:.white/grey} {pos:>5}/{len:5} {>msg}",
            )
            .progress_chars("#>-"),
    );
    let mut map_digest_to_item = HashMap::with_capacity(num_files);
    let mut map_uuid_to_item = HashMap::with_capacity(num_files);

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
            map_uuid_to_item.insert(uuid, inv_item.clone());

            // Write HTML snippet to local file
            let snippet_path = format!("{}/{}.html", &base_snippet_path, uuid);
            let mut snippet_file =
                File::create(&snippet_path).expect("Couldn't create audio file HTML snippet!");
            let snippet_html = html_for_htmx(&inv_item);
            write!(snippet_file, "{}", snippet_html.into_string())
                .expect("Couldn't write audio file HTML snippet!");

            // Push initial HTML for main page to list of snippets
            items.push(html_for_index(&inv_item));
        } else {
            // Push duplicate warning to list of snippets
            items.push(html_index_dupe(
                &file.display().to_string(),
                &map_digest_to_item[&digest].path,
            ));
        }
        pb.inc(1);
    }

    pb.finish_with_message("done");

    html! {
        ul {
            @for item in items {
                li { (item) }
            }
        }
    }
}

/// Create an inventory item and return it together with its UUID.
fn create_inventory_item(digest: &str, path: &Path, glob_pattern: &str) -> (InventoryItem, Uuid) {
    let uuid = Uuid::new_v4();
    let inv_item = InventoryItem {
        digest: String::from(digest),
        local_path: make_path_local(path, glob_pattern),
        path: format!("{}", path.display()),
        uuid,
    };
    (inv_item, uuid)
}

/// Generate HTML for a single file to load into index.html.
fn html_for_htmx(inv_item: &InventoryItem) -> Markup {
    let src = format!("{}{}", SERVER_URL, &inv_item.local_path);
    html! {
        #(inv_item.uuid) {
            figure {
                figcaption { code { (inv_item.path) } }
                audio controls preload="metadata" src=(src) {
                    "Your browser does not support the "
                    code { "audio" }
                    " element."
                }
            }
        }
    }
}

/// Generate HTML for a single file to place in index.html.
fn html_for_index(inv_item: &InventoryItem) -> Markup {
    let hx_get_url = format!("/audio/{}.html", &inv_item.uuid);
    html! {
        #(inv_item.uuid) {
            span .clickable hx-get=(hx_get_url) hx-swap="outerHTML" {
                (PreEscaped("&#9654;&nbsp;"))
            }
            code { (inv_item.path) }
        }
    }
}

/// Make a `/local/` URL path out of the `InventoryItem`'s path.
///
/// ```
/// use pretty_assertions::assert_eq;
/// let expected = "/local/audio/Birk%20CTG.mp3";
/// let p = std::path::Path::new(".\\tests\\files\\audio\\Birk CTG.mp3");
/// let g = ".\\tests\\files\\**/*.mp3";
/// let actual = pixrs::audio::make_path_local(&p, &g);
/// assert_eq!(actual, expected);
/// ```
pub fn make_path_local(path: &Path, glob_pattern: &str) -> String {
    trace!("make_path_local({:?}, {:?})", path, glob_pattern);
    let fwd_pattern = glob_pattern.replace('\\', "/");
    let pattern_graphemes = fwd_pattern.graphemes(true).collect::<Vec<&str>>();
    let wildcard_position = pattern_graphemes.iter().position(|x| *x == "*").unwrap();
    let fwd_path = path.display().to_string().replace('\\', "/");
    let path_graphemes = fwd_path.graphemes(true).collect::<Vec<&str>>();
    let final_path = path_graphemes[wildcard_position..].concat();
    format!(
        "/local/{}",
        urlencoding::encode(&final_path).replace("%2F", "/")
    )
}

/// Write HTML snippet to a file.
fn _write_snippet(_base_snippet_path: &str, _uuid: &Uuid) {}
