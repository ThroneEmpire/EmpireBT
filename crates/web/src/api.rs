//! The web UI's HTTP interface: its JSON API, live updates, and the checks
//! every request passes.
//!
//! Every request: when `web.allowed_hosts` lists names, its `Host` must be
//! one of them, an address, or `localhost`; and every response carries
//! strict security headers. The list is optional: another site pointing
//! its name at us (DNS rebinding) gets no further than the sign-in page,
//! because session cookies belong to the name the user signed in on, and
//! signing in is slowed for guessers. API requests other than signing in need a
//! session cookie or an API token. Requests that change something and come
//! with a cookie must also carry the `X-EmpireBT` header, which pages on
//! other sites cannot add without our consent, and an `Origin` (when sent)
//! matching our own. A browser session cannot write outside the download
//! folder: torrents are added by link, URL, or upload, never by server
//! path, and settings for storage, the network interface, and the web UI
//! itself can only be changed outside the web UI.
//!
//! qBittorrent's Web API, for programs made for qBittorrent, is under
//! `/api/v2/` ([`crate::qbittorrent`]), with qBittorrent's own rules for
//! signing in and cross-site requests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use empirebt_app::{
    AddRequest, AddSource, Command, ContentLayout, DownloadDir, Handle, InfoHash, Level,
    SeedStatus, Snapshot, TorrentView,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;

use crate::assets;
use crate::auth::{self, AuthFile, Guard, LoginLinks, Sessions};
use crate::server::ClientAddr;

/// Largest request body, except `.torrent` uploads.
const MAX_BODY: usize = 64 * 1024;
/// Largest `.torrent` upload.
pub(crate) const MAX_UPLOAD: usize = 16 * 1024 * 1024;
/// Live update streams open at once.
const MAX_STREAMS: usize = 16;
/// Live updates are sent at most this often.
const UPDATE_INTERVAL: Duration = Duration::from_secs(1);
/// Password checks running at once; each takes real CPU time.
const CONCURRENT_VERIFIES: usize = 2;
/// Notices sent with each state.
const NOTICES_SENT: usize = 10;
const COOKIE: &str = "empirebt_session";
/// The header write requests with a session cookie must carry.
const CSRF_HEADER: &str = "x-empirebt";

/// Everything the handlers share.
pub(crate) struct WebState {
    pub engine: Handle,
    pub auth: Mutex<AuthFile>,
    pub sessions: Sessions,
    pub links: LoginLinks,
    pub guard: Guard,
    pub verifying: tokio::sync::Semaphore,
    /// Host names (without port, lowercase) requests may be addressed to,
    /// besides IP addresses, when `web.allowed_hosts` lists any; `None`
    /// allows every name.
    pub hosts: Option<Vec<String>>,
    pub tls: bool,
    pub stop: watch::Receiver<bool>,
    pub streams: AtomicUsize,
    /// What each qBittorrent API client was last sent.
    pub qbittorrent: crate::qbittorrent::Syncs,
}

impl WebState {
    pub fn new(
        engine: Handle,
        auth: AuthFile,
        hosts: Option<Vec<String>>,
        tls: bool,
        stop: watch::Receiver<bool>,
    ) -> Self {
        Self {
            engine,
            auth: Mutex::new(auth),
            sessions: Sessions::default(),
            links: LoginLinks::default(),
            guard: Guard::default(),
            verifying: tokio::sync::Semaphore::new(CONCURRENT_VERIFIES),
            hosts,
            tls,
            stop,
            streams: AtomicUsize::new(0),
            qbittorrent: crate::qbittorrent::Syncs::default(),
        }
    }
}

pub(crate) type Shared = Arc<WebState>;

pub(crate) fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(assets::index))
        .route("/app.js", get(assets::script))
        .route("/style.css", get(assets::style))
        .route("/icon.svg", get(assets::icon))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/session", get(session))
        .route("/api/state", get(state_view))
        .route("/api/events", get(events))
        .route("/api/torrents", post(add))
        .route(
            "/api/torrents/upload",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD)),
        )
        .route("/api/torrents/{hash}", get(details).delete(remove))
        .route("/api/torrents/{hash}/category", put(set_category))
        .route("/api/torrents/{hash}/tags", put(set_tags))
        .route("/api/torrents/{hash}/name", put(rename_torrent))
        .route("/api/torrents/{hash}/rename", post(rename_path))
        .route("/api/torrents/{hash}/move", post(move_torrent))
        .route("/api/torrents/{hash}/peers", get(peers))
        .route("/api/torrents/{hash}/{action}", post(action))
        .route("/api/categories", post(create_category))
        .route("/api/categories/folder", put(set_category_folder))
        .route("/api/categories/remove", post(remove_category))
        .route("/api/tags", post(create_tag))
        .route("/api/tags/remove", post(remove_tag))
        .route("/api/settings", put(change_setting))
        .nest("/api/v2", crate::qbittorrent::router())
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(Arc::clone(&state), checks))
        .with_state(state)
}

// ----- checks ------------------------------------------------------------------

/// How a request proved who it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Credential {
    None,
    Cookie,
    Token,
}

async fn checks(State(state): State<Shared>, request: Request, next: Next) -> Response {
    let tls = state.tls;
    let response = match check(&state, &request) {
        Ok(()) => next.run(request).await,
        Err(response) => response,
    };
    with_security_headers(response, tls)
}

