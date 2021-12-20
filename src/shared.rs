use glob::{glob_with, MatchOptions};
use log::{error, trace, warn};
use std::path::PathBuf;

static GLOPTS: MatchOptions = MatchOptions {
    case_sensitive: false,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

pub(crate) fn find_files<F>(extensions: Vec<&str>, search_path: &str, on_err: F) -> Vec<PathBuf>
where
    F: Fn(i32) -> (),
{
    trace!("find_files({:?}, {:?})", extensions, search_path);
    let mut files: Vec<PathBuf> = vec![];
    for extension in extensions {
        let pattern = &(search_path.to_owned() + extension);
        let entries = glob_with(pattern, GLOPTS);
        match entries {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(path) => files.push(path),
                        Err(e) => warn!("{:?}", e),
                    }
                }
            }
            Err(e) => {
                error!("{:?} (pattern was {:?})", e, pattern);
                eprintln!(
                    "Input error: {:?} is not a valid pattern ({})",
                    search_path, e.msg
                );
                on_err(exitcode::USAGE)
            }
        }
    }
    files
}
