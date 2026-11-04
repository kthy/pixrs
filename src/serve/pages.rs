//! The viewer's HTML pages.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use super::thumbs::{self, ViewSource};
use super::{AppError, AppState, Located, Report};
use crate::catalog::{FileRecord, Filter, HashedFile, Neighbors, Page, Tally};
use crate::dupes::{human_mtime, oldest_first, plural, DupeGroup, Role, Tier};
use crate::library::{LibraryId, Root};
use crate::media::MediaType;
use axum::extract::{Path as UrlPath, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use indicatif::HumanBytes;
use maud::{html, Markup, DOCTYPE};
use serde::{Deserialize, Deserializer};
use std::sync::Arc;
use time::{Month, OffsetDateTime};

/// Files per page in listings.
const PAGE_SIZE: usize = 200;
/// Duplicate groups per page.
const DUPE_PAGE_SIZE: usize = 50;
/// Highest page number honored, so offsets cannot overflow.
const MAX_PAGE: usize = 1_000_000;
/// Tooltip of the mark on a date taken that is before 1980 or in the future.
const DUBIOUS_DATE: &str = "Probably wrong: before 1980 or in the future, as broken metadata \
     often is. pixrs organize ignores it unless --trust-dates is given.";

/// Query parameters shared by the pages. A media type or listing the viewer does not know counts
/// as none given, so a hand-edited link still shows a page.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// Only show this media type.
    #[serde(rename = "type", default, deserialize_with = "known_media_type")]
    media_type: Option<MediaType>,
    /// Page number, from 1.
    page: Option<usize>,
    /// Just the next page of items, for HTMX to append: `partial=1`.
    #[serde(default, deserialize_with = "one")]
    partial: bool,
    /// The listing a file was opened from, for prev/next.
    #[serde(default)]
    from: Listing,
}

/// A media type by name, or `None` for a name that is no media type.
fn known_media_type<'de, D: Deserializer<'de>>(d: D) -> Result<Option<MediaType>, D::Error> {
    Ok(String::deserialize(d)?.parse().ok())
}

/// Whether a number is 1.
fn one<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(u8::deserialize(d)? == 1)
}

impl ListQuery {
    fn page(&self) -> usize {
        self.page.unwrap_or(1).clamp(1, MAX_PAGE)
    }
}

/// The listings a file can be opened from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Listing {
    /// A month, newest first.
    Date,
    /// A folder, in path order; also for any listing name the viewer does not know.
    #[default]
    #[serde(other)]
    Folder,
}

/// The listing a page belongs to, so links into the viewer keep their context.
#[derive(Clone, Copy)]
struct Context {
    listing: Listing,
    media_type: Option<MediaType>,
}

impl Context {
    fn view_url(self, library_id: LibraryId, path: &str) -> String {
        with_query(
            &file_url("view", library_id, path),
            &[
                ("from", (self.listing == Listing::Date).then_some("date")),
                ("type", self.media_type.map(MediaType::as_str)),
            ],
        )
    }
}

/// `path` with each `/`-separated part percent-encoded.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|part| urlencoding::encode(part).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// URL of a file under one of the per-file routes (`view`, `media`, `thumb`, `preview`).
fn file_url(route: &str, library_id: LibraryId, path: &str) -> String {
    format!("/{}/{}/{}", route, library_id, encode_path(path))
}

/// URL of a thumbnail. The mtime makes a changed file's thumbnail a new URL for browser caches.
fn thumb_url(library_id: LibraryId, path: &str, mtime_ns: i64) -> String {
    format!("{}?v={}", file_url("thumb", library_id, path), mtime_ns)
}

/// URL of a folder; `""` is the library root.
fn folder_url(library_id: LibraryId, dir: &str) -> String {
    if dir.is_empty() {
        format!("/folders/{library_id}")
    } else {
        format!("/folders/{}/{}", library_id, encode_path(dir))
    }
}

/// `base` with the query parameters that have a value.
fn with_query(base: &str, params: &[(&str, Option<&str>)]) -> String {
    let query: Vec<String> = params
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_ref()
                .map(|value| format!("{}={}", key, urlencoding::encode(value)))
        })
        .collect();
    if query.is_empty() {
        String::from(base)
    } else {
        format!("{}?{}", base, query.join("&"))
    }
}

