//! Plans of file moves: computed by `pixrs organize` and `pixrs dupes --quarantine`, written to
//! a file, carried out by `pixrs apply`.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::catalog::{library_path, within, Catalog, FileRecord, FileRef, Library, Stamp};
use crate::dates::{from_name, Taken};
use crate::dupes::{plural, DupeFile, DupeGroup, DupeReport, Role, Tier};
use crate::error::{AtPath, Error};
use crate::library::{LibraryId, Root};
use crate::media::MediaType;
use crate::names::folded;
use crate::template::{sanitize, Facts, Rendered, Template};
use crate::text::{printable, shown};
use log::debug;
use same_file::is_same_file;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::OffsetDateTime;

/// Version of the plan and journal formats. Before 1.0, plans and journals of another version
/// are refused, not converted: plan again.
pub(crate) const PLAN_VERSION: u32 = 4;

/// Extensions of files that belong to a media file with the same name, lowercase: edits
/// (`xmp`), iPhone edit lists (`aae`), video thumbnails (`thm`), subtitles (`srt`) and Google
/// Takeout metadata (`json`).
const SIDECAR_EXTENSIONS: &[&str] = &["aae", "json", "srt", "thm", "xmp"];

/// How many moves the summary lists.
const SUMMARY_MOVES: usize = 20;

/// What a plan does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PlanKind {
    /// Rename and move the files of a library by a template.
    Organize {
        /// The template, as given.
        template: String,
        /// Remove folders the moves leave empty.
        prune_empty: bool,
    },
    /// Move copies of kept files to the quarantine folder.
    Quarantine,
}

impl PlanKind {
    /// The name of the kind, as in plan file names.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Organize { .. } => "organize",
            Self::Quarantine => "quarantine",
        }
    }
}

/// One file to move.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    /// Where the file is, as an absolute path.
    pub from: PathBuf,
    /// Where it goes, as an absolute path.
    pub to: PathBuf,
    /// Size and mtime when the plan was made. A file that no longer matches is not moved.
    #[serde(flatten)]
    pub stamp: Stamp,
    /// What the file is, which says what else the move depends on and changes.
    #[serde(flatten)]
    pub kind: MoveKind,
}

/// What a moved file is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "move", rename_all = "kebab-case")]
pub enum MoveKind {
    /// A cataloged file that `organize` moves within its library, or out of it.
    Organize {
        /// Its catalog row.
        row: FileRef,
        /// The row's new path in the same library, or `None` if the file leaves the library.
        new_path: Option<String>,
    },
    /// A cataloged copy of a kept file, moved to quarantine. Its row leaves the catalog.
    Quarantine {
        row: FileRef,
        /// The kept copy, which must still be there unchanged.
        keeper: Keeper,
    },
    /// A sidecar file of the media file that the move at index `of` moves. Not cataloged.
    Sidecar { of: usize },
}

/// The kept copy of a quarantined file, as it was when the plan was made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keeper {
    pub path: PathBuf,
    #[serde(flatten)]
    pub stamp: Stamp,
}

/// A library a plan works in. Folders are never pruned at or above its root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanLibrary {
    pub id: LibraryId,
    pub root: Root,
}

/// A plan, as written to and read from a plan file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    #[serde(flatten)]
    pub kind: PlanKind,
    /// When the plan was made, RFC 3339 in UTC.
    pub created: String,
    /// The catalog the plan was made from, which `apply` updates.
    pub catalog: PathBuf,
    /// The folder every move goes below: the one the files are organized into (the library
    /// root, or the `--to` folder), or the quarantine folder.
    pub destination: PathBuf,
    pub libraries: Vec<PlanLibrary>,
    pub moves: Vec<Move>,
}

/// The version of a plan file, read before the rest so that a plan of another version is
/// refused by its version, not by the first field that changed.
#[derive(Deserialize)]
struct Versioned {
    version: u32,
}

/// Why a plan cannot be made, read or carried out.
#[derive(Debug)]
pub enum PlanError {
    /// `--suffix` lacks `{n}`.
    SuffixWithoutN,
    /// A folder files would be moved to (`what`, like "The quarantine folder") overlaps a
    /// library.
    Overlap {
        what: &'static str,
        folder: PathBuf,
        library: Root,
    },
    /// The file is not a plan.
    NotAPlan {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// The plan was written by another version of pixrs.
    Version { path: PathBuf, found: u32 },
    /// The plan's quarantine folder and one of its libraries overlap.
    QuarantineOverlap { destination: PathBuf, library: Root },
    /// Move `number` (counted from 1), of the file `from`, is not one pixrs would plan.
    BadMove {
        number: usize,
        from: PathBuf,
        problem: MoveProblem,
    },
}

/// What is wrong with a move in a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveProblem {
    /// Its source is not in a library of the plan.
    OutsideLibraries,
    /// Its target is not below the plan's destination.
    OutsideDestination(PathBuf),
    /// It is a sidecar of a move that is not an earlier media move.
    BadSidecar,
    /// It refers to a library that is not in the plan.
    UnknownLibrary,
    /// Its kept copy is not in a library of the plan.
    KeeperOutside,
    /// It is a kind of move another kind of plan makes.
    WrongKind,
}

impl PlanError {
    /// The process exit code: a bad argument is a usage error, a bad plan file a data error.
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::SuffixWithoutN | Self::Overlap { .. } => exitcode::USAGE,
            Self::NotAPlan { .. }
            | Self::Version { .. }
            | Self::QuarantineOverlap { .. }
            | Self::BadMove { .. } => exitcode::DATAERR,
        }
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SuffixWithoutN => write!(f, "--suffix must contain {{n}}"),
            Self::Overlap {
                what,
                folder,
                library,
            } => write!(
                f,
                "{} {} must be outside the library {}, and not contain it",
                what,
                folder.display(),
                library
            ),
            Self::NotAPlan { path, source } => {
                write!(f, "{} is not a plan file: {}", path.display(), source)
            }
            Self::Version { path, found } => write!(
                f,
                "{} has plan version {}, but this version of pixrs only understands version {}; \
                 plan again",
                path.display(),
                found,
                PLAN_VERSION
            ),
            Self::QuarantineOverlap {
                destination,
                library,
            } => write!(
                f,
                "The plan cannot be carried out: the quarantine folder {} and the library {} \
                 overlap",
                destination.display(),
                library
            ),
            Self::BadMove {
                number,
                from,
                problem,
            } => write!(
                f,
                "The plan cannot be carried out: move {} ({}) {}",
                number,
                from.display(),
                problem
            ),
        }
    }
}

impl fmt::Display for MoveProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideLibraries => write!(f, "is not in a library of the plan"),
            Self::OutsideDestination(destination) => {
                write!(f, "goes outside {}", destination.display())
            }
            Self::BadSidecar => {
                write!(
                    f,
                    "is a sidecar of a move that is not an earlier media move"
                )
            }
            Self::UnknownLibrary => write!(f, "refers to a library that is not in the plan"),
            Self::KeeperOutside => {
                write!(f, "has a kept copy that is not in a library of the plan")
            }
            Self::WrongKind => write!(f, "is not a move this kind of plan makes"),
        }
    }
}

