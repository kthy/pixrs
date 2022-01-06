//! The meat of the matter.

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

use crate::statics::GLOPTS;
use crate::structs::{InventoryItem, PixrsOpt};
use data_encoding::HEXLOWER;
use glob::glob_with;
use indicatif::{ProgressBar, ProgressStyle};
use log::{error, info, trace};
use ring::digest::{Context, SHA256};
use std::fs::{create_dir_all, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

pub mod audio;
pub mod html;
pub mod image;
pub mod statics;
pub mod structs;
pub mod video;

/// Create a glob pattern.
fn create_glob_pattern(path: &str, recurse: bool) -> String {
    trace!("create_glob_pattern({:?}, {:?})", path, recurse);

    format!(
        "{}{}/*.",
        path.replace('\\', "/")
            .trim_start_matches("./")
            .trim_end_matches('/'),
        if recurse { "/**" } else { "" }
    )
}

/// Create an inventory item and return it together with its UUID.
fn create_inventory_item(digest: &str, path: &Path, glob_pattern: &str) -> (InventoryItem, Uuid) {
    let uuid = Uuid::new_v4();
    let inv_item = InventoryItem {
        digest: String::from(digest),
        local_path: path_to_local(path, glob_pattern),
        path: format!("{}", path.display()),
        uuid,
    };
    (inv_item, uuid)
}

/// Create a progress bar with the given length and prefixed message.
fn create_progress_bar(opt: &PixrsOpt, len: u64, prefix: String) -> ProgressBar {
    if opt.quiet {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(len).with_prefix(prefix);
        let t = "{prefix} [{elapsed}/{duration}] {wide_bar:.white/grey} {pos:>5}/{len:5} {>msg}";
        pb.set_style(
            ProgressStyle::default_bar()
                .template(t)
                .progress_chars("##-"),
        );
        pb
    }
}

/// Return a list of files with the given `extensions` in a directory given by `glob_pattern`.
fn find_files<F>(extensions: Vec<&str>, glob_pattern: &str, on_err: F) -> Vec<PathBuf>
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

/// Make a `/local/` URL-encoded path out of an `InventoryItem`'s path.
fn path_to_local(path: &Path, glob_pattern: &str) -> String {
    trace!("path_to_local({:?}, {:?})", path, glob_pattern);

    let fwd_pattern = glob_pattern.replace('\\', "/");
    let pattern_graphemes = fwd_pattern.graphemes(true).collect::<Vec<&str>>();
    let wildcard_position = pattern_graphemes
        .iter()
        .position(|grapheme| *grapheme == "*")
        .unwrap();
    let fwd_path = path.display().to_string().replace('\\', "/");
    let path_graphemes = fwd_path.graphemes(true).collect::<Vec<&str>>();
    let final_path = path_graphemes[wildcard_position..].concat();
    format!(
        "/local/{}",
        urlencoding::encode(&final_path).replace("%2F", "/")
    )
}

/// Calculate the SHA256 digest of the file at `path`.
fn sha256_digest(path: &Path) -> String {
    trace!("sha256_digest({:?})", path);

    let input = File::open(path).unwrap();
    let mut reader = BufReader::new(input);
    let mut context = Context::new(&SHA256);
    let mut buffer = [0; 1024];

    loop {
        let count = reader.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        context.update(&buffer[..count]);
    }

    HEXLOWER.encode(context.finish().as_ref())
}

/// If `path` is not an existing directory, execute the provided callback function.
pub fn validate_input<F>(cwd: &str, path: &str, on_err: F)
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
pub fn validate_output<F>(
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
pub fn write_html<F>(opt: &PixrsOpt, path: &str, on_err: F)
where
    F: Fn(i32),
{
    trace!("write_html({:?}, {:?})", &opt, &path);

    let index_html = html::html_index(opt, on_err);

    let mut out_file = match File::create(&path) {
        Err(why) => panic!("Couldn't create {}: {}", path, why),
        Ok(file) => file,
    };

    match out_file.write_all(index_html.into_string().as_bytes()) {
        Err(why) => panic!("Couldn't write to {}: {}", path, why),
        Ok(_) => info!("Successfully wrote to {}", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use tempfile::NamedTempFile;

    #[test]
    fn test_create_glob_pattern() {
        let mut expected = "foo/*.";
        let mut actual = create_glob_pattern("foo", false);
        assert_eq!(actual, expected);

        expected = "foo/**/*.";
        actual = create_glob_pattern("foo", true);
        assert_eq!(actual, expected);

        expected = "tests/files/**/*.";
        actual = create_glob_pattern(".\\tests\\files\\", true);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_find_files() {
        // use std::process::exit;
        let on_err = |_| panic!("at the callback");
        let expected: usize = 14;
        let actual = find_files(vec!["wav"], "tests/files/**/*.", on_err).len();
        assert_eq!(actual, expected);
    }

    #[test]
    #[should_panic]
    fn test_find_files_panic() {
        let on_err = |_| panic!("at the callback");
        let _ = find_files(vec!["wav"], "[", on_err);
    }

    #[test]
    fn test_path_to_local() {
        let expected = "/local/audio/Birk%20CTG.mp3";
        let p = Path::new("tests\\files\\audio/Birk CTG.mp3");
        let g = "tests/files/**/*.mp3";
        let actual = path_to_local(&p, &g);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_path_to_local_unicode() {
        let expected = "/local/%E3%83%AF%E3%82%A4%E3%83%95/bl%C3%A5%20b%C3%A6r%20gr%C3%B8d.txt";
        let p = Path::new(".\\tästs\\漢字\\ワイフ\\blå bær grød.txt");
        let g = ".\\tästs\\漢字\\**/*.txt";
        let actual = path_to_local(&p, &g);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_sha256_digest() {
        let expected = "dbd318c1c462aee872f41109a4dfd3048871a03dedd0fe0e757ced57dad6f2d7";
        let mut temp_file = NamedTempFile::new().expect("Couldn't create temp file!");
        write!(temp_file, "foo bar baz").expect("Couldn't write test file!");
        let actual = sha256_digest(&temp_file.path());
        let _ = temp_file.close();
        assert_eq!(actual, expected);
    }
}
