//! Plans of file moves: computed by `pixrs organize` and `pixrs dupes --quarantine`, written to
//! a file, carried out by `pixrs apply`.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::catalog::{display_root, library_path, Catalog};
use crate::dates::{from_name, from_taken_at};
use crate::dupes::{plural, DupeReport, Tier};
use crate::error::Error;
use crate::media::MediaType;
use crate::names::folded;
use crate::template::{sanitize, Facts, Template};
use crate::text::{printable, shown};
use log::debug;
use same_file::is_same_file;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::OffsetDateTime;

/// Version of the plan file format. Version 2 added `destination`.
const PLAN_VERSION: u32 = 2;

/// Extensions of files that belong to a media file with the same name, lowercase: edits
/// (`xmp`), iPhone edit lists (`aae`), video thumbnails (`thm`), subtitles (`srt`) and Google
/// Takeout metadata (`json`).
const SIDECAR_EXTENSIONS: &[&str] = &["aae", "json", "srt", "thm", "xmp"];

/// How many moves the summary lists.
const SUMMARY_MOVES: usize = 20;

/// What a plan does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanKind {
    Organize,
    Quarantine,
}

/// One file to move.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    /// Where the file is, as an absolute path.
    pub from: String,
    /// Where it goes, as an absolute path.
    pub to: String,
    /// Size and mtime when the plan was made. A file that no longer matches is not moved.
    pub size: u64,
    pub mtime_ns: i64,
    /// The file's catalog row (library ID and path), if it is cataloged; sidecars are not.
    pub cataloged: Option<(i64, String)>,
    /// The row's new library ID and path, or `None` if the file leaves the library.
    pub cataloged_to: Option<(i64, String)>,
    /// For a sidecar file, the index of its media file's move.
    pub sidecar_of: Option<usize>,
    /// For a quarantined copy, the copy that is kept, which must still be there unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keeper: Option<Keeper>,
}

/// The kept copy of a quarantined file, as it was when the plan was made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keeper {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
}

/// A library a plan works in. Folders are never pruned at or above its root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanLibrary {
    pub id: i64,
    pub root: String,
}

/// A plan, as written to and read from a plan file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    pub kind: PlanKind,
    /// When the plan was made, RFC 3339 in UTC.
    pub created: String,
    /// The catalog the plan was made from, which `apply` updates.
    pub catalog: String,
    /// The template, or the quarantine folder.
    pub description: String,
    /// The folder every move goes below: the one the files are organized into (the library
    /// root, or the `--to` folder), or the quarantine folder. Missing from version 1 plans,
    /// which still parse so that they are refused by version, with a message.
    #[serde(default)]
    pub destination: String,
    pub libraries: Vec<PlanLibrary>,
    /// Remove folders the moves leave empty.
    pub prune_empty: bool,
    pub moves: Vec<Move>,
}

