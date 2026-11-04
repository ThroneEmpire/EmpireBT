//! qBittorrent's Web API (v2), as far as EmpireBT can answer it, so
//! programs made for qBittorrent work with EmpireBT: front ends such as
//! qui, and tools that add or watch torrents.
//!
//! It answers as qBittorrent 5.0 (Web API 2.11.4) would, under `/api/v2/`:
//! signing in, the torrent list and its incremental sync (`sync/maindata`,
//! sending only what changed since the client's last `rid`), details,
//! files, trackers, peers, adding (links, URLs, uploads), removing,
//! starting, stopping, checking, reannouncing, renaming, categories,
//! tags, global speed limits, and the preferences EmpireBT has. Methods it
//! does not have answer 404, as qBittorrent does for unknown ones.
//!
//! The same rules as the web UI hold: the web UI's password (any user
//! name) or an API token as `Authorization: Bearer`, the same lockout
//! after failed sign-ins, torrents only by link, URL, or upload, save
//! folders only where the web UI may save, and the same settings out of
//! reach. Cookies are protected as qBittorrent protects them: a request
//! that sends `Origin` or `Referer` must name this server.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, FromRequest, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use empirebt_app::{
    AddRequest, AddSource, Command, ContentLayout, InfoHash, Reply, SeedStatus, Snapshot, Status,
    TorrentView,
};
use serde_json::{Map, Value, json};

use crate::api::{self, Shared, WebState};
use crate::auth;
use crate::server::ClientAddr;

/// The Web API version EmpireBT answers as: qBittorrent 5.0's, with tags
/// set at once and trackers in the torrent list.
pub(crate) const WEB_API_VERSION: &str = "2.11.4";
/// The qBittorrent version that goes with it.
const APP_VERSION: &str = "v5.0.0";
/// qBittorrent's session cookie.
pub(crate) const COOKIE: &str = "SID";
/// Clients whose sync state is kept; the least recently seen goes first.
const MAX_SYNC_CLIENTS: usize = 16;
/// What qBittorrent reports for an ETA it cannot tell: 100 days.
const NO_ETA: u64 = 8_640_000;

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/app/version", get(app_version))
        .route("/app/webapiVersion", get(web_api_version))
        .route("/app/buildInfo", get(build_info))
        .route("/app/preferences", get(preferences))
        .route("/app/setPreferences", post(set_preferences))
        .route("/app/defaultSavePath", get(default_save_path))
        .route("/transfer/info", get(transfer_info))
        .route("/transfer/speedLimitsMode", get(speed_limits_mode))
        .route("/transfer/downloadLimit", get(download_limit))
        .route("/transfer/uploadLimit", get(upload_limit))
        .route("/transfer/setDownloadLimit", post(set_download_limit))
        .route("/transfer/setUploadLimit", post(set_upload_limit))
        .route("/sync/maindata", get(maindata))
        .route("/sync/torrentPeers", get(torrent_peers))
        .route("/torrents/info", get(torrents_info).post(torrents_info))
        .route("/torrents/properties", get(properties))
        .route("/torrents/files", get(files))
        .route("/torrents/trackers", get(trackers))
        .route("/torrents/webseeds", get(webseeds))
        .route(
            "/torrents/add",
            post(add).layer(DefaultBodyLimit::max(api::MAX_UPLOAD)),
        )
        .route("/torrents/delete", post(delete))
        .route("/torrents/stop", post(stop))
        .route("/torrents/pause", post(stop))
        .route("/torrents/start", post(start))
        .route("/torrents/resume", post(start))
        .route("/torrents/recheck", post(recheck))
        .route("/torrents/reannounce", post(reannounce))
        .route("/torrents/rename", post(rename))
        .route("/torrents/renameFile", post(rename_path))
        .route("/torrents/renameFolder", post(rename_path))
        .route("/torrents/setCategory", post(set_category))
        .route("/torrents/categories", get(categories))
        .route("/torrents/createCategory", post(create_category))
        .route("/torrents/editCategory", post(edit_category))
        .route("/torrents/removeCategories", post(remove_categories))
        .route("/torrents/tags", get(tags))
        .route("/torrents/createTags", post(create_tags))
        .route("/torrents/deleteTags", post(delete_tags))
        .route("/torrents/addTags", post(add_tags))
        .route("/torrents/removeTags", post(remove_tags))
        .route("/torrents/setTags", post(set_tags))
        .fallback(unsupported)
}

// ----- requests and answers -------------------------------------------------------

/// A request's parameters, from the query string and a form body, as
/// qBittorrent takes them.
pub(crate) struct Params(HashMap<String, String>);

impl Params {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        self.get(key).is_some_and(|value| value == "true")
    }
}

impl<S: Send + Sync> FromRequest<S> for Params {
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let mut params: HashMap<String, String> = request
            .uri()
            .query()
            .map(|query| serde_urlencoded::from_str(query).unwrap_or_default())
            .unwrap_or_default();
        let form = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"));
        if form {
            let body = Bytes::from_request(request, state)
                .await
                .map_err(IntoResponse::into_response)?;
            let fields: Vec<(String, String)> =
                serde_urlencoded::from_bytes(&body).unwrap_or_default();
            params.extend(fields);
        }
        Ok(Self(params))
    }
}

fn text(status: StatusCode, body: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=UTF-8")],
        body.into(),
    )
        .into_response()
}

fn ok() -> Response {
    text(StatusCode::OK, "")
}

fn json_answer(value: Value) -> Response {
    axum::Json(value).into_response()
}

async fn unsupported() -> Response {
    text(
        StatusCode::NOT_FOUND,
        "EmpireBT does not have this qBittorrent API method",
    )
}

/// Runs engine commands off the async threads, one after another;
/// answers with the first failure, if any.
async fn run_all(state: &Shared, commands: Vec<Command>) -> Response {
    let engine = state.engine.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut failure = None;
        for command in commands {
            if let Err(error) = engine.call(command) {
                failure.get_or_insert(error.message);
            }
        }
        failure
    })
    .await;
    match result {
        Ok(None) => ok(),
        Ok(Some(message)) => text(StatusCode::CONFLICT, message),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "the command failed"),
    }
}

/// The torrents `hashes` names: hashes between `|`, or `all`. Unknown
/// ones are left out, as qBittorrent does.
fn hashes(snapshot: &Snapshot, params: &Params) -> Vec<InfoHash> {
    match params.get("hashes").or(params.get("hash")) {
        Some("all") => snapshot
            .torrents
            .iter()
            .map(|torrent| torrent.hash)
            .collect(),
        Some(list) => list
            .split('|')
            .filter_map(|hash| parse_hash(hash.trim()))
            .filter(|hash| snapshot.torrent(hash).is_some())
            .collect(),
        None => Vec::new(),
    }
}

