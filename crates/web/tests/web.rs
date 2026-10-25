//! The web UI over real HTTP: who gets in, what is refused, and what a
//! signed-in browser or script can do.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::time::{Duration, Instant};

use empirebt_app::{Handle, Store};
use empirebt_web::{Web, WebSettings};

struct Running {
    web: Option<Web>,
    engine: Handle,
    store: Store,
    root: std::path::PathBuf,
    port: u16,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut web) = self.web.take() {
            web.stop();
        }
        self.engine.shutdown();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Running {
    fn web(&self) -> &Web {
        self.web.as_ref().unwrap()
    }

    fn password(&self) -> String {
        self.web().generated_password().unwrap().to_owned()
    }
}

fn start(name: &str, address: &str) -> Running {
    start_with(name, address, false)
}

fn start_with(name: &str, address: &str, plain_http: bool) -> Running {
    let root = std::env::temp_dir().join(format!("empirebt-web-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = Store::open(root.join("data")).unwrap();
    let mut config = store.config().unwrap();
    config.set("console.start_on_add", "false").unwrap();
    let client_config = config
        .client_config()
        .unwrap()
        .with_listen_port(0)
        .with_dht(false)
        .with_lsd(false)
        .with_download_directory(root.join("downloads"));
    let engine = Handle::start(store.clone(), config, client_config, Vec::new()).unwrap();
    let settings = WebSettings {
        address: address.parse::<IpAddr>().unwrap(),
        port: 0,
        tls: None,
        allowed_hosts: vec!["nas.local".to_owned()],
        plain_http,
    };
    let web = Web::start(engine.clone(), &store, settings).unwrap();
    let port = web.url().rsplit(':').next().unwrap().parse().unwrap();
    Running {
        web: Some(web),
        engine,
        store,
        root,
        port,
    }
}

#[derive(Debug)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }
}

/// A request over a fresh connection. `headers` come after `Host`.
fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        request.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    parse(&raw)
}

fn parse(raw: &[u8]) -> Response {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Response {
        status,
        headers,
        body: body.to_owned(),
    }
}

