//! Present a bunch of media files in a reasonable manner.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 *                                                                                               *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *                                                                                               *
 * This program is free software: you can redistribute it and/or modify                          *
 * it under the terms of the GNU Affero General Public License as                                *
 * published by the Free Software Foundation, either version 3 of the                            *
 * License, or (at your option) any later version.                                               *
 *                                                                                               *
 * This program is distributed in the hope that it will be useful,                               *
 * but WITHOUT ANY WARRANTY; without even the implied warranty of                                *
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the                                  *
 * GNU Affero General Public License for more details.                                           *
 *                                                                                               *
 * You should have received a copy of the GNU Affero General Public License                      *
 * along with this program. If not, see <https://www.gnu.org/licenses/>.                         *
 *************************************************************************************************/

use clap::{CommandFactory, Parser};
use indicatif::HumanDuration;
use log::{debug, error};
use pixrs::catalog::Catalog;
use pixrs::dupes::{find_dupes, write_csv, write_json, write_text};
use pixrs::serve::{default_cache_dir, Config};
use pixrs::statics::LICENSE;
use pixrs::structs::{Cli, Command, DupesOpt, Format, ScanOpt, ServeOpt};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::exit;
use std::time::Instant;

/// This is where the magic happens.
fn main() {
    // Start the timer
    let now = Instant::now();

    // Initialize logger from RUST_LOG environment variable
    env_logger::init();

    set_ctrlc_handler();

    // Load command line arguments
    let cli = Cli::parse();

    // Short circuit to license display
    if cli.license {
        println!("{}", LICENSE);
        exit(exitcode::OK);
    }

    match cli.command {
        Some(Command::Dupes(opt)) => dupes(opt),
        Some(Command::Scan(opt)) => scan(opt, now),
        Some(Command::Serve(opt)) => serve(opt),
        None => {
            Cli::command().print_help().expect("Unable to print help!");
            exit(exitcode::USAGE);
        }
    }
}

/// The catalog path given on the command line, or the default one.
fn catalog_path(given: Option<PathBuf>) -> PathBuf {
    given.or_else(Catalog::default_path).unwrap_or_else(|| {
        eprintln!("Unable to find a data directory for the catalog; pass --catalog");
        exit(exitcode::USAGE)
    })
}

/// Print an error and exit with its exit code.
fn fail(context: &str, e: pixrs::error::Error) -> ! {
    debug!("{:?}", e);
    eprintln!("{}: {}", context, e);
    exit(e.exit_code())
}

/// The catalog IDs of the given libraries. Exits if one has not been scanned.
fn library_ids(catalog: &Catalog, libraries: &[PathBuf], context: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = vec![];
    for library in libraries {
        let root = library
            .canonicalize()
            .ok()
            .and_then(|root| root.to_str().map(String::from));
        let id = match root.map(|root| catalog.find_library(&root)).transpose() {
            Ok(id) => id.flatten(),
            Err(e) => fail(context, e.into()),
        };
        match id {
            Some(id) => ids.push(id),
            None => {
                eprintln!(
                    "{} has not been scanned. Run `pixrs scan {}` first.",
                    library.display(),
                    library.display()
                );
                exit(exitcode::USAGE)
            }
        }
    }
    ids
}

/// Report exact duplicates in the catalog.
fn dupes(opt: DupesOpt) {
    let catalog_path = catalog_path(opt.catalog);
    let catalog =
        Catalog::open_existing(&catalog_path).unwrap_or_else(|e| fail("Dupes failed", e));
    let libraries = library_ids(&catalog, &opt.libraries, "Dupes failed");
    let report = match find_dupes(&catalog, &libraries) {
        Ok(report) => report,
        Err(e) => fail("Dupes failed", e.into()),
    };

    let mut out = BufWriter::new(io::stdout().lock());
    let written = match opt.format {
        Format::Text => write_text(&report, &mut out),
        Format::Json => write_json(&report, &mut out),
        Format::Csv => write_csv(&report, &mut out),
    }
    .and_then(|_| out.flush());
    if let Err(e) = written {
        fail("Dupes failed", e.into());
    }

    let unhashed = report.summary.unhashed_candidates;
    if unhashed > 0 {
        eprintln!(
            "Note: {} files share their size with another file but are not hashed yet, so \
             there may be more duplicates. Scan their libraries again to hash them.",
            unhashed
        );
    }
}

/// Scan a media library into the catalog and print a summary.
fn scan(opt: ScanOpt, now: Instant) {
    let catalog = catalog_path(opt.catalog);
    match pixrs::scan::scan(&opt.library, &catalog, opt.quiet) {
        Ok(s) => {
            println!(
                "Scanned {} in {}: {} added, {} updated, {} removed, {} unchanged, {} hashed, \
                 {} skipped",
                opt.library.display(),
                HumanDuration(now.elapsed()),
                s.added,
                s.updated,
                s.removed,
                s.unchanged,
                s.hashed,
                s.skipped
            );
            println!("Catalog: {}", catalog.display());
        }
        Err(e) => fail("Scan failed", e),
    }
}

/// Browse the catalog in the local viewer until stopped.
fn serve(opt: ServeOpt) {
    let catalog_path = catalog_path(opt.catalog);
    let catalog =
        Catalog::open_existing(&catalog_path).unwrap_or_else(|e| fail("Serve failed", e));
    let libraries = library_ids(&catalog, &opt.libraries, "Serve failed");
    let cache_dir = opt.cache.or_else(default_cache_dir);
    if cache_dir.is_none() {
        eprintln!("No cache directory found; pass --cache to get thumbnails");
    }
    let config = Config {
        catalog,
        libraries,
        cache_dir,
        port: opt.port,
    };
    let ready = |url: &str| {
        println!("Serving the catalog at {}", url);
        println!("Press Ctrl-C to stop.");
        if opt.open {
            open::that(url).unwrap_or_else(|e| {
                error!("{:?}", e);
                eprintln!("Unable to open a browser: {}", e);
            });
        }
    };
    if let Err(e) = pixrs::serve::run(config, ready) {
        fail("Serve failed", e);
    }
}

/// Set Ctrl-C handler
fn set_ctrlc_handler() {
    ctrlc::set_handler(move || {
        println!("Ctrl-C received, stopping.");
        exit(exitcode::OK);
    })
    .expect("Error setting Ctrl-C handler");
}
