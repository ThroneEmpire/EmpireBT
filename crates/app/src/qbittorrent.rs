//! Importing torrents from a running qBittorrent through its Web API:
//! `empirebt import qbittorrent`.
//!
//! Everything is read with the API's read-only requests (see the API's
//! documentation at
//! <https://github.com/qbittorrent/qBittorrent/wiki/WebUI-API-(qBittorrent-5.0)>):
//! the torrent list, categories and tags, and per torrent its files, piece
//! states, and exported `.torrent`. Nothing in qBittorrent is changed.
//!
//! Each torrent comes over where its files are now, with its category,
//! tags, stopped or running state, and verified pieces (trusted unless a
//! recheck is asked for). Torrents EmpireBT cannot handle faithfully yet
//! are skipped or flagged, with the reason.

use crate::labels::Labels;
use crate::{InfoHash, Store};
use empirebt_backend::{ClientConfig, MagnetLink, TorrentInfo, TransferState};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long one API request may take.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Largest API answer read, an exported `.torrent` included.
const MAX_ANSWER: u64 = 64 << 20;

/// A signed-in connection to qBittorrent's Web API.
#[derive(Debug, Clone)]
pub struct Connection {
    /// Like `http://127.0.0.1:8080`, without a trailing slash.
    base: String,
    /// The session cookie, `SID=...`, when signing in was needed.
    cookie: Option<String>,
    binding: empirebt_backend::Binding,
}

impl Connection {
    /// Signs in to the Web UI at `url`. Without a username, no sign-in is
    /// tried, for a qBittorrent that lets this computer in without one.
    pub fn open(
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        binding: empirebt_backend::Binding,
    ) -> Result<Self, String> {
        let base = url.trim().trim_end_matches('/').to_owned();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(format!("{url} is not an http:// or https:// address"));
        }
        let mut connection = Self {
            base,
            cookie: None,
            binding,
        };
        if let Some(username) = username {
            let body = format!(
                "username={}&password={}",
                form_encode(username),
                form_encode(password.unwrap_or_default())
            );
            let response = connection.send(
                "POST",
                "/api/v2/auth/login",
                &[("Content-Type", "application/x-www-form-urlencoded")],
                body.as_bytes(),
            )?;
            // qBittorrent before 5.2 answers 200 with "Ok." or "Fails.";
            // since 5.2, 204 when signed in and 401 when refused.
            match response.status {
                200..=299 if !response.body.starts_with(b"Fails") => {}
                200..=299 | 401 => {
                    return Err("qBittorrent refused the username or password".to_owned());
                }
                403 => {
                    return Err(
                        "qBittorrent has banned this address after failed sign-ins; try again \
                         later"
                            .to_owned(),
                    );
                }
                status => return Err(format!("signing in to qBittorrent failed (HTTP {status})")),
            }
            // The session cookie: `SID` before 5.2, `QBT_SID_<port>` since.
            connection.cookie = response
                .header("set-cookie")
                .and_then(|cookie| cookie.split(';').next())
                .map(str::to_owned);
        }
        // Signed in (or let in): the version answers.
        connection.version()?;
        Ok(connection)
    }

    /// qBittorrent's version, such as `v5.1.2`.
    pub fn version(&self) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.get("/api/v2/app/version")?)
            .trim()
            .to_owned())
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    fn get(&self, target: &str) -> Result<Vec<u8>, String> {
        let response = self.send("GET", target, &[], &[])?;
        match response.status {
            200 => Ok(response.body),
            403 => Err(
                "qBittorrent wants a username and password (use --username and --password)"
                    .to_owned(),
            ),
            status => Err(format!("qBittorrent answered {target} with HTTP {status}")),
        }
    }

    fn json(&self, target: &str) -> Result<Value, String> {
        serde_json::from_slice(&self.get(target)?)
            .map_err(|error| format!("qBittorrent's answer to {target} is not JSON: {error}"))
    }

    fn send(
        &self,
        method: &str,
        target: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Result<empirebt_backend::HttpResponse, String> {
        // qBittorrent refuses requests whose Referer is not its own address.
        let mut headers = vec![("Referer", self.base.as_str())];
        if let Some(cookie) = &self.cookie {
            headers.push(("Cookie", cookie));
        }
        headers.extend_from_slice(extra);
        empirebt_backend::http_request(
            method,
            &format!("{}{target}", self.base),
            &headers,
            body,
            TIMEOUT,
            MAX_ANSWER,
            &self.binding,
        )
        .map_err(|error| error.to_string())
    }
}

