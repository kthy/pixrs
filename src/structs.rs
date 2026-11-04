//! Structs for PixRS.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::dupes::DEFAULT_THRESHOLD;
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

    /// Display the licenses of third-party software included in PixRS
    #[arg(long)]
    pub third_party_notices: bool,

    /// Print debug output to stderr (debug builds only; `RUST_LOG` takes precedence)
    #[arg(long, global = true, hide = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Carry out a plan from `organize` or `dupes --quarantine`
    Apply(ApplyOpt),
    /// Report duplicate and similar files in the catalog
    Dupes(DupesOpt),
    /// Plan how to rename and move a library's files by a template
    Organize(OrganizeOpt),
    /// Point a library at the folder it was moved to
    Relocate(RelocateOpt),
    /// Scan a media library into the catalog
    Scan(ScanOpt),
    /// Browse the catalog in a local web viewer
    Serve(ServeOpt),
    /// Reverse an applied plan, from its journal
    Undo(UndoOpt),
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

    /// Remove the cataloged files of LIBRARY if it holds no media files now. Without it such a
    /// scan is refused, since an unmounted drive looks the same
    #[arg(long)]
    pub allow_empty: bool,
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

    /// Also find similar images (resized or recompressed copies) and videos under 4 seconds.
    /// Expect burst shots among them. Longer similar videos and similar audio are always found
    #[arg(long)]
    pub similar: bool,

    /// How many of the 64 perceptual hash bits two images or video frames may differ in to
    /// count as similar (at most 16)
    #[arg(long, default_value_t = DEFAULT_THRESHOLD, value_parser = clap::value_parser!(u32).range(0..=16))]
    pub threshold: u32,

    /// Plan moving every exact and same-pixel copy of a kept file to this folder (similar files
    /// are never quarantined). Nothing moves until `pixrs apply`
    #[arg(long, value_name = "DIR")]
    pub quarantine: Option<PathBuf>,
}

/// Options for `pixrs organize`.
#[derive(Debug, Args)]
pub struct OrganizeOpt {
    /// The scanned library to organize
    pub library: PathBuf,

    /// Where files go, e.g. "{type}/{year}/{month}/{date}_{time}.{ext}". Tokens: {year}
    /// {month} {day} {hour} {minute} {second} {date} {time} {type} {camera} {name} {ext}
    #[arg(long, conflicts_with = "layout", required_unless_present = "layout")]
    pub template: Option<String>,

    /// Preset folders, combined in order: flat, year, year-month, type (e.g. "type,year-month").
    /// Files are named "{date}_{time}.{ext}"
    #[arg(long)]
    pub layout: Option<String>,

    /// Put the files below this folder instead of the library root
    #[arg(long, value_name = "DIR")]
    pub to: Option<PathBuf>,

    /// Folder name for files without a usable date, in place of the date folders
    #[arg(long, default_value = "undated", value_name = "NAME")]
    pub undated: String,

    // The help cannot spell out the placeholder: clap reads it as a line break in help text.
    /// Added to a file name that is taken. The number (2, 3 and so on) goes where the default
    /// has it
    #[arg(long, default_value = "_{n}")]
    pub suffix: String,

    /// Use dates before 1980 or in the future as they are (e.g. scans of old photos), instead
    /// of treating them as missing
    #[arg(long)]
    pub trust_dates: bool,

    /// Remove folders the moves leave empty
    #[arg(long)]
    pub prune_empty: bool,

    /// The catalog file [default: catalog.sqlite in the OS data directory]
    #[arg(long)]
    pub catalog: Option<PathBuf>,
}

/// Options for `pixrs apply`.
#[derive(Debug, Args)]
pub struct ApplyOpt {
    /// The plan file `organize` or `dupes --quarantine` wrote
    pub plan: PathBuf,
}

/// Options for `pixrs undo`.
#[derive(Debug, Args)]
pub struct UndoOpt {
    /// The journal file `apply` wrote
    pub journal: PathBuf,
}

/// Options for `pixrs relocate`.
#[derive(Debug, Args)]
pub struct RelocateOpt {
    /// Where the library was when it was scanned
    pub old: PathBuf,

    /// Where the library is now
    pub new: PathBuf,

    /// The catalog file [default: catalog.sqlite in the OS data directory]
    #[arg(long)]
    pub catalog: Option<PathBuf>,

    /// Move the library even if most of its files are not found unchanged at NEW
    #[arg(long)]
    pub force: bool,
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

    /// Also find similar images (resized or recompressed copies) and videos under 4 seconds.
    /// Expect burst shots among them. Longer similar videos and similar audio are always found
    #[arg(long)]
    pub similar: bool,

    /// How many of the 64 perceptual hash bits two images or video frames may differ in to
    /// count as similar (at most 16)
    #[arg(long, default_value_t = DEFAULT_THRESHOLD, value_parser = clap::value_parser!(u32).range(0..=16))]
    pub threshold: u32,

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

#[cfg(test)]
mod tests {
    use super::*;

    use clap::CommandFactory;

    #[test]
    fn test_debug_is_accepted_before_and_after_the_subcommand() {
        for args in [
            vec!["pixrs", "--debug", "scan", "lib"],
            vec!["pixrs", "scan", "--debug", "lib"],
            vec!["pixrs", "scan", "lib", "--debug"],
            vec!["pixrs", "serve", "--debug"],
            vec!["pixrs", "undo", "journal.jsonl", "--debug"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            assert!(cli.debug, "{:?}", args);
        }
        assert!(!Cli::try_parse_from(["pixrs", "scan", "lib"]).unwrap().debug);
    }

    #[test]
    fn test_debug_is_not_in_the_help() {
        let mut cli = Cli::command();
        cli.build();
        for help in [cli.render_help(), cli.render_long_help()] {
            assert!(!help.to_string().contains("--debug"), "{}", help);
        }
        for sub in cli.get_subcommands_mut() {
            for help in [sub.render_help(), sub.render_long_help()] {
                assert!(!help.to_string().contains("--debug"), "{}", help);
            }
        }
    }
}