impl Plan {
    /// Why the plan must not be carried out, if it must not: a plan file is input like any
    /// other file, so before anything is moved every source must be in one of the plan's
    /// libraries, every target below its destination, and the moves must refer to each other
    /// and to the libraries consistently.
    pub fn check(&self) -> Result<(), Error> {
        let fault = |what: String| Error::Usage(format!("The plan cannot be carried out: {what}"));
        let below = |path: &str, folder: &str| {
            let (path, folder) = (Path::new(path), Path::new(folder));
            path.starts_with(folder) && path != folder
        };
        let in_a_library = |path: &str| {
            self.libraries
                .iter()
                .any(|library| below(path, &library.root))
        };
        let known = |library_id: i64| self.libraries.iter().any(|l| l.id == library_id);
        if self.kind == PlanKind::Quarantine {
            for library in &self.libraries {
                if below(&self.destination, &library.root)
                    || below(&library.root, &self.destination)
                    || library.root == self.destination
                {
                    return Err(fault(format!(
                        "the quarantine folder {} and the library {} overlap",
                        self.destination,
                        display_root(&library.root)
                    )));
                }
            }
        }
        for (index, m) in self.moves.iter().enumerate() {
            let fault = |what: &str| fault(format!("move {} ({}) {}", index + 1, m.from, what));
            if !in_a_library(&m.from) {
                return Err(fault("is not in a library of the plan"));
            }
            if !below(&m.to, &self.destination) {
                return Err(fault(&format!("goes outside {}", self.destination)));
            }
            if let Some(media) = m.sidecar_of {
                let earlier_media = media < index && self.moves[media].sidecar_of.is_none();
                if !earlier_media {
                    return Err(fault(
                        "is a sidecar of a move that is not an earlier media move",
                    ));
                }
            }
            for (library_id, _) in m.cataloged.iter().chain(&m.cataloged_to) {
                if !known(*library_id) {
                    return Err(fault("refers to a library that is not in the plan"));
                }
            }
            if let Some(keeper) = &m.keeper {
                if !in_a_library(&keeper.path) {
                    return Err(fault(
                        "has a kept copy that is not in a library of the plan",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// What planning found, besides the moves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PlanNotes {
    /// Files already where the plan would put them.
    pub unchanged: usize,
    /// Files with no usable date, put in the undated folder.
    pub undated: usize,
    /// Files whose target was taken and that got a suffix.
    pub collisions: Vec<(String, String)>,
    /// Warnings to show, e.g. sidecars that cannot follow their media file.
    pub warnings: Vec<String>,
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

/// Targets taken so far: by the plan, or by files on disk. Compared ignoring letter case and
/// Unicode normalization on every OS, since Windows and macOS file systems ignore case and macOS
/// ignores normalization, so a plan comes out the same everywhere.
#[derive(Default)]
struct Taken {
    claimed: HashSet<String>,
    on_disk: Listings,
    /// Folders by their `folded` path, as spelled on disk or first in the plan. A target folder
    /// that only differs in letter case or normalization from one of them is that folder, so
    /// changing the spelling of a folder name is never a move.
    folders: HashMap<String, PathBuf>,
}

impl Taken {
    fn key(path: &Path) -> String {
        folded(&path.to_string_lossy())
    }

    /// `base` joined with `folders`, each spelled as it already is on disk or in the plan. If a
    /// folder on the way is a symbolic link (in any letter case), `Err` with the link: files are
    /// never moved through links, which could lead anywhere.
    fn folder(&mut self, base: &Path, folders: &[String]) -> Result<PathBuf, PathBuf> {
        let mut dir = base.to_path_buf();
        for name in folders {
            let wanted = dir.join(name);
            let key = Self::key(&wanted);
            if let Some(known) = self.folders.get(&key) {
                dir = known.clone();
                continue;
            }
            let folded_name = folded(name);
            let same_name: Vec<String> = listing(&mut self.on_disk, &dir)
                .unwrap_or_default()
                .iter()
                .filter(|n| folded(n) == folded_name)
                .cloned()
                .collect();
            let kind = |n: &String| {
                fs::symlink_metadata(dir.join(n))
                    .ok()
                    .map(|m| m.file_type())
            };
            if let Some(link) = same_name
                .iter()
                .find(|n| kind(n).is_some_and(|t| t.is_symlink()))
            {
                return Err(dir.join(link));
            }
            let existing = same_name
                .iter()
                .find(|n| kind(n).is_some_and(|t| t.is_dir()))
                .cloned();
            dir = existing.map_or(wanted, |n| dir.join(n));
            self.folders.insert(key, dir.clone());
        }
        Ok(dir)
    }

    /// Whether `target` is free for the file now at `own`. Anything on disk with that name in
    /// any letter case or normalization takes it, except `own` itself, so a rename that only
    /// changes letter case or normalization is allowed unless another file has exactly the new
    /// name. A folder that cannot be read might hold anything: not free.
    fn is_free(&mut self, target: &Path, own: &Path) -> bool {
        self.check(target, own) == Some(true)
    }

    /// `is_free`, with `None` for a folder that cannot be read.
    fn check(&mut self, target: &Path, own: &Path) -> Option<bool> {
        if self.claimed.contains(&Self::key(target)) {
            return Some(false);
        }
        if target == own {
            return Some(true);
        }
        let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
            return Some(false);
        };
        let folded_name = folded(&name.to_string_lossy());
        let own_name = (own.parent() == Some(dir))
            .then(|| own.file_name())
            .flatten()
            .and_then(|n| n.to_str());
        listing(&mut self.on_disk, dir).map(|names| {
            !names
                .iter()
                .any(|n| folded(n) == folded_name && Some(n.as_str()) != own_name)
        })
    }

    fn claim(&mut self, target: &Path) {
        self.claimed.insert(Self::key(target));
    }

    /// The first free target among `target` and its suffixed variants, claimed; `None` if the
    /// target folder cannot be read.
    fn claim_free(&mut self, target: &Path, own: &Path, suffix: &str) -> Option<PathBuf> {
        let mut candidate = target.to_path_buf();
        let mut n = 2;
        while !self.check(&candidate, own)? {
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
        for (link, n) in &self.0 {
            notes.warnings.push(format!(
                "{} {} stay: {} is a symbolic link, and files are never moved through links",
                n,
                plural(*n, "file", "files"),
                link.display()
            ));
        }
    }
}

/// `dir/stem.ext` as `dir/stem<suffix>.ext`.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.to_string(), String::new()),
    };
    path.with_file_name(sanitize(&format!("{stem}{suffix}{ext}")))
}

/// `path` as UTF-8, or a usage error naming `what`.
fn utf8(path: &Path, what: &str) -> Result<String, Error> {
    path.to_str()
        .map(String::from)
        .ok_or_else(|| Error::Usage(format!("{} {} is not valid UTF-8", what, path.display())))
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
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// The sidecar files of a media file `dir/name`: files in `dir` named `stem.<sidecar ext>` or
/// `name.<sidecar ext>`, with their new names for a media file renamed to `new_name`.
fn sidecars(dir: &Path, name: &str, new_name: &str, cache: &mut Listings) -> Vec<Sidecar> {
    let (old_stem, new_stem) = (stem(name), stem(new_name));
    let mut found = vec![];
    for candidate in listing(cache, dir).unwrap_or_default() {
        let Some((base, ext)) = candidate.rsplit_once('.') else {
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
fn add_sidecars(moves: &mut Vec<Move>, taken: &mut Taken, notes: &mut PlanNotes) {
    let mut cache = Listings::new();
    let moving: HashSet<String> = moves
        .iter()
        .map(|m| Taken::key(Path::new(&m.from)))
        .collect();
    let mut moved: HashSet<String> = HashSet::new();
    let media_moves = moves.len();
    for i in 0..media_moves {
        let (from, to) = (PathBuf::from(&moves[i].from), PathBuf::from(&moves[i].to));
        let (Some(dir), Some(name), Some(to_dir), Some(new_name)) = (
            from.parent(),
            from.file_name().and_then(|n| n.to_str()),
            to.parent(),
            to.file_name().and_then(|n| n.to_str()),
        ) else {
            continue;
        };
        for sidecar in sidecars(dir, name, new_name, &mut cache) {
            let source = dir.join(&sidecar.name);
            // A sidecar shared by several media files (RAW and JPEG) follows the first.
            if moved.contains(&Taken::key(&source)) {
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
                            && !moving.contains(&Taken::key(&dir.join(other.as_str())))
                    });
                if let Some(staying) = staying {
                    notes.warnings.push(format!(
                        "{} stays with {}, which does not move",
                        source.display(),
                        staying
                    ));
                    moved.insert(Taken::key(&source));
                    continue;
                }
            }
            moved.insert(Taken::key(&source));
            let target = to_dir.join(&sidecar.new_name);
            if !taken.is_free(&target, &source) {
                notes.warnings.push(format!(
                    "{} stays: {} is taken",
                    source.display(),
                    target.display()
                ));
                continue;
            }
            taken.claim(&target);
            let Ok(stamp) = fs::metadata(&source).and_then(|m| crate::scan::stamp(&m)) else {
                continue;
            };
            let (Ok(from), Ok(to)) = (utf8(&source, "Sidecar"), utf8(&target, "Target")) else {
                continue;
            };
            if from == to {
                continue;
            }
            moves.push(Move {
                from,
                to,
                size: stamp.size,
                mtime_ns: stamp.mtime_ns,
                cataloged: None,
                cataloged_to: None,
                sidecar_of: Some(i),
                keeper: None,
            });
        }
    }
}

/// Plan how to organize one library by a template.
#[allow(clippy::too_many_lines, reason = "to be split by phase")]
pub fn plan_organize(
    catalog: &Catalog,
    catalog_path: &Path,
    library_id: i64,
    root: &Path,
    opts: &OrganizeOptions,
) -> Result<(Plan, PlanNotes), Error> {
    let base = opts.to.clone().unwrap_or_else(|| root.to_path_buf());
    if !opts.suffix.contains("{n}") {
        return Err(Error::Usage(String::from("--suffix must contain {n}")));
    }
    let mut notes = PlanNotes::default();
    let mut claims = Taken::default();
    let mut links = Links::default();
    let mut targets: Vec<(PathBuf, PathBuf, crate::catalog::FileRecord)> = vec![];
    for file in catalog.files(library_id)? {
        let source = library_path(root, &file.path);
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
        let plausible = |t: &crate::dates::Taken| opts.trust_dates || t.is_plausible(opts.now);
        let from_meta = file.meta.taken_at.as_deref().and_then(from_taken_at);
        let (taken, date_from) = match from_meta.filter(plausible) {
            Some(taken) => (Some(taken), "date from metadata"),
            None => match from_name(name).filter(plausible) {
                Some(taken) => (Some(taken), "date from the file name"),
                None => (None, "no date"),
            },
        };
        if let (Some(raw), Some(false)) = (&file.meta.taken_at, from_meta.map(|t| plausible(&t))) {
            debug!(
                "{}: metadata date {} is before 1980 or in the future; ignored",
                printable(&file.path),
                printable(raw)
            );
        }
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
        let mut segments = opts.template.render(&facts, opts.undated);
        let file_name = segments.pop().expect("a template renders a file name");
        let target = match claims.folder(&base, &segments) {
            Ok(folder) => folder.join(file_name),
            Err(link) => {
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
            shown(target.strip_prefix(&base).unwrap_or(&target))
        );
        targets.push((source, target, file));
    }
    links.warn(&mut notes);
    // Deterministic: by target, then by where the file is now.
    targets.sort_by(|a, b| (Taken::key(&a.1), &a.0).cmp(&(Taken::key(&b.1), &b.0)));

    let mut moves: Vec<Move> = vec![];
    for (source, target, file) in targets {
        let Some(assigned) = claims.claim_free(&target, &source, opts.suffix) else {
            notes.warnings.push(format!(
                "{} stays: the folder {} cannot be read",
                source.display(),
                target.parent().unwrap_or(&target).display()
            ));
            continue;
        };
        if assigned == source {
            debug!("{} stays: it is in place", shown(&source));
            notes.unchanged += 1;
            continue;
        }
        if assigned != target {
            notes
                .collisions
                .push((target.display().to_string(), assigned.display().to_string()));
        }
        moves.push(Move {
            from: utf8(&source, "File")?,
            to: utf8(&assigned, "Target")?,
            size: file.stamp.size,
            mtime_ns: file.stamp.mtime_ns,
            cataloged_to: relative_to(root, &assigned).map(|rel| (library_id, rel)),
            cataloged: Some((library_id, file.path)),
            sidecar_of: None,
            keeper: None,
        });
    }
    add_sidecars(&mut moves, &mut claims, &mut notes);

    let plan = Plan {
        version: PLAN_VERSION,
        kind: PlanKind::Organize,
        created: timestamp(opts.now),
        catalog: utf8(catalog_path, "Catalog")?,
        description: String::from(opts.template.source()),
        destination: utf8(&base, "Target folder")?,
        libraries: vec![PlanLibrary {
            id: library_id,
            root: utf8(root, "Library")?,
        }],
        prune_empty: opts.prune_empty,
        moves,
    };
    Ok((plan, notes))
}

/// Plan moving every copy that has the same bytes or pixels as its group's keeper to `dir`, as
/// `dir/<library name>/<path in the library>`. With `only` libraries, only copies in them move.
/// Similar files are left alone: similar images are mostly burst shots, not copies, and a
/// re-encoded video may be the better copy. Exact copies in a similar group still move. Each
/// move requires its group's kept copy to still be there, unchanged, when the plan is applied.
#[allow(clippy::too_many_lines, reason = "to be split by phase")]
pub fn plan_quarantine(
    catalog: &Catalog,
    catalog_path: &Path,
    report: &DupeReport,
    only: &[i64],
    dir: &Path,
    now: OffsetDateTime,
) -> Result<(Plan, PlanNotes), Error> {
    let mut libraries = catalog.libraries()?;
    libraries.sort_by(|a, b| a.root.cmp(&b.root));
    // One folder per library, named after its last folder; a number tells same names apart.
    let mut folder_of: HashMap<i64, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for library in &libraries {
        let shown = display_root(&library.root);
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
    let roots: HashMap<i64, String> = libraries.iter().map(|l| (l.id, l.root.clone())).collect();

    let mut notes = PlanNotes::default();
    let mut taken = Taken::default();
    let mut links = Links::default();
    let mut moves = vec![];
    let mut involved: Vec<i64> = vec![];
    for group in &report.groups {
        if group
            .files
            .iter()
            .all(|f| f.matches == Some(Tier::Similar) || f.keep)
        {
            continue;
        }
        let keeper = &group.files[0];
        let Some(keeper_root) = roots.get(&keeper.library_id) else {
            continue;
        };
        debug!(
            "{} group of {} files keeps {}",
            group.tier.as_str(),
            group.files.len(),
            shown(&library_path(Path::new(keeper_root), &keeper.path))
        );
        let keeper = Keeper {
            path: utf8(
                &library_path(Path::new(keeper_root), &keeper.path),
                "Kept file",
            )?,
            size: keeper.size,
            mtime_ns: keeper.mtime_ns,
        };
        for file in group.files.iter().skip(1) {
            if !matches!(file.matches, Some(Tier::Exact | Tier::SamePixels)) {
                debug!(
                    "{} stays: only similar to the kept file",
                    printable(&file.path)
                );
                continue;
            }
            if !only.is_empty() && !only.contains(&file.library_id) {
                debug!(
                    "{} stays: not in the libraries given",
                    printable(&file.path)
                );
                continue;
            }
            let (Some(root), Some(folder)) =
                (roots.get(&file.library_id), folder_of.get(&file.library_id))
            else {
                continue;
            };
            let source = library_path(Path::new(root), &file.path);
            // One file under two names (a hard link, or one folder cataloged under two
            // spellings) is no copy: moving it would leave nothing behind.
            if is_same_file(&source, &keeper.path).unwrap_or(false) {
                notes.warnings.push(format!(
                    "{} stays: it is the same file as the kept copy {}",
                    source.display(),
                    keeper.path
                ));
                continue;
            }
            let mut segments: Vec<String> = std::iter::once(folder.clone())
                .chain(file.path.split('/').map(String::from))
                .collect();
            let name = segments.pop().unwrap_or_default();
            let target = match taken.folder(dir, &segments) {
                Ok(folder) => folder.join(name),
                Err(link) => {
                    debug!(
                        "{} stays: {} is a symbolic link",
                        shown(&source),
                        shown(&link)
                    );
                    links.note(link);
                    continue;
                }
            };
            let Some(assigned) = taken.claim_free(&target, &source, "_{n}") else {
                notes.warnings.push(format!(
                    "{} stays: the folder {} cannot be read",
                    source.display(),
                    target.parent().unwrap_or(&target).display()
                ));
                continue;
            };
            if assigned != target {
                notes
                    .collisions
                    .push((target.display().to_string(), assigned.display().to_string()));
            }
            if !involved.contains(&file.library_id) {
                involved.push(file.library_id);
            }
            moves.push(Move {
                from: utf8(&source, "File")?,
                to: utf8(&assigned, "Target")?,
                size: file.size,
                mtime_ns: file.mtime_ns,
                cataloged: Some((file.library_id, file.path.clone())),
                cataloged_to: None,
                sidecar_of: None,
                keeper: Some(keeper.clone()),
            });
        }
    }
    add_sidecars(&mut moves, &mut taken, &mut notes);
    links.warn(&mut notes);
    let plan = Plan {
        version: PLAN_VERSION,
        kind: PlanKind::Quarantine,
        created: timestamp(now),
        catalog: utf8(catalog_path, "Catalog")?,
        description: utf8(dir, "Quarantine folder")?,
        destination: utf8(dir, "Quarantine folder")?,
        libraries: libraries
            .iter()
            .filter(|l| involved.contains(&l.id))
            .map(|l| PlanLibrary {
                id: l.id,
                root: l.root.clone(),
            })
            .collect(),
        prune_empty: false,
        moves,
    };
    Ok((plan, notes))
}

/// `dir` resolved (symlinks and `..`), if it is outside every cataloged library and contains
/// none: files moved there must not be cataloged again by the next scan.
pub fn outside_libraries(catalog: &Catalog, dir: &Path, what: &str) -> Result<PathBuf, Error> {
    let resolved = crate::scan::resolve(dir)?;
    for library in catalog.libraries()? {
        if resolved.starts_with(&library.root) || Path::new(&library.root).starts_with(&resolved) {
            return Err(Error::Usage(format!(
                "{} {} must be outside the library {}, and not contain it",
                what,
                resolved.display(),
                display_root(&library.root)
            )));
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
    let dir = state_dir(Path::new(&plan.catalog), "plans");
    fs::create_dir_all(&dir)?;
    let stamp = now
        .format(format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .unwrap_or_default();
    let kind = match plan.kind {
        PlanKind::Organize => "organize",
        PlanKind::Quarantine => "quarantine",
    };
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

/// Read a plan file.
pub fn read_plan(path: &Path) -> Result<Plan, Error> {
    let text = fs::read_to_string(path)?;
    let plan: Plan = serde_json::from_str(&text)
        .map_err(|e| Error::Usage(format!("{} is not a plan file: {}", path.display(), e)))?;
    if plan.version != PLAN_VERSION {
        return Err(Error::Usage(format!(
            "{} has plan version {}, but this version of pixrs only understands version {}; \
             plan again",
            path.display(),
            plan.version,
            PLAN_VERSION
        )));
    }
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
    let sidecars = plan.moves.iter().filter(|m| m.sidecar_of.is_some()).count();
    // Names from the library, and warnings that quote them.
    for m in plan.moves.iter().take(SUMMARY_MOVES) {
        writeln!(out, "{}\n  -> {}", printable(&m.from), printable(&m.to))?;
    }
    if plan.moves.len() > SUMMARY_MOVES {
        writeln!(
            out,
            "... and {} more (see the plan file)",
            plan.moves.len() - SUMMARY_MOVES
        )?;
    }
    for (wanted, got) in &notes.collisions {
        writeln!(
            out,
            "Warning: {} is taken, using {}",
            printable(wanted),
            printable(got)
        )?;
    }
    for warning in &notes.warnings {
        writeln!(out, "Warning: {}", printable(warning))?;
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
        if plan.kind == PlanKind::Organize {
            format!(", {} undated", notes.undated)
        } else {
            String::new()
        }
    )?;
    writeln!(out, "Plan: {}", path.display())?;
    if plan.moves.is_empty() {
        writeln!(out, "Nothing to do.")
    } else {
        writeln!(
            out,
            "Nothing has been moved. To carry out the plan, run:\n  pixrs apply \"{}\"",
            path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::{FileRecord, MediaMeta, Stamp};
    use crate::media::MediaType;
    use pretty_assertions::assert_eq;
    use tempfile::{tempdir, TempDir};
    use time::macros::datetime;

    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        catalog_path: PathBuf,
        catalog: Catalog,
        id: i64,
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
        let rel = |p: &str| relative_to(&f.root, Path::new(p)).unwrap();
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
            plan.moves.iter().filter(|m| m.sidecar_of.is_some()).count(),
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
        plan.moves[0].from = String::from("/lib/a\u{1b}[2J.jpg");
        notes
            .collisions
            .push((String::from("x\u{7}.jpg"), String::from("x\u{7}_2.jpg")));
        notes.warnings.push(String::from("b.jpg\rDone. stays"));
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

    #[test]
    fn test_plans_are_checked() {
        type Fault<'a> = Box<dyn Fn(&mut Plan) + 'a>;
        let f = fixture(&[("a/1.jpg", None)]);
        fs::write(f.root.join("a/1.xmp"), b"edits").unwrap();
        let (plan, _) = organize(&f, "{name}.{ext}", false);
        assert_eq!(plan.moves.len(), 2);
        assert_eq!(plan.moves[1].sidecar_of, Some(0));
        plan.check().unwrap();
        let outside = f.root.parent().unwrap().join("elsewhere.jpg");
        let outside = outside.to_str().unwrap();
        let faults: Vec<(&str, Fault)> = vec![
            (
                "sidecar of a later move",
                Box::new(|p| p.moves[1].sidecar_of = Some(5)),
            ),
            (
                "sidecar of itself",
                Box::new(|p| p.moves[1].sidecar_of = Some(1)),
            ),
            (
                "sidecar of a sidecar",
                Box::new(|p| p.moves[0].sidecar_of = Some(1)),
            ),
            (
                "source outside",
                Box::new(|p| p.moves[0].from = String::from(outside)),
            ),
            (
                "target outside",
                Box::new(|p| p.moves[0].to = String::from(outside)),
            ),
            (
                "target is the destination",
                Box::new(|p| p.moves[0].to = p.destination.clone()),
            ),
            (
                "unknown library",
                Box::new(|p| p.moves[0].cataloged = Some((99, String::from("1.jpg")))),
            ),
            (
                "unknown library at the target",
                Box::new(|p| p.moves[0].cataloged_to = Some((99, String::from("1.jpg")))),
            ),
            (
                "kept copy outside",
                Box::new(|p| {
                    p.moves[0].keeper = Some(Keeper {
                        path: String::from(outside),
                        size: 1,
                        mtime_ns: 0,
                    });
                }),
            ),
            (
                "quarantine folder is the library",
                Box::new(|p| {
                    p.kind = PlanKind::Quarantine;
                    p.destination = p.libraries[0].root.clone();
                }),
            ),
            (
                "quarantine folder contains the library",
                Box::new(|p| {
                    p.kind = PlanKind::Quarantine;
                    p.destination = String::from(f.root.parent().unwrap().to_str().unwrap());
                    for m in &mut p.moves {
                        m.to = m.to.replace("lib", "lib_2");
                    }
                }),
            ),
        ];
        for (what, fault) in faults {
            let mut faulty = plan.clone();
            fault(&mut faulty);
            let err = faulty.check().unwrap_err();
            assert!(matches!(err, Error::Usage(_)), "{what}: {err}");
        }
        // Moving the files into a folder outside the library, or another library's copies into
        // quarantine, is fine.
        let mut moved_out = plan.clone();
        moved_out.destination = String::from(f.root.parent().unwrap().to_str().unwrap());
        for m in &mut moved_out.moves {
            m.to = m.to.replace("lib", "sorted");
        }
        moved_out.check().unwrap();

        // A plan from before the target folder was recorded is refused by its version.
        let path = write_plan(&plan, datetime!(2026-10-20 12:00 UTC)).unwrap();
        let old = fs::read_to_string(&path)
            .unwrap()
            .replace("\"version\": 2", "\"version\": 1");
        fs::write(&path, old).unwrap();
        let err = read_plan(&path).unwrap_err();
        assert!(err.to_string().contains("plan version 1"), "{}", err);
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
            notes.warnings[0].starts_with("2 files stay:"),
            "{}",
            notes.warnings[0]
        );
        assert!(notes.warnings[0].contains("2023"), "{}", notes.warnings[0]);
        // A link under another letter case is that folder on Windows and macOS, so it counts
        // everywhere.
        symlink(&outside, f.root.join("Image")).unwrap();
        let (plan, notes) = organize(&f, "{type}/{name}.{ext}", false);
        assert_eq!(plan.moves, vec![]);
        assert!(notes.warnings[0].contains("Image"), "{:?}", notes.warnings);
        // A link elsewhere in the library is no concern.
        symlink(&outside, f.root.join("elsewhere")).unwrap();
        let (plan, _) = organize(&f, "{name}.{ext}", false);
        assert_eq!(plan.moves.len(), 2);
    }
}