/// How to bring the torrents over.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Path prefixes to rewrite: qBittorrent's paths are as it sees them,
    /// so a qBittorrent in Docker reports `/downloads/...` for what this
    /// computer has elsewhere.
    pub maps: Vec<(String, PathBuf)>,
    /// Check every torrent's data instead of trusting qBittorrent's
    /// progress.
    pub recheck: bool,
}

/// What an import will do, or did.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// qBittorrent's address and version.
    pub source: String,
    /// Category names and their folders.
    pub categories: BTreeMap<String, Option<PathBuf>>,
    pub tags: BTreeSet<String>,
    pub torrents: Vec<Planned>,
}

/// One of qBittorrent's torrents.
#[derive(Debug, Clone)]
pub struct Planned {
    pub name: String,
    pub hash: Option<InfoHash>,
    /// Why it is left out, when it is.
    pub skip: Option<String>,
    /// Things to know about how it comes over.
    pub warnings: Vec<String>,
    pub folder: Option<PathBuf>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub state: TransferState,
    /// Verified pieces, when trusted.
    pub pieces: Option<Vec<bool>>,
    source: Option<Source>,
}

#[derive(Debug, Clone)]
enum Source {
    Metainfo(Vec<u8>),
    Magnet(String),
}

impl Planned {
    /// The verified share, when known.
    pub fn progress(&self) -> Option<f64> {
        let pieces = self.pieces.as_ref()?;
        (!pieces.is_empty())
            .then(|| pieces.iter().filter(|piece| **piece).count() as f64 / pieces.len() as f64)
    }
}

/// Asks qBittorrent for its torrents, categories, and tags, and plans
/// their import into `store`, which is left unchanged.
pub fn plan(
    qbittorrent: &Connection,
    options: &Options,
    store: &Store,
    config: &ClientConfig,
) -> Result<Plan, String> {
    let map = |text: &str| map_path(text, &options.maps);
    let preferences = qbittorrent.json("/api/v2/app/preferences")?;
    let default_save = preferences
        .get("save_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(map);
    let mut plan = Plan {
        source: format!(
            "qBittorrent {} at {}",
            qbittorrent.version()?,
            qbittorrent.url()
        ),
        ..Plan::default()
    };
    plan.tags = qbittorrent
        .json("/api/v2/torrents/tags")?
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|tag| crate::clean_name(tag).ok())
        .collect();
    plan.categories = read_categories(
        &qbittorrent.json("/api/v2/torrents/categories")?,
        &|text| map(text),
        default_save.as_deref(),
    );

    let (client, _) = store.load(config.clone())?;
    let labels = Labels::load(&store.labels_path())?;
    let list = qbittorrent.json("/api/v2/torrents/info")?;
    for torrent in list.as_array().map(Vec::as_slice).unwrap_or_default() {
        let mut planned = plan_torrent(qbittorrent, torrent, options, &|text| map(text))?;
        if let Some(hash) = planned.hash
            && client.torrent_index(&hash).is_some()
        {
            planned.skip = Some("already in EmpireBT".to_owned());
        }
        if let Some(category) = &planned.category {
            plan.categories.entry(category.clone()).or_insert(None);
        }
        plan.tags.extend(planned.tags.iter().cloned());
        plan.torrents.push(planned);
    }
    plan.torrents
        .sort_by_key(|torrent| torrent.name.to_lowercase());
    // Categories EmpireBT already has keep their folders.
    for (name, folder) in &mut plan.categories {
        if labels.folders.contains_key(name) {
            *folder = None;
        }
    }
    Ok(plan)
}