// A ready response as the error is the axum way; once per request.
#[allow(clippy::result_large_err)]
fn check(state: &WebState, request: &Request) -> Result<(), Response> {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    // Addresses are always fine: no other site can own one.
    let name = host_name(host);
    let path = request.uri().path();
    if let Some(hosts) = &state.hosts
        && name.parse::<std::net::IpAddr>().is_err()
        && !hosts.contains(&name)
    {
        let message = format!(
            "{name} is not in web.allowed_hosts, so EmpireBT does not answer to it. Add it \
             there (EMPIREBT_WEB_ALLOWED_HOSTS in Docker), or empty the setting to allow \
             any name."
        );
        // A person opening the page reads the reason, not a blank page.
        if !path.starts_with("/api/") {
            return Err((
                StatusCode::MISDIRECTED_REQUEST,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                message,
            )
                .into_response());
        }
        return Err(error(StatusCode::MISDIRECTED_REQUEST, &message));
    }
    if path.starts_with("/api/v2/") {
        return check_qbittorrent(state, request, host);
    }
    let credential = credential(state, headers, COOKIE);
    let public = !path.starts_with("/api/") || path == "/api/login";
    if !public && credential == Credential::None {
        return Err(error(StatusCode::UNAUTHORIZED, "sign in first"));
    }
    let writes = !matches!(*request.method(), Method::GET | Method::HEAD);
    // An API token is not sent by browsers on their own, so it needs no
    // cross-site protection; cookies are, so they do.
    if writes && credential != Credential::Token {
        if !headers.contains_key(CSRF_HEADER) {
            return Err(error(StatusCode::FORBIDDEN, "missing X-EmpireBT header"));
        }
        if let Some(origin) = headers.get(header::ORIGIN)
            && !names_us(headers, host, origin.to_str().unwrap_or(""))
        {
            return Err(error(StatusCode::FORBIDDEN, "cross-site request refused"));
        }
    }
    Ok(())
}

/// Whether `url` (an `Origin` or `Referer`) is on this server. Behind a
/// reverse proxy that does not pass `Host` on, the name the browser used
/// arrives as `X-Forwarded-Host`; cross-site pages cannot send that, or
/// `X-EmpireBT`, without our consent.
fn names_us(headers: &HeaderMap, host: &str, url: &str) -> bool {
    let named = url
        .split_once("://")
        .map_or("", |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or("");
    let forwarded = headers
        .get("x-forwarded-host")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim);
    named.eq_ignore_ascii_case(host)
        || forwarded.is_some_and(|forwarded| named.eq_ignore_ascii_case(forwarded))
}

/// qBittorrent's rules for its API: signed in by its `SID` cookie or an
/// API token, else 403 (which its clients take as "sign in again"); and a
/// request with a cookie that sends `Origin` or `Referer` must name this
/// server, as qBittorrent's cross-site protection checks. Its clients are
/// programs, which send neither.
#[allow(clippy::result_large_err)]
fn check_qbittorrent(state: &WebState, request: &Request, host: &str) -> Result<(), Response> {
    let headers = request.headers();
    let credential = credential(state, headers, crate::qbittorrent::COOKIE);
    let forbidden = || {
        (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/plain; charset=UTF-8")],
            "Forbidden",
        )
            .into_response()
    };
    if request.uri().path() != "/api/v2/auth/login" && credential == Credential::None {
        return Err(forbidden());
    }
    if credential != Credential::Token {
        for name in [header::ORIGIN, header::REFERER] {
            if let Some(value) = headers.get(name)
                && !names_us(headers, host, value.to_str().unwrap_or(""))
            {
                return Err(forbidden());
            }
        }
    }
    Ok(())
}

fn credential(state: &WebState, headers: &HeaderMap, cookie_name: &str) -> Credential {
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        return if lock(&state.auth).has_token(token.trim()) {
            Credential::Token
        } else {
            Credential::None
        };
    }
    // A client retrying a request after signing in again can send the
    // old cookie and the new one under the same name (Go's HTTP client
    // does); any live one will do.
    if cookies(headers, cookie_name).any(|token| state.sessions.check(&token)) {
        Credential::Cookie
    } else {
        Credential::None
    }
}

/// The values of every cookie called `wanted`, in the order sent.
fn cookies<'a>(headers: &'a HeaderMap, wanted: &'a str) -> impl Iterator<Item = String> + 'a {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(move |(name, _)| *name == wanted)
        .map(|(_, value)| value.to_owned())
}

/// The value of the cookie called `wanted`; the last, when sent more than
/// once, as it is the newest.
pub(crate) fn cookie(headers: &HeaderMap, wanted: &str) -> Option<String> {
    cookies(headers, wanted).last()
}

/// The host name of a `Host` header, without port or brackets, lowercase.
pub(crate) fn host_name(host: &str) -> String {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
            _ => host,
        }
    };
    name.to_ascii_lowercase()
}

fn with_security_headers(mut response: Response, tls: bool) -> Response {
    let headers = response.headers_mut();
    let set = |headers: &mut HeaderMap, name: &'static str, value: &'static str| {
        headers.insert(name, HeaderValue::from_static(value));
    };
    set(
        headers,
        "content-security-policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
         connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    );
    set(headers, "x-content-type-options", "nosniff");
    set(headers, "x-frame-options", "DENY");
    set(headers, "referrer-policy", "no-referrer");
    set(headers, "cross-origin-opener-policy", "same-origin");
    set(headers, "cross-origin-resource-policy", "same-origin");
    set(
        headers,
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), interest-cohort=()",
    );
    if !headers.contains_key(header::CACHE_CONTROL) {
        set(headers, "cache-control", "no-store");
    }
    if tls {
        set(headers, "strict-transport-security", "max-age=31536000");
    }
    response
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

// ----- signing in -------------------------------------------------------------

#[derive(Deserialize)]
struct Login {
    password: Option<String>,
    /// A one-time login link's token.
    token: Option<String>,
}

