//! Structs for PixRS.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
 *************************************************************************************************/

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
pub struct PixrsOpt {
    /// The base path to search
    #[structopt()]
    pub base_path: String,
    /// Clobber output file if it exists
    #[structopt(short, long)]
    pub clobber: bool,
    /// Create output directory if it does not exist
    #[structopt(short = "d", long = "create-dir")]
    pub create_output_dir: bool,
    /// Open output in default browser when finished
    #[structopt(short, long)]
    pub open: bool,
    /// The path where the output is placed
    #[structopt(parse(from_os_str))]
    pub out_path: PathBuf,
    /// Recurse into subdirs below the base path
    #[structopt(short, long)]
    pub recurse: bool,
}