/// Reads `torrents/categories`: each category's `savePath`. A relative
/// path is inside the default save folder; an empty one means none.
fn read_categories(
    answer: &Value,
    map: &dyn Fn(&str) -> PathBuf,
    default_save: Option<&Path>,
) -> BTreeMap<String, Option<PathBuf>> {
    let mut categories = BTreeMap::new();
    for (name, category) in answer.as_object().into_iter().flatten() {
        // Subcategories (`Films/HD`) become categories of their own name.
        let Ok(name) = crate::clean_name(name) else {
            continue;
        };
        let path = category
            .get("savePath")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let folder = (!path.is_empty()).then(|| {
            let folder = map(path);
            match default_save {
                Some(base) if folder.is_relative() => base.join(folder),
                _ => folder,
            }
        });
        categories.insert(name, folder.filter(|folder| folder.is_absolute()));
    }
    categories
}

/// Plans one torrent from its `torrents/info` entry, asking for its files,
/// pieces, and `.torrent`.
fn plan_torrent(
    qbittorrent: &Connection,
    torrent: &Value,
    options: &Options,
    map: &dyn Fn(&str) -> PathBuf,
) -> Result<Planned, String> {
    let text = |key: &str| torrent.get(key).and_then(Value::as_str).unwrap_or_default();
    let id = text("hash");
    let state = text("state");
    let mut planned = Planned {
        name: text("name").to_owned(),
        hash: None,
        skip: None,
        warnings: Vec::new(),
        folder: None,
        category: crate::clean_name(text("category")).ok(),
        tags: text("tags")
            .split(',')
            .filter_map(|tag| crate::clean_name(tag).ok())
            .collect(),
        // qBittorrent 5 says "stopped", earlier versions "paused".
        state: if state.starts_with("stopped") || state.starts_with("paused") {
            TransferState::Paused
        } else {
            TransferState::Downloading
        },
        pieces: None,
        source: None,
    };
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        planned.skip = Some("qBittorrent gave it no usable hash".to_owned());
        return Ok(planned);
    }
    let has_metadata = torrent
        .get("has_metadata")
        .and_then(Value::as_bool)
        .unwrap_or(state != "metaDL");
    if !has_metadata {
        let magnet = text("magnet_uri");
        match MagnetLink::parse(magnet) {
            Ok(link) => {
                planned.hash = Some(*link.info_hash());
                planned.source = Some(Source::Magnet(magnet.to_owned()));
                planned.folder = Some(map(text("save_path")));
                planned
                    .warnings
                    .push("its metadata never arrived; it comes over as a magnet link".to_owned());
            }
            Err(error) => planned.skip = Some(format!("its magnet link is unusable: {error}")),
        }
        return Ok(planned);
    }

    // The exported .torrent carries the trackers, and a v2 torrent's piece
    // layers, which qBittorrent keeps outside its own copy.
    let metainfo = qbittorrent.get(&format!("/api/v2/torrents/export?hash={id}"))?;
    let info = TorrentInfo::from_bytes(&metainfo).map_err(|error| {
        format!(
            "qBittorrent's .torrent for {} is unusable: {error}",
            planned.name
        )
    })?;
    planned.hash = Some(*info.info_hash());
    planned.name = info.name().to_owned();
    planned.source = Some(Source::Metainfo(metainfo));

    // Files as qBittorrent has them now: renamed or skipped ones show.
    let files = qbittorrent.json(&format!("/api/v2/torrents/files?hash={id}"))?;
    let files = files.as_array().map(Vec::as_slice).unwrap_or_default();
    let expected = expected_paths(&info);
    let renamed: Vec<&str> = files
        .iter()
        .filter_map(|file| {
            let index = file.get("index")?.as_u64()? as usize;
            let name = file.get("name")?.as_str()?;
            (expected.get(index).is_some_and(|path| path != name)).then_some(name)
        })
        .collect();
    if !renamed.is_empty() {
        planned.skip = Some(format!(
            "files were renamed or moved inside the torrent in qBittorrent ({}), and EmpireBT \
             cannot follow that yet",
            renamed.join(", ")
        ));
        return Ok(planned);
    }

    // Where the files are now: in the incomplete folder until done.
    let folder = content_folder(text("content_path"), &info).or_else(|| {
        let save = text("save_path");
        (!save.is_empty()).then(|| save.to_owned())
    });
    let Some(folder) = folder
        .map(|folder| map(&folder))
        .filter(|folder| folder.is_absolute())
    else {
        planned.skip = Some("qBittorrent gave no full path for its files".to_owned());
        return Ok(planned);
    };
    planned.folder = Some(folder.clone());

    let skipped = files
        .iter()
        .filter(|file| file.get("priority").and_then(Value::as_i64) == Some(0))
        .count();
    if skipped > 0 {
        planned.state = TransferState::Paused;
        planned.warnings.push(format!(
            "{skipped} file{} skipped in qBittorrent; EmpireBT would download every file, so it \
             comes over paused",
            if skipped == 1 { " was" } else { "s were" }
        ));
    }
    let own_limit = |key: &str| {
        torrent
            .get(key)
            .and_then(Value::as_f64)
            .is_some_and(|limit| limit >= 0.0)
    };
    if own_limit("ratio_limit")
        || own_limit("seeding_time_limit")
        || own_limit("inactive_seeding_time_limit")
    {
        planned
            .warnings
            .push("its share limits are not kept (EmpireBT has none yet)".to_owned());
    }
    if matches!(state, "error" | "missingFiles") {
        planned.state = TransferState::Paused;
        planned.warnings.push(format!(
            "qBittorrent reports it as {}; it comes over paused",
            if state == "error" {
                "failing"
            } else {
                "missing its files"
            }
        ));
    }

    let states = qbittorrent.json(&format!("/api/v2/torrents/pieceStates?hash={id}"))?;
    let pieces: Vec<bool> = states
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|state| state.as_i64() == Some(2))
        .collect();
    // The content (a file, or the torrent's folder) shows whether paths
    // are right here; skipped files may never have been made.
    let content = folder.join(info.name());
    if pieces.iter().any(|piece| *piece) && !content.exists() {
        planned.state = TransferState::Paused;
        planned.warnings.push(format!(
            "its files are not where qBittorrent has them ({}); it comes over paused, to be \
             checked once the files are there (or use --map)",
            folder.display()
        ));
    } else if !options.recheck {
        planned.pieces = Some(pieces);
    }
    let complete = planned
        .pieces
        .as_ref()
        .is_some_and(|pieces| !pieces.is_empty() && pieces.iter().all(|piece| *piece));
    if planned.state == TransferState::Downloading && complete {
        planned.state = TransferState::Seeding;
    }
    Ok(planned)
}