/// `base` showing only `media_type`, if there is one.
fn with_type(base: &str, media_type: Option<MediaType>) -> String {
    with_query(base, &[("type", media_type.map(MediaType::as_str))])
}

/// The page after `page` of the listing at `here`.
fn next_page(here: &str, media_type: Option<MediaType>, page: usize) -> String {
    with_query(
        here,
        &[
            ("type", media_type.map(MediaType::as_str)),
            ("page", Some(&(page + 1).to_string())),
        ],
    )
}

/// The last part of a path.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A whole page. `here` is the page's URL without query, for the media type links; pages
/// without them pass `None`.
fn layout(
    title: &str,
    here: Option<&str>,
    media_type: Option<MediaType>,
    content: &Markup,
) -> Markup {
    let types = [
        ("all", None),
        ("images", Some(MediaType::Image)),
        ("videos", Some(MediaType::Video)),
        ("audio", Some(MediaType::Audio)),
    ];
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - PixRS" }
                // No favicon, and no request for one.
                link rel="icon" href="data:,";
                link rel="stylesheet" href="/static/pixrs.css";
                script src="/static/htmx-4.0.0.min.js" defer {}
                script src="/static/pixrs.js" defer {}
            }
            body {
                header {
                    nav {
                        a.brand href="/" { "PixRS" }
                        a href="/" { "Folders" }
                        a href="/dates" { "Dates" }
                        a href="/dupes" { "Duplicates" }
                    }
                    @if let Some(here) = here {
                        nav.types {
                            @for (label, kind) in types {
                                a href=(with_type(here, kind))
                                    class=[(kind == media_type).then_some("active")] { (label) }
                            }
                        }
                    }
                }
                main { (content) }
            }
        }
    }
}

/// One file in a listing.
fn tile(library_id: LibraryId, file: &FileRecord, context: Context) -> Markup {
    html! {
        a.tile href=(context.view_url(library_id, &file.path)) title=(file.path) {
            img loading="lazy" alt=""
                src=(thumb_url(library_id, &file.path, file.stamp.mtime_ns));
            span.name { (file_name(&file.path)) }
            @match file.media_type {
                MediaType::Video => span.badge { "video" },
                MediaType::Audio => span.badge { "audio" },
                MediaType::Image => {},
            }
        }
    }
}

/// The link to the next page: followed by HTMX when it scrolls into view, which replaces it with
/// that page's items, or clicked when JavaScript is off.
fn more(next: Option<String>) -> Markup {
    html! {
        @if let Some(next) = next {
            @let separator = if next.contains('?') { "&" } else { "?" };
            a.more href=(next) hx-get=(format!("{next}{separator}partial=1"))
                hx-trigger="revealed" hx-swap="outerHTML" { "Next page" }
        }
    }
}

/// Answer either the whole page or, for HTMX, only the items and the next-page link.
fn respond(query: &ListQuery, items: Markup, page: impl FnOnce(Markup) -> Markup) -> Response {
    if query.partial {
        html_response(items)
    } else {
        html_response(page(items))
    }
}

/// `markup` as an HTML response.
fn html_response(markup: Markup) -> Response {
    Html(markup.into_string()).into_response()
}

/// The offset and one extra row, to find out whether there is a next page. `page` is at most
/// `MAX_PAGE`.
const fn page_slice(page: usize, size: usize) -> Page {
    Page {
        limit: size + 1,
        offset: (page - 1) * size,
    }
}

/// `/`: the libraries.
pub async fn libraries(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let libraries = state.query(crate::catalog::Catalog::libraries).await?;
    let media_type = query.media_type;
    let content = html! {
        h1 { "Libraries" }
        @let shown: Vec<_> = libraries.iter().filter(|l| state.shows(l.id)).collect();
        @if shown.is_empty() {
            p { "No libraries yet. Run " code { "pixrs scan <LIBRARY>" } " first." }
        }
        ul.folders {
            @for library in shown {
                li {
                    a href=(with_type(&folder_url(library.id, ""), media_type)) {
                        (library.root.to_string())
                    }
                    span.count { (library.files) " files" }
                    @if let Some(scanned_at) = &library.scanned_at {
                        span.meta { "scanned " (scanned_at) }
                    }
                }
            }
        }
    };
    Ok(html_response(layout(
        "Libraries",
        Some("/"),
        media_type,
        &content,
    )))
}

