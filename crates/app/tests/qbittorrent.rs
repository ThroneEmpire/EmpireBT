//! Importing from qBittorrent's Web API.
//!
//! The import itself is tested against real qBittorrents, opt in (they
//! need Docker, and libtorrent set up for interop):
//!
//! ```text
//! cargo test -p empirebt-app --test qbittorrent -- --ignored
//! EMPIREBT_QBITTORRENT_VERSIONS=5.3.0 cargo test -p empirebt-app --test qbittorrent -- --ignored
//! ```
//!
//! `interop/qbittorrent.py` starts each version with torrents in every case
//! the importer handles. Signing in, which qBittorrent 5.2 changed, is
//! also tested on every run, against a stand-in.

use empirebt_app::qbittorrent::{self, Connection, Options};
use empirebt_app::{Handle, Status, Store};
use empirebt_backend::{Binding, TransferState};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A stand-in for qBittorrent's sign-in, answering as `generation` does:
/// before 5.2, "200 Ok." with an `SID` cookie and "200 Fails."; since,
/// "204" with a `QBT_SID_<port>` cookie and "401". Other requests need
/// the cookie, as in qBittorrent.
fn sign_in_stand_in(new: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (cookie, accepted, refused) = if new {
        (
            "QBT_SID_8080=new-session",
            "204 No Content",
            ("401 Unauthorized", "Unauthorized"),
        )
    } else {
        ("SID=old-session", "200 OK", ("200 OK", "Fails."))
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    break;
                }
                headers.push_str(&line.to_ascii_lowercase());
            }
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let (status, extra, answer) = if request.starts_with("POST /api/v2/auth/login") {
                if body == b"username=admin&password=secret" {
                    let body = if new { "" } else { "Ok." };
                    (
                        accepted,
                        format!("Set-Cookie: {cookie}; HttpOnly; path=/\r\n"),
                        body,
                    )
                } else {
                    (refused.0, String::new(), refused.1)
                }
            } else if headers.contains(&format!("cookie: {}", cookie.to_ascii_lowercase())) {
                ("200 OK", String::new(), "v5.0.0")
            } else {
                ("403 Forbidden", String::new(), "Forbidden")
            };
            // Like qBittorrent, the connection stays open after answering.
            let mut stream = reader.into_inner();
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: keep-alive\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    base
}

#[test]
fn signs_in_to_qbittorrent_before_and_after_5_2() {
    for new in [false, true] {
        let server = sign_in_stand_in(new);
        let connection = Connection::open(&server, Some("admin"), Some("secret"), Binding::any())
            .unwrap_or_else(|error| panic!("new sign-in {new}: {error}"));
        assert_eq!(connection.version().unwrap(), "v5.0.0");
        let error =
            Connection::open(&server, Some("admin"), Some("wrong"), Binding::any()).unwrap_err();
        assert!(error.contains("refused"), "new sign-in {new}: {error}");
        let error = Connection::open(&server, None, None, Binding::any()).unwrap_err();
        assert!(error.contains("--username"), "new sign-in {new}: {error}");
    }
    let error = Connection::open("ftp://example", None, None, Binding::any()).unwrap_err();
    assert!(error.contains("http"), "{error}");
}

/// A real qBittorrent in Docker, stopped when dropped.
struct Live {
    process: Child,
    url: String,
    password: String,
    downloads: PathBuf,
    version: String,
}

impl Live {
    fn start(version: &str) -> Self {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/qbittorrent.py");
        let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_owned());
        let mut process = Command::new(python)
            .arg("-I")
            .arg(&script)
            .arg(version)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("python runs");
        let mut line = String::new();
        BufReader::new(process.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let ready: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("qBittorrent {version} did not start (see above)"));
        let text = |key: &str| ready[key].as_str().unwrap().to_owned();
        Self {
            url: text("url"),
            password: text("password"),
            downloads: PathBuf::from(text("downloads")),
            version: text("version"),
            process,
        }
    }

    fn connect(&self) -> Connection {
        Connection::open(
            &self.url,
            Some("admin"),
            Some(&self.password),
            Binding::any(),
        )
        .unwrap_or_else(|error| panic!("qBittorrent {}: {error}", self.version))
    }

    fn options(&self) -> Options {
        Options {
            maps: vec![("/downloads".to_owned(), self.downloads.clone())],
            recheck: false,
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // Closing its input stops the container.
        drop(self.process.stdin.take());
        let _ = self.process.wait();
    }
}