/// Each file's path as qBittorrent names it in `torrents/files`, unless
/// renamed: `name/path` for a multi-file torrent, the name for a single
/// file. Padding files of hybrid torrents are not listed.
fn expected_paths(info: &TorrentInfo) -> Vec<String> {
    let slashed = |path: &Path| path.to_string_lossy().replace('\\', "/");
    if info.format() == empirebt_backend::TorrentFormat::V2 {
        let single = info.v2_files().len() == 1;
        return info
            .v2_files()
            .iter()
            .map(|file| {
                if single || file.path().starts_with(info.name()) {
                    slashed(file.path())
                } else {
                    slashed(&Path::new(info.name()).join(file.path()))
                }
            })
            .collect();
    }
    let files: Vec<_> = info
        .files()
        .iter()
        .filter(|file| !file.is_padding())
        .collect();
    if files.len() == 1 && files[0].path() == Path::new(info.name()) {
        return vec![info.name().to_owned()];
    }
    // Paths may already start with the torrent's folder.
    files
        .iter()
        .map(|file| {
            if file.path().starts_with(info.name()) {
                slashed(file.path())
            } else {
                slashed(&Path::new(info.name()).join(file.path()))
            }
        })
        .collect()
}

/// The folder holding a torrent's content: `content_path` is the file
/// itself for a single file, or the torrent's own folder otherwise.
fn content_folder(content: &str, info: &TorrentInfo) -> Option<String> {
    let content = content.trim_end_matches('/');
    let (folder, last) = content.rsplit_once('/')?;
    (last == info.name() && !folder.is_empty()).then(|| folder.to_owned())
}

/// Rewrites the first matching prefix of `text`.
fn map_path(text: &str, maps: &[(String, PathBuf)]) -> PathBuf {
    for (from, to) in maps {
        let from = from.trim_end_matches('/');
        if text == from {
            return to.clone();
        }
        if let Some(rest) = text
            .strip_prefix(from)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            return to.join(rest);
        }
    }
    PathBuf::from(text)
}

