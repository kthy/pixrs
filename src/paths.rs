//! Paths given on the command line, resolved to where they really are.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::error::{AtPath, Error};
use std::io;
use std::path::{absolute, Component, Path, PathBuf};

/// `path` made absolute, with its longest existing prefix canonicalized so that symlinks and `..`
/// cannot hide where it really is. The parts that do not exist yet are applied lexically.
pub fn resolve(path: &Path) -> io::Result<PathBuf> {
    let path = absolute(path)?;
    for ancestor in path.ancestors() {
        if let Ok(mut resolved) = ancestor.canonicalize() {
            let Ok(rest) = path.strip_prefix(ancestor) else {
                continue;
            };
            for part in rest.components() {
                match part {
                    Component::ParentDir => {
                        resolved.pop();
                    }
                    Component::Normal(name) => resolved.push(name),
                    _ => {}
                }
            }
            return Ok(resolved);
        }
    }
    Ok(path)
}

/// `path` canonical, or `None` if nothing is there. An error names `path` if it cannot be
/// resolved for another reason, like permissions.
pub fn canonical(path: &Path) -> Result<Option<PathBuf>, Error> {
    match path.canonicalize() {
        Ok(canonical) => Ok(Some(canonical)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e).at(path),
    }
}
