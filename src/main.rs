//! Display a bunch of photographs in a reasonable manner.

use chrono::offset::Local;
use indicatif::HumanDuration;
use log::{debug, error, info};
use maud::{html, DOCTYPE};
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::Instant;
use structopt::StructOpt;

mod audio;
mod image;
mod shared;
mod video;

#[derive(Debug, StructOpt)]
struct PixrsOpt {
    /// The base path to search
    #[structopt()]
    base_path: String,
    /// Clobber output file if it exists
    #[structopt(short, long)]
    clobber: bool,
    /// The path where the output is placed
    #[structopt(parse(from_os_str))]
    out_path: PathBuf,
    /// Recurse into subdirs below the base path
    #[structopt(short, long)]
    recurse: bool,
}

/// This is where the magic happens.
fn main() -> anyhow::Result<()> {
    let now = Instant::now();
    let opt = PixrsOpt::from_args();
    let cwd = format!("{}", env::current_dir()?.as_path().display());
    let idx = format!("{}/index.html", opt.out_path.display());

    env_logger::init();
    info!("Executing: {:?}", opt);

    // Callback to stop the timer and exit with the given code.
    let pixrs_exit = |code| {
        info!("Execution finished in {}", HumanDuration(now.elapsed()));
        exit(code)
    };

    exit_if_nonexisting_input(&cwd, &opt.base_path, pixrs_exit);

    exit_if_existing_output(&cwd, &opt.out_path, &idx, opt.clobber, pixrs_exit);

    let glob_pattern = create_glob_pattern(&opt.base_path, opt.recurse);

    let output = html! {
        (DOCTYPE)
        html lang="en" {
            head {}
            body {
                header {
                    h1 { "PixRS" }
                    p {
                        "Output from "
                        code { (opt.base_path) }
                        " generated "
                        (Local::now().format("%A, %e %b %Y @ %T%z"))
                    }
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

    write_html(&idx, &output);

    pixrs_exit(exitcode::OK);

    Ok(())
}

/// If `path`/index.html exists and clobber is not `True`, execute the provided callback function.
fn exit_if_existing_output<F>(cwd: &str, path: &Path, idx: &str, clobber: bool, on_err: F)
where
    F: Fn(i32),
{
    if path.is_dir() {
        if !clobber && Path::new(&idx).exists() {
            error!("{:?} exists and clobber is false", idx);
            eprintln!(
                "Error: {:?} already exists - to overwrite it, run PixRS with the --clobber flag",
                idx
            );
            on_err(exitcode::CANTCREAT);
        }
    } else {
        error!("{:?} is not a directory", path);
        eprintln!(
            "Input error: {:?} is not a directory I can write to from {}",
            path, cwd
        );
        on_err(exitcode::USAGE);
    }
}

/// If `path` is not an existing directory, execute the provided callback function.
fn exit_if_nonexisting_input<F>(cwd: &str, path: &str, on_err: F)
where
    F: Fn(i32),
{
    if !Path::new(path).is_dir() {
        error!("{:?} is not a directory", path);
        eprintln!(
            "Input error: {:?} is not a directory I can read from {}",
            path, cwd
        );
        on_err(exitcode::USAGE);
    }
}

/// Create the glob pattern.
fn create_glob_pattern(path: &str, recurse: bool) -> String {
    format!("{}{}/*.", path, if recurse { "/**" } else { "" })
}

/// Write `html` to `path`.
fn write_html(path: &str, html: &str) {
    debug!("{}", html);

    let mut out_file = match File::create(&path) {
        Err(why) => panic!("Couldn't create {}: {}", path, why),
        Ok(file) => file,
    };

    match out_file.write_all(html.as_bytes()) {
        Err(why) => panic!("Couldn't write to {}: {}", path, why),
        Ok(_) => info!("Successfully wrote to {}", path),
    }
}
