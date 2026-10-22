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
use indicatif::{HumanCount, HumanDuration};
use log::{debug, error};
use pixrs::catalog::{cataloged_spelling, display_root, Catalog};
use pixrs::dupes::{
    find_dupes, plural, write_csv, write_json, write_text, DupeReport, Similarity, Tier,
};
use pixrs::error::Error;
use pixrs::plan::{
    outside_libraries, plan_organize, plan_quarantine, read_plan, write_plan, write_summary,
    OrganizeOptions,
};
use pixrs::serve::{default_cache_dir, Config};
use pixrs::statics::{LICENSE, THIRD_PARTY_NOTICES};
use pixrs::structs::{
    ApplyOpt, Cli, Command, DupesOpt, Format, OrganizeOpt, RelocateOpt, ScanOpt, ServeOpt, UndoOpt,
};
use pixrs::template::Template;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::Instant;
use time::OffsetDateTime;

/// This is where the magic happens.
fn main() {
    // Start the timer
    let now = Instant::now();

    // Log from RUST_LOG; without it, only pixrs's own errors. Dependencies log errors pixrs
    // already reports (nom-exif on truncated files), and the lines break the progress bars.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("pixrs=error"))
        .init();

    set_ctrlc_handler();

    // Load command line arguments
    let cli = Cli::parse();

    // Short circuit to license display. Long texts are often piped to a pager; quitting it
    // early is not an error.
    if cli.license {
        let _ = writeln!(io::stdout(), "{}", LICENSE);
        exit(exitcode::OK);
    }
    if cli.third_party_notices {
        let _ = io::stdout().write_all(THIRD_PARTY_NOTICES.as_bytes());
        exit(exitcode::OK);
    }

    match cli.command {
        Some(Command::Apply(opt)) => apply(opt),
        Some(Command::Dupes(opt)) => dupes(opt),
        Some(Command::Organize(opt)) => organize(opt),
        Some(Command::Relocate(opt)) => relocate(opt),
        Some(Command::Undo(opt)) => undo(opt),
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
    let known = catalog
        .libraries()
        .unwrap_or_else(|e| fail(context, e.into()));
    let mut ids: Vec<i64> = vec![];
    for library in libraries {
        // Canonical, and spelled as the catalog spells it on a case-insensitive file system.
        let path = library
            .canonicalize()
            .ok()
            .map(|path| cataloged_spelling(&known, &path));
        let root = path.as_deref().and_then(|p| p.to_str().map(String::from));
        let id = match root.map(|root| catalog.find_library(&root)).transpose() {
            Ok(id) => id.flatten(),
            Err(e) => fail(context, e.into()),
        };
        match id {
            Some(id) => ids.push(id),
            None => {
                let containing = path
                    .as_deref()
                    .and_then(|path| catalog.containing_library(path).ok().flatten());
                if let Some((_, root)) = containing {
                    eprintln!(
                        "{} is part of the library {}. Pass the library instead.",
                        library.display(),
                        display_root(&root)
                    );
                    exit(exitcode::USAGE)
                }
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
    let similarity = Similarity {
        threshold: opt.threshold,
        images: opt.similar,
    };
    let report = match find_dupes(&catalog, &libraries, similarity) {
        Ok(report) => report,
        Err(e) => fail("Dupes failed", e.into()),
    };
    if let Some(dir) = &opt.quarantine {
        quarantine(&catalog, &catalog_path, &report, &libraries, dir);
        return;
    }

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

/// Plan moving every exact and same-pixel copy of a kept file to `dir`; with library
/// arguments, only copies in those libraries.
fn quarantine(
    catalog: &Catalog,
    catalog_path: &Path,
    report: &DupeReport,
    libraries: &[i64],
    dir: &Path,
) {
    let context = "Quarantine failed";
    let dir = outside_libraries(catalog, dir, "The quarantine folder")
        .unwrap_or_else(|e| fail(context, e));
    let now = OffsetDateTime::now_utc();
    let (plan, notes) = plan_quarantine(catalog, catalog_path, report, libraries, &dir, now)
        .unwrap_or_else(|e| fail(context, e));
    let path = write_plan(&plan, now).unwrap_or_else(|e| fail(context, e));
    let similar = report
        .groups
        .iter()
        .flat_map(|g| &g.files)
        .filter(|f| f.matches == Some(Tier::Similar))
        .count();
    if similar > 0 {
        println!(
            "{} {} only similar to the kept file and left alone.",
            similar,
            plural(similar, "file is", "files are")
        );
    }
    write_summary(&plan, &notes, &path, &mut io::stdout().lock())
        .unwrap_or_else(|e| fail(context, e.into()));
}

/// Plan how to organize a library and print the plan.
fn organize(opt: OrganizeOpt) {
    let context = "Organize failed";
    let catalog_path = catalog_path(opt.catalog);
    let catalog = Catalog::open_existing(&catalog_path).unwrap_or_else(|e| fail(context, e));
    let library_id = library_ids(&catalog, std::slice::from_ref(&opt.library), context)[0];
    let root = opt
        .library
        .canonicalize()
        .unwrap_or_else(|e| fail(context, e.into()));
    let template = match (&opt.template, &opt.layout) {
        (Some(template), _) => Template::parse(template),
        (None, Some(layout)) => Template::from_layout(layout),
        (None, None) => unreachable!("clap requires --template or --layout"),
    }
    .unwrap_or_else(|e| fail(context, Error::Usage(e)));
    let to = opt
        .to
        .as_deref()
        .map(|to| pixrs::scan::resolve(to).unwrap_or_else(|e| fail(context, e.into())));
    let now = OffsetDateTime::now_utc();
    let opts = OrganizeOptions {
        template: &template,
        undated: &opt.undated,
        suffix: &opt.suffix,
        trust_dates: opt.trust_dates,
        to,
        prune_empty: opt.prune_empty,
        now,
    };
    let (plan, notes) = plan_organize(&catalog, &catalog_path, library_id, &root, &opts)
        .unwrap_or_else(|e| fail(context, e));
    let path = write_plan(&plan, now).unwrap_or_else(|e| fail(context, e));
    write_summary(&plan, &notes, &path, &mut io::stdout().lock())
        .unwrap_or_else(|e| fail(context, e.into()));
}

/// Carry out a plan file.
fn apply(opt: ApplyOpt) {
    let context = "Apply failed";
    let plan = read_plan(&opt.plan).unwrap_or_else(|e| fail(context, e));
    let summary = pixrs::apply::apply(&opt.plan, &plan).unwrap_or_else(|e| {
        if let Some(journal) = &e.journal {
            eprintln!(
                "Stopped before the plan was done. To reverse what was done, run:\n  pixrs \
                 undo \"{}\"",
                journal.display()
            );
        }
        fail(context, e.error)
    });
    for (file, reason) in &summary.skipped {
        println!("Skipped {}: {}", file, reason);
    }
    println!(
        "Moved {} {}, skipped {}, removed {} empty {}.",
        summary.moved,
        plural(summary.moved, "file", "files"),
        summary.skipped.len(),
        summary.folders_removed,
        plural(summary.folders_removed, "folder", "folders")
    );
    println!(
        "Journal: {}\nTo reverse it, run:\n  pixrs undo \"{}\"",
        summary.journal.display(),
        summary.journal.display()
    );
}

/// Reverse an applied plan from its journal.
fn undo(opt: UndoOpt) {
    let summary = pixrs::apply::undo(&opt.journal).unwrap_or_else(|e| fail("Undo failed", e));
    for warning in &summary.warnings {
        println!("Warning: {}", warning);
    }
    println!(
        "Moved {} {} back.",
        summary.moved_back,
        plural(summary.moved_back, "file", "files")
    );
}

/// Point a library at the folder it was moved to and print a summary.
fn relocate(opt: RelocateOpt) {
    let catalog = catalog_path(opt.catalog);
    let s = pixrs::relocate::relocate(&catalog, &opt.old, &opt.new, opt.force)
        .unwrap_or_else(|e| fail("Relocate failed", e));
    println!(
        "{} of {} files compared are at {} unchanged.",
        s.matching,
        s.compared,
        opt.new.display()
    );
    for root in &s.folded {
        println!(
            "Folded the library {} into this one, with its hashes.",
            root
        );
    }
    println!(
        "The catalog lists the {} {} in the library {} now. Scan it to pick up any changes.",
        HumanCount(s.files as u64),
        plural(s.files, "file", "files"),
        s.library
    );
}

/// Scan a media library into the catalog and print a summary.
fn scan(opt: ScanOpt, now: Instant) {
    let catalog = catalog_path(opt.catalog);
    match pixrs::scan::scan(&opt.library, &catalog, opt.quiet) {
        Ok(s) => {
            for root in &s.folded {
                println!(
                    "Folded the library {} into this one, with its hashes.",
                    root
                );
            }
            if let Some(root) = &s.part_of {
                println!(
                    "{} is part of the library {}; scanned only that part.",
                    opt.library.display(),
                    root
                );
            }
            println!(
                "Scanned {} in {}: {} added, {} updated, {} removed, {} unchanged, {} hashed, \
                 {} decoded, {} skipped",
                opt.library.display(),
                HumanDuration(now.elapsed()),
                s.added,
                s.updated,
                s.removed,
                s.unchanged,
                s.hashed,
                s.media_hashed,
                s.skipped
            );
            if s.media_undecodable > 0 {
                println!(
                    "{} files could not be decoded; they are left out of similarity matching \
                     until they or the decoders change.",
                    s.media_undecodable
                );
            }
            if s.media_without_decoder > 0 {
                println!(
                    "{} files have no decoder (RAW; HEIC/AVIF without ffmpeg 7.1 or later; video \
                     without ffmpeg and ffprobe; audio without fpcalc on PATH); they are left \
                     out of same-pixel and similarity matching.",
                    s.media_without_decoder
                );
            }
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
        similarity: Similarity {
            threshold: opt.threshold,
            images: opt.similar,
        },
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
        pixrs::apply::release_lock();
        exit(exitcode::OK);
    })
    .expect("Error setting Ctrl-C handler");
}
