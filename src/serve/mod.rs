//! The local web viewer: browse the catalog, view and play media, compare duplicates.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

mod pages;
mod thumbs;

use crate::catalog::{library_path, Catalog, FileRecord};
use crate::dupes::{group_dupes, load_dupes, DupeReport};
use crate::error::Error;
use crate::scan::resolve;
use axum::body::Body;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use directories::ProjectDirs;
use log::warn;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use thumbs::{Size, Thumbnailer};
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower_http::services::ServeFile;

/// HTMX, compiled in so the viewer works offline.
static HTMX: &str = include_str!("assets/htmx-4.0.0.min.js");
/// Style sheet.
static CSS: &str = include_str!("assets/pixrs.css");
/// Keyboard navigation and the notice for videos the browser cannot play.
static JS: &str = include_str!("assets/pixrs.js");

/// What the viewer needs to run.
pub struct Config {
    pub catalog: Catalog,
    /// Only show files in these libraries; all if empty.
    pub libraries: Vec<i64>,
    /// Where thumbnails are cached. Without one, every file gets a placeholder.
    pub cache_dir: Option<PathBuf>,
    pub port: u16,
    /// Perceptual hash distance up to which images count as similar; `None` to not look for
    /// similar images.
    pub similar: Option<u32>,
}

/// Shared state of the running viewer.
pub(crate) struct AppState {
    catalog: Mutex<Catalog>,
    libraries: Vec<i64>,
    thumbs: Thumbnailer,
    /// `ffprobe`, if it is on `PATH`.
    ffprobe: Option<PathBuf>,
    /// The `Host` header values the viewer answers to.
    hosts: Vec<String>,
    pub(crate) similar: Option<u32>,
    /// The last duplicate report, so paging through it does not compare every pair of images
    /// again. An async lock, held while a report is made, so concurrent requests share it.
    dupes: tokio::sync::Mutex<Option<CachedDupes>>,
}

/// How often the duplicate report is made again at most while the catalog keeps changing.
const DUPES_REFRESH: Duration = Duration::from_secs(30);

/// A duplicate report and when it was made.
pub(crate) struct CachedDupes {
    /// The catalog's `data_version` when it was made.
    version: i64,
    made: Instant,
    report: Arc<DupeReport>,
}

/// An error answered to the browser.
#[derive(Debug)]
pub(crate) enum AppError {
    NotFound,
    Internal(String),
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Internal(format!("catalog error: {}", e))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "Not found").into_response(),
            AppError::Internal(msg) => {
                warn!("{}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        }
    }
}

/// A cataloged file and where it is on disk.
pub(crate) struct Located {
    /// Root of its library, as stored in the catalog.
    pub root: String,
    pub full_path: PathBuf,
    pub file: FileRecord,
}

