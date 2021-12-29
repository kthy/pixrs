use glob::{glob_with, MatchOptions};
use log::{debug, error, info, trace};
use std::path::PathBuf;

static GLOPTS: MatchOptions = MatchOptions {
    case_sensitive: false,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

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
