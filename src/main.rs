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

use clap::Parser;
use indicatif::HumanDuration;
use log::{error, info};
use pixrs::statics::{PROTOCOL, SERVER_URL};
use pixrs::structs::PixrsOpt;
use pixrs::{validate_base_path_exists, create_out_path_unless_no_clobber, write_html};
use rouille::{match_assets, router, start_server, Response};
use std::env;
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
    let opt = PixrsOpt::parse();

    // Short circuit to license display
    if opt.license {
        panic!("FIXME: --license option not implemented");
    }

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

fn serve(assets_path: String, local_assets_path: String, url: String) -> Response {
    println!("\nNow serving your media files at {}", &url);
    println!("Press Ctrl-C in this window to stop serving.");
    start_server(
        SERVER_URL
            .strip_prefix(PROTOCOL)
            .unwrap_or_else(|| panic!("Expected SERVER_URL to start with '{}'", PROTOCOL)),
        move |req| {
            router!(req,
                (GET) (/) => { Response::redirect_303("/index.html") },
                _ => {
                    if let Some(req) = req.remove_prefix("/local") {
                        match_assets(&req, &local_assets_path)
                    } else {
                        match_assets(req, &assets_path).with_unique_header("Cache-Control", "no-store")
                    }
                },
            )
        },
    );
}

/// Set Ctrl-C handler
fn set_ctrlc_handler() {
    ctrlc::set_handler(move || {
        println!("Ctrl-C received, stopping.");
        exit(exitcode::OK);
    })
    .expect("Error setting Ctrl-C handler");
}
