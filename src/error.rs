//! Errors for PixRS.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use std::fmt;
use std::io;

/// Errors that end a command.
#[derive(Debug)]
pub enum Error {
    /// The user asked for something that cannot be done.
    Usage(String),
    /// Reading or writing a file failed.
    Io(io::Error),
    /// The catalog database failed.
    Catalog(rusqlite::Error),
}

impl Error {
    /// The process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => exitcode::USAGE,
            Error::Io(_) => exitcode::IOERR,
            Error::Catalog(_) => exitcode::SOFTWARE,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(msg) => write!(f, "{}", msg),
            Error::Io(e) => write!(f, "{}", e),
            Error::Catalog(e) => write!(f, "catalog error: {}", e),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Usage(_) => None,
            Error::Io(e) => Some(e),
            Error::Catalog(e) => Some(e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Catalog(e)
    }
}
