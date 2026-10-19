//! Structs for PixRS.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::statics::{ABOUT, AFTER_HELP};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

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
    /// Report exact duplicates in the catalog
    Dupes(DupesOpt),
    /// Scan a media library into the catalog
    Scan(ScanOpt),
    /// Browse the catalog in a local web viewer
    Serve(ServeOpt),
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

/// Options for `pixrs dupes`.
#[derive(Debug, Args)]
pub struct DupesOpt {
    /// Only report duplicates of files in these libraries [default: all scanned libraries]
    pub libraries: Vec<PathBuf>,

    /// The catalog file [default: catalog.sqlite in the OS data directory]
    #[arg(long)]
    pub catalog: Option<PathBuf>,

    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

/// Options for `pixrs serve`.
#[derive(Debug, Args)]
pub struct ServeOpt {
    /// Only show files in these libraries [default: all scanned libraries]
    pub libraries: Vec<PathBuf>,

    /// The catalog file [default: catalog.sqlite in the OS data directory]
    #[arg(long)]
    pub catalog: Option<PathBuf>,

    /// The thumbnail cache folder [default: thumbnails in the OS cache directory]
    #[arg(long)]
    pub cache: Option<PathBuf>,

    /// The port to listen on, on localhost only
    #[arg(long, default_value_t = 2635)]
    pub port: u16,

    /// Open the viewer in the default browser
    #[arg(short, long)]
    pub open: bool,
}

/// Report output formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// For people to read
    Text,
    /// One object with all groups and a summary
    Json,
    /// One row per file
    Csv,
}