/// A torrent named by `hash`, or 404 as qBittorrent answers.
#[allow(clippy::result_large_err)]
fn one(snapshot: &Snapshot, params: &Params) -> Result<InfoHash, Response> {
    params
        .get("hash")
        .and_then(|hash| parse_hash(hash.trim()))
        .filter(|hash| snapshot.torrent(hash).is_some())
        .ok_or_else(|| text(StatusCode::NOT_FOUND, "Torrent hash was not found"))
}

fn parse_hash(text: &str) -> Option<InfoHash> {
    let text = text.to_ascii_lowercase();
    if text.len() != 40 || !text.is_ascii() {
        return None;
    }
    let mut hash = [0; 20];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

/// Names between commas, trimmed, empty ones left out.
fn names(list: Option<&str>) -> Vec<String> {
    list.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

// ----- signing in -------------------------------------------------------------

async fn login(
    State(state): State<Shared>,
    Extension(client): Extension<ClientAddr>,
    params: Params,
) -> Response {
    if state.guard.wait(client.0).is_some() {
        tracing::warn!(target: "web", "qBittorrent API sign-in from {} refused: locked out after failures", client.0);
        return text(
            StatusCode::FORBIDDEN,
            "Your IP address has been banned after too many failed authentication attempts.",
        );
    }
    let Some(hash) = api::lock(&state.auth).password.clone() else {
        return text(StatusCode::OK, "Fails.");
    };
    let password = params.get("password").unwrap_or_default().to_owned();
    let Ok(_permit) = state.verifying.acquire().await else {
        return text(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    };
    let valid = tokio::task::spawn_blocking(move || auth::verify_password(&hash, &password))
        .await
        .unwrap_or(false);
    if !valid {
        tracing::warn!(target: "web", "failed qBittorrent API sign-in from {}", client.0);
        state.guard.failed(client.0);
        return text(StatusCode::OK, "Fails.");
    }
    tracing::info!(target: "web", "signed in to the qBittorrent API from {}", client.0);
    state.guard.succeeded(client.0);
    let token = state.sessions.create();
    let mut response = text(StatusCode::OK, "Ok.");
    let secure = if state.tls { "; Secure" } else { "" };
    let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict{secure}");
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn logout(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(token) = api::cookie(&headers, COOKIE) {
        state.sessions.end(&token);
    }
    ok()
}

// ----- the application ------------------------------------------------------

async fn app_version() -> Response {
    text(StatusCode::OK, APP_VERSION)
}

async fn web_api_version() -> Response {
    text(StatusCode::OK, WEB_API_VERSION)
}

async fn build_info() -> Response {
    json_answer(json!({
        "qt": "",
        "libtorrent": format!("EmpireBT {}", env!("CARGO_PKG_VERSION")),
        "boost": "",
        "openssl": "",
        "zlib": "",
        "bitness": usize::BITS,
        "platform": std::env::consts::OS,
    }))
}

/// A setting's current value.
fn setting(snapshot: &Snapshot, key: &str) -> String {
    snapshot
        .settings
        .iter()
        .find(|item| item.setting.key == key)
        .map(|item| item.value.clone())
        .unwrap_or_default()
}

fn number(snapshot: &Snapshot, key: &str) -> u64 {
    setting(snapshot, key).parse().unwrap_or(0)
}

/// EmpireBT's settings under qBittorrent's preference names.
fn preferences_json(snapshot: &Snapshot) -> Value {
    let on = |key: &str| setting(snapshot, key) == "true";
    json!({
        "save_path": api::slashed(&snapshot.download_dir),
        "temp_path_enabled": false,
        "temp_path": "",
        "listen_port": snapshot.network.listen_port,
        "random_port": false,
        "upnp": false,
        "dht": on("dht.enabled"),
        "pex": true,
        "lsd": on("network.lsd"),
        "anonymous_mode": on("network.anonymous"),
        "encryption": match setting(snapshot, "network.encryption").as_str() {
            "required" => 1,
            "disabled" => 2,
            _ => 0,
        },
        "bittorrent_protocol": match setting(snapshot, "network.utp").as_str() {
            "disabled" => 1,
            _ => 0,
        },
        "max_connec": number(snapshot, "network.max_connections"),
        "max_connec_per_torrent": number(snapshot, "network.max_peers"),
        "dl_limit": snapshot.rate_limits.0,
        "up_limit": snapshot.rate_limits.1,
        "alt_dl_limit": 0,
        "alt_up_limit": 0,
        "scheduler_enabled": false,
        "queueing_enabled": false,
        "max_active_downloads": -1,
        "max_active_uploads": -1,
        "max_active_torrents": -1,
        "max_ratio_enabled": false,
        "max_ratio": -1,
        "max_seeding_time_enabled": false,
        "max_seeding_time": -1,
        "auto_tmm_enabled": false,
        "torrent_content_layout": "Original",
        "add_stopped_enabled": !on("console.start_on_add"),
        "start_paused_enabled": !on("console.start_on_add"),
        "use_subcategories": false,
        "web_ui_address": "",
        "web_ui_port": 0,
        "current_network_interface": snapshot.network.interface.clone().unwrap_or_default(),
    })
}

async fn preferences(State(state): State<Shared>) -> Response {
    json_answer(preferences_json(&state.engine.snapshot()))
}

/// A preference sent as JSON, as the setting it is in EmpireBT and its
/// value there; `None` for those EmpireBT does not have.
fn preference_setting(key: &str, value: &Value) -> Option<(&'static str, String)> {
    let flag = |value: &Value| value.as_bool().map(|on| on.to_string());
    let whole = |value: &Value| value.as_i64().map(|number| number.max(0) as u64);
    Some(match key {
        "listen_port" => ("network.port", whole(value)?.to_string()),
        "dht" => ("dht.enabled", flag(value)?),
        "lsd" => ("network.lsd", flag(value)?),
        "anonymous_mode" => ("network.anonymous", flag(value)?),
        "encryption" => (
            "network.encryption",
            match value.as_i64()? {
                1 => "required",
                2 => "disabled",
                _ => "enabled",
            }
            .to_owned(),
        ),
        "bittorrent_protocol" => (
            "network.utp",
            match value.as_i64()? {
                1 => "disabled",
                2 => "preferred",
                _ => "enabled",
            }
            .to_owned(),
        ),
        "max_connec" => ("network.max_connections", whole(value)?.to_string()),
        "max_connec_per_torrent" => ("network.max_peers", whole(value)?.to_string()),
        // Bytes a second there, KiB a second here.
        "dl_limit" => (
            "network.download_limit",
            whole(value)?.div_ceil(1024).to_string(),
        ),
        "up_limit" => (
            "network.upload_limit",
            whole(value)?.div_ceil(1024).to_string(),
        ),
        "add_stopped_enabled" | "start_paused_enabled" => {
            ("console.start_on_add", (!value.as_bool()?).to_string())
        }
        _ => return None,
    })
}

/// Changes the preferences EmpireBT has, leaving out what it does not,
/// as qBittorrent leaves out unknown ones.
async fn set_preferences(State(state): State<Shared>, params: Params) -> Response {
    let Some(Ok(Value::Object(sent))) = params.get("json").map(serde_json::from_str::<Value>)
    else {
        return text(StatusCode::BAD_REQUEST, "json is missing or not an object");
    };
    let snapshot = state.engine.snapshot();
    let commands = sent
        .iter()
        .filter_map(|(key, value)| preference_setting(key, value))
        .filter(|(key, value)| api::editable_from_web(key) && setting(&snapshot, key) != *value)
        .map(|(key, value)| Command::ChangeSetting {
            key: key.to_owned(),
            value,
        })
        .collect();
    run_all(&state, commands).await
}

async fn default_save_path(State(state): State<Shared>) -> Response {
    text(
        StatusCode::OK,
        api::slashed(&state.engine.snapshot().download_dir),
    )
}

// ----- transfer -------------------------------------------------------------------

/// Speeds, limits, and the network, as `transfer/info` and the sync's
/// `server_state` share them.
fn transfer_json(snapshot: &Snapshot) -> Map<String, Value> {
    let network = &snapshot.network;
    let connected =
        matches!(network.seed_status, SeedStatus::Listening { .. }) && network.interface_up;
    let Value::Object(map) = json!({
        "connection_status": if connected { "connected" } else { "disconnected" },
        "dht_nodes": network.dht_nodes.unwrap_or(0),
        "dl_info_speed": snapshot.totals.0 as u64,
        "up_info_speed": snapshot.totals.1 as u64,
        "dl_info_data": 0,
        "up_info_data": 0,
        "dl_rate_limit": snapshot.rate_limits.0,
        "up_rate_limit": snapshot.rate_limits.1,
    }) else {
        unreachable!("an object")
    };
    map
}

fn server_state(snapshot: &Snapshot) -> Map<String, Value> {
    let mut map = transfer_json(snapshot);
    for (key, value) in [
        ("alltime_dl", json!(0)),
        ("alltime_ul", json!(0)),
        ("global_ratio", json!("0.00")),
        ("free_space_on_disk", json!(0)),
        ("queueing", json!(false)),
        ("use_alt_speed_limits", json!(false)),
        ("use_subcategories", json!(false)),
        ("refresh_interval", json!(1500)),
        (
            "total_peer_connections",
            json!(snapshot.network.seeding_peers),
        ),
        ("total_wasted_session", json!(0)),
        ("average_time_queue", json!(0)),
        ("queued_io_jobs", json!(0)),
        ("total_buffers_size", json!(0)),
        ("total_queued_size", json!(0)),
        ("read_cache_hits", json!("0")),
        ("read_cache_overload", json!("0")),
        ("write_cache_overload", json!("0")),
    ] {
        map.insert(key.to_owned(), value);
    }
    map
}

async fn transfer_info(State(state): State<Shared>) -> Response {
    json_answer(Value::Object(transfer_json(&state.engine.snapshot())))
}

async fn speed_limits_mode() -> Response {
    // EmpireBT has no alternative speed limits.
    text(StatusCode::OK, "0")
}

async fn download_limit(State(state): State<Shared>) -> Response {
    text(
        StatusCode::OK,
        state.engine.snapshot().rate_limits.0.to_string(),
    )
}

async fn upload_limit(State(state): State<Shared>) -> Response {
    text(
        StatusCode::OK,
        state.engine.snapshot().rate_limits.1.to_string(),
    )
}

async fn set_limit(state: &Shared, params: &Params, key: &str) -> Response {
    let Some(limit) = params
        .get("limit")
        .and_then(|limit| limit.parse::<i64>().ok())
    else {
        return text(StatusCode::BAD_REQUEST, "limit is missing");
    };
    let kib = (limit.max(0) as u64).div_ceil(1024);
    run_all(
        state,
        vec![Command::ChangeSetting {
            key: key.to_owned(),
            value: kib.to_string(),
        }],
    )
    .await
}

async fn set_download_limit(State(state): State<Shared>, params: Params) -> Response {
    set_limit(&state, &params, "network.download_limit").await
}

async fn set_upload_limit(State(state): State<Shared>, params: Params) -> Response {
    set_limit(&state, &params, "network.upload_limit").await
}

// ----- torrents as qBittorrent shows them --------------------------------------------

/// qBittorrent's state for a torrent.
fn state_name(torrent: &TorrentView) -> &'static str {
    let complete = torrent.fraction.is_some_and(|fraction| fraction >= 1.0);
    match torrent.status {
        Status::Queued => {
            if complete {
                "queuedUP"
            } else {
                "queuedDL"
            }
        }
        Status::Metadata => "metaDL",
        Status::Checking => {
            if complete {
                "checkingUP"
            } else {
                "checkingDL"
            }
        }
        Status::Downloading if torrent.down_rate > 0.0 => "downloading",
        Status::Announcing | Status::Downloading | Status::Waiting | Status::Retrying => {
            "stalledDL"
        }
        Status::Seeding if torrent.up_rate > 0.0 => "uploading",
        Status::Seeding => "stalledUP",
        Status::Pausing | Status::Paused => {
            if complete {
                "stoppedUP"
            } else {
                "stoppedDL"
            }
        }
        Status::Error => {
            if torrent
                .error
                .as_deref()
                .is_some_and(|error| error.contains("missing"))
            {
                "missingFiles"
            } else {
                "error"
            }
        }
    }
}

/// Where its content is: its one file, or the folder that holds all its
/// files, else its save folder.
fn content_path(torrent: &TorrentView) -> String {
    let files = torrent.files();
    let root = match files.as_slice() {
        [] => None,
        [file] => Some(file.path.clone()),
        [first, rest @ ..] => first.path.iter().next().and_then(|top| {
            let top = std::path::Path::new(top);
            (first.path.components().count() > 1
                && rest.iter().all(|file| file.path.starts_with(top)))
            .then(|| top.to_owned())
        }),
    };
    match root {
        Some(root) => api::slashed(&torrent.save_dir.join(root)),
        None => api::slashed(&torrent.save_dir),
    }
}

fn magnet_uri(torrent: &TorrentView) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (byte as char).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect()
    };
    let mut uri = format!(
        "magnet:?xt=urn:btih:{}&dn={}",
        auth::hex(&torrent.hash),
        encode(&torrent.name)
    );
    for tracker in &torrent.trackers {
        uri.push_str("&tr=");
        uri.push_str(&encode(tracker));
    }
    uri
}

/// Trackers as `torrents/trackers` lists them after the DHT, PeX, and
/// local discovery rows: each with its status (2 working, 4 not working,
/// 1 not contacted yet).
fn tracker_rows(torrent: &TorrentView) -> Vec<Value> {
    let running = !matches!(
        torrent.status,
        Status::Paused | Status::Pausing | Status::Error
    );
    let failing = torrent
        .announce
        .as_ref()
        .filter(|status| status.trackers_ok == 0)
        .and_then(|status| status.last_error.clone());
    let count = |value: Option<u64>| value.map_or(-1, |value| value as i64);
    torrent
        .trackers
        .iter()
        .enumerate()
        .map(|(tier, url)| {
            let (status, message) = match (&failing, running) {
                (_, false) => (1, String::new()),
                (Some(error), true) => (4, error.clone()),
                (None, true) => (2, String::new()),
            };
            json!({
                "url": url,
                "tier": tier,
                "status": status,
                "msg": message,
                "num_peers": count(torrent.swarm.1),
                "num_seeds": count(torrent.swarm.0),
                "num_leeches": count(torrent.swarm.1),
                "num_downloaded": count(torrent.completed),
            })
        })
        .collect()
}

/// A torrent as `torrents/info` and the sync describe it. What EmpireBT
/// does not keep (when it was added, totals across runs, share limits)
/// reads as qBittorrent's "none".
fn torrent_object(torrent: &TorrentView) -> Map<String, Value> {
    let info = torrent.info.as_ref();
    let size = torrent.size().unwrap_or(0);
    let left = torrent.remaining();
    let progress = torrent.fraction.unwrap_or(0.0);
    let eta = if torrent.down_rate >= 1.0 && left > 0 {
        (left as f64 / torrent.down_rate) as u64
    } else {
        NO_ETA
    };
    let v1 = info.is_none_or(|info| info.format() != empirebt_backend::TorrentFormat::V2);
    let tracker = match &torrent.announce {
        Some(status) if status.trackers_ok > 0 => torrent.trackers.first().cloned(),
        _ => None,
    };
    let Value::Object(map) = json!({
        "hash": auth::hex(&torrent.hash),
        "infohash_v1": if v1 { auth::hex(&torrent.hash) } else { String::new() },
        "infohash_v2": info
            .and_then(|info| info.v2_info_hash())
            .map(|hash| auth::hex(hash))
            .unwrap_or_default(),
        "name": torrent.name,
        "size": size,
        "total_size": size,
        "progress": progress,
        "amount_left": left,
        "completed": size - left,
        "downloaded": 0,
        "uploaded": torrent.announce.as_ref().map_or(0, |status| status.uploaded),
        "downloaded_session": 0,
        "uploaded_session": torrent.announce.as_ref().map_or(0, |status| status.uploaded),
        "dlspeed": torrent.down_rate as u64,
        "upspeed": torrent.up_rate as u64,
        "eta": eta,
        "state": state_name(torrent),
        "category": torrent.category.clone().unwrap_or_default(),
        "tags": torrent.tags.join(", "),
        "save_path": api::slashed(&torrent.save_dir),
        "download_path": "",
        "content_path": content_path(torrent),
        "root_path": content_path(torrent),
        "has_metadata": info.is_some(),
        "magnet_uri": magnet_uri(torrent),
        "private": info.is_some_and(|info| info.is_private()),
        "comment": "",
        "num_seeds": 0,
        "num_leechs": torrent.peers.unwrap_or(0),
        "num_complete": torrent.swarm.0.unwrap_or(0),
        "num_incomplete": torrent.swarm.1.unwrap_or(0),
        "connections_count": torrent.peers.unwrap_or(0),
        "connections_limit": 0,
        "piece_size": info.map_or(0, |info| info.piece_length()),
        "pieces_num": info.map_or(0, |info| info.piece_count().max(info.v2_piece_count())),
        "pieces_have": torrent.verified_pieces.unwrap_or(0),
        "tracker": tracker.unwrap_or_default(),
        "trackers_count": torrent.trackers.len(),
        "ratio": 0,
        "added_on": 0,
        "completion_on": 0,
        "last_activity": 0,
        "seen_complete": 0,
        "time_active": 0,
        "seeding_time": 0,
        "reannounce": torrent.announce.as_ref().map_or(0, |status| {
            status
                .next_announce
                .saturating_duration_since(Instant::now())
                .as_secs()
        }),
        "availability": -1,
        "popularity": 0,
        "priority": 0,
        "dl_limit": 0,
        "up_limit": 0,
        "max_ratio": -1,
        "max_seeding_time": -1,
        "max_inactive_seeding_time": -1,
        "ratio_limit": -2,
        "seeding_time_limit": -2,
        "inactive_seeding_time_limit": -2,
        "share_limit_action": "Default",
        "seq_dl": false,
        "f_l_piece_prio": false,
        "force_start": false,
        "super_seeding": false,
        "auto_tmm": false,
        "total_wasted": 0,
        "created_by": "",
        "creation_date": 0,
    }) else {
        unreachable!("an object")
    };
    map
}

fn categories_json(snapshot: &Snapshot) -> Map<String, Value> {
    snapshot
        .categories
        .iter()
        .map(|name| {
            let folder = snapshot
                .category_folders
                .get(name)
                .map(|folder| api::slashed(folder))
                .unwrap_or_default();
            (name.clone(), json!({ "name": name, "savePath": folder }))
        })
        .collect()
}

/// Each tracker URL with the torrents that use it.
fn trackers_map(snapshot: &Snapshot) -> HashMap<String, Vec<String>> {
    let mut trackers: HashMap<String, Vec<String>> = HashMap::new();
    for torrent in &snapshot.torrents {
        for url in &torrent.trackers {
            trackers
                .entry(url.clone())
                .or_default()
                .push(auth::hex(&torrent.hash));
        }
    }
    trackers
}

// ----- the incremental sync -----------------------------------------------------

/// What one client was last sent, so the next sync carries only changes.
#[derive(Default)]
struct SyncState {
    rid: u64,
    seen: Option<Instant>,
    torrents: HashMap<String, Map<String, Value>>,
    categories: Map<String, Value>,
    tags: Vec<String>,
    server: Map<String, Value>,
    trackers: HashMap<String, Vec<String>>,
}

/// Each signed-in client's sync state, by its credential.
#[derive(Default)]
pub(crate) struct Syncs(Mutex<HashMap<auth::Hash, SyncState>>);

/// The fields of `now` that differ from `before`.
fn changed(before: &Map<String, Value>, now: &Map<String, Value>) -> Map<String, Value> {
    now.iter()
        .filter(|(key, value)| before.get(*key) != Some(*value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// `sync/maindata` for the client known by `client`: everything when
/// `rid` is not the one it was last given, else what changed since.
fn sync(state: &WebState, client: auth::Hash, rid: u64) -> Value {
    let snapshot = state.engine.snapshot();
    let torrents: HashMap<String, Map<String, Value>> = snapshot
        .torrents
        .iter()
        .map(|torrent| (auth::hex(&torrent.hash), torrent_object(torrent)))
        .collect();
    let categories = categories_json(&snapshot);
    let tags = snapshot.tags.clone();
    let server = server_state(&snapshot);
    let trackers = trackers_map(&snapshot);

    let mut syncs = api::lock(&state.qbittorrent.0);
    if !syncs.contains_key(&client)
        && syncs.len() >= MAX_SYNC_CLIENTS
        && let Some(oldest) = syncs
            .iter()
            .min_by_key(|(_, sync)| sync.seen)
            .map(|(key, _)| *key)
    {
        syncs.remove(&oldest);
    }
    let last = syncs.entry(client).or_default();
    let full = rid == 0 || rid != last.rid;
    let mut out = Map::new();
    out.insert("rid".to_owned(), json!(last.rid + 1));
    out.insert("full_update".to_owned(), json!(full));
    if full {
        out.insert("torrents".to_owned(), json!(torrents));
        out.insert("categories".to_owned(), Value::Object(categories.clone()));
        out.insert("tags".to_owned(), json!(tags));
        out.insert("server_state".to_owned(), Value::Object(server.clone()));
        out.insert("trackers".to_owned(), json!(trackers));
    } else {
        let mut torrents_changed = Map::new();
        for (hash, torrent) in &torrents {
            let fields = match last.torrents.get(hash) {
                Some(before) => changed(before, torrent),
                None => torrent.clone(),
            };
            if !fields.is_empty() {
                torrents_changed.insert(hash.clone(), Value::Object(fields));
            }
        }
        let removed: Vec<&String> = last
            .torrents
            .keys()
            .filter(|hash| !torrents.contains_key(*hash))
            .collect();
        let categories_changed = changed(&last.categories, &categories);
        let categories_removed: Vec<&String> = last
            .categories
            .keys()
            .filter(|name| !categories.contains_key(*name))
            .collect();
        let tags_added: Vec<&String> = tags.iter().filter(|tag| !last.tags.contains(tag)).collect();
        let tags_removed: Vec<&String> =
            last.tags.iter().filter(|tag| !tags.contains(tag)).collect();
        let server_changed = changed(&last.server, &server);
        let trackers_changed: HashMap<&String, &Vec<String>> = trackers
            .iter()
            .filter(|(url, hashes)| last.trackers.get(*url) != Some(*hashes))
            .collect();
        let trackers_removed: Vec<&String> = last
            .trackers
            .keys()
            .filter(|url| !trackers.contains_key(*url))
            .collect();
        let mut put = |key: &str, value: Value, empty: bool| {
            if !empty {
                out.insert(key.to_owned(), value);
            }
        };
        put(
            "torrents",
            json!(torrents_changed),
            torrents_changed.is_empty(),
        );
        put("torrents_removed", json!(removed), removed.is_empty());
        put(
            "categories",
            json!(categories_changed),
            categories_changed.is_empty(),
        );
        put(
            "categories_removed",
            json!(categories_removed),
            categories_removed.is_empty(),
        );
        put("tags", json!(tags_added), tags_added.is_empty());
        put("tags_removed", json!(tags_removed), tags_removed.is_empty());
        put(
            "server_state",
            json!(server_changed),
            server_changed.is_empty(),
        );
        put(
            "trackers",
            json!(trackers_changed),
            trackers_changed.is_empty(),
        );
        put(
            "trackers_removed",
            json!(trackers_removed),
            trackers_removed.is_empty(),
        );
    }
    *last = SyncState {
        rid: last.rid + 1,
        seen: Some(Instant::now()),
        torrents,
        categories,
        tags,
        server,
        trackers,
    };
    Value::Object(out)
}

/// Who is asking, for their sync state: the hash of their session cookie
/// or API token, never the secret itself.
fn client_key(headers: &HeaderMap) -> auth::Hash {
    let secret = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| token.trim().to_owned())
        .or_else(|| api::cookie(headers, COOKIE))
        .unwrap_or_default();
    auth::token_hash(&secret)
}

async fn maindata(State(state): State<Shared>, headers: HeaderMap, params: Params) -> Response {
    let rid = params
        .get("rid")
        .and_then(|rid| rid.parse().ok())
        .unwrap_or(0);
    json_answer(sync(&state, client_key(&headers), rid))
}

/// A torrent's peers, always in full (`full_update`), which clients take
/// as they take a partial update.
async fn torrent_peers(State(state): State<Shared>, params: Params) -> Response {
    let snapshot = state.engine.snapshot();
    let hash = match one(&snapshot, &params) {
        Ok(hash) => hash,
        Err(response) => return response,
    };
    let engine = state.engine.clone();
    let peers = match tokio::task::spawn_blocking(move || engine.call(Command::Peers(hash))).await {
        Ok(Ok(Reply::Peers(peers))) => peers,
        _ => Vec::new(),
    };
    let peers: Map<String, Value> = peers
        .iter()
        .map(|peer| {
            let info = &peer.info;
            let flags = info.flags();
            let letters: Vec<String> = flags.iter().map(|(flag, _)| flag.to_string()).collect();
            let meanings: Vec<String> = flags
                .iter()
                .map(|(flag, meaning)| format!("{flag} = {meaning}"))
                .collect();
            (
                info.address.to_string(),
                json!({
                    "ip": info.address.ip().to_string(),
                    "port": info.address.port(),
                    "client": info.client.clone().unwrap_or_default(),
                    "peer_id_client": "",
                    "connection": if info.utp { "μTP" } else { "BT" },
                    "flags": letters.join(" "),
                    "flags_desc": meanings.join("\n"),
                    "country_code": peer.country_code().unwrap_or_default().to_ascii_lowercase(),
                    "country": "",
                    "progress": info.progress.unwrap_or(0.0),
                    "dl_speed": info.down_rate as u64,
                    "up_speed": info.up_rate as u64,
                    "downloaded": info.downloaded,
                    "uploaded": info.uploaded,
                    "relevance": 0,
                    "files": "",
                }),
            )
        })
        .collect();
    let rid = params
        .get("rid")
        .and_then(|rid| rid.parse::<u64>().ok())
        .unwrap_or(0);
    json_answer(json!({
        "rid": rid + 1,
        "full_update": true,
        "show_flags": true,
        "peers": peers,
    }))
}

// ----- listing and details ----------------------------------------------------

/// Whether `torrent` passes `torrents/info`'s `filter`.
fn passes(filter: &str, torrent: &Map<String, Value>) -> bool {
    let state = torrent["state"].as_str().unwrap_or_default();
    let speed = |key: &str| torrent[key].as_u64().unwrap_or(0);
    let active = speed("dlspeed") > 0 || speed("upspeed") > 0;
    let stopped = matches!(state, "stoppedDL" | "stoppedUP");
    match filter {
        "downloading" => matches!(
            state,
            "downloading" | "stalledDL" | "metaDL" | "queuedDL" | "checkingDL" | "forcedDL"
        ),
        "seeding" => matches!(
            state,
            "uploading" | "stalledUP" | "queuedUP" | "checkingUP" | "forcedUP"
        ),
        "completed" => torrent["progress"].as_f64().unwrap_or(0.0) >= 1.0,
        "stopped" | "paused" => stopped,
        "running" | "resumed" => !stopped,
        "active" => active,
        "inactive" => !active,
        "stalled" => matches!(state, "stalledUP" | "stalledDL"),
        "stalled_uploading" => state == "stalledUP",
        "stalled_downloading" => state == "stalledDL",
        "checking" => matches!(state, "checkingUP" | "checkingDL" | "checkingResumeData"),
        "errored" => matches!(state, "error" | "missingFiles"),
        _ => true,
    }
}

fn compare(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&right.as_f64().unwrap_or(0.0)),
        (Value::String(left), Value::String(right)) => {
            left.to_lowercase().cmp(&right.to_lowercase())
        }
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        _ => std::cmp::Ordering::Equal,
    }
}

async fn torrents_info(State(state): State<Shared>, params: Params) -> Response {
    let snapshot = state.engine.snapshot();
    let wanted: Option<HashSet<InfoHash>> = params
        .get("hashes")
        .map(|_| hashes(&snapshot, &params).into_iter().collect());
    let filter = params.get("filter").unwrap_or("all");
    let mut list: Vec<Map<String, Value>> = snapshot
        .torrents
        .iter()
        .filter(|torrent| {
            wanted
                .as_ref()
                .is_none_or(|wanted| wanted.contains(&torrent.hash))
        })
        .filter(|torrent| {
            params
                .get("category")
                .is_none_or(|category| torrent.category.as_deref().unwrap_or_default() == category)
        })
        .filter(|torrent| {
            params.get("tag").is_none_or(|tag| {
                if tag.is_empty() {
                    torrent.tags.is_empty()
                } else {
                    torrent.tags.iter().any(|own| own == tag)
                }
            })
        })
        .map(|torrent| {
            let mut object = torrent_object(torrent);
            if params.flag("includeTrackers") {
                object.insert("trackers".to_owned(), json!(tracker_rows(torrent)));
            }
            object
        })
        .filter(|object| passes(filter, object))
        .collect();
    if let Some(key) = params.get("sort") {
        list.sort_by(|left, right| {
            compare(
                left.get(key).unwrap_or(&Value::Null),
                right.get(key).unwrap_or(&Value::Null),
            )
        });
    }
    if params.flag("reverse") {
        list.reverse();
    }
    let offset = params
        .get("offset")
        .and_then(|offset| offset.parse::<i64>().ok())
        .unwrap_or(0);
    let offset = if offset < 0 {
        list.len().saturating_sub(offset.unsigned_abs() as usize)
    } else {
        offset as usize
    };
    let limit = params
        .get("limit")
        .and_then(|limit| limit.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(usize::MAX);
    let list: Vec<Value> = list
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(Value::Object)
        .collect();
    json_answer(Value::Array(list))
}

async fn properties(State(state): State<Shared>, params: Params) -> Response {
    let snapshot = state.engine.snapshot();
    let torrent = match one(&snapshot, &params) {
        Ok(hash) => snapshot.torrent(&hash).expect("checked"),
        Err(response) => return response,
    };
    let object = torrent_object(torrent);
    let field = |key: &str| object.get(key).cloned().unwrap_or(Value::Null);
    json_answer(json!({
        "hash": field("hash"),
        "infohash_v1": field("infohash_v1"),
        "infohash_v2": field("infohash_v2"),
        "name": field("name"),
        "save_path": field("save_path"),
        "download_path": "",
        "comment": "",
        "created_by": "",
        "creation_date": -1,
        "addition_date": 0,
        "completion_date": -1,
        "last_seen": -1,
        "is_private": field("private"),
        "piece_size": field("piece_size"),
        "pieces_num": field("pieces_num"),
        "pieces_have": field("pieces_have"),
        "total_size": field("total_size"),
        "total_downloaded": 0,
        "total_downloaded_session": 0,
        "total_uploaded": field("uploaded"),
        "total_uploaded_session": field("uploaded_session"),
        "total_wasted": 0,
        "dl_speed": field("dlspeed"),
        "up_speed": field("upspeed"),
        "dl_speed_avg": 0,
        "up_speed_avg": 0,
        "dl_limit": -1,
        "up_limit": -1,
        "eta": field("eta"),
        "nb_connections": field("connections_count"),
        "nb_connections_limit": -1,
        "peers": field("num_leechs"),
        "peers_total": field("num_incomplete"),
        "seeds": 0,
        "seeds_total": field("num_complete"),
        "share_ratio": 0,
        "seeding_time": 0,
        "time_elapsed": 0,
        "reannounce": field("reannounce"),
    }))
}

/// Its files, as qBittorrent lists them: paths from the save folder,
/// with the pieces each spans; progress is the torrent's.
async fn files(State(state): State<Shared>, params: Params) -> Response {
    let snapshot = state.engine.snapshot();
    let torrent = match one(&snapshot, &params) {
        Ok(hash) => snapshot.torrent(&hash).expect("checked"),
        Err(response) => return response,
    };
    let Some(info) = &torrent.info else {
        return json_answer(json!([]));
    };
    let piece = u64::from(info.piece_length()).max(1);
    let progress = torrent.fraction.unwrap_or(0.0);
    let mut offset = 0_u64;
    let mut index = 0;
    let mut list = Vec::new();
    for file in info.files() {
        let start = offset;
        offset += file.length();
        if file.is_padding() {
            continue;
        }
        let path = torrent
            .renames
            .get(file.path())
            .cloned()
            .unwrap_or_else(|| file.path().to_owned());
        let last = if file.length() == 0 {
            start / piece
        } else {
            (offset - 1) / piece
        };
        list.push(json!({
            "index": index,
            "name": api::slashed(&path),
            "size": file.length(),
            "progress": progress,
            "priority": 1,
            "is_seed": progress >= 1.0,
            "piece_range": [start / piece, last],
            "availability": -1,
        }));
        index += 1;
    }
    json_answer(Value::Array(list))
}

async fn trackers(State(state): State<Shared>, params: Params) -> Response {
    let snapshot = state.engine.snapshot();
    let torrent = match one(&snapshot, &params) {
        Ok(hash) => snapshot.torrent(&hash).expect("checked"),
        Err(response) => return response,
    };
    let private = torrent.info.as_ref().is_some_and(|info| info.is_private());
    let network = &snapshot.network;
    let pseudo = |url: &str, on: bool| {
        json!({
            "url": url,
            "tier": -1,
            "status": if on && !private { 2 } else { 0 },
            "msg": if private { "This torrent is private" } else { "" },
            "num_peers": 0,
            "num_seeds": 0,
            "num_leeches": 0,
            "num_downloaded": 0,
        })
    };
    let mut rows = vec![
        pseudo("** [DHT] **", network.dht_nodes.is_some()),
        pseudo("** [PeX] **", true),
        pseudo("** [LSD] **", network.lsd),
    ];
    rows.extend(tracker_rows(torrent));
    json_answer(Value::Array(rows))
}

async fn webseeds(State(state): State<Shared>, params: Params) -> Response {
    match one(&state.engine.snapshot(), &params) {
        Ok(_) => json_answer(json!([])),
        Err(response) => response,
    }
}

// ----- changing torrents --------------------------------------------------------

fn content_layout(text: Option<&str>) -> ContentLayout {
    match text {
        Some("Subfolder") => ContentLayout::Subfolder,
        Some("NoSubfolder") => ContentLayout::NoSubfolder,
        _ => ContentLayout::Original,
    }
}

/// `torrents/add`: links (`urls`, one a line) and `.torrent` uploads
/// (`torrents`), as a multipart form or a plain one.
async fn add(State(state): State<Shared>, request: Request) -> Response {
    let multipart = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("multipart/form-data"));
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut uploads: Vec<Vec<u8>> = Vec::new();
    if multipart {
        let mut form = match axum::extract::Multipart::from_request(request, &()).await {
            Ok(form) => form,
            Err(rejection) => return text(StatusCode::BAD_REQUEST, rejection.body_text()),
        };
        loop {
            match form.next_field().await {
                Ok(Some(field)) => {
                    let name = field.name().unwrap_or_default().to_owned();
                    let is_file = field.file_name().is_some();
                    match field.bytes().await {
                        Ok(bytes) if name == "torrents" || is_file => uploads.push(bytes.to_vec()),
                        Ok(bytes) => {
                            fields.insert(name, String::from_utf8_lossy(&bytes).into_owned());
                        }
                        Err(error) => return text(StatusCode::BAD_REQUEST, error.body_text()),
                    }
                }
                Ok(None) => break,
                Err(error) => return text(StatusCode::BAD_REQUEST, error.body_text()),
            }
        }
    } else {
        match Params::from_request(request, &()).await {
            Ok(params) => fields = params.0,
            Err(response) => return response,
        }
    }
    let params = Params(fields);
    let snapshot = state.engine.snapshot();
    let save_dir = match api::allowed_save_dir(&snapshot, params.get("savepath")) {
        Ok(save_dir) => save_dir,
        Err(response) => return response,
    };
    let start = if params.flag("stopped") || params.flag("paused") {
        Some(false)
    } else if params.get("stopped") == Some("false") || params.get("paused") == Some("false") {
        Some(true)
    } else {
        None
    };
    let mut layout = content_layout(params.get("contentLayout"));
    // Before qBittorrent 4.3.2: whether to keep the top folder.
    match params.get("root_folder") {
        Some("true") => layout = ContentLayout::Subfolder,
        Some("false") => layout = ContentLayout::NoSubfolder,
        _ => {}
    }
    let mut sources: Vec<AddSource> = uploads.into_iter().map(AddSource::Torrent).collect();
    for line in params.get("urls").unwrap_or_default().lines() {
        let link = line.trim();
        if link.is_empty() {
            continue;
        }
        // Paths on the server are not for API clients to read.
        if !(link.starts_with("magnet:")
            || link.starts_with("http://")
            || link.starts_with("https://"))
        {
            return text(
                StatusCode::BAD_REQUEST,
                "add magnet links or .torrent URLs, or upload .torrent files",
            );
        }
        sources.push(AddSource::Link(link.to_owned()));
    }
    if sources.is_empty() {
        return text(StatusCode::BAD_REQUEST, "nothing to add");
    }
    let category = params
        .get("category")
        .map(str::trim)
        .filter(|category| !category.is_empty())
        .map(str::to_owned);
    let tags = names(params.get("tags"));
    let commands: Vec<Command> = sources
        .into_iter()
        .map(|source| {
            Command::AddWith(AddRequest {
                save_dir: save_dir.clone(),
                category: category.clone(),
                tags: tags.clone(),
                start,
                layout,
                ..AddRequest::new(source)
            })
        })
        .collect();
    let total = commands.len();
    let engine = state.engine.clone();
    let failures = tokio::task::spawn_blocking(move || {
        commands
            .into_iter()
            .filter_map(|command| engine.call(command).err())
            .map(|failure| failure.message)
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    if failures.len() == total {
        tracing::info!(target: "web", "qBittorrent API add failed: {}", failures.join("; "));
        return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Fails.");
    }
    text(StatusCode::OK, "Ok.")
}

/// Runs `command` for each torrent `hashes` names.
async fn each(state: &Shared, params: &Params, command: impl Fn(InfoHash) -> Command) -> Response {
    let commands = hashes(&state.engine.snapshot(), params)
        .into_iter()
        .map(command)
        .collect();
    run_all(state, commands).await
}

async fn delete(State(state): State<Shared>, params: Params) -> Response {
    if params.flag("deleteFiles") {
        each(&state, &params, Command::RemoveWithFiles).await
    } else {
        each(&state, &params, Command::Remove).await
    }
}

async fn stop(State(state): State<Shared>, params: Params) -> Response {
    each(&state, &params, Command::Pause).await
}

async fn start(State(state): State<Shared>, params: Params) -> Response {
    each(&state, &params, Command::Start).await
}

async fn recheck(State(state): State<Shared>, params: Params) -> Response {
    each(&state, &params, Command::Recheck).await
}

async fn reannounce(State(state): State<Shared>, params: Params) -> Response {
    each(&state, &params, Command::Reannounce).await
}

async fn rename(State(state): State<Shared>, params: Params) -> Response {
    let torrent = match one(&state.engine.snapshot(), &params) {
        Ok(hash) => hash,
        Err(response) => return response,
    };
    let name = params
        .get("name")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let Some(name) = name else {
        return text(StatusCode::BAD_REQUEST, "name is missing");
    };
    run_all(
        &state,
        vec![Command::RenameTorrent {
            torrent,
            name: Some(name),
        }],
    )
    .await
}

/// `renameFile` and `renameFolder`: `oldPath` to `newPath`, both from the
/// save folder, as EmpireBT's paths are.
async fn rename_path(State(state): State<Shared>, params: Params) -> Response {
    let torrent = match one(&state.engine.snapshot(), &params) {
        Ok(hash) => hash,
        Err(response) => return response,
    };
    let (Some(from), Some(to)) = (params.get("oldPath"), params.get("newPath")) else {
        return text(StatusCode::BAD_REQUEST, "oldPath and newPath are needed");
    };
    run_all(
        &state,
        vec![Command::RenamePath {
            torrent,
            from: from.into(),
            to: to.trim().into(),
        }],
    )
    .await
}

async fn set_category(State(state): State<Shared>, params: Params) -> Response {
    let category = params
        .get("category")
        .map(str::trim)
        .filter(|category| !category.is_empty())
        .map(str::to_owned);
    // qBittorrent refuses a category it does not have.
    if let Some(name) = &category
        && !state.engine.snapshot().categories.contains(name)
    {
        return text(StatusCode::CONFLICT, "Incorrect category name");
    }
    each(&state, &params, |torrent| Command::SetCategory {
        torrent,
        category: category.clone(),
    })
    .await
}

async fn categories(State(state): State<Shared>) -> Response {
    json_answer(Value::Object(categories_json(&state.engine.snapshot())))
}

/// A category's folder as it may be set from here; empty takes it away.
#[allow(clippy::result_large_err)]
fn category_commands(
    snapshot: &Snapshot,
    params: &Params,
    create: bool,
) -> Result<Vec<Command>, Response> {
    let Some(name) = params
        .get("category")
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return Err(text(StatusCode::BAD_REQUEST, "category is missing"));
    };
    let folder = api::allowed_save_dir(snapshot, params.get("savePath"))?;
    let mut commands = Vec::new();
    if create {
        commands.push(Command::CreateCategory(name.to_owned()));
    } else if !snapshot.categories.iter().any(|known| known == name) {
        return Err(text(StatusCode::CONFLICT, "Category does not exist"));
    }
    if folder.is_some() || !create {
        commands.push(Command::SetCategoryFolder {
            category: name.to_owned(),
            folder,
        });
    }
    Ok(commands)
}

async fn create_category(State(state): State<Shared>, params: Params) -> Response {
    match category_commands(&state.engine.snapshot(), &params, true) {
        Ok(commands) => run_all(&state, commands).await,
        Err(response) => response,
    }
}

async fn edit_category(State(state): State<Shared>, params: Params) -> Response {
    match category_commands(&state.engine.snapshot(), &params, false) {
        Ok(commands) => run_all(&state, commands).await,
        Err(response) => response,
    }
}

async fn remove_categories(State(state): State<Shared>, params: Params) -> Response {
    let known = state.engine.snapshot().categories.clone();
    let commands = params
        .get("categories")
        .unwrap_or_default()
        .split('\n')
        .map(str::trim)
        .filter(|name| known.iter().any(|known| known == name))
        .map(|name| Command::RemoveCategory(name.to_owned()))
        .collect();
    run_all(&state, commands).await
}

async fn tags(State(state): State<Shared>) -> Response {
    json_answer(json!(state.engine.snapshot().tags))
}

async fn create_tags(State(state): State<Shared>, params: Params) -> Response {
    let known = state.engine.snapshot().tags.clone();
    let commands = names(params.get("tags"))
        .into_iter()
        .filter(|tag| !known.contains(tag))
        .map(Command::CreateTag)
        .collect();
    run_all(&state, commands).await
}

async fn delete_tags(State(state): State<Shared>, params: Params) -> Response {
    let known = state.engine.snapshot().tags.clone();
    let commands = names(params.get("tags"))
        .into_iter()
        .filter(|tag| known.contains(tag))
        .map(Command::RemoveTag)
        .collect();
    run_all(&state, commands).await
}

/// Sets each named torrent's tags to what `change` makes of them.
async fn change_tags(
    state: &Shared,
    params: &Params,
    change: impl Fn(&[String], &[String]) -> Vec<String>,
) -> Response {
    let snapshot = state.engine.snapshot();
    let named = names(params.get("tags"));
    let commands = hashes(&snapshot, params)
        .into_iter()
        .filter_map(|hash| {
            let own = &snapshot.torrent(&hash)?.tags;
            let tags = change(own, &named);
            (tags != *own).then_some(Command::SetTags {
                torrent: hash,
                tags,
            })
        })
        .collect();
    run_all(state, commands).await
}

async fn add_tags(State(state): State<Shared>, params: Params) -> Response {
    change_tags(&state, &params, |own, named| {
        let mut tags = own.to_vec();
        tags.extend(named.iter().filter(|tag| !own.contains(tag)).cloned());
        tags
    })
    .await
}

async fn remove_tags(State(state): State<Shared>, params: Params) -> Response {
    // No tags named removes them all, as in qBittorrent.
    change_tags(&state, &params, |own, named| {
        if named.is_empty() {
            Vec::new()
        } else {
            own.iter()
                .filter(|tag| !named.contains(tag))
                .cloned()
                .collect()
        }
    })
    .await
}

async fn set_tags(State(state): State<Shared>, params: Params) -> Response {
    change_tags(&state, &params, |_, named| named.to_vec()).await
}