fn store(name: &str) -> (Store, PathBuf) {
    let root = std::env::temp_dir().join(format!("empirebt-qbt-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    (Store::open(root.join("data")).unwrap(), root)
}

#[test]
#[ignore = "needs Docker and libtorrent: cargo test -p empirebt-app --test qbittorrent -- --ignored"]
fn imports_from_real_qbittorrents() {
    let versions =
        std::env::var("EMPIREBT_QBITTORRENT_VERSIONS").unwrap_or_else(|_| "5.1.2,5.3.0".to_owned());
    for version in versions
        .split(',')
        .map(str::trim)
        .filter(|version| !version.is_empty())
    {
        let live = Live::start(version);
        assert_eq!(live.version, format!("v{version}"));
        imports_from(&live);
        recheck_leaves_progress_to_a_check(&live);
        files_out_of_reach_come_over_paused(&live);
        let error =
            Connection::open(&live.url, Some("admin"), Some("wrong"), Binding::any()).unwrap_err();
        assert!(error.contains("refused"), "{version}: {error}");
    }
}

fn imports_from(live: &Live) {
    let version = &live.version;
    let (store, root) = store(&format!("import-{version}"));
    let config = store.config().unwrap().client_config().unwrap();
    let qbittorrent = live.connect();
    let plan = qbittorrent::plan(&qbittorrent, &live.options(), &store, &config).unwrap();
    let downloads = &live.downloads;
    assert!(
        plan.source
            .starts_with(&format!("qBittorrent {version} at ")),
        "{}",
        plan.source
    );

    // Categories with their folders, mapped; all tags.
    assert_eq!(plan.categories["Linux"], Some(downloads.join("linux")));
    assert_eq!(plan.categories["Films"], Some(downloads.join("films")));
    assert_eq!(plan.categories["Empty"], None);
    assert_eq!(
        plan.tags.iter().map(String::as_str).collect::<Vec<_>>(),
        ["iso", "music", "unused"]
    );

    let torrent = |name: &str| {
        plan.torrents
            .iter()
            .find(|torrent| torrent.name == name)
            .unwrap_or_else(|| panic!("{version}: {name} is not in the plan"))
    };
    // Complete and seeding, in its category's folder.
    let mini = torrent("mini.iso");
    assert_eq!(mini.skip, None);
    assert_eq!(mini.state, TransferState::Seeding, "{version}");
    assert_eq!(mini.folder, Some(downloads.join("linux")));
    assert_eq!(mini.category.as_deref(), Some("Linux"));
    assert_eq!(mini.tags, ["iso"]);
    assert_eq!(mini.progress(), Some(1.0));
    // A renamed file, and a name of its own: both come over.
    let album = torrent("album");
    assert_eq!(album.skip, None, "{version}");
    assert_eq!(album.state, TransferState::Seeding, "{version}");
    assert_eq!(album.shown_name.as_deref(), Some("Album (renamed)"));
    assert_eq!(album.folder, Some(downloads.join("complete")));
    assert_eq!(
        album.renames,
        [(
            PathBuf::from("album/02.flac"),
            PathBuf::from("album/02 - renamed.flac")
        )]
        .into()
    );
    // Laid out without the torrent's folder.
    let flat = torrent("flat");
    assert_eq!(flat.skip, None, "{version}");
    assert_eq!(flat.state, TransferState::Seeding, "{version}");
    assert_eq!(flat.folder, Some(downloads.join("flat")));
    assert_eq!(
        flat.renames,
        [
            (PathBuf::from("flat/one.txt"), PathBuf::from("one.txt")),
            (PathBuf::from("flat/two.txt"), PathBuf::from("two.txt")),
        ]
        .into()
    );
    // Half done and stopped, its files still in the incomplete folder.
    let partial = torrent("partial.bin");
    assert_eq!(partial.state, TransferState::Paused);
    assert_eq!(partial.folder, Some(downloads.join("incomplete")));
    assert_eq!(partial.progress(), Some(0.5));
    // A skipped file: comes over paused, with a warning.
    let pair = torrent("pair");
    assert_eq!(pair.state, TransferState::Paused);
    assert!(
        pair.warnings
            .iter()
            .any(|warning| warning.contains("skipped")),
        "{version}: {:?}",
        pair.warnings
    );
    // A hybrid, which qBittorrent's API calls by its v2 hash.
    let hybrid = torrent("hybrid.dat");
    assert_eq!(hybrid.state, TransferState::Seeding);
    assert_eq!(hybrid.category.as_deref(), Some("Films"));
    // A magnet link whose metadata never arrived.
    let magnet = torrent("never-resolved");
    assert_eq!(magnet.state, TransferState::Paused);
    assert!(
        magnet
            .warnings
            .iter()
            .any(|warning| warning.contains("magnet"))
    );

    // Planning changes nothing.
    assert!(store.load(config.clone()).unwrap().0.torrents().is_empty());
    let imported = qbittorrent::import(&plan, &store, &config).unwrap();
    assert_eq!(imported.torrents, 7);
    assert_eq!(imported.categories, 3);
    assert_eq!(imported.tags, 3);

    // Trusted progress needs no check: the resume data matches the files.
    let (mut client, _) = store.load(config.clone()).unwrap();
    assert_eq!(client.torrents().len(), 7);
    let position = |client: &empirebt_backend::Client, name: &str| {
        client
            .torrents()
            .iter()
            .position(|torrent| torrent.name() == name)
            .unwrap()
    };
    let mini = position(&client, "mini.iso");
    assert!(client.resume_is_current(mini));
    assert_eq!(client.torrents()[mini].state(), TransferState::Seeding);
    // The exported .torrent brought the tracker qBittorrent had.
    let info = client.torrents()[mini].info().unwrap();
    assert!(
        info.tracker_urls()
            .iter()
            .any(|url| url == "http://tracker.example/announce"),
        "{version}: {:?}",
        info.tracker_urls()
    );
    // And the hybrid's piece layers: its data verifies, v1 and v2 both.
    let hybrid = position(&client, "hybrid.dat");
    let check = client.spawn_verify(hybrid).unwrap();
    assert_eq!(client.finish_transfer(check), Ok(4));
    // Renamed files are found under their names: trusted, and verified.
    for name in ["album", "flat"] {
        let index = position(&client, name);
        assert!(client.resume_is_current(index), "{version}: {name}");
        let check = client.spawn_verify(index).unwrap();
        assert!(client.finish_transfer(check).is_ok(), "{version}: {name}");
        assert_eq!(
            client.torrents()[index].state(),
            TransferState::Seeding,
            "{version}: {name}"
        );
    }
    assert_eq!(
        client.torrents()[position(&client, "album")].display_name(),
        "Album (renamed)"
    );
    let magnet = position(&client, "never-resolved");
    assert_eq!(
        client.torrents()[magnet].magnet().unwrap().trackers(),
        ["udp://tracker.example:6969/announce"]
    );

    // A second import finds everything already there.
    let again = qbittorrent::plan(&qbittorrent, &live.options(), &store, &config).unwrap();
    assert!(
        again
            .torrents
            .iter()
            .all(|torrent| torrent.skip.as_deref() == Some("already in EmpireBT"))
    );

    // EmpireBT opens with them where they were, in the states they had.
    let mut config_file = store.config().unwrap();
    config_file.set("console.start_on_add", "false").unwrap();
    let client_config = config
        .clone()
        .with_listen_port(0)
        .with_dht(false)
        .with_lsd(false);
    let handle = Handle::start(store.clone(), config_file, client_config, Vec::new()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mini, partial, snapshot) = loop {
        let snapshot = handle.snapshot();
        let find = |name: &str| {
            snapshot
                .torrents
                .iter()
                .find(|torrent| torrent.name == name)
                .cloned()
                .unwrap()
        };
        let (mini, partial) = (find("mini.iso"), find("partial.bin"));
        if (mini.status == Status::Seeding && partial.fraction == Some(0.5))
            || Instant::now() > deadline
        {
            break (mini, partial, snapshot);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(mini.status, Status::Seeding);
    assert_eq!(mini.category.as_deref(), Some("Linux"));
    assert_eq!(&mini.save_dir, &downloads.join("linux"));
    assert_eq!(partial.status, Status::Paused);
    assert_eq!(partial.fraction, Some(0.5));
    assert_eq!(
        snapshot.category_folders.get("Films"),
        Some(&downloads.join("films"))
    );
    handle.shutdown();
    let _ = std::fs::remove_dir_all(root);
}

fn recheck_leaves_progress_to_a_check(live: &Live) {
    let (store, root) = store(&format!("recheck-{}", live.version));
    let config = store.config().unwrap().client_config().unwrap();
    let options = Options {
        recheck: true,
        ..live.options()
    };
    let plan = qbittorrent::plan(&live.connect(), &options, &store, &config).unwrap();
    assert!(plan.torrents.iter().all(|torrent| torrent.pieces.is_none()));
    // Running torrents come over downloading, to be checked as they start.
    let mini = plan
        .torrents
        .iter()
        .find(|torrent| torrent.name == "mini.iso")
        .unwrap();
    assert_eq!(mini.state, TransferState::Downloading);
    let _ = std::fs::remove_dir_all(root);
}

fn files_out_of_reach_come_over_paused(live: &Live) {
    let (store, root) = store(&format!("moved-{}", live.version));
    let config = store.config().unwrap().client_config().unwrap();
    // Without the map, /downloads does not exist here.
    let plan = qbittorrent::plan(&live.connect(), &Options::default(), &store, &config).unwrap();
    let mini = plan
        .torrents
        .iter()
        .find(|torrent| torrent.name == "mini.iso")
        .unwrap();
    assert_eq!(mini.state, TransferState::Paused);
    assert!(mini.pieces.is_none());
    assert!(
        mini.warnings
            .iter()
            .any(|warning| warning.contains("--map"))
    );
    let _ = std::fs::remove_dir_all(root);
}