impl AppState {
    /// Run a catalog query on the blocking thread pool.
    pub(crate) async fn query<T, F>(self: &Arc<Self>, f: F) -> Result<T, AppError>
    where
        T: Send + 'static,
        F: FnOnce(&Catalog) -> rusqlite::Result<T> + Send + 'static,
    {
        let state = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            // A panic in another query leaves the connection usable.
            let catalog = state.catalog.lock().unwrap_or_else(PoisonError::into_inner);
            f(&catalog)
        })
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .map_err(AppError::from)
    }

    /// The duplicate report for the libraries shown. With `fresh` (the first page), it is made
    /// again if the catalog changed, but at most every `DUPES_REFRESH` while a scan keeps
    /// changing it; later pages always use the report the first page showed, so paging through
    /// it cannot skip or repeat groups. One report is made at a time and shared by every request
    /// waiting for it. Only reading the catalog holds it; comparing the images does not, so
    /// other pages stay responsive meanwhile.
    pub(crate) async fn dupes(self: &Arc<Self>, fresh: bool) -> Result<Arc<DupeReport>, AppError> {
        let mut cached = self.dupes.lock().await;
        if let Some(c) = cached.as_mut() {
            if !fresh || c.made.elapsed() < DUPES_REFRESH {
                return Ok(Arc::clone(&c.report));
            }
            if self.query(|catalog| catalog.data_version()).await? == c.version {
                c.made = Instant::now();
                return Ok(Arc::clone(&c.report));
            }
        }
        let version = self.query(|catalog| catalog.data_version()).await?;
        let (libraries, similar) = (self.libraries.clone(), self.similar.is_some());
        let input = self
            .query(move |catalog| load_dupes(catalog, &libraries, similar))
            .await?;
        let state = Arc::clone(self);
        let report = tokio::task::spawn_blocking(move || {
            Arc::new(group_dupes(input, &state.libraries, state.similar))
        })
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
        *cached = Some(CachedDupes {
            version,
            made: Instant::now(),
            report: Arc::clone(&report),
        });
        Ok(report)
    }

    /// Whether the listings show the library.
    pub(crate) fn shows(&self, library_id: i64) -> bool {
        self.libraries.is_empty() || self.libraries.contains(&library_id)
    }

    /// Look up a file the browser asked for. Only files that are in the catalog are found, so a
    /// request cannot reach anything else on disk. Files in libraries the listings leave out are
    /// found too, since duplicate groups show copies in every library.
    pub(crate) async fn locate(
        self: &Arc<Self>,
        library_id: i64,
        path: String,
    ) -> Result<Located, AppError> {
        let found = self
            .query(move |catalog| match catalog.library_root(library_id)? {
                Some(root) => Ok(catalog.file(library_id, &path)?.map(|file| (root, file))),
                None => Ok(None),
            })
            .await?;
        let (root, file) = found.ok_or(AppError::NotFound)?;
        Ok(Located {
            full_path: library_path(Path::new(&root), &file.path),
            root,
            file,
        })
    }
}

/// The default thumbnail cache, in the OS cache directory.
pub fn default_cache_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "pixrs").map(|dirs| dirs.cache_dir().join("thumbnails"))
}

/// Start the viewer on `127.0.0.1` and serve until the process is stopped. Calls `ready` with
/// the viewer's URL once it is listening.
pub fn run(config: Config, ready: impl FnOnce(&str)) -> Result<(), Error> {
    if let Some(cache_dir) = &config.cache_dir {
        check_cache_outside_libraries(&config.catalog, cache_dir)?;
    }
    let (ffmpeg, ffprobe) = (find_tool("ffmpeg"), find_tool("ffprobe"));

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", config.port))
            .await
            .map_err(|e| {
                Error::Usage(format!(
                    "Unable to listen on 127.0.0.1:{}: {}",
                    config.port, e
                ))
            })?;
        // The port the OS picked, if it was 0.
        let port = listener.local_addr()?.port();
        let app = router(Arc::new(AppState::new(config, port, ffmpeg, ffprobe)));
        ready(&format!("http://127.0.0.1:{}/", port));
        axum::serve(listener, app).await?;
        Ok(())
    })
}

impl AppState {
    fn new(config: Config, port: u16, ffmpeg: Option<PathBuf>, ffprobe: Option<PathBuf>) -> Self {
        AppState {
            catalog: Mutex::new(config.catalog),
            libraries: config.libraries,
            thumbs: Thumbnailer::new(config.cache_dir, ffmpeg),
            ffprobe,
            hosts: allowed_hosts(port),
            similar: config.similar,
            dupes: tokio::sync::Mutex::new(None),
        }
    }
}

/// The `Host` header values for the viewer on `port`. Browsers leave out port 80.
fn allowed_hosts(port: u16) -> Vec<String> {
    let mut hosts = vec![format!("localhost:{}", port), format!("127.0.0.1:{}", port)];
    if port == 80 {
        hosts.extend([String::from("localhost"), String::from("127.0.0.1")]);
    }
    hosts
}

/// Refuse a thumbnail cache inside any cataloged library, shown or not: writing there would
/// modify the library, and the next scan would catalog the thumbnails.
fn check_cache_outside_libraries(catalog: &Catalog, cache_dir: &Path) -> Result<(), Error> {
    let cache_dir = resolve(cache_dir)?;
    for library in catalog.libraries()? {
        if cache_dir.starts_with(&library.root) {
            return Err(Error::Usage(format!(
                "The thumbnail cache {} must be outside the library {}; pass --cache",
                cache_dir.display(),
                crate::catalog::display_root(&library.root)
            )));
        }
    }
    Ok(())
}