impl std::error::Error for PlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `Display` shows the cause, so the chain goes on below it: a reporter that prints
        // every source shows each message once.
        match self {
            Self::NotAPlan { source, .. } => source.source(),
            _ => None,
        }
    }
}

/// Whether `path` is inside `folder`, and not `folder` itself.
pub(crate) fn below(path: &Path, folder: &Path) -> bool {
    path.starts_with(folder) && path != folder
}

impl Plan {
    /// Why the plan must not be carried out, if it must not: a plan file is input like any
    /// other file, so before anything is moved every source must be in one of the plan's
    /// libraries, every target below its destination, and the moves must be of the plan's kind
    /// and refer to each other and to the libraries consistently.
    pub fn check(&self) -> Result<(), PlanError> {
        let in_a_library =
            |path: &Path| self.libraries.iter().any(|l| below(path, l.root.as_path()));
        let known = |row: &FileRef| self.libraries.iter().any(|l| l.id == row.library_id);
        let quarantine = self.kind == PlanKind::Quarantine;
        if quarantine {
            let overlapping = self.libraries.iter().find(|l| {
                within(&self.destination, l.root.as_path())
                    || within(l.root.as_path(), &self.destination)
            });
            if let Some(library) = overlapping {
                return Err(PlanError::QuarantineOverlap {
                    destination: self.destination.clone(),
                    library: library.root.clone(),
                });
            }
        }
        for (index, m) in self.moves.iter().enumerate() {
            let fault = |problem| PlanError::BadMove {
                number: index + 1,
                from: m.from.clone(),
                problem,
            };
            if !in_a_library(&m.from) {
                return Err(fault(MoveProblem::OutsideLibraries));
            }
            if !below(&m.to, &self.destination) {
                return Err(fault(MoveProblem::OutsideDestination(
                    self.destination.clone(),
                )));
            }
            let problem = match &m.kind {
                MoveKind::Organize { row, .. } if !quarantine => {
                    (!known(row)).then_some(MoveProblem::UnknownLibrary)
                }
                MoveKind::Quarantine { row, keeper } if quarantine => {
                    if !known(row) {
                        Some(MoveProblem::UnknownLibrary)
                    } else if !in_a_library(&keeper.path) {
                        Some(MoveProblem::KeeperOutside)
                    } else {
                        None
                    }
                }
                MoveKind::Sidecar { of } => {
                    let earlier_media = *of < index
                        && self
                            .moves
                            .get(*of)
                            .is_some_and(|media| !matches!(media.kind, MoveKind::Sidecar { .. }));
                    (!earlier_media).then_some(MoveProblem::BadSidecar)
                }
                MoveKind::Organize { .. } | MoveKind::Quarantine { .. } => {
                    Some(MoveProblem::WrongKind)
                }
            };
            if let Some(problem) = problem {
                return Err(fault(problem));
            }
        }
        Ok(())
    }
}

/// A target the plan gave another name than the template or layout wanted, since something
/// had that name already.
#[derive(Debug, PartialEq, Eq)]
pub struct Collision {
    pub wanted: PathBuf,
    pub got: PathBuf,
}

/// What planning found, besides the moves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PlanNotes {
    /// Files already where the plan would put them.
    pub unchanged: usize,
    /// Files with no usable date, put in the undated folder.
    pub undated: usize,
    /// Files whose target was taken and that got a suffix.
    pub collisions: Vec<Collision>,
    /// Warnings to show, e.g. sidecars that cannot follow their media file.
    pub warnings: Vec<PlanWarning>,
}

/// A file that stays where it is, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanWarning {
    /// `files` files would have been moved through the symbolic link `link`.
    ThroughLink { link: PathBuf, files: usize },
    /// A sidecar stays with the media file `media` in its folder, which does not move.
    StaysWithMedia { sidecar: PathBuf, media: String },
    /// The new name of a sidecar is taken.
    SidecarTargetTaken { sidecar: PathBuf, target: PathBuf },
    /// The target folder cannot be read, so any name in it could be taken.
    FolderUnreadable { file: PathBuf, folder: PathBuf },
    /// A copy is the same file as the kept one, under another name.
    SameAsKeeper { file: PathBuf, keeper: PathBuf },
}

impl fmt::Display for PlanWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThroughLink { link, files } => write!(
                f,
                "{} {} stay: {} is a symbolic link, and files are never moved through links",
                files,
                plural(*files, "file", "files"),
                link.display()
            ),
            Self::StaysWithMedia { sidecar, media } => write!(
                f,
                "{} stays with {}, which does not move",
                sidecar.display(),
                media
            ),
            Self::SidecarTargetTaken { sidecar, target } => write!(
                f,
                "{} stays: {} is taken",
                sidecar.display(),
                target.display()
            ),
            Self::FolderUnreadable { file, folder } => write!(
                f,
                "{} stays: the folder {} cannot be read",
                file.display(),
                folder.display()
            ),
            Self::SameAsKeeper { file, keeper } => write!(
                f,
                "{} stays: it is the same file as the kept copy {}",
                file.display(),
                keeper.display()
            ),
        }
    }
}

/// How `pixrs organize` lays out files.
pub struct OrganizeOptions<'a> {
    pub template: &'a Template,
    /// Folder name for files without a usable date.
    pub undated: &'a str,
    /// Suffix for colliding names, with `{n}` for the number (2, 3 and so on).
    pub suffix: &'a str,
    /// Use dates before 1980 or in the future as they are.
    pub trust_dates: bool,
    /// Put files below this folder instead of the library root.
    pub to: Option<PathBuf>,
    pub prune_empty: bool,
    pub now: OffsetDateTime,
}

/// Names of the files and folders in `dir`, as they are spelled, read once per folder. Empty
/// if the folder does not exist; `None` if it cannot be read, which callers must treat as
/// "anything could be there".
fn listing<'a>(cache: &'a mut Listings, dir: &Path) -> Option<&'a [String]> {
    cache
        .entry(dir.to_path_buf())
        .or_insert_with(|| match fs::read_dir(dir) {
            Ok(entries) => Some(
                entries
                    .filter_map(Result::ok)
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect(),
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some(vec![]),
            Err(_) => None,
        })
        .as_deref()
}

/// Folder listings read so far.
type Listings = HashMap<PathBuf, Option<Vec<String>>>;

/// Whether a target name can be had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Free,
    Taken,
    /// Its folder cannot be read, so anything could be there.
    Unreadable,
}

/// A target folder, as `Claims::folder` finds it.
enum Folder {
    /// The folder, spelled as it is on disk or first in the plan.
    At(PathBuf),
    /// A symbolic link on the way, which files are never moved through.
    ThroughLink(PathBuf),
}

/// Targets taken so far: by the plan, or by files on disk. Compared ignoring letter case and
/// Unicode normalization on every OS, since Windows and macOS file systems ignore case and macOS
/// ignores normalization, so a plan comes out the same everywhere.
#[derive(Default)]
struct Claims {
    claimed: HashSet<String>,
    on_disk: Listings,
    /// Folders by their `folded` path, as spelled on disk or first in the plan. A target folder
    /// that only differs in letter case or normalization from one of them is that folder, so
    /// changing the spelling of a folder name is never a move.
    folders: HashMap<String, PathBuf>,
}

