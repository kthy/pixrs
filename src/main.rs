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

use axum::http::{header, HeaderValue};
use axum::response::Redirect;
use axum::routing::get;
use axum::Router;
use clap::{CommandFactory, Parser};
use indicatif::HumanDuration;
use log::{debug, error, info};
use pixrs::catalog::Catalog;
use pixrs::dupes::{find_dupes, write_csv, write_json, write_text};
use pixrs::statics::{LICENSE, PROTOCOL, SERVER_URL};
use pixrs::structs::{Cli, Command, DupesOpt, Format, PixrsOpt, ScanOpt};
use pixrs::{create_out_path_unless_no_clobber, validate_base_path_exists, write_html};
use std::env;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::exit;
use std::time::Instant;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeader;

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
        Some(Command::Gallery(opt)) => gallery(opt, now),
        Some(Command::Scan(opt)) => scan(opt, now),
        None => {
            Cli::command().print_help().expect("Unable to print help!");
            exit(exitcode::USAGE);
        }
    }
}

/// Generate a static HTML gallery, then optionally open and serve it.
fn gallery(opt: PixrsOpt, now: Instant) {
    let cwd = infer_current_working_dir();

    // Callback to stop the timer and exit with the given code.
    let callback_to_exit = |code| {
        info!(
            "Execution interrupted after {}",
            HumanDuration(now.elapsed())
        );
        exit(code)
    };

    // Initial skirmish ends, now engage the enemy's main force!
    info!("Executing: {:?}", opt);

    // Construct path to index page
    let idx = format!("{}/index.html", opt.out_path.display());

    validate_base_path_exists(&cwd, &opt.base_path, callback_to_exit);

    create_out_path_unless_no_clobber(
        &cwd,
        &opt.out_path,
        &idx,
        opt.clobber,
        opt.create_output_dir,
        callback_to_exit,
    );

    write_html(&opt, &idx, callback_to_exit);

    info!(
        "HTML generation finished in {}",
        HumanDuration(now.elapsed())
    );

    let url = format!("{}/index.html", SERVER_URL);

    if opt.open {
        open_in_browser(&url);
    }

    if !opt.no_server {
        serve(opt.out_path.display().to_string(), opt.base_path, url);
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

/// Report exact duplicates in the catalog.
fn dupes(opt: DupesOpt) {
    let catalog_path = catalog_path(opt.catalog);
    let catalog =
        Catalog::open_existing(&catalog_path).unwrap_or_else(|e| fail("Dupes failed", e));
    let mut libraries: Vec<i64> = vec![];
    for library in &opt.libraries {
        let root = library
            .canonicalize()
            .ok()
            .and_then(|root| root.to_str().map(String::from));
        let id = match root.map(|root| catalog.find_library(&root)).transpose() {
            Ok(id) => id.flatten(),
            Err(e) => fail("Dupes failed", e.into()),
        };
        match id {
            Some(id) => libraries.push(id),
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

/// Infer current working directory
fn infer_current_working_dir() -> String {
    let cwd = format!(
        "{}",
        env::current_dir()
            .expect("Unable to retrieve current directory!")
            .as_path()
            .display()
    );
    cwd
}

fn open_in_browser(url: &str) {
    open::that(url).unwrap_or_else(|e| {
        error!("{:?}", e);
        eprintln!("Error opening output: {}", e);
    });
}

fn serve(assets_path: String, local_assets_path: String, url: String) {
    let addr = SERVER_URL
        .strip_prefix(PROTOCOL)
        .unwrap_or_else(|| panic!("Expected SERVER_URL to start with '{}'", PROTOCOL));
    let assets = SetResponseHeader::overriding(
        ServeDir::new(assets_path),
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    let app = Router::new()
        .route("/", get(|| async { Redirect::to("/index.html") }))
        .nest_service("/local", ServeDir::new(local_assets_path))
        .fallback_service(assets);

    tokio::runtime::Runtime::new()
        .expect("Unable to start async runtime!")
        .block_on(async {
            let listener = TcpListener::bind(addr)
                .await
                .unwrap_or_else(|e| panic!("Unable to bind to {}: {}", addr, e));
            println!("\nNow serving your media files at {}", url);
            println!("Press Ctrl-C in this window to stop serving.");
            axum::serve(listener, app).await.expect("Server error!");
        });
}

/// Set Ctrl-C handler
fn set_ctrlc_handler() {
    ctrlc::set_handler(move || {
        println!("Ctrl-C received, stopping.");
        exit(exitcode::OK);
    })
    .expect("Error setting Ctrl-C handler");
}