async fn login(
    State(state): State<Shared>,
    Extension(client): Extension<ClientAddr>,
    axum::Json(login): axum::Json<Login>,
) -> Response {
    if let Some(wait) = state.guard.wait(client.0) {
        tracing::warn!(target: "web", "sign-in from {} refused: locked out after failures", client.0);
        let mut response = error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed attempts; try again in a few minutes",
        );
        if let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    let valid = match (login.token, login.password) {
        (Some(token), _) => state.links.redeem(&token),
        (None, Some(password)) => {
            let Some(hash) = lock(&state.auth).password.clone() else {
                return error(StatusCode::UNAUTHORIZED, "no password is set");
            };
            let Ok(_permit) = state.verifying.acquire().await else {
                return error(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
            };
            tokio::task::spawn_blocking(move || auth::verify_password(&hash, &password))
                .await
                .unwrap_or(false)
        }
        (None, None) => false,
    };
    if !valid {
        // Who tried matters; what they typed is never logged.
        tracing::warn!(target: "web", "failed sign-in from {}", client.0);
        state.guard.failed(client.0);
        return error(StatusCode::UNAUTHORIZED, "wrong password");
    }
    tracing::info!(target: "web", "signed in from {}", client.0);
    state.guard.succeeded(client.0);
    let token = state.sessions.create();
    let mut response = StatusCode::NO_CONTENT.into_response();
    let secure = if state.tls { "; Secure" } else { "" };
    let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict{secure}");
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn logout(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie(&headers, COOKIE) {
        state.sessions.end(&token);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let secure = if state.tls { "; Secure" } else { "" };
    let cookie = format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}");
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn session() -> Response {
    axum::Json(json!({ "signedIn": true })).into_response()
}

// ----- state ------------------------------------------------------------------

/// Settings a browser session may not change: where files go, which
/// network interface carries traffic, files read for the port, and the web
/// UI's own setup.
pub(crate) fn editable_from_web(key: &str) -> bool {
    !(key.starts_with("storage.")
        || key.starts_with("web.")
        || key == "network.interface"
        || key == "network.port_file")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TorrentJson<'a> {
    hash: String,
    name: &'a str,
    status: &'static str,
    status_label: &'static str,
    progress: Option<f64>,
    size: Option<u64>,
    remaining: u64,
    down: f64,
    up: f64,
    peers: Option<usize>,
    seeders: Option<u64>,
    leechers: Option<u64>,
    /// How many finished downloading it, from tracker scrapes.
    completed: Option<u64>,
    category: Option<&'a str>,
    tags: &'a [String],
    error: Option<&'a str>,
    /// A magnet link whose metadata has not arrived.
    magnet: bool,
    /// The folder its files belong in: where they are, or where they go
    /// once complete.
    save_path: std::borrow::Cow<'a, str>,
    /// Downloading in the incomplete folder: that folder.
    download_path: Option<std::borrow::Cow<'a, str>>,
    /// Its files are moving: where to, and how far (0 to 1).
    moving_to: Option<std::borrow::Cow<'a, str>>,
    move_progress: Option<f64>,
    /// Payload over every run, and uploaded over downloaded.
    downloaded: u64,
    uploaded: u64,
    ratio: f64,
    /// When it was added and finished, in Unix seconds; `null` unknown.
    added_at: Option<u64>,
    finished_at: Option<u64>,
}

fn torrent_json(torrent: &TorrentView) -> TorrentJson<'_> {
    TorrentJson {
        hash: auth::hex(&torrent.hash),
        name: &torrent.name,
        status: status_key(torrent.status),
        status_label: torrent.status.label(),
        progress: torrent.fraction,
        size: torrent.size(),
        remaining: torrent.remaining(),
        down: torrent.down_rate,
        up: torrent.up_rate,
        peers: torrent.peers,
        seeders: torrent.swarm.0,
        leechers: torrent.swarm.1,
        completed: torrent.completed,
        category: torrent.category.as_deref(),
        tags: &torrent.tags,
        error: torrent.error.as_deref(),
        magnet: torrent.info.is_none(),
        save_path: torrent.final_dir().to_string_lossy(),
        download_path: torrent
            .complete_dir
            .as_ref()
            .map(|_| torrent.save_dir.to_string_lossy()),
        moving_to: torrent
            .moving
            .as_ref()
            .map(|moving| moving.to.to_string_lossy()),
        // Whole percents: steady between updates.
        move_progress: torrent
            .moving
            .as_ref()
            .map(|moving| (moving.fraction * 100.0).floor() / 100.0),
        downloaded: torrent.history.downloaded,
        uploaded: torrent.history.uploaded,
        // Three decimals: enough to show, and steady between updates.
        ratio: (torrent.ratio * 1000.0).round() / 1000.0,
        added_at: Some(torrent.history.added).filter(|added| *added > 0),
        finished_at: Some(torrent.history.completed).filter(|completed| *completed > 0),
    }
}

fn status_key(status: empirebt_app::Status) -> &'static str {
    use empirebt_app::Status;
    match status {
        Status::Queued => "queued",
        Status::Metadata => "metadata",
        Status::Checking => "checking",
        Status::Announcing => "announcing",
        Status::Downloading => "downloading",
        Status::Waiting => "waiting",
        Status::Retrying => "retrying",
        Status::Pausing => "pausing",
        Status::Moving => "moving",
        Status::Seeding => "seeding",
        Status::Paused => "paused",
        Status::Error => "error",
    }
}

/// Speeds, counts, and network status: small, sent with every update.
fn header_json(snapshot: &Snapshot) -> serde_json::Value {
    let network = &snapshot.network;
    let seeding = match &network.seed_status {
        SeedStatus::Idle => json!({ "state": "idle" }),
        SeedStatus::Starting => json!({ "state": "starting" }),
        SeedStatus::Listening { torrents } => json!({ "state": "listening", "torrents": torrents }),
        SeedStatus::Failed(error) => json!({ "state": "failed", "error": error }),
    };
    let dht = match &network.dht {
        empirebt_app::DhtState::Disabled => json!({ "state": "disabled" }),
        empirebt_app::DhtState::Failed(error) => json!({ "state": "failed", "error": error }),
        empirebt_app::DhtState::Running => json!({
            "state": "running",
            "nodes": network.dht_nodes,
            "ipv6Nodes": network.dht_ipv6_nodes,
        }),
    };
    json!({
        "generation": snapshot.generation,
        "totals": { "down": snapshot.totals.0, "up": snapshot.totals.1 },
        "limits": { "down": snapshot.rate_limits.0, "up": snapshot.rate_limits.1 },
        "activeCount": snapshot.active_count,
        "seedingCount": snapshot.seeding_count,
        // Payload over every run (removed torrents too), and this run.
        "transferred": {
            "downloaded": snapshot.transferred.downloaded,
            "uploaded": snapshot.transferred.uploaded,
            "downloadedSession": snapshot.transferred.downloaded_session,
            "uploadedSession": snapshot.transferred.uploaded_session,
        },
        "network": {
            "port": network.listen_port,
            "seeding": seeding,
            "peers": network.seeding_peers,
            "dht": dht,
            "utp": network.utp,
            "lsd": network.lsd,
            "interface": network.interface,
            "interfaceUp": network.interface_up,
        },
    })
}

/// Categories, tags, and settings: sent when they change.
fn lists_json(snapshot: &Snapshot) -> serde_json::Value {
    let settings: Vec<_> = snapshot
        .settings
        .iter()
        .map(|item| {
            json!({
                "key": item.setting.key,
                "kind": setting_kind(item.setting.kind),
                "choices": match item.setting.kind {
                    empirebt_backend::SettingKind::Choice(choices) => choices.to_vec(),
                    _ => Vec::new(),
                },
                "description": item.setting.description,
                "value": item.value,
                "needsRestart": item.setting.needs_restart,
                "overridden": item.overridden,
                "editable": editable_from_web(item.setting.key),
            })
        })
        .collect();
    json!({
        "about": {
            "version": env!("CARGO_PKG_VERSION"),
            "copyright": empirebt_app::COPYRIGHT,
            "license": empirebt_app::LICENSE_NOTICE,
            "source": empirebt_app::SOURCE,
        },
        "categories": snapshot.categories,
        "tags": snapshot.tags,
        "settings": settings,
        "categoryFolders": snapshot.category_folders,
        "saveDirs": snapshot
            .allowed_dirs
            .iter()
            .map(|dir| dir.to_string_lossy())
            .collect::<Vec<_>>(),
        // Where new torrents download first unless told otherwise.
        "incompleteDir": snapshot.incomplete_dir.as_ref().map(|dir| dir.to_string_lossy()),
    })
}

/// Notices newer than `after`, at most the last few.
fn notices_json(snapshot: &Snapshot, after: u64) -> Vec<serde_json::Value> {
    let skip = snapshot.notices.len().saturating_sub(NOTICES_SENT);
    snapshot
        .notices
        .iter()
        .skip(skip)
        .filter(|notice| notice.id > after)
        .map(|notice| {
            json!({
                "id": notice.id,
                "level": match notice.level {
                    Level::Info => "info",
                    Level::Success => "success",
                    Level::Error => "error",
                },
                "text": notice.text,
                "ageMs": notice.at.elapsed().as_millis() as u64,
            })
        })
        .collect()
}

/// What a client was last sent, so updates carry only what changed: with
/// thousands of torrents, most of them idle, the full state every second
/// would be hundreds of kilobytes.
#[derive(Debug, Default)]
pub(crate) struct Sent {
    /// Each torrent's JSON, by hash.
    torrents: HashMap<String, String>,
    order: Vec<String>,
    lists: String,
    notice: u64,
}

/// An update for a client last sent `sent` (everything, when `full`), as
/// JSON text: `header` always; `torrents` that changed or are new,
/// `removed` hashes, `order` when it changed, `lists` when they changed,
/// and new `notices`.
pub(crate) fn update(sent: &mut Sent, snapshot: &Snapshot, full: bool) -> String {
    if full {
        *sent = Sent::default();
    }
    let mut changed = Vec::new();
    let mut order = Vec::with_capacity(snapshot.torrents.len());
    let mut current = HashMap::with_capacity(snapshot.torrents.len());
    for torrent in &snapshot.torrents {
        let hash = auth::hex(&torrent.hash);
        let text = serde_json::to_string(&torrent_json(torrent)).unwrap_or_default();
        if sent.torrents.get(&hash) != Some(&text) {
            changed.push(text.clone());
        }
        order.push(hash.clone());
        current.insert(hash, text);
    }
    let removed: Vec<&String> = sent
        .torrents
        .keys()
        .filter(|hash| !current.contains_key(*hash))
        .collect();
    let mut out = format!(
        "{{\"full\":{full},\"header\":{},\"torrents\":[{}],\"removed\":{}",
        header_json(snapshot),
        changed.join(","),
        serde_json::to_string(&removed).unwrap_or_default(),
    );
    if order != sent.order {
        out.push_str(&format!(
            ",\"order\":{}",
            serde_json::to_string(&order).unwrap_or_default()
        ));
    }
    let lists = lists_json(snapshot).to_string();
    if lists != sent.lists {
        out.push_str(&format!(",\"lists\":{lists}"));
    }
    let notices = notices_json(snapshot, sent.notice);
    out.push_str(&format!(
        ",\"notices\":{}}}",
        serde_json::Value::from(notices)
    ));
    sent.notice = snapshot
        .notices
        .last()
        .map_or(sent.notice, |notice| notice.id);
    sent.torrents = current;
    sent.order = order;
    sent.lists = lists;
    out
}

fn setting_kind(kind: empirebt_backend::SettingKind) -> &'static str {
    use empirebt_backend::SettingKind;
    match kind {
        SettingKind::Path => "path",
        SettingKind::Port => "port",
        SettingKind::Count => "count",
        SettingKind::Seconds => "seconds",
        SettingKind::Rate => "rate",
        SettingKind::Limit => "limit",
        SettingKind::Bool => "bool",
        SettingKind::HostList => "hosts",
        SettingKind::Choice(_) => "choice",
        SettingKind::Interface => "interface",
        SettingKind::Address => "address",
        SettingKind::Text => "text",
    }
}

