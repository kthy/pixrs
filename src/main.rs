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
use pixrs::catalog::{cataloged_spelling, Catalog};
use pixrs::dupes::{
    find_dupes, plural, write_csv, write_json, write_text, DupeReport, Role, Similarity, Tier,
};
use pixrs::error::Error;
use pixrs::plan::{
    outside_libraries, plan_organize, plan_quarantine, read_plan, write_plan, write_summary,
    OrganizeOptions,
};
use pixrs::scan::ScanOptions;
use pixrs::serve::{default_cache_dir, Config};
use pixrs::statics::{LICENSE, THIRD_PARTY_NOTICES};
use pixrs::structs::{
    ApplyOpt, Cli, Command, DupesOpt, Format, OrganizeOpt, RelocateOpt, ScanOpt, ServeOpt, UndoOpt,
};
use pixrs::template::Template;
use pixrs::text::{printable, printable_lines, shown};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::Instant;
use time::OffsetDateTime;

/// This is where the magic happens.
fn main() {
    // Start the timer
    let now = Instant::now();

    // Load command line arguments
    let cli = Cli::parse();

    // Log as RUST_LOG says; without it, pixrs's own errors, or its debug output with --debug.
    pixrs::logging::init(cli.debug);

    set_ctrlc_handler();

    // Short circuit to license display. Long texts are often piped to a pager; quitting it
    // early is not an error.
    if cli.license {
        let _ = writeln!(io::stdout(), "{LICENSE}");
        exit(exitcode::OK);
    }
    if cli.third_party_notices {
        let _ = io::stdout().write_all(THIRD_PARTY_NOTICES.as_bytes());
        exit(exitcode::OK);
    }

    let Some(command) = cli.command else {
        let _ = Cli::command().print_help();
        exit(exitcode::USAGE);
    };
    let context = context(&command);
    let done = match command {
        Command::Apply(opt) => apply(&opt),
        Command::Dupes(opt) => dupes(opt),
        Command::Organize(opt) => organize(opt),
        Command::Relocate(opt) => relocate(opt),
        Command::Undo(opt) => undo(&opt),
        Command::Scan(opt) => scan(opt, now),
        Command::Serve(opt) => serve(opt),
    };
    if let Err(e) = done {
        fail(context, &e);
    }
}

/// What the error message of a failed command starts with.
const fn context(command: &Command) -> &'static str {
    match command {
        Command::Apply(_) => "Apply failed",
        Command::Dupes(opt) if opt.quarantine.is_some() => "Quarantine failed",
        Command::Dupes(_) => "Dupes failed",
        Command::Organize(_) => "Organize failed",
        Command::Relocate(_) => "Relocate failed",
        Command::Undo(_) => "Undo failed",
        Command::Scan(_) => "Scan failed",
        Command::Serve(_) => "Serve failed",
    }
}

/// The catalog path given on the command line, or the default one.
fn catalog_path(given: Option<PathBuf>) -> Result<PathBuf, Error> {
    given.or_else(Catalog::default_path).ok_or(Error::NoDataDir)
}

/// Print an error and exit with its exit code. The message can quote names from a library.
fn fail(context: &str, e: &Error) -> ! {
    debug!("{e:?}");
    eprintln!("{}: {}", context, printable_lines(&e.to_string()));
    exit(e.exit_code())
}

/// The catalog IDs of the given libraries, which must have been scanned.
fn library_ids(catalog: &Catalog, libraries: &[PathBuf]) -> Result<Vec<i64>, Error> {
    let known = catalog.libraries()?;
    let mut ids: Vec<i64> = vec![];
    for library in libraries {
        // Canonical, and spelled as the catalog spells it on a case-insensitive file system.
        let path = library
            .canonicalize()
            .ok()
            .map(|path| cataloged_spelling(&known, &path));
        let root = path.as_deref().and_then(|p| p.to_str().map(String::from));
        let id = root
            .map(|root| catalog.find_library(&root))
            .transpose()?
            .flatten();
        if let Some(id) = id {
            ids.push(id);
        } else {
            let containing = path
                .as_deref()
                .and_then(|path| catalog.containing_library(path).ok().flatten());
            if let Some(containing) = containing {
                return Err(Error::PartOfLibrary {
                    path: library.clone(),
                    root: containing.root,
                });
            }
            return Err(Error::NotScanned(library.clone()));
        }
    }
    Ok(ids)
}

/// Report exact duplicates in the catalog.
fn dupes(opt: DupesOpt) -> Result<(), Error> {
    let catalog_path = catalog_path(opt.catalog)?;
    let catalog = Catalog::open_existing(&catalog_path)?;
    let libraries = library_ids(&catalog, &opt.libraries)?;
    let similarity = Similarity {
        threshold: opt.threshold,
        images: opt.similar,
    };
    let report = find_dupes(&catalog, &libraries, similarity)?;
    if let Some(dir) = &opt.quarantine {
        return quarantine(&catalog, &catalog_path, &report, &libraries, dir);
    }

    let mut out = BufWriter::new(io::stdout().lock());
    let written = match opt.format {
        Format::Text => write_text(&report, &mut out),
        Format::Json => write_json(&report, &mut out),
        Format::Csv => write_csv(&report, &mut out),
    }
    .and_then(|()| out.flush());
    written?;

    let unhashed = report.summary.unhashed_candidates;
    if unhashed > 0 {
        eprintln!(
            "Note: {unhashed} files share their size with another file but are not hashed yet, so \
             there may be more duplicates. Scan their libraries again to hash them."
        );
    }
    Ok(())
}

