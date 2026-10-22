//! Point a library in the catalog at the place it was moved to.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::apply::Lock;
use crate::catalog::{display_root, library_path, Catalog, Library};
use crate::error::Error;
use crate::scan::{progress_bar, resolve, stamp};
use same_file::is_same_file;
use std::fs;
use std::path::{absolute, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Most cataloged files compared with the new place before the catalog is changed.
const SAMPLE: usize = 100;

/// Files compared at once. On a network share each lookup waits for round trips, about a second
/// per file on an SMB share mounted in WSL.
const COMPARERS: usize = 8;

/// Share of the compared files, in percent, that must be at the new place unchanged.
const MIN_MATCHING: usize = 90;

/// What `relocate` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelocateSummary {
    /// Files in the library.
    pub files: usize,
    /// Files compared with the new place, and how many of them were there unchanged.
    pub compared: usize,
    pub matching: usize,
    /// The root of the library the files are in now: the new place, or the library it is part
    /// of.
    pub library: String,
    /// Roots of the libraries inside the new place that were folded in.
    pub folded: Vec<String>,
}

/// Point the library at `old` at `new`, where its files are now, keeping everything the catalog
/// knows about them. `old` need not exist any more. Before the catalog is changed, up to
/// `SAMPLE` files, spread over the library, are compared with what is at `new`; unless `force`
/// is set, at least `MIN_MATCHING` percent must be there with the same size and mtime. If `new`
/// is part of another library, the files become part of it; libraries inside `new` are folded
/// in, as a scan of `new` would.
///
/// Neither place is changed; files are only looked at.
pub fn relocate(
    catalog_path: &Path,
    old: &Path,
    new: &Path,
    force: bool,
) -> Result<RelocateSummary, Error> {
    let catalog_path = resolve(catalog_path)?;
    if !catalog_path.is_file() {
        return Err(Error::Usage(format!(
            "There is no catalog at {}. Run `pixrs scan <LIBRARY>` first.",
            catalog_path.display()
        )));
    }
    let _lock = Lock::take(&catalog_path)?;
    let catalog = Catalog::open(&catalog_path)?;
    let libraries = catalog.libraries()?;
    let library = find(&libraries, old)?;

    let new = new
        .canonicalize()
        .ok()
        .filter(|new| new.is_dir())
        .ok_or_else(|| Error::Usage(format!("{} is not a directory", new.display())))?;
    let new_str = new
        .to_str()
        .ok_or_else(|| Error::Usage(format!("{} is not valid UTF-8", new.display())))?;
    // Also under another letter case on a case-insensitive file system, which Linux keeps.
    if new == Path::new(&library.root) || is_same_file(&new, &library.root).unwrap_or(false) {
        return Err(Error::Usage(format!(
            "The library is already at {}",
            display_root(&library.root)
        )));
    }
    if catalog_path.starts_with(&new) {
        return Err(Error::Usage(format!(
            "The catalog {} must be outside the library {}",
            catalog_path.display(),
            new.display()
        )));
    }

    let files = catalog.files(library.id)?;
    let step = files.len().div_ceil(SAMPLE).max(1);
    let sample: Vec<_> = files.iter().step_by(step).collect();
    let pb = progress_bar(sample.len() as u64, "Comparing", false, false);
    let matching = AtomicUsize::new(0);
    thread::scope(|s| {
        for chunk in sample.chunks(sample.len().div_ceil(COMPARERS).max(1)) {
            let (new, pb, matching) = (&new, &pb, &matching);
            s.spawn(move || {
                for file in chunk {
                    let same = fs::metadata(library_path(new, &file.path))
                        .and_then(|meta| stamp(&meta))
                        .is_ok_and(|stamp| stamp == file.stamp);
                    if same {
                        matching.fetch_add(1, Ordering::Relaxed);
                    }
                    pb.inc(1);
                }
            });
        }
    });
    pb.finish_and_clear();
    let matching = matching.into_inner();
    if !force && matching * 100 < sample.len() * MIN_MATCHING {
        return Err(Error::Usage(format!(
            "Only {} of {} cataloged files compared are in {} with the same size and \
             modification time. Is it the right folder? With --force the library is moved \
             anyway, and the next scan reads the files that differ again.",
            matching,
            sample.len(),
            new.display()
        )));
    }

    let (root, folded) = catalog.relocate(library.id, new_str)?;
    Ok(RelocateSummary {
        files: files.len(),
        compared: sample.len(),
        matching,
        library: display_root(&root),
        folded: folded.iter().map(|root| display_root(root)).collect(),
    })
}

