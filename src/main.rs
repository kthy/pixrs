//! Present a bunch of media files in a reasonable manner.

use indicatif::HumanDuration;
use log::info;
use maud::{html, DOCTYPE};
use std::env;
use std::path::PathBuf;
use std::process::exit;
use std::time::Instant;
use structopt::StructOpt;

mod audio;
mod image;
mod shared;
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

/// This is where the magic happens.
fn main() {
    let now = Instant::now();
    let opt = PixrsOpt::from_args();
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
        info!("Execution finished in {}", HumanDuration(now.elapsed()));
        exit(code)
    };

    shared::validate_input(&cwd, &opt.base_path, pixrs_exit);

    shared::validate_output(
        &cwd,
        &opt.out_path,
        &idx,
        opt.clobber,
        opt.create_output_dir,
        pixrs_exit,
    );

    let glob_pattern = shared::create_glob_pattern(&opt.base_path, opt.recurse);

    let output = html! {
        (DOCTYPE)
        html lang="en" {
            head {}
            body {
                header {
                    (shared::html_header(&opt.base_path))
                }
                main {
                    section #audio { (audio::process(&glob_pattern, pixrs_exit)) }
                    section #image { (image::process(&glob_pattern, pixrs_exit)) }
                    section #video { (video::process(&glob_pattern, pixrs_exit)) }
                }
            }
        }
    }
    .into_string();

    shared::write_html(&idx, &output);

    if opt.open {
        shared::open_in_browser(&idx);
    }

    pixrs_exit(exitcode::OK);
}