/// `/folders/{lib}`: the root folder of a library.
pub async fn folder_root(
    State(state): State<Arc<AppState>>,
    UrlPath(library_id): UrlPath<LibraryId>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    folder_page(state, library_id, String::new(), query).await
}

/// `/folders/{lib}/{*dir}`: a folder in a library.
pub async fn folder(
    State(state): State<Arc<AppState>>,
    UrlPath((library_id, dir)): UrlPath<(LibraryId, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let dir = String::from(dir.trim_end_matches('/'));
    folder_page(state, library_id, dir, query).await
}

/// What a folder page shows, from the catalog.
struct Folder {
    /// The library's root, if it is in the catalog.
    root: Option<Root>,
    subfolders: Vec<Tally>,
    files: Vec<FileRecord>,
}

async fn folder_page(
    state: Arc<AppState>,
    library_id: LibraryId,
    dir: String,
    query: ListQuery,
) -> Result<Response, AppError> {
    if !state.shows(library_id) {
        return Err(AppError::NotFound);
    }
    let media_type = query.media_type;
    let page = query.page();
    let queried_dir = dir.clone();
    let Folder {
        root,
        subfolders,
        mut files,
    } = state
        .query(move |catalog| {
            let root = catalog.library_root(library_id)?;
            let subfolders = if page == 1 {
                catalog.subfolders(library_id, &queried_dir, media_type)?
            } else {
                vec![]
            };
            let slice = page_slice(page, PAGE_SIZE);
            let files = catalog.folder_files(library_id, &queried_dir, media_type, slice)?;
            Ok(Folder {
                root,
                subfolders,
                files,
            })
        })
        .await?;
    let root = root.ok_or(AppError::NotFound)?;
    if !dir.is_empty()
        && page == 1
        && subfolders.is_empty()
        && files.is_empty()
        && media_type.is_none()
    {
        return Err(AppError::NotFound);
    }

    let here = folder_url(library_id, &dir);
    let has_more = files.len() > PAGE_SIZE;
    files.truncate(PAGE_SIZE);
    let next = has_more.then(|| next_page(&here, media_type, page));
    let context = Context {
        listing: Listing::Folder,
        media_type,
    };
    let items = html! {
        @for file in &files { (tile(library_id, file, context)) }
        (more(next))
    };

    let title = if dir.is_empty() {
        root.to_string()
    } else {
        String::from(file_name(&dir))
    };
    Ok(respond(&query, items, |items| {
        let content = html! {
            (crumbs(library_id, &root, &dir, media_type))
            @if !subfolders.is_empty() {
                ul.folders {
                    @for sub in &subfolders {
                        @let path = if dir.is_empty() {
                            sub.name.clone()
                        } else {
                            format!("{dir}/{}", sub.name)
                        };
                        li {
                            a href=(with_type(&folder_url(library_id, &path), media_type)) {
                                (sub.name)
                            }
                            span.count { (sub.files) }
                        }
                    }
                }
            }
            @if files.is_empty() && subfolders.is_empty() {
                p { "No files here." }
            }
            div.grid { (items) }
        };
        layout(&title, Some(&here), media_type, &content)
    }))
}

/// The links from the libraries to the folder `dir` of a library, one per folder on the way.
fn crumbs(library_id: LibraryId, root: &Root, dir: &str, media_type: Option<MediaType>) -> Markup {
    // Each folder on the way, as its path in the library and its name.
    let folders: Vec<(&str, &str)> = if dir.is_empty() {
        vec![]
    } else {
        let ends = dir.match_indices('/').map(|(end, _)| end);
        let paths = ends.filter_map(|end| dir.get(..end)).chain([dir]);
        paths.zip(dir.split('/')).collect()
    };
    html! {
        nav.crumbs {
            a href="/" { "Libraries" }
            " / "
            a href=(with_type(&folder_url(library_id, ""), media_type)) {
                (root)
            }
            @for (path, name) in folders {
                " / "
                a href=(with_type(&folder_url(library_id, path), media_type)) { (name) }
            }
        }
    }
}

/// `/dates`: the newest month.
pub async fn dates_latest(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let media_type = query.media_type;
    let filter = Filter {
        libraries: state.libraries.clone(),
        media_type,
    };
    let months = state.query(move |catalog| catalog.months(&filter)).await?;
    if let Some(latest) = months.first() {
        let url = with_type(&format!("/dates/{}", latest.name), media_type);
        Ok(Redirect::to(&url).into_response())
    } else {
        let content = html! { h1 { "Dates" } p { "No files." } };
        Ok(html_response(layout(
            "Dates",
            Some("/dates"),
            media_type,
            &content,
        )))
    }
}

/// A month of the dates listing, written `YYYY-MM` in URLs and the catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct YearMonth {
    year: u16,
    month: Month,
}