impl Claims {
    fn key(path: &Path) -> String {
        folded(&path.to_string_lossy())
    }

    /// `base` joined with `folders`, each spelled as it already is on disk or in the plan, or
    /// the symbolic link on the way if a folder is one (in any letter case): files are never
    /// moved through links, which could lead anywhere.
    fn folder(&mut self, base: &Path, folders: &[impl AsRef<str>]) -> Folder {
        let mut dir = base.to_path_buf();
        for name in folders {
            let name = name.as_ref();
            let wanted = dir.join(name);
            let key = Self::key(&wanted);
            if let Some(known) = self.folders.get(&key) {
                dir = known.clone();
                continue;
            }
            let folded_name = folded(name);
            let mut existing = None;
            let names = listing(&mut self.on_disk, &dir).unwrap_or_default();
            for same_name in names.iter().filter(|n| folded(n) == folded_name) {
                let path = dir.join(same_name);
                match fs::symlink_metadata(&path).map(|m| m.file_type()) {
                    Ok(kind) if kind.is_symlink() => return Folder::ThroughLink(path),
                    Ok(kind) if kind.is_dir() && existing.is_none() => existing = Some(path),
                    _ => {}
                }
            }
            dir = existing.unwrap_or(wanted);
            self.folders.insert(key, dir.clone());
        }
        Folder::At(dir)
    }

    /// Whether `target` is free for the file now at `own`. Anything on disk with that name in
    /// any letter case or normalization takes it, except `own` itself, so a rename that only
    /// changes letter case or normalization is allowed unless another file has exactly the new
    /// name. A folder that cannot be read might hold anything: not free.
    fn is_free(&mut self, target: &Path, own: &Path) -> bool {
        self.check(target, own) == Target::Free
    }

    /// `is_free`, telling a taken target from one in a folder that cannot be read.
    fn check(&mut self, target: &Path, own: &Path) -> Target {
        if self.claimed.contains(&Self::key(target)) {
            return Target::Taken;
        }
        if target == own {
            return Target::Free;
        }
        let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
            return Target::Taken;
        };
        let folded_name = folded(&name.to_string_lossy());
        let own_name = (own.parent() == Some(dir))
            .then(|| own.file_name())
            .flatten()
            .and_then(OsStr::to_str);
        match listing(&mut self.on_disk, dir) {
            None => Target::Unreadable,
            Some(names)
                if names
                    .iter()
                    .any(|n| folded(n) == folded_name && Some(n.as_str()) != own_name) =>
            {
                Target::Taken
            }
            Some(_) => Target::Free,
        }
    }

    fn claim(&mut self, target: &Path) {
        self.claimed.insert(Self::key(target));
    }

    /// The first free target among `target` and its suffixed variants, claimed; `None` if the
    /// target folder cannot be read.
    fn claim_free(&mut self, target: &Path, own: &Path, suffix: &str) -> Option<PathBuf> {
        let mut candidate = target.to_path_buf();
        let mut n = 2;
        loop {
            match self.check(&candidate, own) {
                Target::Free => break,
                Target::Taken => {}
                Target::Unreadable => return None,
            }
            candidate = with_suffix(target, &suffix.replace("{n}", &n.to_string()));
            n += 1;
        }
        self.claim(&candidate);
        Some(candidate)
    }
}

/// Symbolic links that files would have been moved through, and how many files each kept
/// where they are, for one warning per link.
#[derive(Default)]
struct Links(BTreeMap<PathBuf, usize>);

impl Links {
    fn note(&mut self, link: PathBuf) {
        *self.0.entry(link).or_default() += 1;
    }

    fn warn(&self, notes: &mut PlanNotes) {
        for (link, &files) in &self.0 {
            notes.warnings.push(PlanWarning::ThroughLink {
                link: link.clone(),
                files,
            });
        }
    }
}

/// A file name split before its last extension, the dot going with the extension: `photo.jpg`
/// as `photo` and `.jpg`. A name without one, or with only a leading dot, has none.
pub(crate) fn split_name(name: &str) -> (&str, &str) {
    name.rfind('.')
        .filter(|&dot| dot > 0)
        .and_then(|dot| name.split_at_checked(dot))
        .unwrap_or((name, ""))
}

/// `dir/stem.ext` as `dir/stem<suffix>.ext`.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = split_name(&name);
    path.with_file_name(sanitize(&format!("{stem}{suffix}{ext}")))
}

/// `path`, if it is UTF-8 (as plan files need it), or an error naming `what` it is.
fn utf8(path: &Path, what: &'static str) -> Result<PathBuf, Error> {
    crate::error::utf8(path, what).map(PathBuf::from)
}