/// The library whose root is `old`. It may not exist any more, so it is compared resolved as far
/// as it exists and as given (made absolute), with and without Windows' `\\?\` prefix.
fn find<'a>(libraries: &'a [Library], old: &Path) -> Result<&'a Library, Error> {
    let mut wanted: Vec<PathBuf> = vec![];
    wanted.extend(resolve(old).ok());
    wanted.extend(absolute(old).ok());
    let found = libraries.iter().find(|library| {
        let shown = display_root(&library.root);
        wanted
            .iter()
            .any(|path| path == Path::new(&library.root) || path == Path::new(&shown))
    });
    found.ok_or_else(|| {
        let roots: Vec<String> = libraries
            .iter()
            .map(|library| format!("\n  {}", display_root(&library.root)))
            .collect();
        Error::Usage(format!(
            "{} is not a library in the catalog. The libraries are:{}",
            old.display(),
            roots.concat()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use std::fs::{create_dir_all, rename, write};
    use tempfile::{tempdir, TempDir};

    /// A library at `old`, scanned into a catalog next to it.
    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        fn new(files: &[(&str, &[u8])]) -> Self {
            let dir = tempdir().unwrap();
            for (rel, contents) in files {
                let path = dir.path().join("old").join(rel);
                create_dir_all(path.parent().unwrap()).unwrap();
                write(path, contents).unwrap();
            }
            let fx = Fixture { dir };
            crate::scan::scan(&fx.path("old"), &fx.catalog(), true).unwrap();
            fx
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        fn catalog(&self) -> PathBuf {
            self.path("catalog.sqlite")
        }

        fn root(&self, rel: &str) -> String {
            self.path(rel)
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        }

        /// Root and paths of every cataloged library, with whether each file has a hash.
        fn rows(&self) -> Vec<(String, Vec<(String, bool)>)> {
            let catalog = Catalog::open(&self.catalog()).unwrap();
            let libraries = catalog.libraries().unwrap();
            libraries
                .into_iter()
                .map(|library| {
                    let files = catalog.files(library.id).unwrap();
                    let files = files
                        .into_iter()
                        .map(|f| (f.path, f.sha256.is_some()))
                        .collect();
                    (library.root, files)
                })
                .collect()
        }
    }

    #[test]
    fn test_relocate_keeps_rows_and_hashes() {
        let fx = Fixture::new(&[("a.jpg", b"same"), ("sub/b.jpg", b"size")]);
        let old = fx.path("old");
        let old_root = fx.root("old");
        create_dir_all(fx.path("data")).unwrap();
        rename(&old, fx.path("data/k")).unwrap();

        let summary = relocate(&fx.catalog(), &old, &fx.path("data/k"), false).unwrap();
        assert_eq!(
            (summary.files, summary.compared, summary.matching),
            (2, 2, 2)
        );
        let new_root = fx.root("data/k");
        assert_eq!(summary.library, display_root(&new_root));
        assert_eq!(
            fx.rows(),
            vec![(
                new_root.clone(),
                vec![
                    (String::from("a.jpg"), true),
                    (String::from("sub/b.jpg"), true)
                ]
            )]
        );
        // A scan at the new place finds nothing to do.
        let scan = crate::scan::scan(&fx.path("data/k"), &fx.catalog(), true).unwrap();
        assert_eq!((scan.unchanged, scan.added, scan.hashed), (2, 0, 0));
        assert!(relocate(&fx.catalog(), Path::new(&old_root), &old, false).is_err());
    }

    #[test]
    fn test_relocate_refuses_a_folder_without_the_files() {
        let fx = Fixture::new(&[("a.jpg", b"a"), ("b.jpg", b"b")]);
        create_dir_all(fx.path("elsewhere")).unwrap();
        write(fx.path("elsewhere/a.jpg"), b"a").unwrap();

        let e = relocate(&fx.catalog(), &fx.path("old"), &fx.path("elsewhere"), false);
        assert!(e.unwrap_err().to_string().contains("Only 0 of 2"));
        assert_eq!(fx.rows()[0].0, fx.root("old"));

        let summary = relocate(&fx.catalog(), &fx.path("old"), &fx.path("elsewhere"), true);
        assert_eq!(summary.unwrap().matching, 0);
        assert_eq!(fx.rows()[0].0, fx.root("elsewhere"));
    }

    #[test]
    fn test_relocate_into_another_library_merges() {
        let fx = Fixture::new(&[("a.jpg", b"same"), ("b.jpg", b"size")]);
        create_dir_all(fx.path("data")).unwrap();
        write(fx.path("data/c.jpg"), b"c").unwrap();
        crate::scan::scan(&fx.path("data"), &fx.catalog(), true).unwrap();
        rename(fx.path("old"), fx.path("data/k")).unwrap();

        let summary = relocate(&fx.catalog(), &fx.path("old"), &fx.path("data/k"), false);
        assert_eq!(summary.unwrap().library, display_root(&fx.root("data")));
        assert_eq!(
            fx.rows(),
            vec![(
                fx.root("data"),
                vec![
                    (String::from("c.jpg"), false),
                    (String::from("k/a.jpg"), true),
                    (String::from("k/b.jpg"), true)
                ]
            )]
        );
    }

    #[test]
    fn test_relocate_names_the_libraries_when_old_is_unknown() {
        let fx = Fixture::new(&[("a.jpg", b"a")]);
        let e = relocate(&fx.catalog(), &fx.path("nope"), &fx.path("old"), false).unwrap_err();
        assert!(e.to_string().contains(&display_root(&fx.root("old"))));
    }
}
