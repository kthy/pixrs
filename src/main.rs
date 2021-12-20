use indicatif::ProgressBar;
use log::{error, info};
use std::env;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Instant};
use structopt::StructOpt;

/// Display a bunch of photographs in a reasonable manner.

mod audio;
mod image;
mod shared;
mod video;

#[derive(Debug, StructOpt)]
struct PixrsOpt {
    /// The base path to search
    #[structopt(default_value = ".")]
    base_path: String,
    /// Clobber output file if it exists
    #[structopt(short, long)]
    clobber: bool,
    /// Recurse into subdirs below the base path
    #[structopt(short, long)]
    recurse: bool,
    /// The path where the output file is placed
    #[structopt(parse(from_os_str), short = "o", long = "output", default_value = ".")]
    out_path: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let now = Instant::now();
    let opt = PixrsOpt::from_args();

    env_logger::init();
    info!("Executing: {:?}", opt);

    let pixrs_exit = |code| {
        info!("Execution finished in {:?}", now.elapsed());
        exit(code);
    };

    let cwd = format!("{}", env::current_dir()?.as_path().display());

    // Check if opt.base_path is a directory that exists
    if !Path::new(&opt.base_path).is_dir() {
        error!("{:?} is not a directory", opt.base_path);
        eprintln!("Input error: {:?} is not a directory I can read from {}", opt.base_path, cwd);
        pixrs_exit(exitcode::USAGE)
    }

    // Check if opt.out_path has content
    if opt.out_path.is_dir() {
        let idx = format!("{}/index.html", opt.out_path.display());
        if !opt.clobber && Path::new(&idx).exists() {
            error!("{:?} exists and clobber is false", idx);
            eprintln!("Error: {:?} already exists - to overwrite it, run PixRS with the --clobber flag", idx);
            pixrs_exit(exitcode::CANTCREAT)
        }
    } else {
        error!("{:?} is not a directory", opt.out_path);
        eprintln!("Input error: {:?} is not a directory I can write to from {}", opt.out_path, cwd);
        pixrs_exit(exitcode::USAGE)
    }

    let glob_base = format!("{}{}/*.", opt.base_path, if opt.recurse { "/**" } else { "" });

    let audio_file_extensions = vec!["mp3", "wav"];
    let audio_files = shared::find_files(audio_file_extensions, &glob_base, pixrs_exit);
    let len_audio_files = audio_files.len();
    info!("Found {} audio files", len_audio_files);

    let image_file_extensions = vec!["gif", "jpg", "jpeg", "png"];
    let image_files = shared::find_files(image_file_extensions, &glob_base, pixrs_exit);
    let len_image_files = image_files.len();
    info!("Found {} image files", len_image_files);

    let video_file_extensions = vec!["avi", "m4v", "mp4", "mpeg", "mpg"];
    let video_files = shared::find_files(video_file_extensions, &glob_base, pixrs_exit);
    let len_video_files = video_files.len();
    info!("Found {} video files", len_video_files);

    let total_len = len_audio_files + len_image_files + len_video_files;
    let pb = ProgressBar::new(total_len as u64);

    audio::process(audio_files, &pb, pixrs_exit);
    image::process(image_files, &pb, pixrs_exit);
    video::process(video_files, &pb, pixrs_exit);
    pb.finish_with_message("done");

    Ok(pixrs_exit(exitcode::OK))
}