/// `target` relative to `root` with `/` separators, if it is inside it.
fn relative_to(root: &Path, target: &Path) -> Option<String> {
    let rel = target.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

/// A sidecar file of a media file: its name, its new name, and whether it matched by the media
/// file's name without extension (`IMG_1.xmp`), which other media files can share, rather than
/// its whole name (`IMG_1.jpg.json`).
struct Sidecar {
    name: String,
    new_name: String,
    by_stem: bool,
}

/// The file name without its last extension.
fn stem(name: &str) -> &str {
    split_name(name).0
}

/// The sidecar files of a media file `dir/name`: files in `dir` named `stem.<sidecar ext>` or
/// `name.<sidecar ext>`, with their new names for a media file renamed to `new_name`.
fn sidecars(dir: &Path, name: &str, new_name: &str, cache: &mut Listings) -> Vec<Sidecar> {
    let (old_stem, new_stem) = (stem(name), stem(new_name));
    let mut found = vec![];
    for candidate in listing(cache, dir).unwrap_or_default() {
        let (base, ext) = split_name(candidate);
        let Some(ext) = ext.strip_prefix('.') else {
            continue;
        };
        let ext = ext.to_ascii_lowercase();
        if !SIDECAR_EXTENSIONS.contains(&ext.as_str()) || !dir.join(candidate).is_file() {
            continue;
        }
        if base.eq_ignore_ascii_case(name) {
            found.push(Sidecar {
                name: candidate.clone(),
                new_name: format!("{new_name}.{ext}"),
                by_stem: false,
            });
        } else if base.eq_ignore_ascii_case(old_stem) {
            found.push(Sidecar {
                name: candidate.clone(),
                new_name: format!("{new_stem}.{ext}"),
                by_stem: true,
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Plan sidecar moves for `moves` (media moves only, all from cataloged files). A sidecar that
/// another media file in its folder shares, and that file stays, stays too.
fn add_sidecars(moves: &mut Vec<Move>, claims: &mut Claims, notes: &mut PlanNotes) {
    let mut cache = Listings::new();
    let moving: HashSet<String> = moves.iter().map(|m| Claims::key(&m.from)).collect();
    let mut moved: HashSet<String> = HashSet::new();
    let mut sidecar_moves = vec![];
    for (media, m) in moves.iter().enumerate() {
        let (Some(dir), Some(name), Some(to_dir), Some(new_name)) = (
            m.from.parent(),
            m.from.file_name().and_then(OsStr::to_str),
            m.to.parent(),
            m.to.file_name().and_then(OsStr::to_str),
        ) else {
            continue;
        };
        for sidecar in sidecars(dir, name, new_name, &mut cache) {
            let source = dir.join(&sidecar.name);
            // A sidecar shared by several media files (RAW and JPEG) follows the first.
            if !moved.insert(Claims::key(&source)) {
                continue;
            }
            if sidecar.by_stem {
                let sidecar_stem = stem(&sidecar.name);
                let staying = listing(&mut cache, dir)
                    .unwrap_or_default()
                    .iter()
                    .find(|other| {
                        stem(other).eq_ignore_ascii_case(sidecar_stem)
                            && MediaType::from_path(Path::new(other.as_str())).is_some()
                            && !moving.contains(&Claims::key(&dir.join(other.as_str())))
                    });
                if let Some(staying) = staying {
                    notes.warnings.push(PlanWarning::StaysWithMedia {
                        sidecar: source,
                        media: staying.clone(),
                    });
                    continue;
                }
            }
            let target = to_dir.join(&sidecar.new_name);
            if !claims.is_free(&target, &source) {
                notes.warnings.push(PlanWarning::SidecarTargetTaken {
                    sidecar: source,
                    target,
                });
                continue;
            }
            let Ok(stamp) = fs::metadata(&source).and_then(|m| Stamp::of(&m)) else {
                continue;
            };
            // Both are UTF-8: the folders are those of UTF-8 moves, the names from listings,
            // which hold UTF-8 names only.
            if source == target {
                continue;
            }
            claims.claim(&target);
            sidecar_moves.push(Move {
                from: source,
                to: target,
                stamp,
                kind: MoveKind::Sidecar { of: media },
            });
        }
    }
    moves.extend(sidecar_moves);
}

/// A cataloged file and where the template wants it.
struct Wanted {
    source: PathBuf,
    target: PathBuf,
    file: FileRecord,
}

/// Where a file's date comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DateSource {
    Metadata(Taken),
    FileName(Taken),
    None,
}

impl DateSource {
    const fn taken(self) -> Option<Taken> {
        match self {
            Self::Metadata(taken) | Self::FileName(taken) => Some(taken),
            Self::None => None,
        }
    }
}

impl std::fmt::Display for DateSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Metadata(_) => "date from metadata",
            Self::FileName(_) => "date from the file name",
            Self::None => "no date",
        })
    }
}

/// When a file was taken, from its metadata or else from its name. Dates before 1980 or in the
/// future count as missing unless dates are trusted.
fn date_taken(file: &FileRecord, name: &str, opts: &OrganizeOptions) -> DateSource {
    let plausible = |t: &Taken| opts.trust_dates || t.is_plausible(opts.now);
    let from_meta = file.meta.taken();
    if let (Some(raw), Some(false)) = (&file.meta.taken_at, from_meta.map(|t| plausible(&t))) {
        debug!(
            "{}: metadata date {} is before 1980 or in the future; ignored",
            printable(&file.path),
            printable(raw)
        );
    }
    if let Some(taken) = from_meta.filter(plausible) {
        DateSource::Metadata(taken)
    } else if let Some(taken) = from_name(name).filter(plausible) {
        DateSource::FileName(taken)
    } else {
        DateSource::None
    }
}

/// Where the template wants each file of the library, below `base`, in a deterministic order:
/// by target, then by where the file is now. Files that would go through a symbolic link stay,
/// with a warning.
fn wanted_targets(
    files: Vec<FileRecord>,
    root: &Path,
    base: &Path,
    opts: &OrganizeOptions,
    claims: &mut Claims,
    notes: &mut PlanNotes,
) -> Vec<Wanted> {
    let mut links = Links::default();
    let mut wanted: Vec<Wanted> = vec![];
    for file in files {
        let source = library_path(root, &file.path);
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let (stem, ext) = split_name(name);
        let ext = ext.strip_prefix('.').unwrap_or(ext);
        let date_from = date_taken(&file, name, opts);
        let taken = date_from.taken();
        if taken.is_none() {
            notes.undated += 1;
        }
        let camera = file.meta.camera();
        let ext = ext.to_lowercase();
        let facts = Facts {
            taken,
            media_type: file.media_type,
            camera: camera.as_deref(),
            name: stem,
            ext: &ext,
        };
        let Rendered {
            folders,
            file: file_name,
        } = opts.template.render(&facts, opts.undated);
        let target = match claims.folder(base, &folders) {
            Folder::At(folder) => folder.join(file_name),
            Folder::ThroughLink(link) => {
                debug!(
                    "{} stays: {} is a symbolic link",
                    printable(&file.path),
                    shown(&link)
                );
                links.note(link);
                continue;
            }
        };
        debug!(
            "{}: {}, wants {}",
            printable(&file.path),
            date_from,
            shown(target.strip_prefix(base).unwrap_or(&target))
        );
        wanted.push(Wanted {
            source,
            target,
            file,
        });
    }
    links.warn(notes);
    wanted.sort_by_cached_key(|w| (Claims::key(&w.target), w.source.clone()));
    wanted
}

/// The moves that put each file where it is wanted, or at a free variant of that name.
fn organize_moves(
    wanted: Vec<Wanted>,
    library_id: LibraryId,
    root: &Path,
    suffix: &str,
    claims: &mut Claims,
    notes: &mut PlanNotes,
) -> Result<Vec<Move>, Error> {
    let mut moves = vec![];
    for Wanted {
        source,
        target,
        file,
    } in wanted
    {
        let Some(assigned) = claims.claim_free(&target, &source, suffix) else {
            notes.warnings.push(PlanWarning::FolderUnreadable {
                folder: target.parent().unwrap_or(&target).to_path_buf(),
                file: source,
            });
            continue;
        };
        if assigned == source {
            debug!("{} stays: it is in place", shown(&source));
            notes.unchanged += 1;
            continue;
        }
        let new_path = relative_to(root, &assigned);
        if assigned != target {
            notes.collisions.push(Collision {
                wanted: target,
                got: assigned.clone(),
            });
        }
        moves.push(Move {
            from: utf8(&source, "File")?,
            to: utf8(&assigned, "Target")?,
            stamp: file.stamp,
            kind: MoveKind::Organize {
                row: FileRef {
                    library_id,
                    path: file.path,
                },
                new_path,
            },
        });
    }
    Ok(moves)
}

/// Plan how to organize one library by a template.
pub fn plan_organize(
    catalog: &Catalog,
    catalog_path: &Path,
    library_id: LibraryId,
    root: &Path,
    opts: &OrganizeOptions,
) -> Result<(Plan, PlanNotes), Error> {
    if !opts.suffix.contains("{n}") {
        return Err(PlanError::SuffixWithoutN.into());
    }
    let base = opts.to.clone().unwrap_or_else(|| root.to_path_buf());
    let mut notes = PlanNotes::default();
    let mut claims = Claims::default();
    let files = catalog.files(library_id)?;
    let wanted = wanted_targets(files, root, &base, opts, &mut claims, &mut notes);
    let mut moves = organize_moves(
        wanted,
        library_id,
        root,
        opts.suffix,
        &mut claims,
        &mut notes,
    )?;
    add_sidecars(&mut moves, &mut claims, &mut notes);

    let plan = Plan {
        version: PLAN_VERSION,
        kind: PlanKind::Organize {
            template: String::from(opts.template.source()),
            prune_empty: opts.prune_empty,
        },
        created: timestamp(opts.now),
        catalog: utf8(catalog_path, "Catalog")?,
        destination: utf8(&base, "Target folder")?,
        libraries: vec![PlanLibrary {
            id: library_id,
            root: Root::new(root).ok_or_else(|| Error::NotUtf8 {
                what: "Library",
                path: root.to_path_buf(),
            })?,
        }],
        moves,
    };
    Ok((plan, notes))
}

/// One folder name per library in the quarantine folder: the last folder of its root, with a
/// number to tell same names apart.
fn quarantine_folders(libraries: &[Library]) -> HashMap<LibraryId, String> {
    let mut folder_of = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for library in libraries {
        let shown = library.root.to_string();
        let last = Path::new(&shown).file_name().map_or_else(
            || String::from("library"),
            |n| n.to_string_lossy().into_owned(),
        );
        let mut name = sanitize(&last);
        let mut n = 2;
        while !used.insert(folded(&name)) {
            name = sanitize(&format!("{last}_{n}"));
            n += 1;
        }
        folder_of.insert(library.id, name);
    }
    folder_of
}

/// A quarantine plan being made.
struct Quarantine<'a> {
    /// The quarantine folder.
    dir: &'a Path,
    /// Libraries to quarantine copies from; all if empty.
    only: &'a [LibraryId],
    roots: HashMap<LibraryId, &'a Path>,
    folder_of: HashMap<LibraryId, String>,
    claims: Claims,
    links: Links,
    notes: PlanNotes,
    moves: Vec<Move>,
    /// Libraries copies are quarantined from and their kept copies are in, in the order first
    /// seen: `apply` checks that each kept copy is in one of the plan's libraries.
    involved: Vec<LibraryId>,
}

