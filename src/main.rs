//! Present a bunch of media files in a reasonable manner.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 *                                                                                               *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
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

use indicatif::HumanDuration;
use log::{error, info};
use pixrs::statics::{PROTOCOL, SERVER_URL};
use pixrs::structs::PixrsOpt;
use pixrs::{validate_input, validate_output, write_html};
use rouille::{match_assets, router, start_server, Response};
use std::env;
use std::process::exit;
use std::time::Instant;
use structopt::StructOpt;

/// This is where the magic happens.
fn main() {
    // Start the timer
    let now = Instant::now();

    // Initialize logger from RUST_LOG environment variable
    env_logger::init();

    // Set Ctrl-C handler
    ctrlc::set_handler(move || {
        println!("Ctrl-C received, stopping.");
        exit(exitcode::OK);
    })
    .expect("Error setting Ctrl-C handler");

    // Load command line arguments
    let opt = PixrsOpt::from_args();

    // Sanity check
    if opt.no_server && opt.open {
        eprintln!("Cannot use `--no-server` and `--open` flags at the same time!");
        exit(exitcode::USAGE)
    }

    // Infer current working directory
    let cwd = format!(
        "{}",
        env::current_dir()
            .expect("Unable to retrieve current directory!")
            .as_path()
            .display()
    );

    // Construct path to index page
    let idx = format!("{}/index.html", opt.out_path.display());

    // Callback to stop the timer and exit with the given code.
    let pixrs_exit = |code| {
        info!(
            "Execution interrupted after {}",
            HumanDuration(now.elapsed())
        );
        exit(code)
    };

    // Initial skirmish ends, now engage the enemy's main force!
    info!("Executing: {:?}", opt);

    validate_input(&cwd, &opt.base_path, pixrs_exit);

    validate_output(
        &cwd,
        &opt.out_path,
        &idx,
        opt.clobber,
        opt.create_output_dir,
        pixrs_exit,
    );

    write_html(&opt, &idx, pixrs_exit);

    info!(
        "HTML generation finished in {}",
        HumanDuration(now.elapsed())
    );

    let url = format!("{}/index.html", SERVER_URL);

    if opt.open {
        open_in_browser(&url);
    }

    if !opt.no_server {
        serve(opt, &url);
    }
}

fn open_in_browser(url: &str) {
    open::that(&url).unwrap_or_else(|e| {
        error!("{:?}", e);
        eprintln!("Error opening output: {}", e);
    });
}

fn serve(opt: PixrsOpt, url: &str) -> Response {
    println!("\nNow serving your media files at {}", &url);
    println!("Press Ctrl-C in this window to stop serving.");
    start_server(
        SERVER_URL.strip_prefix(&PROTOCOL).expect(&format!(
            "Expected SERVER_URL to start with '{}'",
            &PROTOCOL
        )),
        move |req| {
            router!(req,
                (GET) (/) => { Response::redirect_303("/index.html") },
                _ => {
                    if let Some(req) = req.remove_prefix("/local") {
                        return match_assets(&req, &opt.base_path);
                    }
                    match_assets(req, &opt.out_path).with_unique_header("Cache-Control", "no-store")
                },
            )
        },
    );
}
