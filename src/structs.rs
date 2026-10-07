//! Structs for PixRS.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::statics::{ABOUT, AFTER_HELP};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct InventoryItem {
    pub digest: String,
    pub local_path: String,
    pub path: String,
    pub uuid: Uuid,
}

/// Command-line interface.
#[derive(Debug, Parser)]
#[command(
    version,
    about = ABOUT,
    after_help = AFTER_HELP,
    term_width = 80,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Display full license notice
    #[arg(short, long)]
    pub license: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Generate a static HTML gallery and serve it
    Gallery(PixrsOpt),
    /// Scan a media library into the catalog
    Scan(ScanOpt),
}

/// Options for `pixrs gallery`.
#[derive(Debug, Args)]
pub struct PixrsOpt {
    /// The base path to search
    #[arg(default_value = ".")]
    pub base_path: String,

    /// Clobber output file if it exists
    #[arg(short, long)]
    pub clobber: bool,

    /// Create output directory if it does not exist
    #[arg(short = 'd', long = "create-dir")]
    pub create_output_dir: bool,

    /// Do not start local server when finished (conflicts with --open)
    #[arg(short, long, conflicts_with = "open")]
    pub no_server: bool,

    /// Open output in default browser when finished (conflicts with --no-server)
    #[arg(short, long)]
    pub open: bool,

    /// The path where the output is placed
    #[arg(default_value = ".")]
    pub out_path: PathBuf,

    /// Disable progress bars
    #[arg(short, long)]
    pub quiet: bool,

    /// Recurse into subdirs below the base path
    #[arg(short, long)]
    pub recurse: bool,
}

/// Options for `pixrs scan`.
#[derive(Debug, Args)]
pub struct ScanOpt {
    /// The media library to scan
    pub library: PathBuf,

    /// The catalog file [default: catalog.sqlite in the OS data directory]
    #[arg(long)]
    pub catalog: Option<PathBuf>,

    /// Disable progress bars
    #[arg(short, long)]
    pub quiet: bool,
}
