//! Routes. Front strips Mailu's `WEB_WEBMAIL` prefix, so kubicle serves `/`; the client builds every URL from the
//! base path it was loaded under.

use std::{collections::HashSet, convert::Infallible, io::Write as _, net::SocketAddr, num::NonZeroUsize, path::PathBuf, sync::Arc};
use std::{sync::LazyLock, sync::PoisonError, sync::atomic::AtomicU64, sync::atomic::Ordering, time::Duration};

use axum::{Json, Router, body::Body, extract::ConnectInfo, extract::DefaultBodyLimit, extract::FromRequestParts, extract::Multipart};
use axum::{extract::Path, extract::Query, extract::Request, extract::State, http::HeaderMap, http::HeaderValue, http::Method};
use axum::{http::StatusCode, http::Uri, http::header, http::request::Parts, middleware, middleware::Next, response::IntoResponse};
use axum::{response::Redirect, response::Response, response::sse::Event as SseEvent, response::sse::Sse, routing::get, routing::post};
use bytes::Bytes;
use kubicle_api::{Change, Chosen, ComposeMode, ComposeStart, Draft, DraftAttachment, DraftCopy, Event, Folder, FolderPrefs, FolderRole};
use kubicle_api::{FolderSizes, KEEPALIVE_SECONDS, ListPage, ListQuery, LogoutPurge, Message, Messages, Prefill, RemoteImages, Row, Search};
use kubicle_api::{SentCopy, Settings, SortColumn, SortOrder, SourceRef, ThemeSource, Viewing, ZipMode};
use kubicle_imap::header::Fetched;
use kubicle_mail::{links::DOWNLOAD, links::IMAGE, links::INLINE, links::PartRoute, links::VIEW, render};
use kubicle_mailbox::{Error, compose, compose::Identity, settings::Store, sync::Mailbox};
use mail_parser::mailbox::mbox::MessageIterator;
use regex::Regex;
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{sync::OwnedSemaphorePermit, sync::Semaphore, sync::broadcast::error::RecvError, sync::mpsc};
use tower_http::{services::ServeDir, set_header::SetResponseHeaderLayer};
use tracing::{debug, info, warn};
use zip::{CompressionMethod, write::SimpleFileOptions};

use crate::{assets, config::Config, proxy::Proxy, session, session::Session, session::Sessions, sign, sign::ImageQuery, sign::PartQuery};
use crate::{sign::Signer, themes};

pub struct App {
    pub config: Config,
    /// `Session::build`, from `assets::build`.
    pub build: String,
    pub sessions: Sessions,
    pub store: Store,
    pub signer: Signer,
    pub proxy: Proxy,
    pub renders: Renders,
}

/// What parses mail for a page or a Compose, as many at once as there are CPUs, on the blocking pool so it holds
/// up no other request. Each work leaves a marker while it runs: one that killed the process left it, and that
/// message is not parsed again by this build, so a tab that reloads after the restart and prefetches it again does
/// not kill the process again.
pub struct Renders {
    /// `rendering/<build>` in the data directory, `<build>` the content hash of the server binary's path: a new
    /// build may carry the fix, and parses such mail again. Its `live/` holds a marker per work running,
    /// `<message>.<n>`; at start each one left there becomes the crash record `<message>`, deleted to try again. A
    /// message's name is the content hash of the user, the folder, the UIDVALIDITY and the UID, a line each.
    dir: PathBuf,
    turns: Arc<Semaphore>,
    /// Numbers the live markers, so works on one message never share or delete each other's.
    next: AtomicU64,
}

impl Renders {
    pub fn new(data_dir: &std::path::Path) -> std::io::Result<Self> {
        let build = assets::content_hash(std::env::current_exe()?.as_os_str().as_encoded_bytes());
        let all = data_dir.join("rendering");
        for old in std::fs::read_dir(&all).into_iter().flatten().flatten().filter(|e| e.file_name() != *build) {
            std::fs::remove_dir_all(old.path())?;
        }
        let (dir, live) = (all.join(&build), all.join(&build).join("live"));
        std::fs::create_dir_all(&live)?;
        for left in std::fs::read_dir(&live)?.flatten() {
            let name = left.file_name().to_string_lossy().into_owned();
            std::fs::rename(left.path(), dir.join(name.split('.').next().unwrap_or_default()))?;
        }
        let turns = Arc::new(Semaphore::new(std::thread::available_parallelism().map_or(1, NonZeroUsize::get)));
        Ok(Self { dir, turns, next: AtomicU64::new(0) })
    }

    /// A message's name in `dir`.
    fn key(user: &str, folder: &str, validity: u32, uid: u32) -> String {
        assets::content_hash(format!("{user}\n{folder}\n{validity}\n{uid}").as_bytes())
    }

    /// Parses `raw`, the message `message` names (`key`), and runs `work` on it, on the blocking pool in a turn: a
    /// page or a Compose gets its `Mail` only here, so none is parsed outside a marker. The work holds the turn and
    /// its marker, not the request: a closed tab left a marker, and more works than CPUs ran. 422 for a crash record.
    async fn run<T: Send + 'static>(
        &self, message: String, raw: Bytes, work: impl FnOnce(&render::Mail) -> Answer<T> + Send + 'static,
    ) -> Answer<T> {
        if self.dir.join(&message).exists() {
            return Err(Error::status(StatusCode::UNPROCESSABLE_ENTITY, "kubicle stopped while showing this message; download it instead"));
        }
        let turn = Arc::clone(&self.turns).acquire_owned().await.expect("the semaphore is never closed");
        let marker = self.dir.join("live").join(format!("{message}.{}", self.next.fetch_add(1, Ordering::Relaxed)));
        let done = tokio::task::spawn_blocking(move || {
            struct Removed(PathBuf); // removes the marker however the work ends: a panic does not end the process
            impl Drop for Removed {
                fn drop(&mut self) { drop(std::fs::remove_file(&self.0)) }
            }
            let _turn = turn;
            std::fs::write(&marker, b"")?;
            let _marker = Removed(marker);
            work(&render::parse(&raw)?)
        });
        done.await.map_err(|e| Error::status(StatusCode::INTERNAL_SERVER_ERROR, format!("the render failed: {e}")))?
    }
}

/// The app's own pages may run only its own wasm. A srcdoc frame inherits this policy on top of its own
/// (render.rs), and Firefox needs that: it races `srcset` loads past the frame's meta policy.
const APP_CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; \
                       img-src 'self' data:; connect-src 'self'; frame-src 'self'; object-src 'none'; base-uri 'none'; \
                       form-action 'self'; frame-ancestors 'self'";

