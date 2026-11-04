//! How file names are compared. Names are stored as the file system spells them, but compared
//! in Unicode NFC: macOS may hand out a name decomposed (NFD: `e` and a combining accent) where
//! Linux and Windows have it composed (NFC: `é`), so one file can be read under both spellings.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use unicode_normalization::{is_nfc, UnicodeNormalization};

/// `name` in Unicode NFC; borrowed if it already is.
pub fn nfc(name: &str) -> Cow<'_, str> {
    if is_nfc(name) {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(name.nfc().collect())
    }
}

/// What two names have in common if a file system that ignores letter case and Unicode
/// normalization (macOS) takes them for the same name: lowercase, in NFC.
pub fn folded(name: &str) -> String {
    nfc(&name.to_lowercase()).into_owned()
}

/// The file at the `/`-separated relative path `rel` below `root`, with each part that is not in
/// its folder under that spelling replaced by a name there that is the same in NFC, if there is
/// one. `None` if a part is in neither form.
pub fn respelled_path(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        let exact = path.join(part);
        if fs::symlink_metadata(&exact).is_ok() {
            path = exact;
            continue;
        }
        let wanted = nfc(part);
        let found = fs::read_dir(&path).ok()?.flatten().find_map(|entry| {
            let name = entry.file_name();
            (nfc(name.to_str()?) == wanted).then_some(name)
        })?;
        path.push(found);
    }
    Some(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use std::fs::{create_dir_all, write};
    use tempfile::tempdir;

    /// `é` composed (NFC) and decomposed (NFD).
    pub const NFC: &str = "Caf\u{e9}";
    pub const NFD: &str = "Cafe\u{301}";

    #[test]
    fn test_nfc() {
        assert!(matches!(nfc("plain.jpg"), Cow::Borrowed(_)));
        assert!(matches!(nfc(NFC), Cow::Borrowed(_)));
        assert_eq!(nfc(NFD), NFC);
        assert_ne!(NFC, NFD);
    }

    #[test]
    fn test_folded() {
        assert_eq!(folded("CAFE\u{301}.JPG"), folded("caf\u{e9}.jpg"));
        assert_ne!(folded("cafe.jpg"), folded("caf\u{e9}.jpg"));
    }

    #[test]
    fn test_respelled_path() {
        let dir = tempdir().unwrap();
        create_dir_all(dir.path().join(NFD)).unwrap();
        write(dir.path().join(NFD).join(format!("{NFD}.jpg")), b"x").unwrap();

        let rel = format!("{NFC}/{NFC}.jpg");
        // On a file system that ignores normalization (macOS), the name as given is there too.
        let found = respelled_path(dir.path(), &rel).unwrap();
        assert_eq!(fs::read(found).unwrap(), b"x");
        assert_eq!(respelled_path(dir.path(), "Cafe/x.jpg"), None);
    }
}