/// A signed-in browser's cookie.
fn sign_in(running: &Running) -> String {
    let body = format!(r#"{{"password":"{}"}}"#, running.password());
    let response = send(
        running.port,
        "POST",
        "/api/login",
        &[("X-EmpireBT", "1"), ("Content-Type", "application/json")],
        body.as_bytes(),
    );
    assert_eq!(response.status, 204, "{}", response.body);
    let cookie = response.header("set-cookie").unwrap();
    cookie.split(';').next().unwrap().to_owned()
}

/// A browser's request with its session cookie and the CSRF header.
fn browser(running: &Running, cookie: &str, method: &str, path: &str, body: &str) -> Response {
    send(
        running.port,
        method,
        path,
        &[
            ("Cookie", cookie),
            ("X-EmpireBT", "1"),
            ("Origin", &format!("http://127.0.0.1:{}", running.port)),
            ("Content-Type", "application/json"),
        ],
        body.as_bytes(),
    )
}

const MAGNET: &str = "magnet:?xt=urn:btih:0101010101010101010101010101010101010101&dn=web-test";
const HASH: &str = "0101010101010101010101010101010101010101";

#[test]
fn refuses_everything_to_strangers() {
    let running = start("strangers", "127.0.0.1");
    let page = send(running.port, "GET", "/", &[], b"");
    assert_eq!(page.status, 200);
    assert!(
        page.body
            .contains("<script type=\"module\" src=\"/app.js\">")
    );
    let csp = page.header("content-security-policy").unwrap();
    assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(page.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
    assert_eq!(page.header("referrer-policy"), Some("no-referrer"));
    assert!(
        page.header("strict-transport-security").is_none(),
        "plain HTTP"
    );

    for path in ["/api/state", "/api/events", "/api/session"] {
        let response = send(running.port, "GET", path, &[], b"");
        assert_eq!(response.status, 401, "{path}");
        assert_eq!(response.header("cache-control"), Some("no-store"));
    }
    let add = send(
        running.port,
        "POST",
        "/api/torrents",
        &[("X-EmpireBT", "1"), ("Content-Type", "application/json")],
        format!(r#"{{"source":"{MAGNET}"}}"#).as_bytes(),
    );
    assert_eq!(add.status, 401);
    assert!(running.engine.snapshot().torrents.is_empty());

    // Another site's name pointed at us (DNS rebinding) is refused, even
    // for the public page; names we were told about and addresses work.
    let rebound = send(running.port, "GET", "/", &[("Host", "evil.example")], b"");
    assert_eq!(rebound.status, 421);
    assert!(
        rebound.header("content-security-policy").is_some(),
        "headers on errors too"
    );
    for host in ["nas.local:8080", "localhost", "[::1]:8080", "192.168.1.9"] {
        assert_eq!(
            send(running.port, "GET", "/", &[("Host", host)], b"").status,
            200,
            "{host}"
        );
    }
    assert_eq!(send(running.port, "GET", "/nothing", &[], b"").status, 404);
}

#[test]
fn signs_in_with_the_password_or_a_one_time_link() {
    let running = start("sign-in", "127.0.0.1");
    let response = send(
        running.port,
        "POST",
        "/api/login",
        &[("X-EmpireBT", "1"), ("Content-Type", "application/json")],
        format!(r#"{{"password":"{}"}}"#, running.password()).as_bytes(),
    );
    assert_eq!(response.status, 204);
    let cookie = response.header("set-cookie").unwrap();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
        "{cookie}"
    );
    assert!(!cookie.contains("Secure"), "plain HTTP on loopback");
    let session = cookie.split(';').next().unwrap();
    assert_eq!(
        browser(&running, session, "GET", "/api/session", "").status,
        200
    );

    let wrong = browser(
        &running,
        "",
        "POST",
        "/api/login",
        r#"{"password":"guess"}"#,
    );
    assert_eq!(wrong.status, 401);
    assert_eq!(wrong.json()["error"], "wrong password");

    // A one-time link signs in once.
    let link = running.web().login_link();
    let token = link.split("#login=").nth(1).unwrap();
    let body = format!(r#"{{"token":"{token}"}}"#);
    assert_eq!(
        browser(&running, "", "POST", "/api/login", &body).status,
        204
    );
    assert_eq!(
        browser(&running, "", "POST", "/api/login", &body).status,
        401
    );

    // Signing out ends the session.
    assert_eq!(
        browser(&running, session, "POST", "/api/logout", "").status,
        204
    );
    assert_eq!(
        browser(&running, session, "GET", "/api/session", "").status,
        401
    );
}

#[test]
fn makes_guessers_wait() {
    let running = start("guessing", "127.0.0.1");
    for _ in 0..5 {
        let response = browser(
            &running,
            "",
            "POST",
            "/api/login",
            r#"{"password":"guess"}"#,
        );
        assert_eq!(response.status, 401);
    }
    let body = format!(r#"{{"password":"{}"}}"#, running.password());
    let banned = browser(&running, "", "POST", "/api/login", &body);
    assert_eq!(banned.status, 429, "even the right password waits");
    assert!(banned.header("retry-after").is_some());
}

#[test]
fn refuses_writes_from_other_sites() {
    let running = start("csrf", "127.0.0.1");
    let cookie = sign_in(&running);
    let body = format!(r#"{{"source":"{MAGNET}"}}"#);
    // Without the custom header no write goes through, signed in or not.
    let plain = send(
        running.port,
        "POST",
        "/api/torrents",
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        body.as_bytes(),
    );
    assert_eq!(plain.status, 403);
    let login = send(
        running.port,
        "POST",
        "/api/login",
        &[("Content-Type", "application/json")],
        br#"{"password":"x"}"#,
    );
    assert_eq!(login.status, 403, "no login CSRF either");
    // With the header but from another origin.
    let foreign = send(
        running.port,
        "POST",
        "/api/torrents",
        &[
            ("Cookie", &cookie),
            ("X-EmpireBT", "1"),
            ("Origin", "http://evil.example"),
            ("Content-Type", "application/json"),
        ],
        body.as_bytes(),
    );
    assert_eq!(foreign.status, 403);
    assert!(running.engine.snapshot().torrents.is_empty());
    assert_eq!(
        browser(&running, &cookie, "POST", "/api/torrents", &body).status,
        200
    );
}

#[test]
fn a_signed_in_browser_drives_the_engine() {
    let running = start("drive", "127.0.0.1");
    let cookie = sign_in(&running);
    let added = browser(
        &running,
        &cookie,
        "POST",
        "/api/torrents",
        &format!(r#"{{"source":"{MAGNET}"}}"#),
    );
    assert_eq!(added.json()["added"], HASH);
    let duplicate = browser(
        &running,
        &cookie,
        "POST",
        "/api/torrents",
        &format!(r#"{{"source":"{MAGNET}"}}"#),
    );
    assert_eq!(duplicate.status, 400);

    // Paths on the server are not for browsers.
    let path = browser(
        &running,
        &cookie,
        "POST",
        "/api/torrents",
        r#"{"source":"/etc/passwd"}"#,
    );
    assert_eq!(path.status, 400);

    let mut torrent =
        b"d4:infod6:lengthi5e4:name9:hello.txt12:piece lengthi16384e6:pieces20:".to_vec();
    torrent.extend([7; 20]);
    torrent.extend(b"ee");
    // Saved in a folder inside the download folder; never outside it.
    let encode = |path: &std::path::Path| path.to_string_lossy().replace('/', "%2F");
    let refused = send(
        running.port,
        "POST",
        &format!("/api/torrents/upload?savePath={}", encode("/etc".as_ref())),
        &[("Cookie", &cookie), ("X-EmpireBT", "1")],
        &torrent,
    );
    assert_eq!(refused.status, 403, "{}", refused.body);
    let refused = browser(
        &running,
        &cookie,
        "POST",
        "/api/torrents",
        r#"{"source":"magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a","savePath":"/tmp"}"#,
    );
    assert_eq!(refused.status, 403, "{}", refused.body);
    let folder = running.root.join("downloads").join("isos");
    let uploaded = send(
        running.port,
        "POST",
        &format!("/api/torrents/upload?savePath={}", encode(&folder)),
        &[("Cookie", &cookie), ("X-EmpireBT", "1")],
        &torrent,
    );
    assert_eq!(uploaded.status, 200, "{}", uploaded.body);

    let added = uploaded.json()["added"].as_str().unwrap().to_owned();

    // A category's folder: only one a browser may use; new torrents in the
    // category go there.
    let films = running.root.join("downloads").join("films");
    let refused = browser(
        &running,
        &cookie,
        "PUT",
        "/api/categories/folder",
        r#"{"name":"Films","savePath":"/etc"}"#,
    );
    assert_eq!(refused.status, 403, "{}", refused.body);
    let body = serde_json::json!({ "name": "Films", "savePath": films }).to_string();
    let set = browser(&running, &cookie, "PUT", "/api/categories/folder", &body);
    assert_eq!(set.status, 204, "{}", set.body);
    let mut film = b"d4:infod6:lengthi6e4:name8:film.mkv12:piece lengthi16384e6:pieces20:".to_vec();
    film.extend([9; 20]);
    film.extend(b"ee");
    let film = send(
        running.port,
        "POST",
        "/api/torrents/upload?category=Films&tags=hd,new",
        &[("Cookie", &cookie), ("X-EmpireBT", "1")],
        &film,
    );
    assert_eq!(film.status, 200, "{}", film.body);
    let film = film.json()["added"].as_str().unwrap().to_owned();
    let details = browser(
        &running,
        &cookie,
        "GET",
        &format!("/api/torrents/{film}"),
        "",
    )
    .json();
    assert_eq!(details["savePath"], films.to_string_lossy().as_ref());
    assert_eq!(details["torrent"]["category"], "Films");
    assert_eq!(details["torrent"]["tags"], serde_json::json!(["hd", "new"]));
    // A single file can go in a folder of its own, and be shown by
    // another name.
    let mut clip = b"d4:infod6:lengthi7e4:name8:clip.mkv12:piece lengthi16384e6:pieces20:".to_vec();
    clip.extend([8; 20]);
    clip.extend(b"ee");
    let refused = send(
        running.port,
        "POST",
        "/api/torrents/upload?layout=sideways",
        &[("Cookie", &cookie), ("X-EmpireBT", "1")],
        &clip,
    );
    assert_eq!(refused.status, 400, "{}", refused.body);
    let clip = send(
        running.port,
        "POST",
        "/api/torrents/upload?layout=subfolder",
        &[("Cookie", &cookie), ("X-EmpireBT", "1")],
        &clip,
    );
    assert_eq!(clip.status, 200, "{}", clip.body);
    let clip = clip.json()["added"].as_str().unwrap().to_owned();
    let renamed = browser(
        &running,
        &cookie,
        "PUT",
        &format!("/api/torrents/{clip}/name"),
        r#"{"name":"Holiday clip"}"#,
    );
    assert_eq!(renamed.status, 204, "{}", renamed.body);
    let escape = browser(
        &running,
        &cookie,
        "POST",
        &format!("/api/torrents/{clip}/rename"),
        r#"{"from":"clip/clip.mkv","to":"../clip.mkv"}"#,
    );
    assert!(escape.status >= 400, "{}", escape.body);
    let details = browser(
        &running,
        &cookie,
        "GET",
        &format!("/api/torrents/{clip}"),
        "",
    )
    .json();
    assert_eq!(details["torrent"]["name"], "Holiday clip");
    assert_eq!(details["originalName"], "clip.mkv");
    assert_eq!(details["files"][0]["path"], "clip/clip.mkv");
    assert_eq!(details["files"][0]["original"], "clip.mkv");
    // Removing it can delete its files too.
    let file = running.root.join("downloads").join("clip").join("clip.mkv");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"partial").unwrap();
    let removed = browser(
        &running,
        &cookie,
        "DELETE",
        &format!("/api/torrents/{clip}?deleteFiles=true"),
        "",
    );
    assert_eq!(removed.status, 204, "{}", removed.body);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while file.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the file was not deleted"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !file.parent().unwrap().exists(),
        "its emptied folder goes too"
    );
    browser(
        &running,
        &cookie,
        "DELETE",
        &format!("/api/torrents/{film}"),
        "",
    );
    let details = browser(
        &running,
        &cookie,
        "GET",
        &format!("/api/torrents/{added}"),
        "",
    )
    .json();
    assert_eq!(details["savePath"], folder.to_string_lossy().as_ref());
    assert_eq!(
        details["torrent"]["savePath"],
        folder.to_string_lossy().as_ref()
    );

    let path = format!("/api/torrents/{HASH}");
    assert_eq!(
        browser(
            &running,
            &cookie,
            "PUT",
            &format!("{path}/tags"),
            r#"{"tags":["web"," linux "]}"#
        )
        .status,
        204
    );
    assert_eq!(
        browser(
            &running,
            &cookie,
            "PUT",
            &format!("{path}/category"),
            r#"{"category":"ISOs"}"#
        )
        .status,
        204
    );
    let bad = browser(
        &running,
        &cookie,
        "PUT",
        &format!("{path}/tags"),
        "{\"tags\":[\"a\\tb\"]}",
    );
    assert_eq!(bad.status, 400);
    assert_eq!(
        browser(&running, &cookie, "POST", &format!("{path}/pause"), "").status,
        204
    );

    let state = browser(&running, &cookie, "GET", "/api/state", "").json();
    let torrents = state["torrents"].as_array().unwrap();
    assert_eq!(torrents.len(), 2);
    let ours = torrents
        .iter()
        .find(|torrent| torrent["hash"] == HASH)
        .unwrap();
    assert_eq!(ours["name"], "web-test");
    assert_eq!(ours["category"], "ISOs");
    assert_eq!(ours["tags"], serde_json::json!(["linux", "web"]));
    assert_eq!(state["full"], true);
    assert_eq!(
        state["lists"]["categories"],
        serde_json::json!(["Films", "ISOs"])
    );
    assert!(state["lists"]["categoryFolders"]["Films"].is_string());
    assert_eq!(
        state["lists"]["saveDirs"][0],
        running.root.join("downloads").to_string_lossy().as_ref()
    );
    assert_eq!(state["order"].as_array().unwrap().len(), 2);

    // Peers, with the country database's credit; none connect to a
    // paused torrent.
    let peers = browser(&running, &cookie, "GET", &format!("{path}/peers"), "");
    assert_eq!(peers.status, 200, "{}", peers.body);
    let peers = peers.json();
    assert_eq!(peers["peers"], serde_json::json!([]));
    assert!(peers["attribution"].as_str().unwrap().contains("DB-IP"));
    let unknown = format!("/api/torrents/{}/peers", "ab".repeat(20));
    assert_eq!(browser(&running, &cookie, "GET", &unknown, "").status, 404);

    let details = browser(&running, &cookie, "GET", &path, "").json();
    assert_eq!(details["torrent"]["magnet"], true);
    assert_eq!(
        browser(&running, &cookie, "GET", "/api/torrents/zz", "").status,
        404
    );

    // Settings: speed limits from the web, storage and the web UI's own
    // setup only from outside it.
    let limit = browser(
        &running,
        &cookie,
        "PUT",
        "/api/settings",
        r#"{"key":"network.upload_limit","value":"300"}"#,
    );
    assert_eq!(limit.status, 204);
    assert_eq!(running.engine.snapshot().rate_limits.1, 300 * 1024);
    for key in ["storage.download_dir", "web.address", "network.interface"] {
        let body = format!(r#"{{"key":"{key}","value":"0.0.0.0"}}"#);
        assert_eq!(
            browser(&running, &cookie, "PUT", "/api/settings", &body).status,
            403,
            "{key}"
        );
    }
    let shown = state["lists"]["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|setting| setting["key"] == "storage.download_dir")
        .unwrap()
        .clone();
    assert_eq!(shown["editable"], false);

    assert_eq!(browser(&running, &cookie, "DELETE", &path, "").status, 204);
    assert!(running.engine.snapshot().torrent(&[1; 20]).is_none());
}

#[test]
fn api_tokens_work_for_scripts_until_revoked() {
    let running = start("tokens", "127.0.0.1");
    let token = empirebt_web::create_token(&running.store, "backup script").unwrap();
    // The running server reads tokens at start; restart it.
    let mut running = running;
    running.web.take().unwrap().stop();
    let settings = WebSettings {
        address: "127.0.0.1".parse().unwrap(),
        port: 0,
        tls: None,
        allowed_hosts: Vec::new(),
        plain_http: false,
    };
    let web = Web::start(running.engine.clone(), &running.store, settings).unwrap();
    running.port = web.url().rsplit(':').next().unwrap().parse().unwrap();
    running.web = Some(web);

    let bearer = format!("Bearer {token}");
    // No cookie and no CSRF header: browsers never send this on their own.
    let added = send(
        running.port,
        "POST",
        "/api/torrents",
        &[
            ("Authorization", &bearer),
            ("Content-Type", "application/json"),
        ],
        format!(r#"{{"source":"{MAGNET}"}}"#).as_bytes(),
    );
    assert_eq!(added.status, 200, "{}", added.body);
    let forged = send(
        running.port,
        "GET",
        "/api/state",
        &[("Authorization", "Bearer 00ff")],
        b"",
    );
    assert_eq!(forged.status, 401);
    assert_eq!(empirebt_web::revoke_tokens(&running.store).unwrap(), 1);
}

#[test]
fn limits_request_sizes() {
    let running = start("sizes", "127.0.0.1");
    let cookie = sign_in(&running);
    let huge = format!(r#"{{"source":"{}"}}"#, "a".repeat(70 * 1024));
    assert_eq!(
        browser(&running, &cookie, "POST", "/api/torrents", &huge).status,
        413
    );
    let upload = vec![b'd'; 16 * 1024 * 1024 + 1];
    let response = send(
        running.port,
        "POST",
        "/api/torrents/upload",
        &[("Cookie", &cookie), ("X-EmpireBT", "1")],
        &upload,
    );
    assert_eq!(response.status, 413);
}

#[test]
fn streams_live_updates_and_stops_with_them_open() {
    let mut running = start("events", "127.0.0.1");
    let cookie = sign_in(&running);
    let mut stream = TcpStream::connect(("127.0.0.1", running.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: {cookie}\r\n\r\n"
    )
    .unwrap();
    let mut received = Vec::new();
    let mut buffer = [0; 4096];
    // Notices end every update.
    while !String::from_utf8_lossy(&received).contains("\"notices\":") {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "stream ended early");
        received.extend_from_slice(&buffer[..read]);
    }
    let text = String::from_utf8_lossy(&received);
    assert!(text.contains("text/event-stream"), "{text}");
    assert!(text.contains("event: update"), "{text}");
    assert!(text.contains("\"full\":true"), "{text}");
    assert!(text.contains("\"torrents\":[]"), "{text}");

    // Stopping does not wait for the open stream to end on its own.
    let started = Instant::now();
    running.web.take().unwrap().stop();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// Behind a reverse proxy (as in Docker), `web.https = off` serves plain
/// HTTP on every address; signing in is still required.
#[test]
fn serves_plain_http_everywhere_when_a_proxy_adds_https() {
    let running = start_with("plain", "0.0.0.0", true);
    assert!(running.web().url().starts_with("http://"));
    assert!(running.web().certificate_fingerprint().is_none());
    let page = send(running.port, "GET", "/", &[], b"");
    assert_eq!(page.status, 200);
    let state = send(running.port, "GET", "/api/state", &[], b"");
    assert_eq!(state.status, 401);
    let cookie = sign_in(&running);
    assert_eq!(
        browser(&running, &cookie, "GET", "/api/state", "").status,
        200
    );
}

#[test]
fn serves_only_https_beyond_this_computer() {
    use tokio_rustls::rustls;
    let running = start("https", "0.0.0.0");
    let fingerprint = running.web().certificate_fingerprint().unwrap().to_owned();
    assert!(running.web().url().starts_with("https://"));

    // Plain HTTP gets nowhere.
    let plain = send(running.port, "GET", "/", &[], b"");
    assert_ne!(plain.status, 200);

    // HTTPS with the generated certificate.
    let (certificate, key) = running.store.web_certificate_paths();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the key is private");
    }
    use rustls::pki_types::pem::PemObject;
    let der = rustls::pki_types::CertificateDer::from_pem_file(&certificate).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let connection = rustls::ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", running.port)).unwrap();
    let mut tls = rustls::StreamOwned::new(connection, tcp);
    let body = format!(r#"{{"password":"{}"}}"#, running.password());
    write!(
        tls,
        "POST /api/login HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-EmpireBT: 1\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    let response = parse(&raw);
    assert_eq!(response.status, 204, "{}", response.body);
    assert!(response.header("set-cookie").unwrap().contains("Secure"));
    assert!(response.header("strict-transport-security").is_some());
    let served = tls.conn.peer_certificates().unwrap()[0].as_ref().to_vec();
    let digest: Vec<String> = ring_digest(&served);
    assert_eq!(digest.join(":"), fingerprint);
}

fn ring_digest(bytes: &[u8]) -> Vec<String> {
    use std::fmt::Write as _;
    // SHA-256 by hand would be silly; the backend's dependency tree has ring.
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest
        .as_ref()
        .iter()
        .map(|byte| {
            let mut text = String::new();
            let _ = write!(text, "{byte:02X}");
            text
        })
        .collect()
}
