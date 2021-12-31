//! Present a bunch of media files in a reasonable manner.

use indicatif::HumanDuration;
use log::{error, info};
use maud::{html, DOCTYPE};
use rouille::{match_assets, router, start_server, Response};
use std::env;
use std::path::PathBuf;
use std::process::exit;
use std::time::Instant;
use structopt::StructOpt;

mod audio;
mod image;
mod video;

/// Command-line options.
#[derive(Debug, StructOpt)]
struct PixrsOpt {
    /// The base path to search
    #[structopt()]
    base_path: String,
    /// Clobber output file if it exists
    #[structopt(short, long)]
    clobber: bool,
    /// Create output directory if it does not exist
    #[structopt(short = "d", long = "create-dir")]
    create_output_dir: bool,
    /// Open output in default browser when finished
    #[structopt(short, long)]
    open: bool,
    /// The path where the output is placed
    #[structopt(parse(from_os_str))]
    out_path: PathBuf,
    /// Recurse into subdirs below the base path
    #[structopt(short, long)]
    recurse: bool,
}

/// HTMX SRI hash.
static HTMX_SRI: &str = "sha384-tvG/2mnCFmGQzYC1Oh3qxQ7CkQ9kMzYjWZSNtrRZygHPDDqottzEJsqS4oUVodhW";
/// HTMX unpkg URL.
static HTMX_URL: &str = "https://unpkg.com/htmx.org@1.6.1";

/// This is where the magic happens.
fn main() {
    let now = Instant::now();
    let opt = PixrsOpt::from_args();

    ctrlc::set_handler(move || {
        println!("Ctrl-C received, stopping.");
        exit(exitcode::OK);
    })
    .expect("Error setting Ctrl-C handler");

    let cwd = format!(
        "{}",
        env::current_dir()
            .expect("Unable to retrieve current directory!")
            .as_path()
            .display()
    );
    let idx = format!("{}/index.html", opt.out_path.display());

    env_logger::init();
    info!("Executing: {:?}", opt);

    // Callback to stop the timer and exit with the given code.
    let pixrs_exit = |code| {
        info!(
            "Execution interrupted after {}",
            HumanDuration(now.elapsed())
        );
        exit(code)
    };

    pixrs::validate_input(&cwd, &opt.base_path, pixrs_exit);

    pixrs::validate_output(
        &cwd,
        &opt.out_path,
        &idx,
        opt.clobber,
        opt.create_output_dir,
        pixrs_exit,
    );

    let glob_pattern = pixrs::create_glob_pattern(&opt.base_path, opt.recurse);

    let output = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "PixRS output" }
                meta name="viewport" content="width=device-width, initial-scale=1";
                style {
                    "
                    code {
                        background-color: #ddd;
                        border-radius: 6px;
                        color: #222;
                        padding: 2px 4px;
                    }

                    li { padding: 2px 0; }

                    .ease-in { transition: all ease-in 1s; }

                    .warning { color: red; }
                    "
                }
                script src=(HTMX_URL) integrity=(HTMX_SRI) crossorigin="anonymous" {}

            }
            body {
                header {
                    (pixrs::html_header(&opt.base_path))
                }
                main {
                    section #audio { (audio::into_html(&opt.out_path, &glob_pattern, pixrs_exit)) }
                    section #image { (image::into_html(&opt.out_path, &glob_pattern, pixrs_exit)) }
                    section #video { (video::into_html(&opt.out_path, &glob_pattern, pixrs_exit)) }
                }
            }
        }
    };

    pixrs::write_html(&idx, &output.into_string());

    info!(
        "HTML generator finished in {}",
        HumanDuration(now.elapsed())
    );

    let url = "http://localhost:2635/index.html";
    if opt.open {
        // pixrs::open_in_browser(&idx);
        open::that(url).unwrap_or_else(|e| {
            error!("{:?}", e);
            eprintln!("Error opening output: {}", e);
        });
    }

    println!("\nNow serving your media files at {}", url);
    println!("Press Ctrl-C in this window to stop serving.");

    start_server("localhost:2635", move |req| {
        // TODO: add No-Cache header
        router!(req,
            (GET) (/) => { Response::redirect_303("/index.html") },
            _ => match_assets(req, &opt.out_path),
        )
    });
}