/// Everything, in the shape of a full live update.
async fn state_view(State(state): State<Shared>) -> Response {
    let body = update(&mut Sent::default(), &state.engine.snapshot(), true);
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Removes itself from the count of open streams when dropped.
struct StreamSlot(Shared);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.streams.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Live updates: everything first, then what changed, at most once a
/// second, until the client leaves or the server stops.
async fn events(State(state): State<Shared>) -> Response {
    if state.streams.fetch_add(1, Ordering::AcqRel) >= MAX_STREAMS {
        state.streams.fetch_sub(1, Ordering::AcqRel);
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many live update streams",
        );
    }
    let slot = StreamSlot(Arc::clone(&state));
    let stream = futures_util::stream::unfold(
        (slot, Sent::default(), 0_u64, None::<Instant>),
        |(slot, mut sent, generation, last)| async move {
            let mut stop = slot.0.stop.clone();
            let mut due = last.map(|last| last + UPDATE_INTERVAL);
            loop {
                if *stop.borrow() {
                    return None;
                }
                if let Some(due) = due {
                    tokio::select! {
                        _ = tokio::time::sleep_until(due.into()) => {}
                        _ = stop.changed() => return None,
                    }
                }
                let snapshot = slot.0.engine.snapshot();
                // The first update always goes out; later ones only when
                // something changed, checked again after another interval.
                if last.is_some() && snapshot.generation == generation {
                    due = Some(Instant::now() + UPDATE_INTERVAL);
                } else {
                    let data = update(&mut sent, &snapshot, last.is_none());
                    let event = Event::default().event("update").data(data);
                    return Some((
                        Ok::<_, std::convert::Infallible>(event),
                        (slot, sent, snapshot.generation, Some(Instant::now())),
                    ));
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn details(State(state): State<Shared>, Path(hash): Path<String>) -> Response {
    let snapshot = state.engine.snapshot();
    let Some(torrent) = parse_hash(&hash).and_then(|hash| snapshot.torrent(&hash)) else {
        return error(StatusCode::NOT_FOUND, "there is no such torrent");
    };
    let info = torrent.info.as_ref();
    let files: Vec<_> = torrent
        .files()
        .iter()
        .map(|file| {
            let mut entry = json!({
                "path": slashed(&file.path),
                "size": file.length,
            });
            if file.path != file.original {
                entry["original"] = slashed(&file.original).into();
            }
            entry
        })
        .collect();
    let announce = torrent.announce.as_ref().map(|status| {
        json!({
            "trackersOk": status.trackers_ok,
            "trackersTotal": status.trackers_total,
            "uploaded": status.uploaded,
            "lastError": status.last_error,
            "dhtAnnounced": status.dht_announced.is_some(),
        })
    });
    axum::Json(json!({
        "torrent": torrent_json(torrent),
        "originalName": torrent.original_name,
        "files": files,
        "trackers": torrent.trackers,
        "pieces": info.map(|info| info.piece_count().max(info.v2_piece_count())),
        "verifiedPieces": torrent.verified_pieces,
        "pieceLength": info.map(|info| info.piece_length()),
        "format": info.map(|info| match info.format() {
            empirebt_backend::TorrentFormat::V1 => "v1",
            empirebt_backend::TorrentFormat::V2 => "v2",
            empirebt_backend::TorrentFormat::Hybrid => "hybrid",
        }),
        "private": info.is_some_and(|info| info.is_private()),
        // Times change every second, so they come with details, not with
        // every live update.
        "history": {
            "activeSeconds": torrent.history.active.as_secs(),
            "seedingSeconds": torrent.history.seeding.as_secs(),
            "downloadedSession": torrent.history.downloaded_session,
            "uploadedSession": torrent.history.uploaded_session,
        },
        "announce": announce,
        "savePath": torrent.final_dir().to_string_lossy(),
        "downloadPath": torrent.complete_dir.as_ref().map(|_| torrent.save_dir.to_string_lossy()),
    }))
    .into_response()
}

// ----- commands ---------------------------------------------------------------

/// Runs an engine command off the async threads and answers with the
/// outcome.
async fn run(state: &Shared, command: Command) -> Response {
    let engine = state.engine.clone();
    match tokio::task::spawn_blocking(move || engine.call(command)).await {
        Ok(Ok(empirebt_app::Reply::Added(hash))) => {
            axum::Json(json!({ "added": auth::hex(&hash) })).into_response()
        }
        Ok(Ok(empirebt_app::Reply::Fetching)) => (
            StatusCode::ACCEPTED,
            axum::Json(json!({ "fetching": true })),
        )
            .into_response(),
        Ok(Ok(empirebt_app::Reply::Done)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(empirebt_app::Reply::Peers(peers))) => axum::Json(peers_json(&peers)).into_response(),
        Ok(Err(failure)) => error(StatusCode::BAD_REQUEST, &failure.message),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "the command failed"),
    }
}

fn parse_hash(text: &str) -> Option<InfoHash> {
    if text.len() != 40 || !text.is_ascii() {
        return None;
    }
    let mut hash = [0; 20];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

// A ready response as the error is the axum way; once per request.
#[allow(clippy::result_large_err)]
fn known_hash(state: &WebState, text: &str) -> Result<InfoHash, Response> {
    parse_hash(text)
        .filter(|hash| state.engine.snapshot().torrent(hash).is_some())
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "there is no such torrent"))
}

fn peers_json(peers: &[empirebt_app::PeerView]) -> serde_json::Value {
    let peers: Vec<_> = peers
        .iter()
        .map(|peer| {
            let info = &peer.info;
            let flags = info.flags();
            let letters: Vec<String> = flags.iter().map(|(flag, _)| flag.to_string()).collect();
            let meanings: Vec<String> = flags
                .iter()
                .map(|(flag, meaning)| format!("{flag} = {meaning}"))
                .collect();
            json!({
                "address": info.address.to_string(),
                "ip": info.address.ip().to_string(),
                "port": info.address.port(),
                "country": peer.country_code(),
                "client": info.client,
                "outgoing": info.outgoing,
                "utp": info.utp,
                "encrypted": info.encrypted,
                "progress": info.progress,
                "down": info.down_rate,
                "up": info.up_rate,
                "downloaded": info.downloaded,
                "uploaded": info.uploaded,
                "interested": info.interested,
                "unchoked": info.unchoked,
                "interesting": info.interesting,
                "chokingUs": info.choking_us,
                "flags": letters.join(" "),
                "flagsDescription": meanings.join("\n"),
            })
        })
        .collect();
    json!({ "peers": peers, "attribution": empirebt_app::GEOIP_ATTRIBUTION })
}

async fn peers(State(state): State<Shared>, Path(hash): Path<String>) -> Response {
    match known_hash(&state, &hash) {
        Ok(hash) => run(&state, Command::Peers(hash)).await,
        Err(response) => response,
    }
}

/// A path inside a torrent with `/` between its parts on every system.
/// A full path keeps its root: `/downloads/a`, not `//downloads/a`.
pub(crate) fn slashed(path: &std::path::Path) -> String {
    use std::path::Component;
    let mut text = String::new();
    for part in path.components() {
        match part {
            Component::Prefix(prefix) => text.push_str(&prefix.as_os_str().to_string_lossy()),
            Component::RootDir => text.push('/'),
            other => {
                if !text.is_empty() && !text.ends_with('/') {
                    text.push('/');
                }
                text.push_str(&other.as_os_str().to_string_lossy());
            }
        }
    }
    text
}

#[derive(Deserialize)]
struct Add {
    source: String,
    #[serde(rename = "savePath")]
    save_path: Option<String>,
    category: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    layout: Option<String>,
    /// Where it downloads first: missing or null follows
    /// `storage.incomplete_dir`, empty is straight into its save folder.
    #[serde(rename = "downloadPath")]
    download_path: Option<String>,
}

/// `original` (or nothing), `subfolder`, or `nosubfolder`.
#[allow(clippy::result_large_err)]
fn layout(text: Option<&str>) -> Result<ContentLayout, Response> {
    match text.map(str::trim).filter(|text| !text.is_empty()) {
        None => Ok(ContentLayout::Original),
        Some(text) => ContentLayout::parse(text).ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "layout is original, subfolder, or nosubfolder",
            )
        }),
    }
}

