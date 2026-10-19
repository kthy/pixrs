//! The viewer's HTML pages.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use super::thumbs::{self, ViewSource};
use super::{AppError, AppState};
use crate::catalog::{display_root, FileRecord, Filter, Page};
use crate::dupes::{human_mtime, plural, DupeGroup, Tier};
use crate::media::MediaType;
use axum::extract::{Path as UrlPath, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use indicatif::HumanBytes;
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;
use std::sync::Arc;

/// Files per page in listings.
const PAGE_SIZE: usize = 200;
/// Duplicate groups per page.
const DUPE_PAGE_SIZE: usize = 50;
/// Highest page number honored, so offsets cannot overflow.
const MAX_PAGE: usize = 1_000_000;

/// Query parameters shared by the pages.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// Only show this media type.
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Page number, from 1.
    page: Option<usize>,
    /// `1` for just the next page of items, for HTMX to append.
    partial: Option<u8>,
    /// The listing a file was opened from, for prev/next: `folder` (default) or `date`.
    from: Option<String>,
}

impl ListQuery {
    fn media_type(&self) -> Option<MediaType> {
        self.kind.as_deref().and_then(|kind| kind.parse().ok())
    }

    fn page(&self) -> usize {
        self.page.unwrap_or(1).clamp(1, MAX_PAGE)
    }

    fn partial(&self) -> bool {
        self.partial == Some(1)
    }

    fn opened_from_date(&self) -> bool {
        self.from.as_deref() == Some("date")
    }
}

/// The listing a page belongs to, so links into the viewer keep their context.
#[derive(Clone, Copy)]
struct Context {
    from_date: bool,
    media_type: Option<MediaType>,
}

