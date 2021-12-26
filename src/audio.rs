use indicatif::ProgressBar;
use log::trace;
use maud::{html, Markup};
use std::path::PathBuf;

pub(crate) fn process<F>(files: Vec<PathBuf>, pb: &ProgressBar, _on_err: F) -> Markup
where
    F: Fn(i32),
{
    let mut snippets: Vec<Markup> = vec![];
    for file in &files {
        trace!("Processing audio: {:?}", file);
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

    html! {
        ul {
            @for snippet in snippets {
                li { (snippet) }
            }
        }
    }
}
