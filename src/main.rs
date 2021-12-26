//! Display a bunch of photographs in a reasonable manner.

use indicatif::{HumanDuration, ProgressBar};
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

    let glob_base = format!(
        "{}{}/*.",
        opt.base_path,
        if opt.recurse { "/**" } else { "" }
    );

    let audio_file_extensions = vec!["mp3", "ogg", "wav"];
    let audio_files = shared::find_files(audio_file_extensions, &glob_base, pixrs_exit);
    let len_audio_files = audio_files.len();
    info!("Found {} audio files", len_audio_files);

    let image_file_extensions = vec!["gif", "jpg", "jpeg", "png"];
    let image_files = shared::find_files(image_file_extensions, &glob_base, pixrs_exit);
    let len_image_files = image_files.len();
    info!("Found {} image files", len_image_files);

    let video_file_extensions = vec!["avi", "m4v", "mp4", "mpeg", "mpg", "webm"];
    let video_files = shared::find_files(video_file_extensions, &glob_base, pixrs_exit);
    let len_video_files = video_files.len();
    info!("Found {} video files", len_video_files);

    let total_len = len_audio_files + len_image_files + len_video_files;
    let pb = ProgressBar::new(total_len as u64);

    let output = html! {
        (DOCTYPE)
        html lang="en" {
            head {}
            body {
                (audio::process(audio_files, &pb, pixrs_exit))
                (image::process(image_files, &pb, pixrs_exit))
                (video::process(video_files, &pb, pixrs_exit))
            }
        }
    }
    .into_string();
    write_html(&idx, &output);

    pb.finish_with_message("done");
    pixrs_exit(exitcode::OK);
    Ok(())
}

/// If path/index.html exists and clobber is not `True`, execute the provided callback function.
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

/// If path is not an existing directory, execute the provided callback function.
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

/// Write html to path.
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