impl Quarantine<'_> {
    /// Plan moving the exact and same-pixel copies of the group's kept file, the group's first.
    fn group(&mut self, group: &DupeGroup) -> Result<(), Error> {
        let nothing_to_move = group
            .files
            .iter()
            .all(|f| matches!(f.role, Role::Keep | Role::Copy(Tier::Similar)));
        if nothing_to_move {
            return Ok(());
        }
        let Some((kept, copies)) = group.files.split_first() else {
            return Ok(());
        };
        let Some(kept_root) = self.roots.get(&kept.library_id) else {
            return Ok(());
        };
        let kept_path = library_path(kept_root, &kept.path);
        debug!(
            "{} group of {} files keeps {}",
            group.tier().as_str(),
            group.files.len(),
            shown(&kept_path)
        );
        let keeper = Keeper {
            path: utf8(&kept_path, "Kept file")?,
            stamp: Stamp {
                size: kept.size,
                mtime_ns: kept.mtime_ns,
            },
        };
        let planned = self.moves.len();
        for file in copies {
            self.copy(file, &keeper)?;
        }
        if self.moves.len() > planned {
            self.involve(kept.library_id);
        }
        Ok(())
    }

    /// Note that the plan works in the library `id`.
    fn involve(&mut self, id: LibraryId) {
        if !self.involved.contains(&id) {
            self.involved.push(id);
        }
    }

    /// Plan moving one copy of `keeper`, if it is one that moves.
    fn copy(&mut self, file: &DupeFile, keeper: &Keeper) -> Result<(), Error> {
        if !matches!(file.role, Role::Copy(Tier::Exact | Tier::SamePixels)) {
            debug!(
                "{} stays: only similar to the kept file",
                printable(&file.path)
            );
            return Ok(());
        }
        if !self.only.is_empty() && !self.only.contains(&file.library_id) {
            debug!(
                "{} stays: not in the libraries given",
                printable(&file.path)
            );
            return Ok(());
        }
        let (Some(root), Some(folder)) = (
            self.roots.get(&file.library_id),
            self.folder_of.get(&file.library_id),
        ) else {
            return Ok(());
        };
        let source = library_path(root, &file.path);
        // One file under two names (a hard link, or one folder cataloged under two spellings)
        // is no copy: moving it would leave nothing behind.
        if is_same_file(&source, &keeper.path).unwrap_or(false) {
            self.notes.warnings.push(PlanWarning::SameAsKeeper {
                file: source,
                keeper: keeper.path.clone(),
            });
            return Ok(());
        }
        let mut folders: Vec<&str> = std::iter::once(folder.as_str())
            .chain(file.path.split('/'))
            .collect();
        let name = folders.pop().unwrap_or_default();
        let target = match self.claims.folder(self.dir, &folders) {
            Folder::At(folder) => folder.join(name),
            Folder::ThroughLink(link) => {
                debug!(
                    "{} stays: {} is a symbolic link",
                    shown(&source),
                    shown(&link)
                );
                self.links.note(link);
                return Ok(());
            }
        };
        let Some(assigned) = self.claims.claim_free(&target, &source, "_{n}") else {
            self.notes.warnings.push(PlanWarning::FolderUnreadable {
                folder: target.parent().unwrap_or(&target).to_path_buf(),
                file: source,
            });
            return Ok(());
        };
        if assigned != target {
            self.notes.collisions.push(Collision {
                wanted: target,
                got: assigned.clone(),
            });
        }
        self.involve(file.library_id);
        self.moves.push(Move {
            from: utf8(&source, "File")?,
            to: utf8(&assigned, "Target")?,
            stamp: Stamp {
                size: file.size,
                mtime_ns: file.mtime_ns,
            },
            kind: MoveKind::Quarantine {
                row: FileRef {
                    library_id: file.library_id,
                    path: file.path.clone(),
                },
                keeper: keeper.clone(),
            },
        });
        Ok(())
    }
}

/// Plan moving every copy that has the same bytes or pixels as its group's keeper to `dir`, as
/// `dir/<library name>/<path in the library>`. With `only` libraries, only copies in them move.
/// Similar files are left alone: similar images are mostly burst shots, not copies, and a
/// re-encoded video may be the better copy. Exact copies in a similar group still move. Each
/// move requires its group's kept copy to still be there, unchanged, when the plan is applied.
pub fn plan_quarantine(
    catalog: &Catalog,
    catalog_path: &Path,
    report: &DupeReport,
    only: &[LibraryId],
    dir: &Path,
    now: OffsetDateTime,
) -> Result<(Plan, PlanNotes), Error> {
    let mut libraries = catalog.libraries()?;
    libraries.sort_by(|a, b| a.root.cmp(&b.root));
    let mut quarantine = Quarantine {
        dir,
        only,
        roots: libraries
            .iter()
            .map(|l| (l.id, Path::new(l.root.as_str())))
            .collect(),
        folder_of: quarantine_folders(&libraries),
        claims: Claims::default(),
        links: Links::default(),
        notes: PlanNotes::default(),
        moves: vec![],
        involved: vec![],
    };
    for group in &report.groups {
        quarantine.group(group)?;
    }
    let Quarantine {
        mut claims,
        links,
        mut notes,
        mut moves,
        involved,
        ..
    } = quarantine;
    add_sidecars(&mut moves, &mut claims, &mut notes);
    links.warn(&mut notes);
    let plan = Plan {
        version: PLAN_VERSION,
        kind: PlanKind::Quarantine,
        created: timestamp(now),
        catalog: utf8(catalog_path, "Catalog")?,
        destination: utf8(dir, "Quarantine folder")?,
        libraries: libraries
            .iter()
            .filter(|l| involved.contains(&l.id))
            .map(|l| PlanLibrary {
                id: l.id,
                root: l.root.clone(),
            })
            .collect(),
        moves,
    };
    Ok((plan, notes))
}

