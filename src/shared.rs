//! The meat of the matter.

use glob::{glob_with, MatchOptions};
use log::{debug, error, info, trace};
use maud::{html, Markup};
use std::fs::{canonicalize, create_dir_all, File};
use std::io::prelude::Write;
use std::path::{Path, PathBuf};
use time::{macros::format_description, OffsetDateTime};

/// Glob options.
static GLOPTS: MatchOptions = MatchOptions {
    case_sensitive: false,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Create a glob pattern.
pub(crate) fn create_glob_pattern(path: &str, recurse: bool) -> String {
    trace!("create_glob_pattern({:?}, {:?})", path, recurse);

    format!("{}{}/*.", path, if recurse { "/**" } else { "" })
}

/// Return a list of files with the given `extensions` in a directory given by `glob_pattern`.
pub(crate) fn find_files<F>(extensions: Vec<&str>, glob_pattern: &str, on_err: F) -> Vec<PathBuf>
where
    F: Fn(i32),
{
    trace!("find_files({:?}, {:?}, _)", extensions, glob_pattern);

    let mut files: Vec<PathBuf> = vec![];
    for extension in extensions {
        let pattern = &(glob_pattern.to_owned() + extension);
        let entries = glob_with(pattern, GLOPTS);
        match entries {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(path) => files.push(path),
                        Err(e) => error!("{:?}", e),
                    }
                }
            }
            Err(e) => {
                error!("{:?} (pattern was {:?})", e, pattern);
                eprintln!(
                    "Input error: {:?} is not a valid pattern ({})",
                    glob_pattern, e.msg
                );
                on_err(exitcode::USAGE)
            }
        }
    }
    files
}

/// Generate HTML header for the output.
pub(crate) fn html_header(from_path: &str) -> Markup {
    trace!("html_header({:?})", from_path);

    let dt = OffsetDateTime::now_local().unwrap_or_else(|e| {
        error!("Unable to get local time (defaulting to UTC): {:?}", e);
        OffsetDateTime::now_utc()
    });
    let dt_fmt = format_description!(
        "[weekday], [day] [month repr:long] [year] at \
         [hour repr:24]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]"
    );
    let formatted_dt = dt.format(dt_fmt).expect("Unable to format DateTime!");
    html!(
        h1 { "PixRS" }
        p { "Output from " code { (from_path) } " generated " (formatted_dt) ":" }
    )
}

/// Open a file in the default browser.
pub(crate) fn open_in_browser(path: &str) {
    trace!("open_in_browser({:?})", path);

    let canonical_path = canonicalize(path);
    match canonical_path {
        Ok(cpath) => {
            let file_url = format!("file:///{}", cpath.display());
            debug!("Opening {:?}", file_url);
            open::that(file_url).unwrap_or_else(|e| {
                error!("{:?}", e);
                eprintln!("Error opening output: {}", e);
            });
        }
        Err(e) => {
            error!("{:?}", e);
            eprintln!("Error canonicalizing path {:?}: {}", path, e);
        }
    };
}

/// If `path` is not an existing directory, execute the provided callback function.
pub(crate) fn validate_input<F>(cwd: &str, path: &str, on_err: F)
where
    F: Fn(i32),
{
    trace!("exit_if_nonexisting_input({:?}, {:?}, _)", cwd, path);

    if !Path::new(path).is_dir() {
        error!("{:?} is not a directory", path);
        eprintln!(
            "Input error: {:?} is not a directory I can read from {}",
            path, cwd
        );
        on_err(exitcode::USAGE);
    }
}

/// If `path`/index.html exists and `clobber` is not `True`, execute the provided callback
/// function. If `path` does not exist and `create_dir` is `True`, create it.
pub(crate) fn validate_output<F>(
    cwd: &str,
    path: &Path,
    idx: &str,
    clobber: bool,
    create_dir: bool,
    on_err: F,
) where
    F: Fn(i32),
{
    trace!(
        "exit_if_existing_output({:?}, {:?}, {:?}, {:?}, _)",
        cwd,
        path,
        idx,
        clobber
    );

    if path.is_dir() {
        if !clobber && Path::new(&idx).exists() {
            error!("{:?} exists and clobber is false", idx);
            eprintln!(
                "Error: {:?} already exists - to overwrite it, run PixRS with the --clobber flag",
                idx
            );
            on_err(exitcode::CANTCREAT);
        }
    } else if create_dir {
        create_dir_all(path).expect("Unable to create output directory!");
    } else {
        error!("{:?} is not a directory", path);
        eprintln!(
            "Input error: {:?} is not a directory I can write to from {}",
            path, cwd
        );
        on_err(exitcode::USAGE);
    }
}

/// Write `html` to `path`.
pub(crate) fn write_html(path: &str, html: &str) {
    trace!("write_html({:?}, {:?})", path, html);

    let mut out_file = match File::create(&path) {
        Err(why) => panic!("Couldn't create {}: {}", path, why),
        Ok(file) => file,
    };

    match out_file.write_all(html.as_bytes()) {
        Err(why) => panic!("Couldn't write to {}: {}", path, why),
        Ok(_) => info!("Successfully wrote to {}", path),
    }
}
