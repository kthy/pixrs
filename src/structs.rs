//! Structs for PixRS.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::statics::{ABOUT, AFTER_HELP};
use clap::Parser;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct InventoryItem {
    pub digest: String,
    pub local_path: String,
    pub path: String,
    pub uuid: Uuid,
}

/// Command-line options.
#[derive(Debug, Parser)]
#[command(version, about = ABOUT, after_help = AFTER_HELP, term_width = 80)]
pub struct PixrsOpt {
    /// The base path to search
    #[arg(default_value = ".", required_unless_present = "license")]
    pub base_path: String,

    /// Clobber output file if it exists
    #[arg(short, long)]
    pub clobber: bool,

    /// Create output directory if it does not exist
    #[arg(short = 'd', long = "create-dir")]
    pub create_output_dir: bool,

    /// Display full license notice
    #[arg(short, long)]
    pub license: bool,

    /// Do not start local server when finished (conflicts with --open)
    #[arg(short, long, conflicts_with = "open")]
    pub no_server: bool,

    /// Open output in default browser when finished (conflicts with --no-server)
    #[arg(short, long)]
    pub open: bool,

    /// The path where the output is placed
    #[arg(default_value = ".", required_unless_present = "license")]
    pub out_path: PathBuf,

    /// Disable progress bars
    #[arg(short, long)]
    pub quiet: bool, // TODO: implement

    /// Recurse into subdirs below the base path
    #[arg(short, long)]
    pub recurse: bool,
}