/// `name` if it is on `PATH`, with a note if it is not.
fn find_tool(name: &str) -> Option<PathBuf> {
    let found = crate::decode::find_tool(name);
    if found.is_none() {
        eprintln!(
            "{} not found on PATH; some thumbnails will be placeholders",
            name
        );
    }
    found
}

/// All routes, behind the `Host` check.
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(pages::libraries))
        .route("/folders/{lib}", get(pages::folder_root))
        .route("/folders/{lib}/{*dir}", get(pages::folder))
        .route("/dates", get(pages::dates_latest))
        .route("/dates/{month}", get(pages::dates))
        .route("/dupes", get(pages::dupes))
        .route("/view/{lib}/{*path}", get(pages::view))
        .route("/media/{lib}/{*path}", get(media))
        .route("/thumb/{lib}/{*path}", get(thumb))
        .route("/preview/{lib}/{*path}", get(preview))
        .route(
            "/static/htmx-4.0.0.min.js",
            get(|| asset(HTMX, "text/javascript")),
        )
        .route("/static/pixrs.css", get(|| asset(CSS, "text/css")))
        .route("/static/pixrs.js", get(|| asset(JS, "text/javascript")))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            check_host,
        ))
        .with_state(state)
}

/// Answer only requests from the viewer itself or typed into the browser.
///
/// A web page on another site can make the browser send requests to `127.0.0.1` through a DNS
/// name it controls (DNS rebinding); those carry that name in `Host` and are refused. It can
/// also embed viewer URLs directly, e.g. to test whether a file exists; browsers mark those with
/// `Sec-Fetch-Site`, and anything but `same-origin` or `none` (typed, bookmarked) is refused.
/// Other ports on `127.0.0.1` count as `same-site`, so that is refused too.
async fn check_host(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    let allowed = |name: &str| state.hosts.iter().any(|h| h.eq_ignore_ascii_case(name));
    let authority_ok = req.uri().authority().is_none_or(|a| allowed(a.as_str()));
    if !(host.is_some_and(allowed) && authority_ok) {
        return (StatusCode::MISDIRECTED_REQUEST, "Unknown host").into_response();
    }
    let site = headers.get("sec-fetch-site").map(|site| site.as_bytes());
    if site.is_some_and(|site| site != b"same-origin" && site != b"none") {
        return (
            StatusCode::FORBIDDEN,
            "Requests from other sites are refused",
        )
            .into_response();
    }
    next.run(req).await
}

/// A compiled-in asset.
async fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        body,
    )
        .into_response()
}

/// The original file, with range requests so audio and video can seek.
async fn media(
    State(state): State<Arc<AppState>>,
    UrlPath((lib, path)): UrlPath<(i64, String)>,
    req: Request,
) -> Result<Response, AppError> {
    let located = state.locate(lib, path).await?;
    let res = ServeFile::new(located.full_path)
        .oneshot(req)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(res.map(Body::new))
}

/// A small JPEG for listings, or a placeholder.
async fn thumb(
    State(state): State<Arc<AppState>>,
    UrlPath((lib, path)): UrlPath<(i64, String)>,
) -> Result<Response, AppError> {
    let located = state.locate(lib, path).await?;
    Ok(rendition(&state, &located, Size::Thumb).await)
}

/// A large JPEG of an image the browser cannot show itself, or a placeholder.
async fn preview(
    State(state): State<Arc<AppState>>,
    UrlPath((lib, path)): UrlPath<(i64, String)>,
) -> Result<Response, AppError> {
    let located = state.locate(lib, path).await?;
    Ok(rendition(&state, &located, Size::Preview).await)
}

