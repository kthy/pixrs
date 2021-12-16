#![allow(unused)]

#[macro_use]
extern crate log;

use anyhow::{Context, Result};
use indicatif::ProgressBar;
use log::{info, trace};
use std::path::PathBuf;
use std::{thread, time};
use structopt::StructOpt;

/// Display a bunch of photographs in a reasonable manner.

#[derive(Debug, StructOpt)]
struct PixrsOpt {
    /// The base path to search
    #[structopt(parse(from_os_str), default_value = ".")]
    base_path: PathBuf,
    /// Clobber output file if it exists
    #[structopt(short, long)]
    clobber: bool,
    /// Activate debug mode
    #[structopt(short, long)]
    debug: bool,
    /// Recurse into subdirs below the base path
    #[structopt(short, long)]
    recurse: bool,
    /// The path where the output file is placed
    #[structopt(parse(from_os_str), short = "o", long = "output", default_value = ".")]
    out_path: PathBuf,
}

fn main() -> Result<()> {
    let now = time::Instant::now();
    env_logger::init();
    let opt = PixrsOpt::from_args();
    info!("Executing with {:?}", opt);
    let pb = ProgressBar::new(100);
    for i in 0..100 {
        thread::sleep(time::Duration::from_millis(10));
        trace!("Sleeping … ({})", i);
        pb.inc(1);
    }
    pb.finish_with_message("done");
    info!("Finished execution in {:?}", now.elapsed());
    Ok(())
}