/// `dir` resolved (symlinks and `..`), if it is outside every cataloged library and contains
/// none: files moved there must not be cataloged again by the next scan.
pub fn outside_libraries(
    catalog: &Catalog,
    dir: &Path,
    what: &'static str,
) -> Result<PathBuf, Error> {
    let resolved = crate::paths::resolve(dir).at(dir)?;
    for library in catalog.libraries()? {
        let root = library.root.as_path();
        if within(&resolved, root) || within(root, &resolved) {
            return Err(PlanError::Overlap {
                what,
                folder: resolved,
                library: library.root,
            }
            .into());
        }
    }
    Ok(resolved)
}

/// RFC 3339 in UTC.
fn timestamp(now: OffsetDateTime) -> String {
    now.format(&Rfc3339).unwrap_or_default()
}

/// The folder plans and journals of a catalog go in: next to the catalog file.
pub fn state_dir(catalog_path: &Path, what: &str) -> PathBuf {
    catalog_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(what)
}

/// Write the plan to `<catalog folder>/plans/<kind>-<time>.json` and return the path.
pub fn write_plan(plan: &Plan, now: OffsetDateTime) -> Result<PathBuf, Error> {
    let dir = state_dir(&plan.catalog, "plans");
    fs::create_dir_all(&dir)?;
    let stamp = now
        .format(format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .unwrap_or_default();
    let kind = plan.kind.as_str();
    let mut path = dir.join(format!("{kind}-{stamp}.json"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{kind}-{stamp}-{n}.json"));
        n += 1;
    }
    let file = fs::File::create(&path)?;
    serde_json::to_writer_pretty(&file, plan).map_err(io::Error::from)?;
    file.sync_all()?;
    Ok(path)
}

/// Read a plan file of this version, and check it.
pub fn read_plan(path: &Path) -> Result<Plan, Error> {
    let text = fs::read_to_string(path).at(path)?;
    let not_a_plan = |source| PlanError::NotAPlan {
        path: path.to_path_buf(),
        source,
    };
    let Versioned { version } = serde_json::from_str(&text).map_err(not_a_plan)?;
    if version != PLAN_VERSION {
        return Err(PlanError::Version {
            path: path.to_path_buf(),
            found: version,
        }
        .into());
    }
    let plan: Plan = serde_json::from_str(&text).map_err(not_a_plan)?;
    plan.check()?;
    Ok(plan)
}

/// Print what a plan will do.
pub fn write_summary(
    plan: &Plan,
    notes: &PlanNotes,
    path: &Path,
    out: &mut impl Write,
) -> io::Result<()> {
    let sidecars = plan
        .moves
        .iter()
        .filter(|m| matches!(m.kind, MoveKind::Sidecar { .. }))
        .count();
    // Names from the library, and warnings that quote them.
    for m in plan.moves.iter().take(SUMMARY_MOVES) {
        writeln!(out, "{}\n  -> {}", shown(&m.from), shown(&m.to))?;
    }
    if plan.moves.len() > SUMMARY_MOVES {
        writeln!(
            out,
            "... and {} more (see the plan file)",
            plan.moves.len() - SUMMARY_MOVES
        )?;
    }
    for Collision { wanted, got } in &notes.collisions {
        writeln!(
            out,
            "Warning: {} is taken, using {}",
            shown(wanted),
            shown(got)
        )?;
    }
    for warning in &notes.warnings {
        writeln!(out, "Warning: {}", printable(&warning.to_string()))?;
    }
    writeln!(
        out,
        "\n{} {} to move ({} of them sidecars), {} already in place, {} {}{}.",
        plan.moves.len(),
        plural(plan.moves.len(), "file", "files"),
        sidecars,
        notes.unchanged,
        notes.collisions.len(),
        plural(notes.collisions.len(), "collision", "collisions"),
        if matches!(plan.kind, PlanKind::Organize { .. }) {
            format!(", {} undated", notes.undated)
        } else {
            String::new()
        }
    )?;
    writeln!(out, "Plan: {}", shown(path))?;
    if plan.moves.is_empty() {
        writeln!(out, "Nothing to do.")
    } else {
        writeln!(
            out,
            "Nothing has been moved. To carry out the plan, run:\n  pixrs apply \"{}\"",
            shown(path)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::{MediaMeta, Stamp};
    use crate::media::MediaType;
    use pretty_assertions::assert_eq;
    use tempfile::{tempdir, TempDir};
    use time::macros::datetime;

    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        catalog_path: PathBuf,
        catalog: Catalog,
        id: LibraryId,
    }

    fn fixture(files: &[(&str, Option<&str>)]) -> Fixture {
        let dir = tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let catalog_path = dir.path().join("catalog.sqlite");
        let catalog = Catalog::open(&catalog_path).unwrap();
        let id = catalog.library_id(root.to_str().unwrap()).unwrap();
        for (path, taken_at) in files {
            let full = library_path(&root, path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, path.as_bytes()).unwrap();
            let media_type = MediaType::from_path(Path::new(path)).unwrap();
            catalog
                .upsert(
                    id,
                    &FileRecord {
                        path: String::from(*path),
                        stamp: Stamp {
                            size: path.len() as u64,
                            mtime_ns: 1,
                        },
                        media_type,
                        sha256: None,
                        meta: MediaMeta {
                            taken_at: taken_at.map(String::from),
                            camera_model: Some(String::from("Pixel 7")),
                            ..MediaMeta::default()
                        },
                    },
                )
                .unwrap();
        }
        Fixture {
            _dir: dir,
            root,
            catalog_path,
            catalog,
            id,
        }
    }

    fn organize(f: &Fixture, template: &str, trust_dates: bool) -> (Plan, PlanNotes) {
        let template = Template::parse(template).unwrap();
        let opts = OrganizeOptions {
            template: &template,
            undated: "undated",
            suffix: "_{n}",
            trust_dates,
            to: None,
            prune_empty: false,
            now: datetime!(2026-10-20 12:00 UTC),
        };
        plan_organize(&f.catalog, &f.catalog_path, f.id, &f.root, &opts).unwrap()
    }

    /// Moves as `from -> to`, relative to the library root.
    fn moves(f: &Fixture, plan: &Plan) -> Vec<String> {
        let rel = |p: &Path| relative_to(&f.root, p).unwrap();
        plan.moves
            .iter()
            .map(|m| format!("{} -> {}", rel(&m.from), rel(&m.to)))
            .collect()
    }

    #[test]
    fn test_dates_and_fallbacks() {
        let f = fixture(&[
            ("a/exif.jpg", Some("2023-05-01T14:32:05+02:00")),
            ("a/2017-10-08 18.15.36.jpg", None),
            ("a/IMG-20190501-WA0001.jpg", None),
            ("a/DSC0047.mp4", None),
            ("a/broken.mov", Some("1904-01-01T00:00:00")),
            ("a/scan 1965-07-14.jpg", Some("1965-07-14T00:00:00")),
        ]);
        let (plan, notes) = organize(&f, "{type}/{year}/{date}_{time}.{ext}", false);
        let mut got = moves(&f, &plan);
        got.sort();
        assert_eq!(
            got,
            [
                "a/2017-10-08 18.15.36.jpg -> image/2017/2017-10-08_181536.jpg",
                "a/DSC0047.mp4 -> video/undated/DSC0047.mp4",
                "a/IMG-20190501-WA0001.jpg -> image/2019/2019-05-01_WA0001.jpg",
                "a/broken.mov -> video/undated/broken.mov",
                "a/exif.jpg -> image/2023/2023-05-01_143205.jpg",
                "a/scan 1965-07-14.jpg -> image/undated/scan 1965-07-14.jpg",
            ]
        );
        assert_eq!(notes.undated, 3);

        let (plan, _) = organize(&f, "{type}/{year}/{date}_{time}.{ext}", true);
        let got = moves(&f, &plan);
        assert!(got.contains(&String::from(
            "a/scan 1965-07-14.jpg -> image/1965/1965-07-14_000000.jpg"
        )));
        assert!(got.contains(&String::from(
            "a/broken.mov -> video/1904/1904-01-01_000000.mov"
        )));
    }

    #[test]
    fn test_collisions_and_unchanged() {
        let f = fixture(&[
            ("x/1.jpg", Some("2023-05-01T14:32:05")),
            ("y/2.jpg", Some("2023-05-01T14:32:05")),
            ("z/3.JPG", Some("2023-05-01T14:32:05")),
            ("2023-06-01_000000.jpg", Some("2023-06-01T00:00:00")),
            ("Upper.JPG", None),
        ]);
        // An unrelated file on disk takes a name too.
        fs::write(f.root.join("2023-05-01_143205_2.jpg"), b"not cataloged").unwrap();
        let (plan, notes) = organize(&f, "{date}_{time}.{ext}", false);
        let mut got = moves(&f, &plan);
        got.sort();
        assert_eq!(
            got,
            [
                "Upper.JPG -> Upper.jpg",
                "x/1.jpg -> 2023-05-01_143205.jpg",
                "y/2.jpg -> 2023-05-01_143205_3.jpg",
                "z/3.JPG -> 2023-05-01_143205_4.jpg",
            ]
        );
        assert_eq!(notes.unchanged, 1);
        assert_eq!(notes.collisions.len(), 2);
    }

    #[test]
    fn test_names_in_another_normalization_collide() {
        use crate::names::tests::{NFC, NFD};
        let (x, y) = (format!("x/{NFD}.jpg"), format!("y/{NFC}.jpg"));
        let f = fixture(&[(&x, None), (&y, None)]);
        let (plan, notes) = organize(&f, "{name}.{ext}", false);
        assert_eq!(
            moves(&f, &plan),
            [format!("{x} -> {NFD}.jpg"), format!("{y} -> {NFC}_2.jpg")]
        );
        assert_eq!(notes.collisions.len(), 1);
    }

    #[test]
    fn test_sidecars_follow() {
        let f = fixture(&[
            ("IMG_1.jpg", Some("2023-05-01T14:32:05")),
            ("IMG_1.cr2", Some("2023-05-01T14:32:05")),
        ]);
        fs::write(f.root.join("IMG_1.xmp"), b"edits").unwrap();
        fs::write(f.root.join("IMG_1.jpg.json"), b"takeout").unwrap();
        fs::write(f.root.join("IMG_10.xmp"), b"other").unwrap();
        let (plan, _) = organize(&f, "{year}/{date}_{time}.{ext}", false);
        let mut got = moves(&f, &plan);
        got.sort();
        assert_eq!(
            got,
            [
                "IMG_1.cr2 -> 2023/2023-05-01_143205.cr2",
                "IMG_1.jpg -> 2023/2023-05-01_143205.jpg",
                "IMG_1.jpg.json -> 2023/2023-05-01_143205.jpg.json",
                "IMG_1.xmp -> 2023/2023-05-01_143205.xmp",
            ]
        );
        assert_eq!(
            plan.moves
                .iter()
                .filter(|m| matches!(m.kind, MoveKind::Sidecar { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn test_unicode_and_round_trip_of_the_plan_file() {
        let f = fixture(&[("Ærø/Sommer på Ærø.jpg", None)]);
        let (plan, _) = organize(&f, "{type}/{name}.{ext}", false);
        assert_eq!(
            moves(&f, &plan),
            ["Ærø/Sommer på Ærø.jpg -> image/Sommer på Ærø.jpg"]
        );
        let now = datetime!(2026-10-20 12:00 UTC);
        let path = write_plan(&plan, now).unwrap();
        assert!(path.starts_with(f.catalog_path.parent().unwrap().join("plans")));
        assert_eq!(read_plan(&path).unwrap(), plan);
    }

    #[test]
    fn test_summary_escapes_control_characters_in_names() {
        let f = fixture(&[("a.jpg", None)]);
        let (mut plan, mut notes) = organize(&f, "image/{name}.{ext}", false);
        plan.moves[0].from = PathBuf::from("/lib/a\u{1b}[2J.jpg");
        notes.collisions.push(Collision {
            wanted: PathBuf::from("x\u{7}.jpg"),
            got: PathBuf::from("x\u{7}_2.jpg"),
        });
        notes.warnings.push(PlanWarning::SameAsKeeper {
            file: PathBuf::from("b.jpg\rDone."),
            keeper: PathBuf::from("a.jpg"),
        });
        let mut out = vec![];
        write_summary(&plan, &notes, Path::new("plan.json"), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains(['\u{1b}', '\u{7}', '\r']), "{text:?}");
        assert!(text.contains("/lib/a\\u{1b}[2J.jpg\n  -> "), "{}", text);
        assert!(text.contains("Warning: x\\u{7}.jpg is taken, using x\\u{7}_2.jpg"));
        assert!(text.contains("Warning: b.jpg\\rDone. stays"));
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_folder_is_not_free() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture(&[("d/Photo.JPG", None), ("d/Photo.jpg", None)]);
        let dir = f.root.join("d");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
        if fs::read_dir(&dir).is_ok() {
            // Running as root: permissions do not apply.
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let (plan, notes) = organize(&f, "d/{name}.{ext}", false);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(plan.moves, vec![]);
        assert_eq!(notes.warnings.len(), 1, "{:?}", notes.warnings);
    }

    #[test]
    fn test_quarantine_folder_must_not_contain_a_library() {
        let f = fixture(&[("a.jpg", None)]);
        let parent = f.root.parent().unwrap();
        assert!(outside_libraries(&f.catalog, parent, "Quarantine").is_err());
        assert!(outside_libraries(&f.catalog, &f.root.join("q"), "Quarantine").is_err());
        assert!(outside_libraries(&f.catalog, &parent.join("q"), "Quarantine").is_ok());
    }

    /// A plan of a media move (`a/1.jpg`) and its sidecar move, which passes the checks.
    fn checked_plan() -> (Fixture, Plan) {
        let f = fixture(&[("a/1.jpg", None)]);
        fs::write(f.root.join("a/1.xmp"), b"edits").unwrap();
        let (plan, _) = organize(&f, "{name}.{ext}", false);
        assert_eq!(plan.moves.len(), 2);
        assert_eq!(plan.moves[1].kind, MoveKind::Sidecar { of: 0 });
        plan.check().unwrap();
        (f, plan)
    }

    /// A quarantine move of `a/1.jpg`, whose kept copy is at `keeper`.
    fn quarantine_move(f: &Fixture, keeper: &Path) -> MoveKind {
        MoveKind::Quarantine {
            row: FileRef {
                library_id: f.id,
                path: String::from("a/1.jpg"),
            },
            keeper: Keeper {
                path: keeper.to_path_buf(),
                stamp: Stamp {
                    size: 1,
                    mtime_ns: 0,
                },
            },
        }
    }

    /// `p` as a quarantine plan into a folder next to the library, with the media move's kind.
    fn as_quarantine(p: &mut Plan, kind: MoveKind) {
        let q = p.libraries[0].root.as_path().parent().unwrap().join("q");
        p.kind = PlanKind::Quarantine;
        for m in &mut p.moves {
            m.to = q.join(m.to.file_name().unwrap());
        }
        p.destination = q;
        p.moves[0].kind = kind;
    }

    #[test]
    fn test_plans_are_checked() {
        type Fault<'a> = Box<dyn Fn(&mut Plan) + 'a>;
        let (f, plan) = checked_plan();
        let parent = f.root.parent().unwrap().to_path_buf();
        let outside = parent.join("elsewhere.jpg");
        let faults: Vec<(&str, Fault)> = vec![
            (
                "sidecar of a later move",
                Box::new(|p| p.moves[1].kind = MoveKind::Sidecar { of: 5 }),
            ),
            (
                "sidecar of itself",
                Box::new(|p| p.moves[1].kind = MoveKind::Sidecar { of: 1 }),
            ),
            (
                "sidecar of a sidecar",
                Box::new(|p| p.moves[0].kind = MoveKind::Sidecar { of: 1 }),
            ),
            (
                "source outside",
                Box::new(|p| p.moves[0].from.clone_from(&outside)),
            ),
            (
                "target outside",
                Box::new(|p| p.moves[0].to.clone_from(&outside)),
            ),
            (
                "target is the destination",
                Box::new(|p| p.moves[0].to = p.destination.clone()),
            ),
            (
                "unknown library",
                Box::new(|p| {
                    p.moves[0].kind = MoveKind::Organize {
                        row: FileRef {
                            library_id: LibraryId(99),
                            path: String::from("1.jpg"),
                        },
                        new_path: None,
                    };
                }),
            ),
            (
                "quarantine move in an organize plan",
                Box::new(|p| p.moves[0].kind = quarantine_move(&f, &f.root.join("b.jpg"))),
            ),
            (
                "organize move in a quarantine plan",
                Box::new(|p| as_quarantine(p, p.moves[0].kind.clone())),
            ),
            (
                "kept copy outside",
                Box::new(|p| as_quarantine(p, quarantine_move(&f, &outside))),
            ),
            (
                "quarantine folder is the library",
                Box::new(|p| {
                    p.kind = PlanKind::Quarantine;
                    p.destination = p.libraries[0].root.as_path().to_path_buf();
                }),
            ),
            (
                "quarantine folder contains the library",
                Box::new(|p| {
                    p.kind = PlanKind::Quarantine;
                    p.destination.clone_from(&parent);
                    for m in &mut p.moves {
                        m.to = PathBuf::from(m.to.to_str().unwrap().replace("lib", "lib_2"));
                    }
                }),
            ),
        ];
        for (what, fault) in faults {
            let mut faulty = plan.clone();
            fault(&mut faulty);
            let err = faulty.check().unwrap_err();
            assert!(
                matches!(
                    err,
                    PlanError::QuarantineOverlap { .. } | PlanError::BadMove { .. }
                ),
                "{what}: {err}"
            );
        }
    }

    #[test]
    fn test_plans_that_pass_the_checks() {
        let (f, plan) = checked_plan();
        // A quarantine move into a folder outside the library.
        let mut quarantine = plan.clone();
        as_quarantine(&mut quarantine, quarantine_move(&f, &f.root.join("b.jpg")));
        quarantine.check().unwrap();
        // Moving the files into a folder outside the library.
        let mut moved_out = plan;
        moved_out.destination = f.root.parent().unwrap().to_path_buf();
        for m in &mut moved_out.moves {
            m.to = PathBuf::from(m.to.to_str().unwrap().replace("lib", "sorted"));
        }
        moved_out.check().unwrap();
    }

    #[test]
    fn test_plans_of_another_version_are_refused() {
        let (_f, plan) = checked_plan();
        // Refused by its version, whatever else it holds.
        let path = write_plan(&plan, datetime!(2026-10-20 12:00 UTC)).unwrap();
        let old = fs::read_to_string(&path)
            .unwrap()
            .replace(&format!("\"version\": {PLAN_VERSION}"), "\"version\": 2");
        fs::write(&path, old).unwrap();
        let err = read_plan(&path).unwrap_err();
        assert!(err.to_string().contains("plan version 2"), "{}", err);
        fs::write(&path, r#"{"version": 1, "description": "old"}"#).unwrap();
        let err = read_plan(&path).unwrap_err();
        assert!(err.to_string().contains("plan version 1"), "{}", err);
        assert_eq!(err.exit_code(), exitcode::DATAERR);
    }

    #[cfg(unix)]
    #[test]
    fn test_no_file_is_planned_into_a_symbolic_link() {
        use std::os::unix::fs::symlink;
        let f = fixture(&[
            ("incoming/IMG_20230501_143201.jpg", None),
            ("incoming/IMG_20230502_143201.jpg", None),
        ]);
        let outside = f.root.parent().unwrap().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, f.root.join("2023")).unwrap();
        let (plan, notes) = organize(&f, "{year}/{name}.{ext}", false);
        assert_eq!(plan.moves, vec![]);
        assert_eq!(notes.warnings.len(), 1, "{:?}", notes.warnings);
        assert!(
            matches!(&notes.warnings[0], PlanWarning::ThroughLink { link, files: 2 } if link.ends_with("2023")),
            "{:?}",
            notes.warnings
        );
        // A link under another letter case is that folder on Windows and macOS, so it counts
        // everywhere.
        symlink(&outside, f.root.join("Image")).unwrap();
        let (plan, notes) = organize(&f, "{type}/{name}.{ext}", false);
        assert_eq!(plan.moves, vec![]);
        assert!(
            matches!(&notes.warnings[0], PlanWarning::ThroughLink { link, .. } if link.ends_with("Image")),
            "{:?}",
            notes.warnings
        );
        // A link elsewhere in the library is no concern.
        symlink(&outside, f.root.join("elsewhere")).unwrap();
        let (plan, _) = organize(&f, "{name}.{ext}", false);
        assert_eq!(plan.moves.len(), 2);
    }
}
