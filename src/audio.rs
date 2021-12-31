//! Code for handling audio files.

use indicatif::ProgressBar;
use log::{debug, info, trace};
use maud::{html, Markup, PreEscaped};
use pixrs::{find_files, html_dupe, sha256_digest, InventoryItem};
use std::collections::HashMap;
use std::fs::{create_dir_all, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) fn into_html<F>(path: &Path, glob_pattern: &str, on_err: F) -> Markup
where
    F: Fn(i32),
{
    trace!("process({:?}, _)", glob_pattern);

    let base_snippet_path = format!("{}/audio", path.display());
    create_dir_all(&base_snippet_path).expect("Unable to create audio output directory!");

    let mut items: Vec<Markup> = vec![];

    let file_extensions = vec!["mp3", "ogg", "wav"];
    let files: Vec<PathBuf> = find_files(file_extensions, glob_pattern, on_err);
    let num_files = files.len();
    info!("Found {} audio files", num_files);
    let pb = ProgressBar::new(num_files as u64);
    let mut map_digest_to_item = HashMap::with_capacity(num_files);
    let mut map_uuid_to_item = HashMap::with_capacity(num_files);

    for file in files {
        debug!("Processing audio: {:?}", file);

        // Calculate file digest and check if we've seen it before
        let digest = sha256_digest(&file);

        #[allow(clippy::map_entry)]
        if !map_digest_to_item.contains_key(&digest) {
            // Insert new inventory item
            let uuid = Uuid::new_v4();
            let inv_item = InventoryItem {
                digest: digest.clone(),
                path: format!("{}", file.display()),
                uuid,
            };
            map_digest_to_item.insert(digest, inv_item.clone());
            map_uuid_to_item.insert(uuid, inv_item.clone());

            // Create HTML snippet
            let snippet_path = format!("{}/{}.html", &base_snippet_path, uuid);
            let mut snippet_file =
                File::create(&snippet_path).expect("Couldn't create audio file HTML snippet!");
            let snippet_html = html! {
                #(uuid) {
                    figure {
                        figcaption { (file.display()) }
                        audio controls preload="metadata" src=(format!("file:///{}", file.display())) {
                            "Your browser does not support the "
                            code { "audio" }
                            " element."
                        }
                    }
                }
            };
            write!(snippet_file, "{}", snippet_html.into_string())
                .expect("Couldn't write audio file HTML snippet!");

            // Push initial HTML for main page to list of snippets
            let hx_get_url = format!("file:///{}", &snippet_path);
            items.push(html! {
                #(uuid) {
                    span hx-get=(hx_get_url) hx-swap="outerHTML" { (PreEscaped("&nbsp;&#9654;")) }
                    code { (file.display()) }
                }
            });
        } else {
            // Push duplicate warning to list of snippets
            items.push(html_dupe(
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