const SUGGESTIONS: usize = 15; // Roundcube's autocomplete_max

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/api/session", get(session_info))
        // axum's own 2 MB body limit, past `MAX_SETTINGS`: refused before its body was read, front's nginx answered 502
        .route("/api/settings", get(settings).post(save_settings))
        .route("/api/theme/{file}", get(user_theme_css))
        .route("/api/logout", post(logout))
        .route("/api/events", get(events))
        .route("/api/list", get(list))
        .route("/api/view", post(view))
        .route("/api/message", get(message))
        .route("/api/source", get(source))
        .route("/api/download", get(download))
        .route("/api/zip", get(zip))
        .route("/api/sizes", get(sizes))
        .route("/api/print", get(print))
        .route("/api/change", post(change))
        .route("/api/autocomplete", get(autocomplete))
        .route("/api/addresses", get(addresses))
        .route("/api/compose", post(prefill))
        .route("/api/draft", post(save_draft))
        .route("/api/send", post(send))
        .route("/api/redirect", post(redirect))
        // A ceiling on the request: 35 MB of file, which base64 grows to about Mailu's default 50 MB message limit,
        // and the multipart framing. Under it the mail server's own limit decides (`compose::check_upload`).
        .route("/api/upload", get(stored_upload).post(upload).layer(DefaultBodyLimit::max(kubicle_api::MAX_UPLOAD_MIB << 20)))
        .route("/api/upload/remove", post(remove_uploads))
        .route("/api/import", post(import).layer(DefaultBodyLimit::max(kubicle_api::MAX_UPLOAD_MIB << 20)));
    let signed = Router::new()
        .route(&format!("/{INLINE}"), get(async |app, query| part(app, query, PartRoute::Inline).await))
        .route(&format!("/{DOWNLOAD}"), get(async |app, query| part(app, query, PartRoute::Download).await))
        .route(&format!("/{VIEW}"), get(async |app, query| part(app, query, PartRoute::View).await))
        .route(&format!("/{IMAGE}"), get(image));
    let mut files =
        Router::new().fallback_service(ServeDir::new(&app.config.web_dir).append_index_html_on_directories(true).precompressed_br());
    if let Some(dir) = &app.config.themes_dir {
        files = files.nest_service("/system-themes", ServeDir::new(dir));
    }
    let set = |name, value| SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/sso.php", get(sso))
        .merge(api)
        .merge(signed)
        .merge(files.layer(middleware::from_fn(assets::headers)).layer(set(header::CONTENT_SECURITY_POLICY, APP_CSP)))
        // Every answer but the app's own files carries mail or the user's data, JSON included, and is no page: none
        // runs anything opened on its own, and the browser keeps none, as Roundcube sends no-cache on every answer.
        .layer(set(header::CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox"))
        .layer(set(header::CACHE_CONTROL, "private, no-store"))
        .layer(set(header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
        .layer(set(header::REFERRER_POLICY, "no-referrer"))
        .layer(set(header::X_DNS_PREFETCH_CONTROL, "off")) // CSP does not cover DNS prefetch
        .layer(set(header::X_FRAME_OPTIONS, "SAMEORIGIN"))
        .layer(middleware::from_fn_with_state(Arc::clone(&app), trusted_peer))
        .layer(middleware::from_fn(log_request))
        .with_state(app)
}

/// One debug line per request, which the test VMs' journals keep (tests/record.py): method, path, status and time.
/// The query only on `api/` routes, as the signed ones carry their signature in theirs.
async fn log_request(req: Request, next: Next) -> Response {
    let (method, uri, started) = (req.method().clone(), req.uri().clone(), std::time::Instant::now());
    let shown = if uri.path().starts_with("/api/") { uri.to_string() } else { uri.path().to_owned() };
    let response = next.run(req).await;
    debug!(%method, uri = shown, status = response.status().as_u16(), ms = started.elapsed().as_millis(), "request");
    response
}

/// Requests reach kubicle only through front. A peer outside the configured range is refused before routing, but for
/// `/healthz` from loopback: `kubicle-server --health`, which Docker runs inside the container.
async fn trusted_peer(State(app): State<Arc<App>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let peer = peer.ip().to_canonical(); // v4 on a dual-stack listen
    let health = req.uri().path() == "/healthz" && peer.is_loopback();
    if app.config.trusted_proxy.contains(&peer) || health { next.run(req).await } else { StatusCode::FORBIDDEN.into_response() }
}

type Answer<T = StatusCode> = Result<T, Error>;

fn not_found() -> Error { Error::status(StatusCode::NOT_FOUND, "no such message") }

/// The signed-in session behind a request. GET and HEAD need only the cookie; anything that changes state also
/// needs the CSRF header and, when the browser sends one, a matching Origin.
struct Authed(Arc<Session>);

impl FromRequestParts<Arc<App>> for Authed {
    type Rejection = Error;

    #[expect(clippy::unused_async_trait_impl, reason = "the trait's method is async; finding a session awaits nothing")]
    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, Self::Rejection> {
        let unauthorized = || Error::status(StatusCode::UNAUTHORIZED, "no session: open sso.php");
        let cookie = parts.headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).ok_or_else(unauthorized)?;
        let id = session::from_cookie_header(cookie).ok_or_else(unauthorized)?;
        let session = app.sessions.find(id).ok_or_else(unauthorized)?;
        if parts.method != Method::GET && parts.method != Method::HEAD {
            let csrf = parts.headers.get("x-kubicle-csrf").and_then(|v| v.to_str().ok()).unwrap_or_default();
            if !session.csrf_matches(csrf) {
                return Err(Error::status(StatusCode::FORBIDDEN, "missing or wrong CSRF token"));
            }
            if parts.headers.get(header::ORIGIN).is_some_and(|origin| origin.as_bytes() != app.config.public_origin.as_bytes()) {
                return Err(Error::status(StatusCode::FORBIDDEN, "cross-origin request"));
            }
        }
        Ok(Self(session))
    }
}

/// The session a signed URL names, and its query. The frame or tab that loads one sends no cookie, so the signature,
/// over the route and every parameter, stands in for one. Not a use of the session (`Sessions::find_handle`).
struct Signed<T>(Arc<Session>, T);

impl<T: DeserializeOwned> FromRequestParts<Arc<App>> for Signed<T> {
    type Rejection = Error;

    #[expect(clippy::unused_async_trait_impl, reason = "the trait's method is async; verifying awaits nothing")]
    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, Self::Rejection> {
        let forbidden = || Error::status(StatusCode::FORBIDDEN, "invalid or expired link");
        let handle = app.signer.verify(&parts.uri).ok_or_else(forbidden)?;
        let session = app.sessions.find_handle(&handle).ok_or_else(forbidden)?;
        let Query(query) = Query::try_from_uri(&parts.uri).map_err(|_| forbidden())?;
        Ok(Self(session, query))
    }
}

/// Front adds Mailu's identity here and nowhere else, so only this route reads those headers. The pair is proven
/// by an IMAP login with it before any session exists, so a forged header without a live Mailu token gets nothing.
async fn sso(State(app): State<Arc<App>>, headers: HeaderMap) -> Answer<Response> {
    let header = |name| headers.get(name).and_then(|v: &HeaderValue| v.to_str().ok()).filter(|v| !v.is_empty());
    let (Some(user), Some(token)) = (header("x-remote-user"), header("x-remote-user-token")) else {
        return Ok((StatusCode::FORBIDDEN, "Mailu sign-in did not reach kubicle").into_response());
    };
    let current = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(session::from_cookie_header);
    Ok(match app.sessions.sign_in(&app.config.mail, &app.config.data_dir, &app.store, user, token, current).await {
        Ok(session) => {
            let cookie = session::set_cookie(&session.id, app.config.public_origin.starts_with("https://"));
            ([(header::SET_COOKIE, cookie)], Redirect::to("./")).into_response()
        }
        Err(Error::Imap(kubicle_imap::Error::Expired)) => (StatusCode::FORBIDDEN, "Mailu did not accept this sign-in").into_response(),
        Err(e) => {
            warn!(user, error = %e, "sign-in check against the mail server failed");
            (StatusCode::SERVICE_UNAVAILABLE, "the mail server is not reachable right now").into_response()
        }
    })
}

/// `compose::learn_size_limit` from start until front answers, which it may not do yet when kubicle starts: again
/// after 1 s, then twice as long each time up to a minute. Sending works meanwhile, and asks the limit itself.
pub async fn learn_size_limit(app: Arc<App>) {
    let mut wait = Duration::from_secs(1);
    loop {
        match compose::learn_size_limit(&app.config.mail).await {
            Ok(()) => return info!(limit = app.config.mail.size_limit(), "the mail server's message size limit"),
            Err(e) => warn!(error = %e, retry_in = ?wait, "could not ask the mail server its message size limit"),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_mins(1));
    }
}

/// `api/session`'s query: the folder and page the address names, which `Session::page` answers.
#[derive(Deserialize)]
struct Opening {
    folder: Option<String>,
    #[serde(default)]
    page: u32,
}

async fn session_info(
    State(app): State<Arc<App>>, Authed(session): Authed, Query(opening): Query<Opening>,
) -> Answer<Json<kubicle_api::Session>> {
    let settings = app.store.settings(&session.user)?;
    // An engine job, so it runs after the engine's first folder list, which `folders` below then reads.
    let page = match opening.folder {
        Some(folder) => {
            let FolderPrefs { sort, order, threads, .. } = settings.folders.get(&folder).copied().unwrap_or_default();
            let query = ListQuery { folder, page: opening.page, size: settings.page_size, sort, order, threads, search: None };
            session.mailbox.list(query, None, || true).await.ok() // the client asks `api/list` again, which reports why
        }
        None => None,
    };
    let mut themes = themes::list_dir(&app.config.web_dir.join("themes"), ThemeSource::BuiltIn);
    themes.extend(app.config.themes_dir.iter().flat_map(|dir| themes::list_dir(dir, ThemeSource::System)));
    themes.extend(settings.themes.iter().filter_map(|(id, css)| themes::info(id.clone(), css, ThemeSource::User)));
    Ok(Json(kubicle_api::Session {
        user: session.user.clone(),
        csrf: session.csrf.clone(),
        folders: session.mailbox.folders(),
        themes,
        settings,
        logout_url: app.config.logout_url.clone(),
        admin_url: app.config.admin_url.clone(),
        source_url: app.config.source_url.clone(),
        build: app.build.clone(),
        max_message_size: app.config.mail.size_limit(),
        quota: session.mailbox.quota(),
        account: app.store.validity(&session.user)?,
        page,
    }))
}

/// The From addresses compose offers, the user's identities (any address on the user's domains may be typed).
async fn addresses(State(app): State<Arc<App>>, Authed(session): Authed) -> Answer<Json<Vec<kubicle_api::Address>>> {
    let (settings, sent) = (app.store.settings(&session.user)?, session.mailbox.sent().await?);
    let me = Identity { address: &session.user, name: &settings.display_name, domains: &app.config.mail.domains, sent: &sent };
    Ok(Json(me.addresses()))
}

async fn settings(State(app): State<Arc<App>>, Authed(session): Authed) -> Answer<Json<Settings>> {
    Ok(Json(app.store.settings(&session.user)?))
}

/// The one place settings come in: the page size is clamped and the time zone, special folders and user themes
/// are checked here, and trusted after. Answers with the new revision; every tab hears the settings as
/// `Event::Settings`.
async fn save_settings(State(app): State<Arc<App>>, Authed(session): Authed, Json(mut settings): Json<Settings>) -> Answer<Json<u64>> {
    settings.page_size = kubicle_api::clamp_page_size(settings.page_size);
    if let Some(zone) = &settings.time_zone
        && jiff::tz::TimeZone::get(zone).is_err()
    {
        return Err(Error::status(StatusCode::BAD_REQUEST, format!("unknown time zone {zone}")));
    }
    if let Some(reason) = settings.themes.iter().find_map(|(id, css)| themes::reject_reason(id, css)) {
        return Err(Error::status(StatusCode::BAD_REQUEST, reason));
    }
    // Only a choice made now is checked: one deleted since by another mail app must not block every save, and the
    // engine already gives its role back to the server's own folder (`Shared::folders`, sync.rs). Roundcube offers
    // every folder but INBOX, one folder for several roles too.
    let (folders, stored) = (session.mailbox.folders(), app.store.settings(&session.user)?);
    let chosen = settings.special_folders.iter().filter(|(role, name)| stored.special_folders.get(role) != Some(name));
    for (role, name) in chosen.filter_map(|(role, name)| Some((role, name.as_ref()?))) {
        let refusal = if name.eq_ignore_ascii_case("INBOX") {
            format!("INBOX cannot be the {role:?} folder")
        } else if !folders.iter().any(|f| f.selectable && f.name == *name) {
            format!("no folder {name} for a special role")
        } else {
            continue;
        };
        return Err(Error::status(StatusCode::BAD_REQUEST, refusal));
    }
    let saved = app.store.save(&session.user, &settings)?;
    let revision = saved.revision;
    session.mailbox.settings_saved(saved);
    Ok(Json(revision))
}

async fn user_theme_css(State(app): State<Arc<App>>, Authed(session): Authed, Path(file): Path<String>, uri: Uri) -> Answer<Response> {
    let mut settings = app.store.settings(&session.user)?;
    let css = file.strip_suffix(".css").and_then(|id| settings.themes.remove(id));
    let css = css.ok_or_else(|| Error::status(StatusCode::NOT_FOUND, "no such theme"))?;
    let cache = if assets::versioned(uri.query()) { "private, max-age=31536000, immutable" } else { "no-cache" };
    Ok(([(header::CONTENT_TYPE, "text/css; charset=utf-8"), (header::CACHE_CONTROL, cache)], css).into_response())
}

/// Ends kubicle's session; the client then opens `Session::logout_url`, which ends the Mailu session too.
async fn logout(State(app): State<Arc<App>>, Authed(session): Authed) -> Response {
    app.sessions.remove(&session.id); // first: a request dropped during the purge below still signed out
    // Roundcube's "Clear Trash on logout": SEARCH BEFORE that many days ago, a date in the server's own zone.
    let before = match app.store.settings(&session.user).map(|s| s.logout_purge) {
        Ok(LogoutPurge::All) => Some(None),
        Ok(LogoutPurge::OlderThan(days)) => days_ago(&app.config.time_zone, days).map(Some), // no date clears nothing, not all
        Ok(LogoutPurge::Never) | Err(_) => None,
    };
    if let Some(before) = before
        && let Err(e) = session.mailbox.run(move |e| Box::pin(e.purge_trash(before))).await
    {
        warn!(user = session.user, error = %e, "signed out, but Trash was not cleared");
    }
    ([(header::SET_COOKIE, session::clear_cookie())], StatusCode::NO_CONTENT).into_response()
}

/// The mailbox's events as server-sent events. The stream subscribes first and then opens with `Resync`, so no
/// change falls between the two; a listener that fell behind gets `Resync` again. `X-Accel-Buffering: no` stops
/// Mailu front's nginx from holding events back in its proxy buffer.
async fn events(Authed(session): Authed) -> Answer<Response> {
    let events = session.mailbox.events();
    let first = Some(resync(&session));
    // Weak, so the stream does not keep a signed-out or swept session alive, and it checks the token too: a session
    // a reconnect found refused, or signed out, notices within a keepalive, says `SessionExpired` and ends.
    let session = Some(Arc::downgrade(&session));
    let stream = futures_util::stream::unfold((events, session, first), async |(mut events, session, first)| {
        let weak = session?;
        let event = if let Some(event) = first {
            event
        } else {
            let next = tokio::time::timeout(Duration::from_secs(KEEPALIVE_SECONDS), events.recv()).await;
            let Some(live) = weak.upgrade().filter(|s| s.mailbox.accepts(&s.token)) else {
                let sse = SseEvent::default().json_data(&Event::SessionExpired).expect("an Event always serializes");
                return Some((Ok::<_, Infallible>(sse), (events, None, None)));
            };
            match next {
                Err(_) => Event::Keepalive,
                Ok(Ok(event)) => event,
                Ok(Err(RecvError::Lagged(_))) => resync(&live),
                Ok(Err(RecvError::Closed)) => return None,
            }
        };
        let sse = SseEvent::default().json_data(&event).expect("an Event always serializes");
        Some((Ok::<_, Infallible>(sse), (events, Some(weak), None)))
    });
    Ok(([("x-accel-buffering", "no")], Sse::new(stream)).into_response())
}

/// The day `days` before today in `zone`, in days since 1970-01-01; `None` past the calendar's range.
fn days_ago(zone: &jiff::tz::TimeZone, days: u16) -> Option<i64> {
    let day = jiff::Zoned::now().with_time_zone(zone.clone()).date().checked_sub(jiff::Span::new().days(i64::from(days))).ok()?;
    Some(i64::from(day.since(jiff::civil::date(1970, 1, 1)).ok()?.get_days()))
}

fn resync(session: &Session) -> Event { Event::Resync(session.mailbox.folders(), session.mailbox.quota()) }

/// `api/list`'s query: a `ListQuery`, its search as JSON, as a query string nests no values.
#[derive(Deserialize)]
struct ListAsked {
    folder: String,
    #[serde(default)]
    page: u32,
    size: u32,
    sort: SortColumn,
    order: SortOrder,
    threads: bool,
    /// The page that holds this message, in place of `page`.
    around: Option<u32>,
    /// `ListQuery::search`, as JSON.
    search: Option<String>,
}

/// `FolderChanged` brings what changed since; a folder not in the folder list answers an empty page. A search
/// over several folders stops before its next folder once this request ends, as the tab's abort of it does.
async fn list(Authed(session): Authed, Query(asked): Query<ListAsked>) -> Answer<Json<ListPage>> {
    let bad = |_| Error::status(StatusCode::BAD_REQUEST, "search must be a Search as JSON");
    let search = asked.search.as_deref().map(serde_json::from_str::<Search>).transpose().map_err(bad)?;
    let ListAsked { folder, page, size, sort, order, threads, around, .. } = asked;
    let query = ListQuery { folder, page, size: kubicle_api::clamp_page_size(size), sort, order, threads, search };
    // Held until this request ends, which a tab's abort of the query a newer one replaced does: its search stops.
    let waiting = Arc::new(());
    let caller = Arc::downgrade(&waiting);
    Ok(Json(session.mailbox.list(query, around, move || caller.strong_count() > 0).await?))
}

async fn view(Authed(session): Authed, Json(viewing): Json<Viewing>) -> Answer {
    session.mailbox.run(move |e| Box::pin(e.view(viewing.folder))).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Clone, Deserialize)]
struct MessageRef {
    folder: String,
    uid: u32,
    #[serde(default)]
    images: bool,
    text: Option<bool>,
    /// `api/source` only: the header block alone.
    #[serde(default)]
    headers: bool,
    /// `api/print` only: the zone the user sees dates in (`zone`).
    tz: Option<String>,
}

/// The message as the reader and print show it: remote images when allowed in this session or by the Remote
/// images setting, and the body chosen in this session, else the one `prefer_html` picks.
async fn rendered(app: &Arc<App>, session: &Arc<Session>, at: &MessageRef) -> Answer<Message> {
    let (raw, arrival, validity) = session.mailbox.message(&at.folder, at.uid).await?;
    let key = Renders::key(&session.user, &at.folder, validity, at.uid);
    let chosen = session.chosen(&at.folder, validity, at.uid, at.images, at.text);
    let (app, session, at) = (Arc::clone(app), Arc::clone(session), at.clone());
    Arc::clone(&app)
        .renders
        .run(key, raw, move |mail| {
            let links = links(&app, &session, &at.folder, at.uid, validity);
            Ok(render::message(mail, &at.folder, at.uid, arrival, &links, shown(&app, &session, chosen, mail)?))
        })
        .await
}

/// `chosen`, this session's choice for the message (`Session::chosen`), over the user's settings.
fn shown(app: &App, session: &Session, chosen: Chosen, mail: &render::Mail) -> Answer<render::Shown> {
    let settings = app.store.settings(&session.user)?;
    let me = Identity { address: &session.user, name: "", domains: &app.config.mail.domains, sent: &[] };
    let authenticity = render::authenticity(mail, app.config.mail.authserv_id.as_deref(), |email| me.owns(email));
    let trusted = |from: &kubicle_api::Address| settings.trusted_senders.iter().any(|s| s.eq_ignore_ascii_case(&from.email));
    let remote_images = chosen.images
        || match settings.remote_images {
            RemoteImages::Never => false,
            RemoteImages::Trusted =>
                authenticity.as_ref().is_some_and(|a| a.from_verified) && render::addresses(mail.from()).first().is_some_and(trusted),
            RemoteImages::Always => true,
        };
    Ok(render::Shown { text: chosen.text.unwrap_or(!settings.prefer_html), remote_images, authenticity })
}

/// The signed URLs of one message's parts and images.
fn links(app: &App, session: &Session, folder: &str, uid: u32, validity: u32) -> sign::Links {
    app.signer.links(&format!("{}{}", app.config.public_origin, app.config.base_path), &session.handle, folder, uid, validity)
}

async fn message(State(app): State<Arc<App>>, Authed(session): Authed, Query(at): Query<MessageRef>) -> Answer<Json<Message>> {
    Ok(Json(rendered(&app, &session, &at).await?))
}

async fn source(Authed(session): Authed, Query(at): Query<MessageRef>) -> Answer<Response> {
    let (raw, ..) = session.mailbox.message(&at.folder, at.uid).await?;
    let end = if at.headers { render::header_end(&raw) } else { raw.len() };
    Ok(render::source_response(&raw, end))
}

async fn print(State(app): State<Arc<App>>, Authed(session): Authed, Query(at): Query<MessageRef>) -> Answer<Response> {
    let message = rendered(&app, &session, &at).await?;
    let prefix = format!("{}{}", app.config.public_origin, app.config.base_path);
    Ok(render::print_response(&message, &zone(&app, at.tz.as_deref()), &prefix))
}

/// The zone a request names for its dates, the server's own when it names none or one that does not exist. Sent mail,
/// drafts and Resent-Date take the zone chosen in Settings, else the server's, as Roundcube's `user_date` dates them.
fn zone(app: &App, tz: Option<&str>) -> jiff::tz::TimeZone {
    tz.and_then(|z| jiff::tz::TimeZone::get(z).ok()).unwrap_or_else(|| app.config.time_zone.clone())
}

async fn change(State(app): State<Arc<App>>, Authed(session): Authed, Json(change): Json<Change>) -> Answer {
    let settings = app.store.settings(&session.user)?;
    let tz = if let Change::Archive { zone, .. } = &change { Some(zone.as_str()) } else { None };
    let zone = zone(&app, tz);
    let renamed = session.mailbox.run(move |e| Box::pin(e.change(change, settings, zone))).await?;
    // The folder's sort, threads, search fields and collapse go with it, as do those of the folders below it.
    if let Some((from, to)) = renamed {
        let delimiter = session.mailbox.folders().into_iter().find(|f| f.name == to).and_then(|f| f.delimiter);
        // The folder is renamed already: a refused save (the settings' size limit) keeps the old names' preferences.
        match app.store.rename_folder(&session.user, &from, &to, delimiter) {
            Ok(Some(settings)) => session.mailbox.settings_saved(settings),
            Ok(None) => {}
            Err(e) => warn!(user = session.user, error = %e, "renamed, but the folder's preferences kept its old name"),
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AutocompleteQuery {
    q: String,
}

async fn autocomplete(
    State(app): State<Arc<App>>, Authed(session): Authed, Query(query): Query<AutocompleteQuery>,
) -> Answer<Json<Vec<kubicle_api::Address>>> {
    let trusted = app.store.settings(&session.user)?.trusted_senders;
    Ok(Json(compose::suggestions(&session.mailbox.sent().await?, &query.q, &trusted, SUGGESTIONS)))
}

async fn prefill(State(app): State<Arc<App>>, Authed(session): Authed, Json(query): Json<ComposeStart>) -> Answer<Json<Prefill>> {
    let one = match query.messages.as_slice() {
        [] => true,
        [(_, Messages::Uids(uids))] => uids.len() == 1,
        _ => false,
    };
    if query.mode != ComposeMode::ForwardAttachment && !one {
        return Err(Error::status(StatusCode::BAD_REQUEST, "only Forward as attachment takes several messages"));
    }
    let settings = app.store.settings(&session.user)?;
    if query.mode == ComposeMode::ForwardAttachment {
        let sent = session.mailbox.sent().await?;
        let me = Identity { address: &session.user, name: &settings.display_name, domains: &app.config.mail.domains, sent: &sent };
        return Ok(Json(forward_attachments(&app, &session, &me, &settings, query).await?));
    }
    let source = match query.messages.into_iter().next() {
        Some((folder, Messages::Uids(uids))) => uids.first().map(|&uid| SourceRef { folder, uid, mode: query.mode }),
        _ => None,
    };
    // New mail starts from the account's own address: only an answer or a forward picks an identity from Sent.
    let (sent, raw) = match source.as_ref().filter(|s| s.mode != ComposeMode::New) {
        // One job, the message first: it is usually in the folder on screen, which a second job, or a read after
        // Sent's, would wait to SELECT again (up to 200 ms for a large folder).
        Some(source) => {
            let (folder, uid) = (source.folder.clone(), source.uid);
            let (raw, sent, validity) = session
                .mailbox
                .run(move |e| Box::pin(async move { Ok((e.fetch(folder.clone(), uid).await?, e.sent().await?, e.validity(&folder))) }))
                .await?;
            (sent, Some((raw.ok_or_else(not_found)?.0, validity)))
        }
        None if query.mode == ComposeMode::New => (Arc::default(), None),
        None => (session.mailbox.sent().await?, None),
    };
    let validity = raw.as_ref().map_or(0, |(_, v)| *v);
    let html =
        source.as_ref().is_some_and(|s| !session.chosen(&s.folder, validity, s.uid, false, None).text.unwrap_or(!settings.prefer_html));
    let key = source.as_ref().map(|s| Renders::key(&session.user, &s.folder, validity, s.uid));
    let (zone, folders) = (zone(&app, Some(&query.zone)), session.mailbox.folders());
    let work = {
        let app = Arc::clone(&app);
        move |mail: Option<&render::Mail>| {
            let me = Identity { address: &session.user, name: &settings.display_name, domains: &app.config.mail.domains, sent: &sent };
            compose::prefill(&me, source.zip(mail), session.mailbox.uploads(), &zone, &settings, &folders, html)
        }
    };
    // A message is parsed and quoted as the reader renders it: off the async workers, in a turn, under a marker.
    let draft = match key.zip(raw) {
        Some((key, (raw, _))) => app.renders.run(key, raw, move |mail| work(Some(mail))).await?,
        None => work(None)?,
    };
    Ok(Json(Prefill { draft, notice: None, forwards_bcc: false }))
}

/// Forward as attachment, as Roundcube's `write_forward_attachments`: the messages in the order asked, Select all's
/// in the folder's saved sort; past the mail server's size limit, counting 10 KB for the text and each message whole,
/// a message is left out and the notice counts it. Sizes come with the rows, so one left out is never fetched.
async fn forward_attachments(app: &App, session: &Session, me: &Identity<'_>, settings: &Settings, query: ComposeStart) -> Answer<Prefill> {
    bounded(&session.mailbox.folders(), query.messages.iter().map(|(folder, messages)| (folder.as_str(), messages)))?;
    let mut asked: Vec<(String, Row)> = Vec::new();
    for (folder, messages) in query.messages {
        let rows = match messages {
            Messages::Uids(uids) => session.mailbox.rows(&folder, &uids).await?,
            Messages::All => {
                let FolderPrefs { sort, order, .. } = settings.folders.get(&folder).copied().unwrap_or_default();
                let every = ListQuery { folder: folder.clone(), page: 0, size: u32::MAX, sort, order, threads: false, search: None };
                session.mailbox.list(every, None, || true).await?.rows.into_iter().map(|hit| hit.row).collect()
            }
        };
        asked.extend(rows.into_iter().map(|row| (folder.clone(), row)));
    }
    let subject = asked.iter().map(|(_, row)| row.subject.as_str()).find(|s| !s.is_empty()).unwrap_or_default().to_owned();
    let limit = app.config.mail.size_limit();
    let (mut total, mut left_out, mut forwards_bcc) = (kubicle_api::TEXT_ALLOWANCE, 0, false);
    let mut kept = Vec::with_capacity(asked.len());
    for (folder, row) in asked {
        if limit.is_some_and(|limit| total + u64::from(row.size) > limit) {
            left_out += 1;
            continue;
        }
        total += u64::from(row.size);
        kept.push((folder, row));
    }
    // Dropped unfinished, by an error or a closed request, it removes the uploads it stored.
    let mut forward = compose::ForwardAttachments::new(&subject, session.mailbox.uploads());
    // A folder's messages a few MB at a time (`BATCH`), each dropped once it is an upload.
    for run in kept.chunk_by(|a, b| a.0 == b.0) {
        let folder = &run[0].0;
        for batch in batches(run, |(_, row)| row.size) {
            let raws = session.mailbox.messages(folder, batch.iter().map(|(_, row)| row.uid).collect()).await?;
            for ((_, row), raw) in batch.iter().zip(raws) {
                let Some(raw) = raw else { continue }; // gone meanwhile, as Roundcube's forward skips it
                let mail = render::parse_headers(&raw)?;
                forwards_bcc |= mail.header_raw("Bcc").is_some() || mail.header_raw("Resent-Bcc").is_some();
                let source = SourceRef { uid: row.uid, mode: ComposeMode::ForwardAttachment, folder: folder.clone() };
                forward.add(me, source, &mail, &raw)?;
            }
        }
    }
    let draft = forward.finish(me, settings);
    let notice = limit.filter(|_| left_out > 0).map(|limit| compose::left_out_notice(left_out, limit));
    Ok(Prefill { draft, notice, forwards_bcc })
}

/// Stores the draft's copy in place of its previous one; answers with the new copy.
async fn save_draft(State(app): State<Arc<App>>, Authed(session): Authed, Json(draft): Json<Draft>) -> Answer<Json<DraftCopy>> {
    let settings = app.store.settings(&session.user)?;
    let tz = zone(&app, settings.time_zone.as_deref());
    let turn = files_turn(&session, &draft).await;
    let message = compose::build(&draft, session.mailbox.uploads(), &tz, &settings.identities, false, app.config.mail.size_limit())?.copy;
    // The turn goes with the copy, which the job holds even after the browser left.
    let saved = session.mailbox.run(move |e| {
        Box::pin(async move {
            let _turn = turn;
            e.save_draft(message, draft.copy).await
        })
    });
    Ok(Json(saved.await?))
}

/// With files, a turn to hold them in memory (`Mailbox::buffer_turn`), the uploads' and imports' too: building
/// copies each file a few times. A draft of text alone waits for no upload.
async fn files_turn(session: &Session, draft: &Draft) -> Option<OwnedSemaphorePermit> {
    if draft.attachments.is_empty() { None } else { Some(session.mailbox.buffer_turn().await) }
}

/// Sends and answers once submission took the mail; `submit` files the copy, flags the source and drops the draft.
async fn send(State(app): State<Arc<App>>, Authed(session): Authed, Json(draft): Json<Draft>) -> Answer {
    let settings = app.store.settings(&session.user)?;
    let tz = zone(&app, settings.time_zone.as_deref());
    let turn = files_turn(&session, &draft).await;
    let built = compose::build(&draft, session.mailbox.uploads(), &tz, &settings.identities, true, app.config.mail.size_limit())?;
    submit(&app, &session, built, turn, draft.sent_copy, draft.sources, draft.copy).await?;
    let uploads: Vec<&str> = draft.attachments.iter().map(|a| a.id.as_str()).collect();
    if let Err(e) = session.mailbox.uploads().remove(&uploads) {
        warn!(error = %e, "sent, but the uploads stay until pruned");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Redirect: resends a message unchanged and answers once submission took it; its copy is filed as `send` files one.
async fn redirect(State(app): State<Arc<App>>, Authed(session): Authed, Json(redirect): Json<kubicle_api::Redirect>) -> Answer {
    let turn = session.mailbox.buffer_turn().await; // the whole message, up to the mail server's limit
    let (raw, ..) = session.mailbox.message(&redirect.folder, redirect.uid).await?;
    let tz = zone(&app, app.store.settings(&session.user)?.time_zone.as_deref());
    let built = compose::redirect(&raw, &redirect, &tz)?;
    submit(&app, &session, built, Some(turn), redirect.sent_copy, Vec::new(), None).await
}

/// Hands `built` to submission and answers once it took the mail. The copy filed as `sent_copy` says, the flags on
/// `sources` and the removal of the draft's copy `draft` follow in the engine's queue, which may be busy for
/// seconds; a failure there reaches the user as `Event::Notice`. `turn` goes with the copy, which outlives the
/// request. A sent copy aimed at a folder that does not exist, or at Sent where no folder has that role, is refused
/// before anything goes out.
async fn submit(
    app: &App, session: &Session, built: compose::Built, turn: Option<OwnedSemaphorePermit>, sent_copy: SentCopy, sources: Vec<SourceRef>,
    draft: Option<DraftCopy>,
) -> Answer {
    let folders = session.mailbox.folders();
    match &sent_copy {
        SentCopy::Folder(folder) if !folders.iter().any(|f| f.selectable && f.name == *folder) =>
            return Err(Error::status(StatusCode::BAD_REQUEST, "no such folder for the sent copy")),
        SentCopy::Sent if !folders.iter().any(|f| f.roles.contains(&FolderRole::Sent)) => {
            let refusal = "there is no Sent folder: choose where to save the sent message, or Don't save";
            return Err(Error::status(StatusCode::BAD_REQUEST, refusal));
        }
        _ => {}
    }
    let me = Identity { address: &session.user, name: "", domains: &app.config.mail.domains, sent: &[] };
    compose::send(&app.config.mail, &me, &session.token, &built).await?;
    let mailbox = session.mailbox.clone();
    tokio::spawn(async move {
        if let Err(e) = mailbox.run(move |e| Box::pin(e.file_sent(built.copy, sent_copy, sources, draft))).await {
            warn!(error = %e, "sent, but filing the copy, the flags or the draft failed");
            mailbox.notice(format!("Sent, but the copy in Sent, the flags or the draft were not updated: {e}"));
        }
        drop(turn);
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn download(Authed(session): Authed, Query(at): Query<MessageRef>) -> Answer<Response> {
    let (raw, ..) = session.mailbox.message(&at.folder, at.uid).await?;
    Ok(render::eml_response(&render::parse_headers(&raw)?, at.uid)) // the subject alone, so a mail too deep to show downloads
}

async fn sizes(Authed(session): Authed) -> Answer<Json<FolderSizes>> { Ok(Json(session.mailbox.sizes().await?)) }

#[derive(Deserialize)]
struct ZipAsked {
    mode: ZipMode,
    sets: String,
}

/// Roundcube's zipdownload: `kubicle_api::ZipMode`'s archive of a selection, streamed from IMAP a few MB at a time
/// (`BATCH`), so memory stays flat for any folder size. Named after the folder, or `messages.zip` across folders.
async fn zip(State(app): State<Arc<App>>, Authed(session): Authed, Query(asked): Query<ZipAsked>) -> Answer<Response> {
    let bad = || Error::status(StatusCode::BAD_REQUEST, "sets must be a JSON list of [folder, UID set]");
    let pairs: Vec<(String, String)> = serde_json::from_str(&asked.sets).map_err(|_| bad())?;
    let folders = session.mailbox.folders();
    // Each folder once, as the client sends it, so no message is archived twice; `bounded` caps the messages.
    let mut sets: Vec<(String, Option<char>, Messages)> = Vec::with_capacity(pairs.len());
    for (folder, uids) in pairs {
        let gone = || Error::status(StatusCode::NOT_FOUND, "no such folder");
        let delimiter = folders.iter().find(|f| f.name == folder).ok_or_else(gone)?.delimiter;
        let messages = kubicle_api::parse_uid_set(&uids).ok_or_else(bad)?;
        if sets.iter().any(|(f, ..)| *f == folder) {
            return Err(bad());
        }
        sets.push((folder, delimiter, messages));
    }
    bounded(&folders, sets.iter().map(|(folder, _, messages)| (folder.as_str(), messages)))?;
    let name = if let [(folder, ..)] = sets.as_slice() { folder.clone() } else { "messages".to_owned() };
    let (send, receive) = mpsc::channel(4);
    let now = jiff::Zoned::now().with_time_zone(app.config.time_zone.clone());
    tokio::spawn(write_zip(session.mailbox.clone(), sets, asked.mode, name.clone(), now, send));
    let stream = futures_util::stream::unfold(receive, async |mut r| r.recv().await.map(|chunk| (chunk, r)));
    Ok(render::served(Body::from_stream(stream), "application/zip", Some(&format!("{}.zip", render::safe_filename(&name)))))
}

/// The archive's bytes in order, into `send`; a failure ends the stream with it, which the browser shows as a broken
/// download. A message deleted meanwhile is left out, as Roundcube's `get_raw_body` writes nothing for it.
async fn write_zip(
    mailbox: Mailbox, sets: Vec<(String, Option<char>, Messages)>, mode: ZipMode, name: String, now: jiff::Zoned,
    send: mpsc::Sender<Result<Bytes, Error>>,
) {
    let several = sets.len() > 1;
    let pending = Pending::default();
    let mut zip = zip::ZipWriter::new_stream(pending.clone());
    // Each file stored as it is, as mail barely compresses, dated when the archive was made.
    let [month, day, hour, minute, second] = [now.month(), now.day(), now.hour(), now.minute(), now.second()].map(i8::unsigned_abs);
    let year = u16::try_from(now.year()).unwrap_or(1980).clamp(1980, 2107);
    let time = zip::DateTime::from_date_and_time(year, month, day, hour, minute, second).unwrap_or_default();
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored).last_modified_time(time);
    let written = async {
        let put = async || send.send(Ok(Bytes::from(pending.take()))).await.is_ok(); // false: the browser left
        if mode == ZipMode::Mbox {
            // The one mailbox file may pass 4 GiB, which its sizes must say before its first byte.
            zip.start_file(format!("{}.mbox", render::safe_filename(&name)), stored.large_file(true)).map_err(std::io::Error::from)?;
        }
        let (mut first, mut taken) = (true, HashSet::new());
        for (folder, delimiter, messages) in sets {
            let headers = mailbox.headers(folder.clone(), messages).await?;
            for batch in batches(&headers, |h| h.size) {
                let raws = mailbox.messages(&folder, batch.iter().map(|h| h.uid).collect()).await?;
                for (message, raw) in batch.iter().zip(raws) {
                    let Some(raw) = raw else { continue };
                    match mode {
                        ZipMode::Maildir => {
                            let mut file = maildir_name(message, &folder, delimiter, several);
                            // The writer refuses a name twice, as two folders whose names clean to one path give it.
                            while !taken.insert(file.clone()) {
                                file.insert(file.len() - ".eml".len(), '_');
                            }
                            zip.start_file(file, stored).map_err(std::io::Error::from)?;
                            zip.write_all(&raw)?;
                        }
                        ZipMode::Mbox => zip.write_all(&mbox_entry(message, &raw, std::mem::take(&mut first)))?,
                    }
                    if !put().await {
                        return Ok(());
                    }
                }
            }
        }
        zip.finish().map_err(std::io::Error::from)?;
        put().await;
        Ok::<_, Error>(())
    };
    if let Err(e) = written.await {
        warn!(error = %e, "a download of several messages broke off");
        let _ = send.send(Err(e)).await;
    }
}

/// What one zip or Forward as attachment may take: `MAX_UIDS` messages, Select all counted at its folder's size, and
/// Select all's folder in no other entry. Each lists its whole selection first, so one folder's Select all asked 40
/// times in 778 bytes held 265,000 rows. A folder may repeat with UIDs: a selection across folders runs in order.
fn bounded<'a>(folders: &[Folder], sets: impl Iterator<Item = (&'a str, &'a Messages)> + Clone) -> Answer<()> {
    let size = |name: &str| folders.iter().find(|f| f.name == name).map_or(0, |f| f.total as usize);
    let held: usize =
        sets.clone().map(|(folder, messages)| if let Messages::Uids(uids) = messages { uids.len() } else { size(folder) }).sum();
    let all: HashSet<&str> = sets.clone().filter(|(_, m)| matches!(m, Messages::All)).map(|(folder, _)| folder).collect();
    if held > kubicle_api::MAX_UIDS || sets.filter(|(folder, _)| all.contains(folder)).count() > all.len() {
        return Err(Error::status(StatusCode::BAD_REQUEST, "a selection takes at most 100,000 messages, and Select all's folder once"));
    }
    Ok(())
}

/// The most mail a zip or a forward holds at once, fetched in one engine job: a message alone if larger.
const BATCH: u64 = 4 << 20;

/// `messages` in runs of up to `BATCH` bytes of mail, by each one's `size`.
fn batches<T>(messages: &[T], size: impl Fn(&T) -> u32) -> Vec<&[T]> {
    let (mut runs, mut start, mut bytes) = (Vec::new(), 0, 0);
    for (i, message) in messages.iter().enumerate() {
        if i > start && bytes + u64::from(size(message)) > BATCH {
            runs.push(&messages[start..i]);
            (start, bytes) = (i, 0);
        }
        bytes += u64::from(size(message));
    }
    if start < messages.len() {
        runs.push(&messages[start..]);
    }
    runs
}

/// What the zip writer wrote since the last `take`, sent on after each message so the archive streams.
#[derive(Clone, Default)]
struct Pending(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Pending {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap_or_else(PoisonError::into_inner).write(bytes) }

    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

impl Pending {
    fn take(&self) -> Vec<u8> { std::mem::take(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner)) }
}

/// Roundcube's name for a message in a Maildir zip: the UID and the subject's first 16 characters, `:` dropped and
/// `/` as `-`, under the folder's path when the selection spans folders.
fn maildir_name(message: &Fetched, folder: &str, delimiter: Option<char>, several: bool) -> String {
    static BREAKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\t\n\r\x00\x0B]+(?-u:\s)*").expect("a valid pattern"));
    let subject: String = message.subject.chars().take(16).collect();
    let subject = BREAKS.replace_all(&subject, " ");
    let subject = render::safe_filename(&subject.trim_matches([' ', '.', '/', '_']).replace(':', "").replace('/', "-"));
    let path = if several {
        let parts = delimiter.map_or_else(|| vec![folder], |d| folder.split(d).collect());
        let parts: Vec<String> = parts.into_iter().map(render::safe_filename).filter(|p| !p.is_empty()).collect();
        format!("{}/", parts.join("/"))
    } else {
        String::new()
    };
    let subject = if subject.is_empty() { String::new() } else { format!(" {subject}") };
    format!("{path}{}{subject}.eml", message.uid)
}

/// One message of an mbox (RFC 4155), as Roundcube's zipdownload writes it: a `From ` line with the sender's address
/// and the arrival time in UTC, the message with each `>*From ` line quoted once more (mboxrd), and a line end at
/// its end; an empty line before every message but the first.
fn mbox_entry(message: &Fetched, raw: &[u8], first: bool) -> Vec<u8> {
    let sender: String =
        message.from.as_ref().map_or("", |a| a.email.as_str()).chars().map(|c| if c.is_whitespace() { '-' } else { c }).collect();
    let sender = if sender.is_empty() { "MAILER-DAEMON".to_owned() } else { sender };
    let date = jiff::Timestamp::from_second(message.arrival).map(|t| t.strftime("%a %b %d %H:%M:%S %Y").to_string());
    let mut out = if first { Vec::new() } else { b"\r\n".to_vec() };
    out.extend_from_slice(format!("From {sender} {}\r\n", date.unwrap_or_default()).as_bytes());
    for line in raw.split_inclusive(|&b| b == b'\n') {
        if line[line.iter().take_while(|&&b| b == b'>').count()..].starts_with(b"From ") {
            out.push(b'>');
        }
        out.extend_from_slice(line);
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[derive(Deserialize)]
struct ImportQuery {
    folder: String,
}

/// Import: the messages of every `file` field, appended to the folder as they are.
async fn import(Authed(session): Authed, Query(query): Query<ImportQuery>, mut form: Multipart) -> Answer<Json<u32>> {
    let bad_import = || Error::status(StatusCode::BAD_REQUEST, "the import is not a multipart form with file fields");
    let turn = session.mailbox.buffer_turn().await;
    let mut files = Vec::new();
    while let Some(field) = form.next_field().await.map_err(|_| bad_import())? {
        if field.name() == Some("file") {
            files.push(field.bytes().await.map_err(|_| bad_import())?);
        }
    }
    // Each message is split out of its file just before its APPEND: all of them first would be a second copy of the
    // files, in small allocations the allocator keeps after the import (31 MB after a 30 MB mbox). The turn goes with
    // the files, which the job holds even after the browser left.
    let stored = session.mailbox.run(move |e| {
        Box::pin(async move {
            let stored = e.import(query.folder, files.iter().flat_map(|f| messages_in(f)).map(|m| render::imported(&m))).await;
            drop((files, turn));
            stored
        })
    });
    let stored = stored.await?;
    if stored == 0 {
        return Err(Error::status(StatusCode::BAD_REQUEST, "no message in the files: import takes .eml and mbox files"));
    }
    Ok(Json(stored))
}

/// The messages in an imported file, told apart as Roundcube's import does: a file whose first line is an mbox
/// `From ` line holds one message after each such line, `>From ` lines unquoted (mail-parser's mbox reader); one
/// whose first line is a header is one message; anything else holds none. Lines end in CRLF, as IMAP wants.
fn messages_in(file: &[u8]) -> Box<dyn Iterator<Item = Vec<u8>> + Send + '_> {
    let blank: usize = file.split_inclusive(|&b| b == b'\n').take_while(|l| l.trim_ascii().is_empty()).map(<[u8]>::len).sum();
    let file = &file[blank..];
    let first = file.split(|&b| b == b'\n').next().unwrap_or_default();
    let header = first.iter().position(|&b| b == b':').is_some_and(|colon| {
        let (name, rest) = (&first[..colon], &first[colon + 1..]);
        let name_ok = !name.is_empty() && name.iter().all(|b| b.is_ascii_alphabetic() || matches!(b, b'-' | b'_'));
        name_ok && rest.first().is_some_and(u8::is_ascii_whitespace) && !rest.trim_ascii().is_empty()
    });
    let messages: Box<dyn Iterator<Item = Vec<u8>> + Send> = if file.starts_with(b"From ") {
        Box::new(MessageIterator::new(file).filter_map(Result::ok).map(|m| crlf(&m.unwrap_contents())))
    } else if header {
        Box::new(std::iter::once(crlf(file)))
    } else {
        Box::new(std::iter::empty())
    };
    Box::new(messages.filter(|m| !m.is_empty()))
}

fn crlf(message: &[u8]) -> Vec<u8> {
    let lines: Vec<&[u8]> = message.split(|&b| b == b'\n').map(|line| line.strip_suffix(b"\r").unwrap_or(line)).collect();
    lines.join(&b"\r\n"[..]).trim_ascii_end().to_vec()
}

/// One file as the multipart field `file`, kept for the draft until it is sent; refused when it alone could not go
/// out under the mail server's size limit (Compose checks the whole message before it uploads).
async fn upload(State(app): State<Arc<App>>, Authed(session): Authed, mut form: Multipart) -> Answer<Json<DraftAttachment>> {
    let bad_upload = || Error::status(StatusCode::BAD_REQUEST, "the upload is not a multipart form with a file field");
    let _turn = session.mailbox.buffer_turn().await;
    while let Some(field) = form.next_field().await.map_err(|_| bad_upload())? {
        if field.name() == Some("file") {
            let filename = field.file_name().unwrap_or_default().to_owned();
            let content_type = field.content_type().unwrap_or("application/octet-stream").to_owned();
            let data = field.bytes().await.map_err(|_| bad_upload())?;
            compose::check_upload(&app.config.mail, data.len() as u64, &content_type)?;
            return Ok(Json(compose::upload(session.mailbox.uploads(), &filename, &content_type, &data)?));
        }
    }
    Err(bad_upload())
}

/// Frees uploads, by id, that Compose no longer needs: a file removed, a draft discarded or saved to Drafts, whose copy
/// holds its files. Each user's uploads are a file of their own, so no other user's are reachable.
async fn remove_uploads(Authed(session): Authed, Json(ids): Json<Vec<String>>) -> Answer {
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    session.mailbox.uploads().remove(&ids)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UploadQuery {
    id: String,
    /// The name the draft gives it now, which Rename may have changed since the upload.
    name: String,
    /// An image shown in a tab of its own instead of a download, as the reader's Open.
    #[serde(default)]
    open: bool,
}

/// Compose's Open and Download of an attached file: only the user's own uploads, under the same headers as a mail's
/// parts, and only an image type opens, as an image (docs/design.md, Security).
async fn stored_upload(Authed(session): Authed, Query(query): Query<UploadQuery>) -> Answer<Response> {
    let _turn = session.mailbox.buffer_turn().await; // the file is read whole, up to 36 MiB
    let (stored, data) =
        session.mailbox.uploads().get(&query.id)?.ok_or_else(|| Error::status(StatusCode::NOT_FOUND, "no such attachment"))?;
    if let Some(image) = kubicle_api::image_type(&stored.content_type).filter(|_| query.open) {
        return Ok(render::served(data, image, None));
    }
    let name = Some(render::safe_filename(&query.name)).filter(|n| !n.is_empty()).unwrap_or(stored.filename);
    Ok(render::served(data, "application/octet-stream", Some(&name)))
}

/// A signed `inline`, `download` or `view` URL. A page shows its images as the message does in this session.
async fn part(State(app): State<Arc<App>>, Signed(session, query): Signed<PartQuery>, route: PartRoute) -> Answer<Response> {
    // An inline image reads its own part: a view of 200 fetched and parsed the whole message 200 times. An error or
    // a part that does not match falls back to the whole message, which reports what went wrong.
    if let Some(section) = query.section()
        && let Some((mime, body, validity)) = session.mailbox.section(&query.f, query.u, &section.number).await.ok().flatten()
    {
        if validity != query.v {
            return Err(not_found());
        }
        if let Some(response) = render::section_response(&mime, &body, section.size) {
            return Ok(response);
        }
    }
    let (raw, _, validity) = session.mailbox.message(&query.f, query.u).await?;
    if validity != query.v {
        return Err(not_found()); // the folder was recreated since: its UID names another message now
    }
    let key = Renders::key(&session.user, &query.f, validity, query.u);
    Arc::clone(&app)
        .renders
        .run(key, raw, move |mail| {
            // Only a page draws mail, whose remote images and HTML or text follow the message's own choice.
            let shown = if route == PartRoute::View {
                shown(&app, &session, session.chosen(&query.f, validity, query.u, false, None), mail)?
            } else {
                render::Shown::default()
            };
            render::part_response(mail, &query.p, route, &links(&app, &session, &query.f, query.u, validity), &shown)
                .ok_or_else(|| Error::status(StatusCode::NOT_FOUND, "no such part"))
        })
        .await
}

async fn image(State(app): State<Arc<App>>, Signed(_, query): Signed<ImageQuery>) -> Answer<Response> { app.proxy.fetch(&query.url).await }
