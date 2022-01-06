//! Structs for PixRS.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
 *************************************************************************************************/

use crate::statics::{ABOUT, AFTER_HELP};
use std::path::PathBuf;
use structopt::StructOpt;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct InventoryItem {
    pub digest: String,
    pub local_path: String,
    pub path: String,
    pub uuid: Uuid,
}

/// Command-line options.
#[derive(Debug, StructOpt)]
#[structopt(about = ABOUT, after_help = AFTER_HELP, set_term_width = 80)]
pub struct PixrsOpt {
    /// The base path to search
    #[structopt(default_value=".", required_unless = "license")]
    pub base_path: String,
    /// Clobber output file if it exists
    #[structopt(short, long)]
    pub clobber: bool,
    /// Create output directory if it does not exist
    #[structopt(short = "d", long = "create-dir")]
    pub create_output_dir: bool,
    /// Display full license notice
    #[structopt(short, long)]
    pub license: bool,
    /// Do not start local server when finished (forbidden if --open is set)
    #[structopt(short, long, conflicts_with = "open")]
    pub no_server: bool,
    /// Open output in default browser when finished (forbidden if --no-server is set)
    #[structopt(short, long)]
    pub open: bool,
    /// The path where the output is placed
    #[structopt(default_value = ".", parse(from_os_str), required_unless = "license")]
    pub out_path: PathBuf,
    /// Disable progress bars
    #[structopt(short, long)]
    pub quiet: bool, // TODO: implement
    /// Recurse into subdirs below the base path
    #[structopt(short, long)]
    pub recurse: bool,
}
