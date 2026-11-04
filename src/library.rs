//! The identity of a cataloged library: its row id and its root folder.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, Value, ValueRef};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

/// A library's row id in the catalog.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LibraryId(pub i64);

impl fmt::Display for LibraryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ToSql for LibraryId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for LibraryId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(Self)
    }
}

impl From<LibraryId> for Value {
    fn from(id: LibraryId) -> Self {
        Self::Integer(id.0)
    }
}

/// A library's root folder as the catalog stores it: canonical, and valid UTF-8.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Root(String);

impl Root {
    /// The root at the canonical folder `path`, or `None` if it is not valid UTF-8.
    pub fn new(path: &Path) -> Option<Self> {
        path.to_str().map(|root| Self(String::from(root)))
    }

    /// The root as stored.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The root folder.
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// The number of bytes in the stored root; a library inside another has a longer one.
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the stored root is empty, which no scanned library's is.
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for Root {
    fn from(root: &str) -> Self {
        Self(String::from(root))
    }
}

impl From<String> for Root {
    fn from(root: String) -> Self {
        Self(root)
    }
}

impl PartialEq<str> for Root {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Root {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl AsRef<Path> for Root {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

/// The root as people know it: without the `\\?\` prefix that `canonicalize` adds on Windows,
/// which most programs do not accept.
impl fmt::Display for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(unc) = self.0.strip_prefix(r"\\?\UNC\") {
            write!(f, r"\\{unc}")
        } else if let Some(local) = self.0.strip_prefix(r"\\?\") {
            f.write_str(local)
        } else {
            f.write_str(&self.0)
        }
    }
}

impl ToSql for Root {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for Root {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        String::column_result(value).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn test_root_display() {
        let shown = |root: &str| Root::from(root).to_string();
        assert_eq!(shown(r"\\?\C:\Photos"), r"C:\Photos");
        assert_eq!(shown(r"\\?\UNC\nas\share"), r"\\nas\share");
        assert_eq!(shown("/home/me/Photos"), "/home/me/Photos");
    }
}