/// The same options for an upload, in the query string; `tags` is
/// comma-separated.
#[derive(Deserialize)]
struct UploadOptions {
    #[serde(rename = "savePath")]
    save_path: Option<String>,
    category: Option<String>,
    tags: Option<String>,
    layout: Option<String>,
    #[serde(rename = "downloadPath")]
    download_path: Option<String>,
}

/// Where a browser asked a torrent to download before its save folder:
/// unset follows `storage.incomplete_dir`, empty is straight into the
/// save folder, else a folder it may save in.
#[allow(clippy::result_large_err)]
pub(crate) fn allowed_download_dir(
    snapshot: &Snapshot,
    text: Option<&str>,
) -> Result<DownloadDir, Response> {
    match text.map(str::trim) {
        None => Ok(DownloadDir::Default),
        Some("") => Ok(DownloadDir::SaveDir),
        Some(text) => {
            Ok(allowed_save_dir(snapshot, Some(text))?
                .map_or(DownloadDir::SaveDir, DownloadDir::At))
        }
    }
}

/// The folder a browser asked to save a torrent in, if it may: the
/// download directory, a folder in `storage.allowed_dirs`, or a folder
/// inside one of them. Anything else would let a browser write wherever
/// EmpireBT can. Empty means the download directory.
#[allow(clippy::result_large_err)]
pub(crate) fn allowed_save_dir(
    snapshot: &Snapshot,
    text: Option<&str>,
) -> Result<Option<std::path::PathBuf>, Response> {
    let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(text);
    let plain = path.is_absolute()
        && path
            .components()
            .all(|part| !matches!(part, std::path::Component::ParentDir));
    if plain
        && snapshot
            .allowed_dirs
            .iter()
            .any(|root| path.starts_with(root))
    {
        return Ok(Some(path));
    }
    Err(error(
        StatusCode::FORBIDDEN,
        "the web UI can save torrents only in the download folder, folders listed in \
         storage.allowed_dirs, or folders inside them",
    ))
}