/// A cached JPEG of the file at `size`, or a placeholder if there is no way to make one.
async fn rendition(state: &AppState, located: &Located, size: Size) -> Response {
    match state.thumbs.get(located, size).await {
        Some(jpeg) => (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg")),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("max-age=86400"),
                ),
            ],
            jpeg,
        )
            .into_response(),
        None => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("image/svg+xml"),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
            thumbs::placeholder(&located.file),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::{MediaMeta, Stamp};
    use crate::media::MediaType;
    use axum::http::Request as HttpRequest;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::{tempdir, TempDir};

    /// A library with a small PNG, a text file outside the catalog, and a viewer on top.
    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        cache: PathBuf,
        app: Router,
        library_id: i64,
    }

    fn fixture(ffmpeg: bool) -> Fixture {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lib/sub dir")).unwrap();
        let root = dir.path().join("lib").canonicalize().unwrap();
        let png = image::RgbImage::from_pixel(640, 480, image::Rgb([200, 30, 30]));
        png.save(root.join("sub dir/red é.png")).unwrap();
        fs::write(root.join("secret.txt"), b"not media").unwrap();
        fs::write(root.join("photo.heic"), b"not really heic").unwrap();

        let catalog_path = dir.path().join("catalog.sqlite");
        let catalog = Catalog::open(&catalog_path).unwrap();
        let library_id = catalog.library_id(root.to_str().unwrap()).unwrap();
        for (path, media_type) in [
            ("sub dir/red é.png", MediaType::Image),
            ("photo.heic", MediaType::Image),
        ] {
            let size = fs::metadata(library_path(&root, path)).unwrap().len();
            catalog
                .upsert(
                    library_id,
                    &FileRecord {
                        path: String::from(path),
                        stamp: Stamp { size, mtime_ns: 1 },
                        media_type,
                        sha256: None,
                        meta: MediaMeta::default(),
                    },
                )
                .unwrap();
        }
        drop(catalog);

        let cache = dir.path().join("cache");
        let config = Config {
            catalog: Catalog::open_existing(&catalog_path).unwrap(),
            libraries: vec![],
            cache_dir: Some(cache.clone()),
            port: 2635,
            similar: Some(crate::dupes::DEFAULT_THRESHOLD),
        };
        let ffmpeg = ffmpeg.then(|| PathBuf::from("ffmpeg"));
        let app = router(Arc::new(AppState::new(config, 2635, ffmpeg, None)));
        Fixture {
            _dir: dir,
            root,
            cache,
            app,
            library_id,
        }
    }

    async fn get(app: &Router, uri: &str, host: &str) -> Response {
        let req = HttpRequest::get(uri)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    async fn body(res: Response) -> Vec<u8> {
        axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn test_rejects_other_hosts() {
        let f = fixture(false);
        for host in ["evil.example:2635", "localhost:9999", "127.0.0.2:2635"] {
            assert_eq!(
                get(&f.app, "/", host).await.status(),
                StatusCode::MISDIRECTED_REQUEST
            );
        }
        assert_eq!(
            get(&f.app, "/", "localhost:2635").await.status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&f.app, "/", "127.0.0.1:2635").await.status(),
            StatusCode::OK
        );
        assert_eq!(allowed_hosts(80)[2..], ["localhost", "127.0.0.1"]);
        assert_eq!(allowed_hosts(2635).len(), 2);
    }

    #[tokio::test]
    async fn test_rejects_requests_from_other_sites() {
        let f = fixture(false);
        let uri = format!("/thumb/{}/photo.heic", f.library_id);
        for (site, status) in [
            ("cross-site", StatusCode::FORBIDDEN),
            ("same-site", StatusCode::FORBIDDEN),
            ("same-origin", StatusCode::OK),
            ("none", StatusCode::OK),
        ] {
            let req = HttpRequest::get(&uri)
                .header(header::HOST, "localhost:2635")
                .header("sec-fetch-site", site)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                f.app.clone().oneshot(req).await.unwrap().status(),
                status,
                "{}",
                site
            );
        }
    }

    #[tokio::test]
    async fn test_huge_page_numbers_do_not_break_the_viewer() {
        let f = fixture(false);
        for uri in [
            format!("/folders/{}?page=18446744073709551615", f.library_id),
            String::from("/dates/1970-01?page=18446744073709551615"),
            String::from("/dupes?page=18446744073709551615"),
        ] {
            assert_eq!(
                get(&f.app, &uri, "localhost:2635").await.status(),
                StatusCode::OK
            );
        }
        assert_eq!(
            get(&f.app, "/", "localhost:2635").await.status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn test_media_serves_only_cataloged_files() {
        let f = fixture(false);
        let host = "127.0.0.1:2635";
        let lib = f.library_id;
        let res = get(
            &f.app,
            &format!("/media/{}/sub%20dir/red%20%C3%A9.png", lib),
            host,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "image/png");

        for uri in [
            format!("/media/{}/secret.txt", lib),
            format!("/media/{}/sub%20dir/../secret.txt", lib),
            format!("/media/{}/..%2Fsecret.txt", lib),
            format!("/media/{}/%2E%2E/%2E%2E/etc/passwd", lib),
            format!("/media/{}/sub%20dir/red%20%C3%A9.png", lib + 1),
        ] {
            assert_eq!(
                get(&f.app, &uri, host).await.status(),
                StatusCode::NOT_FOUND,
                "{}",
                uri
            );
        }
    }

    #[tokio::test]
    async fn test_media_supports_ranges() {
        let f = fixture(false);
        let req = HttpRequest::get(format!("/media/{}/photo.heic", f.library_id))
            .header(header::HOST, "localhost:2635")
            .header(header::RANGE, "bytes=4-9")
            .body(Body::empty())
            .unwrap();
        let res = f.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body(res).await, b"really");
    }

    #[tokio::test]
    async fn test_thumbnail_is_cached_outside_the_library() {
        let f = fixture(false);
        let uri = format!("/thumb/{}/sub%20dir/red%20%C3%A9.png", f.library_id);
        let res = get(&f.app, &uri, "localhost:2635").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "image/jpeg");
        let jpeg = image::load_from_memory(&body(res).await).unwrap();
        assert_eq!((jpeg.width(), jpeg.height()), (320, 240));

        let cached: Vec<_> = walkdir::WalkDir::new(&f.cache)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .collect();
        assert_eq!(cached.len(), 1);
        let library: Vec<_> = walkdir::WalkDir::new(&f.root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .collect();
        assert_eq!(library.len(), 3);
    }

    #[tokio::test]
    async fn test_placeholder_without_ffmpeg() {
        let f = fixture(false);
        let res = get(
            &f.app,
            &format!("/thumb/{}/photo.heic", f.library_id),
            "localhost:2635",
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "image/svg+xml");
        assert!(String::from_utf8(body(res).await).unwrap().contains("HEIC"));
    }

    #[tokio::test]
    async fn test_pages_render() {
        let f = fixture(false);
        let lib = f.library_id;
        for uri in [
            String::from("/"),
            format!("/folders/{}", lib),
            format!("/folders/{}/sub%20dir", lib),
            format!("/folders/{}/sub%20dir?type=image&page=1&partial=1", lib),
            String::from("/dates/1970-01"),
            String::from("/dupes"),
            format!("/view/{}/sub%20dir/red%20%C3%A9.png", lib),
            format!("/view/{}/photo.heic?from=date", lib),
            String::from("/static/htmx-4.0.0.min.js"),
        ] {
            let res = get(&f.app, &uri, "localhost:2635").await;
            assert_eq!(res.status(), StatusCode::OK, "{}", uri);
        }
        let res = get(&f.app, "/dates", "localhost:2635").await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
    }

    #[test]
    fn test_refuses_cache_inside_a_library() {
        let f = fixture(false);
        let dir = f.root.parent().unwrap();
        let catalog = Catalog::open_existing(&dir.join("catalog.sqlite")).unwrap();
        assert!(matches!(
            check_cache_outside_libraries(&catalog, &f.root.join("thumbs")),
            Err(Error::Usage(_))
        ));
        assert!(check_cache_outside_libraries(&catalog, &f.cache).is_ok());
    }
}