fn form_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// What an import changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Imported {
    pub torrents: usize,
    pub categories: usize,
    pub tags: usize,
}

/// Carries out `plan`: adds its torrents, categories, and tags to the
/// session in `store`, which no running EmpireBT may be using.
pub fn import(plan: &Plan, store: &Store, config: &ClientConfig) -> Result<Imported, String> {
    let (mut client, _) = store.load(config.clone())?;
    let mut labels = Labels::load(&store.labels_path())?;
    let mut imported = Imported::default();
    for (name, folder) in &plan.categories {
        if !labels.categories.contains(name) {
            imported.categories += 1;
        }
        match folder {
            Some(folder) if crate::labels::check_folder(folder).is_ok() => {
                labels.set_folder(name.clone(), Some(folder.clone()))
            }
            _ => {
                labels.categories.insert(name.clone());
            }
        }
    }
    for tag in &plan.tags {
        if labels.tags.insert(tag.clone()) {
            imported.tags += 1;
        }
    }
    for planned in plan
        .torrents
        .iter()
        .filter(|planned| planned.skip.is_none())
    {
        let (Some(source), Some(folder)) = (&planned.source, &planned.folder) else {
            continue;
        };
        let index = match source {
            Source::Metainfo(bytes) => {
                let info = TorrentInfo::from_bytes(bytes).map_err(|error| error.to_string())?;
                if client.torrent_index(info.info_hash()).is_some() {
                    continue;
                }
                client.add_torrent(info)
            }
            Source::Magnet(link) => {
                let link = MagnetLink::parse(link).map_err(|error| error.to_string())?;
                if client.torrent_index(link.info_hash()).is_some() {
                    continue;
                }
                client.add_magnet(link)
            }
        };
        client
            .set_save_dir(index, folder.clone())
            .map_err(|error| error.to_string())?;
        let running = planned.state != TransferState::Paused;
        let adopted = planned
            .pieces
            .as_ref()
            .is_some_and(|pieces| client.adopt_progress(index, pieces, running).is_ok());
        // Without trusted progress, a running torrent checks as it starts
        // and a paused one when EmpireBT opens.
        if !adopted && running {
            let _ = client.mark_started(index);
        }
        let Some(hash) = client.torrents()[index].info_hash().copied() else {
            continue;
        };
        if let Some(category) = &planned.category {
            labels.set_category(hash, Some(category.clone()));
        }
        if !planned.tags.is_empty() {
            labels.set_tags(hash, planned.tags.iter().cloned().collect());
        }
        imported.torrents += 1;
    }
    store.save(&client)?;
    labels.save(&store.labels_path())?;
    Ok(imported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_path_prefixes() {
        let maps = vec![("/downloads".to_owned(), PathBuf::from("/srv/torrents"))];
        assert_eq!(
            map_path("/downloads/films", &maps),
            PathBuf::from("/srv/torrents/films")
        );
        assert_eq!(
            map_path("/downloads", &maps),
            PathBuf::from("/srv/torrents")
        );
        assert_eq!(
            map_path("/downloadsx/a", &maps),
            PathBuf::from("/downloadsx/a")
        );
        assert_eq!(map_path("/other", &maps), PathBuf::from("/other"));
    }

    #[test]
    fn reads_category_folders() {
        let map = |text: &str| PathBuf::from(text);
        let answer: Value = serde_json::from_str(
            r#"{"Films": {"name": "Films", "savePath": "/media/films", "download_path": "/tmp"},
                "Empty": {"name": "Empty", "savePath": ""},
                "Relative": {"name": "Relative", "savePath": "rel"}}"#,
        )
        .unwrap();
        let categories = read_categories(&answer, &map, Some(Path::new("/downloads")));
        assert_eq!(categories["Films"], Some(PathBuf::from("/media/films")));
        assert_eq!(categories["Empty"], None);
        assert_eq!(
            categories["Relative"],
            Some(PathBuf::from("/downloads/rel"))
        );
    }

    #[test]
    fn encodes_form_values() {
        assert_eq!(form_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }
}