/// Adds `source` into `save_dir` (else its category's folder, else the
/// download folder) with its labels.
async fn add_to(
    state: &Shared,
    source: AddSource,
    save_dir: Option<std::path::PathBuf>,
    category: Option<String>,
    tags: Vec<String>,
    layout: ContentLayout,
    download_dir: DownloadDir,
) -> Response {
    let request = AddRequest {
        save_dir,
        layout,
        download_dir,
        category: category.filter(|name| !name.trim().is_empty()),
        tags: tags
            .into_iter()
            .filter(|tag| !tag.trim().is_empty())
            .collect(),
        ..AddRequest::new(source)
    };
    run(state, Command::AddWith(request)).await
}

async fn add(State(state): State<Shared>, axum::Json(add): axum::Json<Add>) -> Response {
    let source = add.source.trim();
    // Paths on the server are not for browsers to read.
    if !(source.starts_with("magnet:")
        || source.starts_with("http://")
        || source.starts_with("https://"))
    {
        return error(
            StatusCode::BAD_REQUEST,
            "add a magnet link or a .torrent URL, or upload a .torrent file",
        );
    }
    let save_dir = match allowed_save_dir(&state.engine.snapshot(), add.save_path.as_deref()) {
        Ok(save_dir) => save_dir,
        Err(response) => return response,
    };
    let layout = match layout(add.layout.as_deref()) {
        Ok(layout) => layout,
        Err(response) => return response,
    };
    let download_dir =
        match allowed_download_dir(&state.engine.snapshot(), add.download_path.as_deref()) {
            Ok(dir) => dir,
            Err(response) => return response,
        };
    add_to(
        &state,
        AddSource::Link(source.to_owned()),
        save_dir,
        add.category,
        add.tags,
        layout,
        download_dir,
    )
    .await
}