impl Context {
    fn view_url(self, library_id: i64, path: &str) -> String {
        with_query(
            &file_url("view", library_id, path),
            &[
                ("from", self.from_date.then(|| String::from("date"))),
                ("type", type_param(self.media_type)),
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
fn file_url(route: &str, library_id: i64, path: &str) -> String {
    format!("/{}/{}/{}", route, library_id, encode_path(path))
}

/// URL of a thumbnail. The mtime makes a changed file's thumbnail a new URL for browser caches.
fn thumb_url(library_id: i64, path: &str, mtime_ns: i64) -> String {
    format!("{}?v={}", file_url("thumb", library_id, path), mtime_ns)
}

/// URL of a folder; `""` is the library root.
fn folder_url(library_id: i64, dir: &str) -> String {
    if dir.is_empty() {
        format!("/folders/{}", library_id)
    } else {
        format!("/folders/{}/{}", library_id, encode_path(dir))
    }
}

/// `base` with the query parameters that have a value.
fn with_query(base: &str, params: &[(&str, Option<String>)]) -> String {
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

fn type_param(media_type: Option<MediaType>) -> Option<String> {
    media_type.map(|t| String::from(t.as_str()))
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
    content: Markup,
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
                                a href=(with_query(here, &[("type", type_param(kind))]))
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
fn tile(library_id: i64, file: &FileRecord, context: Context) -> Markup {
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
            a.more href=(next) hx-get=(format!("{}{}partial=1", next, if next.contains('?') { "&" } else { "?" }))
                hx-trigger="revealed" hx-swap="outerHTML" { "Next page" }
        }
    }
}

/// Answer either the whole page or, for HTMX, only the items and the next-page link.
fn respond(query: &ListQuery, items: Markup, page: impl FnOnce(Markup) -> Markup) -> Response {
    if query.partial() {
        Html(items.into_string()).into_response()
    } else {
        Html(page(items).into_string()).into_response()
    }
}

/// The offset and one extra row, to find out whether there is a next page. `page` is at most
/// `MAX_PAGE`.
fn page_slice(page: usize, size: usize) -> Page {
    Page {
        limit: size as i64 + 1,
        offset: ((page - 1) * size) as i64,
    }
}

/// `/`: the libraries.
pub async fn libraries(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let libraries = state.query(|catalog| catalog.libraries()).await?;
    let media_type = query.media_type();
    let content = html! {
        h1 { "Libraries" }
        @let shown: Vec<_> = libraries.iter().filter(|l| state.shows(l.id)).collect();
        @if shown.is_empty() {
            p { "No libraries yet. Run " code { "pixrs scan <LIBRARY>" } " first." }
        }
        ul.folders {
            @for library in shown {
                li {
                    a href=(with_query(&folder_url(library.id, ""), &[("type", type_param(media_type))])) {
                        (display_root(&library.root))
                    }
                    span.count { (library.files) " files" }
                    @if let Some(scanned_at) = &library.scanned_at {
                        span.meta { "scanned " (scanned_at) }
                    }
                }
            }
        }
    };
    Ok(Html(layout("Libraries", Some("/"), media_type, content).into_string()).into_response())
}

/// `/folders/{lib}`: the root folder of a library.
pub async fn folder_root(
    State(state): State<Arc<AppState>>,
    UrlPath(library_id): UrlPath<i64>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    folder_page(state, library_id, String::new(), query).await
}

/// `/folders/{lib}/{*dir}`: a folder in a library.
pub async fn folder(
    State(state): State<Arc<AppState>>,
    UrlPath((library_id, dir)): UrlPath<(i64, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let dir = String::from(dir.trim_end_matches('/'));
    folder_page(state, library_id, dir, query).await
}

async fn folder_page(
    state: Arc<AppState>,
    library_id: i64,
    dir: String,
    query: ListQuery,
) -> Result<Response, AppError> {
    if !state.shows(library_id) {
        return Err(AppError::NotFound);
    }
    let media_type = query.media_type();
    let page = query.page();
    let queried_dir = dir.clone();
    let (root, subfolders, mut files) = state
        .query(move |catalog| {
            let root = catalog.library_root(library_id)?;
            let subfolders = if page == 1 {
                catalog.subfolders(library_id, &queried_dir, media_type)?
            } else {
                vec![]
            };
            let slice = page_slice(page, PAGE_SIZE);
            let files = catalog.folder_files(library_id, &queried_dir, media_type, slice)?;
            Ok((root, subfolders, files))
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
    let next = has_more.then(|| {
        with_query(
            &here,
            &[
                ("type", type_param(media_type)),
                ("page", Some((page + 1).to_string())),
            ],
        )
    });
    let context = Context {
        from_date: false,
        media_type,
    };
    let items = html! {
        @for file in &files { (tile(library_id, file, context)) }
        (more(next))
    };

    let title = if dir.is_empty() {
        display_root(&root)
    } else {
        String::from(file_name(&dir))
    };
    Ok(respond(&query, items, |items| {
        let crumbs: Vec<(String, &str)> = if dir.is_empty() {
            vec![]
        } else {
            dir.split('/')
                .enumerate()
                .map(|(i, part)| {
                    let prefix: Vec<&str> = dir.split('/').take(i + 1).collect();
                    (folder_url(library_id, &prefix.join("/")), part)
                })
                .collect()
        };
        let content = html! {
            nav.crumbs {
                a href="/" { "Libraries" }
                " / "
                a href=(with_query(&folder_url(library_id, ""), &[("type", type_param(media_type))])) {
                    (display_root(&root))
                }
                @for (url, part) in &crumbs {
                    " / "
                    a href=(with_query(url, &[("type", type_param(media_type))])) { (part) }
                }
            }
            @if !subfolders.is_empty() {
                ul.folders {
                    @for (name, count) in &subfolders {
                        @let sub = if dir.is_empty() { name.clone() } else { format!("{}/{}", dir, name) };
                        li {
                            a href=(with_query(&folder_url(library_id, &sub), &[("type", type_param(media_type))])) {
                                (name)
                            }
                            span.count { (count) }
                        }
                    }
                }
            }
            @if files.is_empty() && subfolders.is_empty() {
                p { "No files here." }
            }
            div.grid { (items) }
        };
        layout(&title, Some(&here), media_type, content)
    }))
}

/// `/dates`: the newest month.
pub async fn dates_latest(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let media_type = query.media_type();
    let filter = Filter {
        libraries: state.libraries.clone(),
        media_type,
    };
    let months = state.query(move |catalog| catalog.months(&filter)).await?;
    match months.first() {
        Some((month, _)) => Ok(Redirect::to(&with_query(
            &format!("/dates/{}", month),
            &[("type", type_param(media_type))],
        ))
        .into_response()),
        None => {
            let content = html! { h1 { "Dates" } p { "No files." } };
            Ok(
                Html(layout("Dates", Some("/dates"), media_type, content).into_string())
                    .into_response(),
            )
        }
    }
}

/// `YYYY-MM` as `May 2024`, or `None` if it is not a month.
fn month_label(month: &str) -> Option<String> {
    const NAMES: [&str; 12] = [
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
    let (year, number) = month.split_once('-')?;
    if year.len() != 4 || !year.bytes().all(|b| b.is_ascii_digit()) || number.len() != 2 {
        return None;
    }
    let index: usize = number.parse().ok()?;
    let name = NAMES.get(index.checked_sub(1)?)?;
    Some(format!("{} {}", name, year))
}

/// `/dates/{month}`: the files dated in a month, newest first, with all months alongside.
pub async fn dates(
    State(state): State<Arc<AppState>>,
    UrlPath(month): UrlPath<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let label = month_label(&month).ok_or(AppError::NotFound)?;
    let media_type = query.media_type();
    let page = query.page();
    let filter = Filter {
        libraries: state.libraries.clone(),
        media_type,
    };
    let queried_month = month.clone();
    let partial = query.partial();
    let (months, mut files) = state
        .query(move |catalog| {
            // The sidebar and the month's count, for whole pages.
            let months = if !partial {
                catalog.months(&filter)?
            } else {
                vec![]
            };
            let files =
                catalog.month_files(&filter, &queried_month, page_slice(page, PAGE_SIZE))?;
            Ok((months, files))
        })
        .await?;

    let here = format!("/dates/{}", month);
    let has_more = files.len() > PAGE_SIZE;
    files.truncate(PAGE_SIZE);
    let next = has_more.then(|| {
        with_query(
            &here,
            &[
                ("type", type_param(media_type)),
                ("page", Some((page + 1).to_string())),
            ],
        )
    });
    let context = Context {
        from_date: true,
        media_type,
    };
    let items = html! {
        @for dated in &files { (tile(dated.library_id, &dated.file, context)) }
        (more(next))
    };

    Ok(respond(&query, items, |items| {
        // Year, files in it, and its months with their file counts.
        type Year<'a> = (&'a str, usize, Vec<(&'a str, usize)>);
        let mut years: Vec<Year> = vec![];
        for (m, count) in &months {
            let year = m.get(..4).unwrap_or(m);
            match years.last_mut() {
                Some((y, total, list)) if *y == year => {
                    *total += count;
                    list.push((m, *count));
                }
                _ => years.push((year, *count, vec![(m, *count)])),
            }
        }
        let count = months
            .iter()
            .find(|(m, _)| *m == month)
            .map_or(0, |(_, n)| *n);
        let content = html! {
            div.dates {
                aside.months {
                    @for (year, total, list) in &years {
                        h3 { (year) " " span.count { (total) } }
                        ul {
                            @for (m, n) in list {
                                li class=[(*m == month).then_some("active")] {
                                    a href=(with_query(&format!("/dates/{}", urlencoding::encode(m)), &[("type", type_param(media_type))])) {
                                        (month_label(m).unwrap_or_else(|| String::from(*m)))
                                    }
                                    " " span.count { (n) }
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
        layout(&label, Some(&here), media_type, content)
    }))
}

/// One duplicate group, copies side by side.
fn dupe_group(group: &DupeGroup) -> Markup {
    let context = Context {
        from_date: false,
        media_type: None,
    };
    html! {
        section.group {
            h2 {
                @match group.tier {
                    Tier::Exact => {
                        (HumanBytes(group.files[0].size)) " x " (group.files.len()) " exact copies, "
                    }
                    Tier::SamePixels => { (group.files.len()) " images with the same pixels, " }
                    Tier::Similar => { (group.files.len()) " similar images, " }
                }
                (HumanBytes(group.reclaimable_bytes())) " reclaimable "
                span.badge.tier { (group.tier.as_str()) }
            }
            div.copies {
                @for f in &group.files {
                    figure.copy.keep[f.keep] {
                        a href=(context.view_url(f.library_id, &f.path)) {
                            img loading="lazy" alt="" src=(thumb_url(f.library_id, &f.path, f.mtime_ns));
                        }
                        figcaption {
                            @if f.keep { span.badge.keep { "keep" } }
                            @if let (Some(matches), true) = (f.matches, group.tier != Tier::Exact) {
                                span.badge.tier { (matches.as_str()) }
                            }
                            div.library { (f.library) }
                            div.path { (f.path) }
                            div.meta {
                                (human_mtime(f.mtime_ns))
                                @if group.tier != Tier::Exact { ", " (HumanBytes(f.size)) }
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
    let report = state.dupes(page == 1).await?;
    let start = (page - 1) * DUPE_PAGE_SIZE;
    let groups = report.groups.get(start..).unwrap_or_default();
    let has_more = groups.len() > DUPE_PAGE_SIZE;
    let groups = &groups[..groups.len().min(DUPE_PAGE_SIZE)];
    let next = has_more.then(|| with_query("/dupes", &[("page", Some((page + 1).to_string()))]));
    let items = html! {
        @for group in groups { (dupe_group(group)) }
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
                    (summary.unhashed_candidates) " files share their size with another file but are \
                     not hashed yet, so there may be more duplicates. Scan their libraries again \
                     to hash them."
                }
            }
            @if summary.unhashed_images > 0 {
                p.note {
                    (summary.unhashed_images) " images have no pixel or perceptual hash yet (not \
                     scanned since they changed, RAW, or HEIC/AVIF without ffmpeg), so there may \
                     be more same-pixel or similar images."
                }
            }
            @if state.similar.is_none() {
                p.note {
                    "Similar images (resized or recompressed copies) are not shown. Start the \
                     viewer with --similar to include them; expect burst shots among them."
                }
            }
            div.groups { (items) }
        };
        layout("Duplicates", None, None, content)
    }))
}

/// Camera make and model, without the make twice when the model starts with it.
fn camera(file: &FileRecord) -> Option<String> {
    match (&file.meta.camera_make, &file.meta.camera_model) {
        (Some(make), Some(model)) if model.starts_with(make.as_str()) => Some(model.clone()),
        (Some(make), Some(model)) => Some(format!("{} {}", make, model)),
        (Some(one), None) | (None, Some(one)) => Some(one.clone()),
        (None, None) => None,
    }
}

/// `/view/{lib}/{*path}`: one file, full size, with its details and prev/next links.
pub async fn view(
    State(state): State<Arc<AppState>>,
    UrlPath((library_id, path)): UrlPath<(i64, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let located = state.locate(library_id, path.clone()).await?;
    let file = located.file.clone();
    let media_type = query.media_type();
    let from_date = query.opened_from_date();
    let filter = Filter {
        libraries: state.libraries.clone(),
        media_type,
    };
    let hashed = file.sha256.clone().map(|sha256| (file.stamp.size, sha256));
    let (prev, next, date, mut copies) = state
        .query(move |catalog| {
            let (prev, next) = if from_date {
                catalog.date_neighbors(&filter, library_id, &path)?
            } else {
                let (prev, next) = catalog.folder_neighbors(library_id, &path, media_type)?;
                (prev.map(|p| (library_id, p)), next.map(|n| (library_id, n)))
            };
            let date = catalog.date(library_id, &path)?;
            let copies = match hashed {
                Some((size, sha256)) => catalog.copies(size, &sha256)?,
                None => vec![],
            };
            Ok((prev, next, date, copies))
        })
        .await?;
    copies.sort_by(|a, b| (a.mtime_ns, &a.root, &a.path).cmp(&(b.mtime_ns, &b.root, &b.path)));

    let codec = match (&state.ffprobe, file.media_type) {
        (Some(ffprobe), MediaType::Video) => {
            state.thumbs.video_codec(ffprobe, &located.full_path).await
        }
        _ => None,
    };

    let context = Context {
        from_date,
        media_type,
    };
    let media_url = file_url("media", library_id, &file.path);
    let content = html! {
        nav.pager {
            @if let Some((lib, p)) = &prev {
                a rel="prev" href=(context.view_url(*lib, p)) { "< Prev" }
            } @else { span {} }
            span.path { (file.path) }
            @if let Some((lib, n)) = &next {
                a rel="next" href=(context.view_url(*lib, n)) { "Next >" }
            } @else { span {} }
        }
        div.stage {
            @match file.media_type {
                MediaType::Image => {
                    @let src = match thumbs::view_source(&file) {
                        ViewSource::Original => media_url.clone(),
                        ViewSource::Preview => format!(
                            "{}?v={}",
                            file_url("preview", library_id, &file.path),
                            file.stamp.mtime_ns
                        ),
                        ViewSource::Placeholder => thumb_url(library_id, &file.path, file.stamp.mtime_ns),
                    };
                    img src=(src) alt=(file_name(&file.path));
                }
                MediaType::Video => {
                    video controls preload="metadata" src=(media_url)
                        poster=(thumb_url(library_id, &file.path, file.stamp.mtime_ns)) {}
                    p.unplayable hidden {
                        "This browser cannot play this video"
                        @if let Some(codec) = &codec { " (" (codec) ")" }
                        ". Open the original to play it in another program."
                    }
                }
                MediaType::Audio => {
                    audio controls preload="metadata" src=(media_url) {}
                }
            }
        }
        dl.details {
            @if let Some(date) = &date {
                dt { @if file.meta.taken_at.is_some() { "Taken" } @else { "Modified" } }
                dd { (date.replacen('T', " ", 1)) }
            }
            @if let Some(camera) = camera(&file) { dt { "Camera" } dd { (camera) } }
            @if let (Some(w), Some(h)) = (file.meta.width, file.meta.height) {
                dt { "Dimensions" } dd { (w) " x " (h) }
            }
            dt { "Size" } dd { (HumanBytes(file.stamp.size)) }
            @if let Some(codec) = &codec { dt { "Codec" } dd { (codec) } }
            dt { "Library" } dd { (display_root(&located.root)) }
            dt { "Path" } dd { (file.path) }
            @if let Some(sha256) = &file.sha256 { dt { "SHA-256" } dd.hash { (sha256) } }
        }
        @if copies.len() > 1 {
            h2 { "Identical copies" }
            ul.copies-list {
                @for (i, copy) in copies.iter().enumerate() {
                    li {
                        @if copy.library_id == library_id && copy.path == file.path {
                            "This file"
                        } @else {
                            a href=(context.view_url(copy.library_id, &copy.path)) {
                                (display_root(&copy.root)) " / " (copy.path)
                            }
                        }
                        @if i == 0 { " " span.badge.keep { "keep" } }
                    }
                }
            }
        }
        p { a href=(media_url) { "Open original" } }
    };
    Ok(Html(layout(file_name(&file.path), None, None, content).into_string()).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn test_urls() {
        assert_eq!(
            file_url("media", 3, "a b/ø#?.jpg"),
            "/media/3/a%20b/%C3%B8%23%3F.jpg"
        );
        assert_eq!(folder_url(1, ""), "/folders/1");
        assert_eq!(
            with_query(
                "/x",
                &[("type", Some(String::from("image"))), ("page", None)]
            ),
            "/x?type=image"
        );
    }

    #[test]
    fn test_month_label() {
        assert_eq!(month_label("2024-05").as_deref(), Some("May 2024"));
        assert_eq!(month_label("2024-13"), None);
        assert_eq!(month_label("2024-00"), None);
        assert_eq!(month_label("../etc"), None);
    }
}