impl YearMonth {
    /// `YYYY-MM`, or `None` if `text` is not a month.
    fn parse(text: &str) -> Option<Self> {
        let digits =
            |part: &str, len| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
        let (year, month) = text.split_once('-')?;
        if !digits(year, 4) || !digits(month, 2) {
            return None;
        }
        Some(Self {
            year: year.parse().ok()?,
            month: Month::try_from(month.parse::<u8>().ok()?).ok()?,
        })
    }

    /// The month as people read it, like `May 2024`.
    fn label(self) -> String {
        format!("{} {}", self.month, self.year)
    }
}

impl std::fmt::Display for YearMonth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}", self.year, u8::from(self.month))
    }
}

/// A catalog month name like `2024-05` as people read it, or as it is if it is not a month.
fn month_label(name: &str) -> String {
    YearMonth::parse(name).map_or_else(|| String::from(name), YearMonth::label)
}

/// A year in the dates sidebar: its months, newest first, and the files in them.
struct Year<'a> {
    name: &'a str,
    files: usize,
    months: Vec<&'a Tally>,
}

/// The months, newest first, by year.
fn years(months: &[Tally]) -> Vec<Year<'_>> {
    let mut years: Vec<Year> = vec![];
    for month in months {
        let year = month.name.get(..4).unwrap_or(&month.name);
        match years.last_mut() {
            Some(last) if last.name == year => {
                last.files += month.files;
                last.months.push(month);
            }
            _ => years.push(Year {
                name: year,
                files: month.files,
                months: vec![month],
            }),
        }
    }
    years
}

/// `/dates/{month}`: the files dated in a month, newest first, with all months alongside.
pub async fn dates(
    State(state): State<Arc<AppState>>,
    UrlPath(month): UrlPath<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let shown = YearMonth::parse(&month).ok_or(AppError::NotFound)?;
    let (month, label) = (shown.to_string(), shown.label());
    let media_type = query.media_type;
    let page = query.page();
    let filter = Filter {
        libraries: state.libraries.clone(),
        media_type,
    };
    let queried_month = month.clone();
    let partial = query.partial;
    let (months, mut files) = state
        .query(move |catalog| {
            // The sidebar and the month's count, for whole pages.
            let months = if partial {
                vec![]
            } else {
                catalog.months(&filter)?
            };
            let files =
                catalog.month_files(&filter, &queried_month, page_slice(page, PAGE_SIZE))?;
            Ok((months, files))
        })
        .await?;

    let here = format!("/dates/{month}");
    let has_more = files.len() > PAGE_SIZE;
    files.truncate(PAGE_SIZE);
    let next = has_more.then(|| next_page(&here, media_type, page));
    let context = Context {
        listing: Listing::Date,
        media_type,
    };
    let items = html! {
        @for dated in &files { (tile(dated.library_id, &dated.file, context)) }
        (more(next))
    };

    Ok(respond(&query, items, |items| {
        let count = months
            .iter()
            .find(|m| m.name == month)
            .map_or(0, |m| m.files);
        let content = html! {
            div.dates {
                aside.months {
                    @for year in years(&months) {
                        h3 { (year.name) " " span.count { (year.files) } }
                        ul {
                            @for m in year.months {
                                li class=[(m.name == month).then_some("active")] {
                                    @let url = format!("/dates/{}", urlencoding::encode(&m.name));
                                    a href=(with_type(&url, media_type)) {
                                        (month_label(&m.name))
                                    }
                                    " " span.count { (m.files) }
                                }
                            }
                        }
                    }
                }
                section {
                    h1 { (label) " " span.count { (count) } }
                    @if files.is_empty() { p { "No files." } }
                    div.grid { (items) }
                }
            }
        };
        layout(&label, Some(&here), media_type, &content)
    }))
}