async fn upload(
    State(state): State<Shared>,
    Query(options): Query<UploadOptions>,
    body: Bytes,
) -> Response {
    let save_dir = match allowed_save_dir(&state.engine.snapshot(), options.save_path.as_deref()) {
        Ok(save_dir) => save_dir,
        Err(response) => return response,
    };
    let layout = match layout(options.layout.as_deref()) {
        Ok(layout) => layout,
        Err(response) => return response,
    };
    let download_dir =
        match allowed_download_dir(&state.engine.snapshot(), options.download_path.as_deref()) {
            Ok(dir) => dir,
            Err(response) => return response,
        };
    let tags = options
        .tags
        .unwrap_or_default()
        .split(',')
        .map(str::to_owned)
        .collect();
    add_to(
        &state,
        AddSource::Torrent(body.to_vec()),
        save_dir,
        options.category,
        tags,
        layout,
        download_dir,
    )
    .await
}

#[derive(Deserialize)]
struct RemoveOptions {
    /// Also delete the torrent's downloaded files.
    #[serde(rename = "deleteFiles", default)]
    delete_files: bool,
}

async fn remove(
    State(state): State<Shared>,
    Path(hash): Path<String>,
    Query(options): Query<RemoveOptions>,
) -> Response {
    match known_hash(&state, &hash) {
        Ok(hash) if options.delete_files => run(&state, Command::RemoveWithFiles(hash)).await,
        Ok(hash) => run(&state, Command::Remove(hash)).await,
        Err(response) => response,
    }
}

async fn action(
    State(state): State<Shared>,
    Path((hash, action)): Path<(String, String)>,
) -> Response {
    let hash = match known_hash(&state, &hash) {
        Ok(hash) => hash,
        Err(response) => return response,
    };
    let command = match action.as_str() {
        "start" => Command::Start(hash),
        "pause" => Command::Pause(hash),
        "toggle" => Command::Toggle(hash),
        "recheck" => Command::Recheck(hash),
        "reannounce" => Command::Reannounce(hash),
        _ => return not_found().await,
    };
    run(&state, command).await
}

#[derive(Deserialize)]
struct CategoryBody {
    category: Option<String>,
}