/// Plan moving every exact and same-pixel copy of a kept file to `dir`; with library
/// arguments, only copies in those libraries.
fn quarantine(
    catalog: &Catalog,
    catalog_path: &Path,
    report: &DupeReport,
    libraries: &[i64],
    dir: &Path,
) -> Result<(), Error> {
    let dir = outside_libraries(catalog, dir, "The quarantine folder")?;
    let now = OffsetDateTime::now_utc();
    let (plan, notes) = plan_quarantine(catalog, catalog_path, report, libraries, &dir, now)?;
    let path = write_plan(&plan, now)?;
    let similar = report
        .groups
        .iter()
        .flat_map(|g| &g.files)
        .filter(|f| f.role == Role::Copy(Tier::Similar))
        .count();
    if similar > 0 {
        println!(
            "{} {} only similar to the kept file and left alone.",
            similar,
            plural(similar, "file is", "files are")
        );
    }
    write_summary(&plan, &notes, &path, &mut io::stdout().lock())?;
    Ok(())
}

/// Plan how to organize a library and print the plan.
fn organize(opt: OrganizeOpt) -> Result<(), Error> {
    let catalog_path = catalog_path(opt.catalog)?;
    let catalog = Catalog::open_existing(&catalog_path)?;
    let library_id = library_ids(&catalog, std::slice::from_ref(&opt.library))?[0];
    let root = opt.library.canonicalize()?;
    let template = match (&opt.template, &opt.layout) {
        (Some(template), _) => Template::parse(template),
        (None, Some(layout)) => Template::from_layout(layout),
        (None, None) => unreachable!("clap requires --template or --layout"),
    }?;
    let to = opt.to.as_deref().map(pixrs::scan::resolve).transpose()?;
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
    let (plan, notes) = plan_organize(&catalog, &catalog_path, library_id, &root, &opts)?;
    let path = write_plan(&plan, now)?;
    write_summary(&plan, &notes, &path, &mut io::stdout().lock())?;
    Ok(())
}

/// Carry out a plan file.
fn apply(opt: &ApplyOpt) -> Result<(), Error> {
    let plan = read_plan(&opt.plan)?;
    let summary = pixrs::apply::apply(&opt.plan, &plan).map_err(|e| {
        if let Some(journal) = &e.journal {
            eprintln!(
                "Stopped before the plan was done. To reverse what was done, run:\n  pixrs \
                 undo \"{}\"",
                journal.display()
            );
        }
        e.error
    })?;
    for skipped in &summary.skipped {
        println!(
            "Skipped {}: {}",
            shown(&skipped.file),
            printable(&skipped.reason.to_string())
        );
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
    Ok(())
}

/// Reverse an applied plan from its journal.
fn undo(opt: &UndoOpt) -> Result<(), Error> {
    let summary = pixrs::apply::undo(&opt.journal)?;
    for warning in &summary.warnings {
        println!("Warning: {}", printable(warning));
    }
    println!(
        "Moved {} {} back.",
        summary.moved_back,
        plural(summary.moved_back, "file", "files")
    );
    Ok(())
}

/// Point a library at the folder it was moved to and print a summary.
fn relocate(opt: RelocateOpt) -> Result<(), Error> {
    let catalog = catalog_path(opt.catalog)?;
    let s = pixrs::relocate::relocate(&catalog, &opt.old, &opt.new, opt.force)?;
    println!(
        "{} of {} files compared are at {} unchanged.",
        s.matching,
        s.compared,
        opt.new.display()
    );
    for root in &s.folded {
        println!("Folded the library {root} into this one, with its hashes.");
    }
    println!(
        "The catalog lists the {} {} in the library {} now. Scan it to pick up any changes.",
        HumanCount(s.files as u64),
        plural(s.files, "file", "files"),
        s.library
    );
    Ok(())
}

/// Scan a media library into the catalog and print a summary.
fn scan(opt: ScanOpt, now: Instant) -> Result<(), Error> {
    let catalog = catalog_path(opt.catalog)?;
    let options = ScanOptions {
        quiet: opt.quiet,
        allow_empty: opt.allow_empty,
    };
    let s = pixrs::scan::scan(&opt.library, &catalog, options)?;
    for root in &s.folded {
        println!("Folded the library {root} into this one, with its hashes.");
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
    if s.respelled > 0 {
        println!(
            "{} cataloged files have their names in another Unicode normalization on \
             disk (accents composed or decomposed, as on macOS); the catalog now spells \
             them as on disk, keeping their hashes.",
            s.respelled
        );
    }
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
    Ok(())
}

/// Browse the catalog in the local viewer until stopped.
fn serve(opt: ServeOpt) -> Result<(), Error> {
    let catalog_path = catalog_path(opt.catalog)?;
    let catalog = Catalog::open_existing(&catalog_path)?;
    let libraries = library_ids(&catalog, &opt.libraries)?;
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
        println!("Serving the catalog at {url}");
        println!("Press Ctrl-C to stop.");
        if opt.open {
            open::that(url).unwrap_or_else(|e| {
                error!("{e:?}");
                eprintln!("Unable to open a browser: {e}");
            });
        }
    };
    pixrs::serve::run(config, ready)
}

/// Set Ctrl-C handler
fn set_ctrlc_handler() {
    ctrlc::set_handler(move || {
        println!("Ctrl-C received, stopping.");
        pixrs::apply::release_lock();
        exit(pixrs::error::INTERRUPTED);
    })
    .expect("Error setting Ctrl-C handler");
}