/// One duplicate group, copies side by side.
fn dupe_group(group: &DupeGroup) -> Markup {
    let context = Context {
        listing: Listing::Folder,
        media_type: None,
    };
    html! {
        section.group {
            h2 {
                @match group.tier() {
                    Tier::Exact => {
                        @if let Some(file) = group.files.first() { (HumanBytes(file.size)) }
                        " x " (group.files.len()) " exact copies, "
                    }
                    Tier::SamePixels => { (group.files.len()) " images with the same pixels, " }
                    Tier::Similar => { (group.files.len()) " similar files, " }
                }
                (HumanBytes(group.reclaimable_bytes())) " reclaimable "
                span.badge.tier { (group.tier().as_str()) }
            }
            div.copies {
                @for f in &group.files {
                    figure.copy.keep[f.role.is_keep()] {
                        a href=(context.view_url(f.library_id, &f.path)) {
                            img loading="lazy" alt=""
                                src=(thumb_url(f.library_id, &f.path, f.mtime_ns));
                        }
                        figcaption {
                            @if f.role.is_keep() { span.badge.keep { "keep" } }
                            // In an exact group every copy matches exactly; no need to say so.
                            @if group.tier() != Tier::Exact {
                                @if let Role::Copy(matches) = f.role {
                                    span.badge.tier { (matches.as_str()) }
                                }
                            }
                            div.library { (f.library) }
                            div.path { (f.path) }
                            div.meta {
                                (human_mtime(f.mtime_ns))
                                @if group.tier() != Tier::Exact { ", " (HumanBytes(f.size)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `/dupes`: duplicate groups, exact first, then same pixels, then similar.
pub async fn dupes(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let page = query.page();
    let report = state
        .dupes(if page == 1 {
            Report::Recent
        } else {
            Report::Shown
        })
        .await?;
    let start = (page - 1) * DUPE_PAGE_SIZE;
    let groups = report.groups.get(start..).unwrap_or_default();
    let next = (groups.len() > DUPE_PAGE_SIZE)
        .then(|| with_query("/dupes", &[("page", Some(&(page + 1).to_string()))]));
    let items = html! {
        @for group in groups.iter().take(DUPE_PAGE_SIZE) { (dupe_group(group)) }
        (more(next))
    };

    let summary = &report.summary;
    Ok(respond(&query, items, |items| {
        let content = html! {
            h1 { "Duplicates" }
            p {
                (summary.groups) " " (plural(summary.groups, "group", "groups")) ", "
                (summary.redundant_files) " redundant "
                (plural(summary.redundant_files, "file", "files")) ", "
                (HumanBytes(summary.reclaimable_bytes)) " reclaimable."
            }
            @if summary.unhashed_candidates > 0 {
                p.note {
                    (summary.unhashed_candidates) " files share their size with another file but \
                     are not hashed yet, so there may be more duplicates. Scan their libraries \
                     again to hash them."
                }
            }
            @if summary.unhashed_media > 0 {
                p.note {
                    (summary.unhashed_media) " files have no image hash, video signature or audio \
                     fingerprint yet (not scanned since they changed, or no decoder: RAW, \
                     HEIC/AVIF without ffmpeg 7.1, video without ffmpeg, audio without fpcalc), \
                     so there may be more same-pixel or similar files."
                }
            }
            @if !state.similarity.images {
                p.note {
                    "Similar images (resized or recompressed copies) are not shown. Start the \
                     viewer with --similar to include them; expect burst shots among them."
                }
            }
            div.groups { (items) }
        };
        layout("Duplicates", None, None, &content)
    }))
}

/// A file's date as the viewer shows it, as `YYYY-MM-DDTHH:MM:SS`.
enum ShownDate {
    /// When it was taken; `dubious` if before 1980 or in the future.
    Taken { at: String, dubious: bool },
    /// Its modification time, as it has no date taken.
    Modified(String),
}

impl ShownDate {
    /// The date of `file`, from `date`, which is when it was taken or else modified.
    fn of(file: &FileRecord, date: String) -> Self {
        if file.meta.taken_at.is_some() {
            let dubious = file
                .meta
                .taken()
                .is_some_and(|taken| !taken.is_plausible(OffsetDateTime::now_utc()));
            Self::Taken { at: date, dubious }
        } else {
            Self::Modified(date)
        }
    }
}

/// What the viewer shows around a file.
struct Surroundings {
    /// The files before and after it in the listing it was opened from.
    neighbors: Neighbors,
    /// When it was taken, or else modified.
    date: Option<ShownDate>,
    /// Every file with its contents, itself included, oldest first.
    copies: Vec<HashedFile>,
}

impl Surroundings {
    async fn of(
        state: &Arc<AppState>,
        library_id: LibraryId,
        file: &FileRecord,
        context: Context,
    ) -> Result<Self, AppError> {
        let filter = Filter {
            libraries: state.libraries.clone(),
            media_type: context.media_type,
        };
        let path = file.path.clone();
        let (size, sha256) = (file.stamp.size, file.sha256.clone());
        let (neighbors, date, mut copies) = state
            .query(move |catalog| {
                let neighbors = match context.listing {
                    Listing::Date => catalog.date_neighbors(&filter, library_id, &path)?,
                    Listing::Folder => {
                        catalog.folder_neighbors(library_id, &path, context.media_type)?
                    }
                };
                let copies = match sha256 {
                    Some(sha256) => catalog.copies(size, &sha256)?,
                    None => vec![],
                };
                Ok((neighbors, catalog.date(library_id, &path)?, copies))
            })
            .await?;
        copies.sort_by(oldest_first);
        Ok(Self {
            neighbors,
            date: date.map(|date| ShownDate::of(file, date)),
            copies,
        })
    }
}

/// `/view/{lib}/{*path}`: one file, full size, with its details and prev/next links.
pub async fn view(
    State(state): State<Arc<AppState>>,
    UrlPath((library_id, path)): UrlPath<(LibraryId, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let Located {
        root,
        full_path,
        file,
    } = state.locate(library_id, path).await?;
    let context = Context {
        listing: query.from,
        media_type: query.media_type,
    };
    let around = Surroundings::of(&state, library_id, &file, context).await?;
    let codec = match file.media_type {
        MediaType::Video => state.thumbs.video_codec(&full_path).await,
        MediaType::Image | MediaType::Audio => None,
    };
    let codec = codec.as_deref();
    let content = html! {
        (pager(&file.path, &around.neighbors, context))
        (stage(library_id, &file, codec))
        (details(&file, &root, around.date.as_ref(), codec))
        (copies_list(library_id, &file.path, &around.copies, context))
        p { a href=(file_url("media", library_id, &file.path)) { "Open original" } }
    };
    Ok(html_response(layout(
        file_name(&file.path),
        None,
        None,
        &content,
    )))
}

/// The links to the previous and next file, around the file's path.
fn pager(path: &str, neighbors: &Neighbors, context: Context) -> Markup {
    html! {
        nav.pager {
            @if let Some(p) = &neighbors.prev {
                a rel="prev" href=(context.view_url(p.library_id, &p.path)) { "< Prev" }
            } @else { span {} }
            span.path { (path) }
            @if let Some(n) = &neighbors.next {
                a rel="next" href=(context.view_url(n.library_id, &n.path)) { "Next >" }
            } @else { span {} }
        }
    }
}

/// The file itself: an image, or a player.
fn stage(library_id: LibraryId, file: &FileRecord, codec: Option<&str>) -> Markup {
    let media_url = file_url("media", library_id, &file.path);
    html! {
        div.stage {
            @match file.media_type {
                MediaType::Image => {
                    img src=(image_source(library_id, file)) alt=(file_name(&file.path));
                }
                MediaType::Video => {
                    video controls preload="metadata" src=(media_url)
                        poster=(thumb_url(library_id, &file.path, file.stamp.mtime_ns)) {}
                    p.unplayable hidden {
                        "This browser cannot play this video"
                        @if let Some(codec) = codec { " (" (codec) ")" }
                        ". Open the original to play it in another program."
                    }
                }
                MediaType::Audio => {
                    audio controls preload="metadata" src=(media_url) {}
                }
            }
        }
    }
}

/// What the viewer shows of an image: the original, a preview, or the placeholder.
fn image_source(library_id: LibraryId, file: &FileRecord) -> String {
    match thumbs::view_source(file) {
        ViewSource::Original => file_url("media", library_id, &file.path),
        ViewSource::Preview => format!(
            "{}?v={}",
            file_url("preview", library_id, &file.path),
            file.stamp.mtime_ns
        ),
        ViewSource::Placeholder => thumb_url(library_id, &file.path, file.stamp.mtime_ns),
    }
}

/// What the catalog knows about the file in the library at `root`.
fn details(
    file: &FileRecord,
    root: &Root,
    date: Option<&ShownDate>,
    codec: Option<&str>,
) -> Markup {
    html! {
        dl.details {
            @match date {
                Some(ShownDate::Taken { at, dubious }) => {
                    dt { "Taken" }
                    dd {
                        (at.replacen('T', " ", 1))
                        @if *dubious {
                            " " abbr.dubious title=(DUBIOUS_DATE) { "?" }
                        }
                    }
                }
                Some(ShownDate::Modified(at)) => {
                    dt { "Modified" }
                    dd { (at.replacen('T', " ", 1)) }
                }
                None => {}
            }
            @if let Some(camera) = file.meta.camera() { dt { "Camera" } dd { (camera) } }
            @if let (Some(w), Some(h)) = (file.meta.width, file.meta.height) {
                dt { "Dimensions" } dd { (w) " x " (h) }
            }
            dt { "Size" } dd { (HumanBytes(file.stamp.size)) }
            @if let Some(codec) = codec { dt { "Codec" } dd { (codec) } }
            dt { "Library" } dd { (root) }
            dt { "Path" } dd { (file.path) }
            @if let Some(sha256) = &file.sha256 { dt { "SHA-256" } dd.hash { (sha256) } }
        }
    }
}

/// The file's identical copies, oldest (the one `dupes` keeps) first, if it has any.
fn copies_list(
    library_id: LibraryId,
    path: &str,
    copies: &[HashedFile],
    context: Context,
) -> Markup {
    html! {
        @if copies.len() > 1 {
            h2 { "Identical copies" }
            ul.copies-list {
                @for (i, copy) in copies.iter().enumerate() {
                    li {
                        @if copy.library_id == library_id && copy.path == path {
                            "This file"
                        } @else {
                            a href=(context.view_url(copy.library_id, &copy.path)) {
                                (copy.root.to_string()) " / " (copy.path)
                            }
                        }
                        @if i == 0 { " " span.badge.keep { "keep" } }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn test_urls() {
        assert_eq!(
            file_url("media", LibraryId(3), "a b/ø#?.jpg"),
            "/media/3/a%20b/%C3%B8%23%3F.jpg"
        );
        assert_eq!(folder_url(LibraryId(1), ""), "/folders/1");
        assert_eq!(
            with_query("/x", &[("type", Some("image")), ("page", None)]),
            "/x?type=image"
        );
    }

    #[test]
    fn test_year_month() {
        let label = |text: &str| YearMonth::parse(text).map(YearMonth::label);
        assert_eq!(label("2024-05").as_deref(), Some("May 2024"));
        let names = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        for (number, name) in (1..=12).zip(names) {
            assert_eq!(
                label(&format!("1999-{number:02}")),
                Some(format!("{name} 1999"))
            );
        }
        for bad in [
            "2024-13", "2024-00", "../etc", "2024-+5", "+024-05", "2024-5", "2024-005",
        ] {
            assert_eq!(YearMonth::parse(bad), None, "{bad}");
        }
        assert_eq!(YearMonth::parse("0999-01").unwrap().to_string(), "0999-01");
        assert_eq!(month_label("unknown"), "unknown");
    }
}