async fn set_category(
    State(state): State<Shared>,
    Path(hash): Path<String>,
    axum::Json(body): axum::Json<CategoryBody>,
) -> Response {
    match known_hash(&state, &hash) {
        Ok(torrent) => {
            let category = body.category.filter(|name| !name.trim().is_empty());
            run(&state, Command::SetCategory { torrent, category }).await
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct TagsBody {
    tags: Vec<String>,
}

async fn set_tags(
    State(state): State<Shared>,
    Path(hash): Path<String>,
    axum::Json(body): axum::Json<TagsBody>,
) -> Response {
    match known_hash(&state, &hash) {
        Ok(torrent) => {
            let tags = body
                .tags
                .into_iter()
                .filter(|tag| !tag.trim().is_empty())
                .collect();
            run(&state, Command::SetTags { torrent, tags }).await
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
struct TorrentNameBody {
    name: Option<String>,
}

/// Chooses the name shown for a torrent; empty or `null` shows its own.
async fn rename_torrent(
    State(state): State<Shared>,
    Path(hash): Path<String>,
    axum::Json(body): axum::Json<TorrentNameBody>,
) -> Response {
    match known_hash(&state, &hash) {
        Ok(torrent) => {
            let name = body.name.filter(|name| !name.trim().is_empty());
            run(&state, Command::RenameTorrent { torrent, name }).await
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

/// Renames a file or folder of a torrent, both paths relative to its
/// folder; the engine keeps them inside it.
async fn rename_path(
    State(state): State<Shared>,
    Path(hash): Path<String>,
    axum::Json(body): axum::Json<RenameBody>,
) -> Response {
    match known_hash(&state, &hash) {
        Ok(torrent) => {
            let command = Command::RenamePath {
                torrent,
                from: body.from.into(),
                to: body.to.trim().into(),
            };
            run(&state, command).await
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct MoveBody {
    #[serde(rename = "savePath")]
    save_path: String,
}

/// Moves a torrent's files, only to a folder the web UI may save in.
async fn move_torrent(
    State(state): State<Shared>,
    Path(hash): Path<String>,
    axum::Json(body): axum::Json<MoveBody>,
) -> Response {
    let torrent = match known_hash(&state, &hash) {
        Ok(torrent) => torrent,
        Err(response) => return response,
    };
    match allowed_save_dir(&state.engine.snapshot(), Some(&body.save_path)) {
        Ok(Some(folder)) => run(&state, Command::Move { torrent, folder }).await,
        Ok(None) => error(StatusCode::BAD_REQUEST, "choose a folder to move it to"),
        Err(response) => response,
    }
}

async fn create_category(
    State(state): State<Shared>,
    axum::Json(body): axum::Json<NameBody>,
) -> Response {
    run(&state, Command::CreateCategory(body.name)).await
}

#[derive(Deserialize)]
struct CategoryFolder {
    name: String,
    /// Empty or missing takes the folder away.
    #[serde(rename = "savePath")]
    save_path: Option<String>,
}

/// Gives a category a folder: like `savePath` when adding, only one the
/// web UI may save in.
async fn set_category_folder(
    State(state): State<Shared>,
    axum::Json(body): axum::Json<CategoryFolder>,
) -> Response {
    let folder = match allowed_save_dir(&state.engine.snapshot(), body.save_path.as_deref()) {
        Ok(folder) => folder,
        Err(response) => return response,
    };
    run(
        &state,
        Command::SetCategoryFolder {
            category: body.name,
            folder,
        },
    )
    .await
}

async fn remove_category(
    State(state): State<Shared>,
    axum::Json(body): axum::Json<NameBody>,
) -> Response {
    run(&state, Command::RemoveCategory(body.name)).await
}

async fn create_tag(
    State(state): State<Shared>,
    axum::Json(body): axum::Json<NameBody>,
) -> Response {
    run(&state, Command::CreateTag(body.name)).await
}

async fn remove_tag(
    State(state): State<Shared>,
    axum::Json(body): axum::Json<NameBody>,
) -> Response {
    run(&state, Command::RemoveTag(body.name)).await
}

#[derive(Deserialize)]
struct SettingBody {
    key: String,
    value: String,
}

async fn change_setting(
    State(state): State<Shared>,
    axum::Json(body): axum::Json<SettingBody>,
) -> Response {
    if !editable_from_web(&body.key) {
        return error(
            StatusCode::FORBIDDEN,
            "this setting can only be changed in the settings file, the console, or with \
             `empirebt config set`",
        );
    }
    run(
        &state,
        Command::ChangeSetting {
            key: body.key,
            value: body.value,
        },
    )
    .await
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(index: usize) -> InfoHash {
        let mut hash = [0; 20];
        hash[..8].copy_from_slice(&(index as u64).to_be_bytes());
        hash
    }

    fn snapshot(torrents: usize) -> Snapshot {
        Snapshot {
            generation: 1,
            torrents: (0..torrents)
                .map(|index| TorrentView {
                    hash: unique(index),
                    name: format!("torrent {index}"),
                    original_name: format!("torrent {index}"),
                    renames: Default::default(),
                    save_dir: "/data".into(),
                    complete_dir: None,
                    moving: None,
                    info: None,
                    trackers: Vec::new(),
                    status: empirebt_app::Status::Paused,
                    fraction: None,
                    verified_pieces: None,
                    down_rate: 0.0,
                    up_rate: 0.0,
                    peers: None,
                    swarm: (None, None),
                    completed: None,
                    announce: None,
                    error: None,
                    category: None,
                    tags: Vec::new(),
                    history: Default::default(),
                    ratio: 0.0,
                })
                .collect(),
            totals: (0.0, 0.0),
            transferred: Default::default(),
            rate_limits: (0, 0),
            active_count: 0,
            seeding_count: 0,
            network: empirebt_app::Network {
                listen_port: 6881,
                seed_status: SeedStatus::Idle,
                seeding_peers: 0,
                dht: empirebt_app::DhtState::Disabled,
                dht_nodes: None,
                dht_ipv6_nodes: 0,
                utp: false,
                lsd: false,
                interface: None,
                interface_up: true,
            },
            download_dir: "/data".into(),
            allowed_dirs: vec!["/data".into(), "/media/films".into()],
            incomplete_dir: None,
            category_folders: Default::default(),
            notices: Vec::new(),
            settings: Vec::new(),
            config_path: "/data/config.toml".into(),
            categories: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn parse(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    /// Browsers save only in the download folder, `storage.allowed_dirs`,
    /// or folders inside them; no climbing out with `..`.
    #[test]
    fn paths_have_one_slash_between_parts() {
        let path = |text: &str| slashed(std::path::Path::new(text));
        assert_eq!(path("/downloads/films"), "/downloads/films");
        assert_eq!(path("/"), "/");
        assert_eq!(path("album/02.flac"), "album/02.flac");
        assert_eq!(path(""), "");
    }

    #[test]
    fn browsers_save_only_in_allowed_folders() {
        let snapshot = snapshot(0);
        let allowed = |text: &str| allowed_save_dir(&snapshot, Some(text)).ok().flatten();
        assert_eq!(allowed_save_dir(&snapshot, None).ok(), Some(None));
        assert_eq!(allowed_save_dir(&snapshot, Some("  ")).ok(), Some(None));
        assert_eq!(allowed("/data"), Some("/data".into()));
        assert_eq!(allowed("/data/linux/isos"), Some("/data/linux/isos".into()));
        assert_eq!(
            allowed("/media/films/2026"),
            Some("/media/films/2026".into())
        );
        for refused in [
            "/etc",
            "/database",
            "/data/../etc",
            "relative/folder",
            "/media",
        ] {
            assert!(
                allowed_save_dir(&snapshot, Some(refused)).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn updates_carry_only_what_changed() {
        let mut sent = Sent::default();
        let mut current = snapshot(1000);
        let first = update(&mut sent, &current, true);
        let full = parse(&first);
        assert_eq!(full["full"], true);
        assert_eq!(full["torrents"].as_array().unwrap().len(), 1000);
        assert_eq!(full["order"].as_array().unwrap().len(), 1000);
        assert!(full.get("lists").is_some());

        // Nothing changed: no torrents, no order, no lists; just the header.
        let idle = update(&mut sent, &current, false);
        let quiet = parse(&idle);
        assert_eq!(quiet["torrents"], json!([]));
        assert!(quiet.get("order").is_none() && quiet.get("lists").is_none());
        assert!(idle.len() < 1000, "{} bytes", idle.len());
        assert!(first.len() > 100 * idle.len());

        // One torrent downloads, one goes, a tag appears, a notice comes.
        current.torrents[5].down_rate = 1234.0;
        current.torrents.remove(9);
        current.tags.push("new".to_owned());
        current.notices.push(empirebt_app::Notice {
            id: 1,
            level: Level::Info,
            text: "hello".to_owned(),
            at: Instant::now(),
        });
        let changes = parse(&update(&mut sent, &current, false));
        let torrents = changes["torrents"].as_array().unwrap();
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0]["down"], 1234.0);
        assert_eq!(changes["removed"], json!([auth::hex(&unique(9))]));
        assert_eq!(changes["order"].as_array().unwrap().len(), 999);
        assert_eq!(changes["lists"]["tags"], json!(["new"]));
        assert_eq!(changes["notices"][0]["text"], "hello");
        // Each notice is sent once.
        let after = parse(&update(&mut sent, &current, false));
        assert_eq!(after["notices"], json!([]));
    }

    #[test]
    fn reads_host_names() {
        assert_eq!(host_name("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_name("LocalHost:8080"), "localhost");
        assert_eq!(host_name("[::1]:8080"), "::1");
        assert_eq!(host_name("nas.local"), "nas.local");
        assert_eq!(host_name("evil.example:80"), "evil.example");
    }

    #[test]
    fn keeps_dangerous_settings_out_of_reach() {
        assert!(!editable_from_web("storage.download_dir"));
        assert!(!editable_from_web("web.address"));
        assert!(!editable_from_web("network.interface"));
        assert!(editable_from_web("network.upload_limit"));
        assert!(editable_from_web("console.start_on_add"));
    }
}
