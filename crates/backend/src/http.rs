//! A small HTTP/1.0 client over TCP or TLS, for tracker announces and
//! downloading `.torrent` files.
//!
//! Requests use HTTP/1.0 so servers answer with a plain body and close the
//! connection; chunked bodies are still decoded for servers that ignore
//! that.

use std::io::{Read, Write};
use std::net::ToSocketAddrs;
use std::time::Duration;

/// Redirects followed by [`get`] before giving up.
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// The path and query, starting with `/`.
    pub path: String,
}

impl Url {
    /// Parses an `http://` or `https://` URL. A URL with no path gets
    /// `default_path`.
    pub fn parse(url: &str, default_path: &str) -> Result<Self, String> {
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err("URL is not HTTP or HTTPS".to_owned());
        };
        // The fragment never goes to the server.
        let rest = rest.split('#').next().unwrap_or_default();
        let (authority, path) = match rest.find(['/', '?']) {
            Some(at) if rest[at..].starts_with('/') => (&rest[..at], rest[at + 1..].to_owned()),
            Some(at) => (&rest[..at], format!("{default_path}{}", &rest[at..])),
            None => (rest, default_path.to_owned()),
        };
        if authority.is_empty() {
            return Err("URL has no hostname".to_owned());
        }
        let default_port = if tls { 443 } else { 80 };
        // A bracketed IPv6 literal contains colons of its own.
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.contains(']') => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| "URL port is invalid".to_owned())?,
            ),
            _ => (authority.to_owned(), default_port),
        };
        if host.is_empty() {
            return Err("URL has no hostname".to_owned());
        }
        Ok(Self {
            tls,
            host,
            port,
            path: format!("/{path}"),
        })
    }

    /// Resolves a redirect's `Location` against this URL.
    fn join(&self, location: &str) -> Result<Self, String> {
        if location.starts_with("http://") || location.starts_with("https://") {
            return Self::parse(location, "");
        }
        let scheme = if self.tls { "https" } else { "http" };
        if let Some(rest) = location.strip_prefix("//") {
            return Self::parse(&format!("{scheme}://{rest}"), "");
        }
        let path = if location.starts_with('/') {
            location.to_owned()
        } else {
            // Relative to the current path's directory.
            let path = self.path.split('?').next().unwrap_or_default();
            let directory = &path[..=path.rfind('/').unwrap_or(0)];
            format!("{directory}{location}")
        };
        Ok(Self {
            tls: self.tls,
            host: self.host.clone(),
            port: self.port,
            path,
        })
    }
}

#[derive(Debug)]
pub(crate) struct Response {
    pub status: u16,
    /// The status line after the version, such as `404 Not Found`.
    pub status_text: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Why a request failed: the server could not be reached or never answered,
/// or it answered with something unusable.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Unreachable(String),
    Invalid(String),
}

impl Error {
    pub fn into_message(self) -> String {
        match self {
            Self::Unreachable(message) | Self::Invalid(message) => message,
        }
    }
}

/// Sends one GET for `target` (path and query) and reads the response, at
/// most `limit` bytes of it, through `binding`'s network interface.
pub(crate) fn request(
    url: &Url,
    target: &str,
    timeout: Duration,
    limit: u64,
    binding: &crate::Binding,
) -> Result<Response, Error> {
    // An address of a family the chosen interface has.
    let address = format!("{}:{}", url.host, url.port)
        .to_socket_addrs()
        .map_err(|error| Error::Unreachable(error.to_string()))?
        .find(|address| binding.reaches(address))
        .ok_or_else(|| Error::Unreachable("hostname resolved to no addresses".to_owned()))?;
    request_at(address, url, target, timeout, limit, binding)
}

/// Like [`request`], with a method, extra header lines (each ending in
/// CRLF), and a body.
fn send(
    url: &Url,
    message: &Message<'_>,
    timeout: Duration,
    limit: u64,
    binding: &crate::Binding,
) -> Result<Response, Error> {
    let address = format!("{}:{}", url.host, url.port)
        .to_socket_addrs()
        .map_err(|error| Error::Unreachable(error.to_string()))?
        .find(|address| binding.reaches(address))
        .ok_or_else(|| Error::Unreachable("hostname resolved to no addresses".to_owned()))?;
    send_at(address, url, message, timeout, limit, binding)
}

/// What to send: the method, the path and query, extra header lines (each
/// ending in CRLF), and the body.
struct Message<'a> {
    method: &'a str,
    target: &'a str,
    headers: &'a str,
    body: &'a [u8],
}

/// Like [`request`], to the server at `address` (one of `url`'s host's).
pub(crate) fn request_at(
    address: std::net::SocketAddr,
    url: &Url,
    target: &str,
    timeout: Duration,
    limit: u64,
    binding: &crate::Binding,
) -> Result<Response, Error> {
    let message = Message {
        method: "GET",
        target,
        headers: "",
        body: &[],
    };
    send_at(address, url, &message, timeout, limit, binding)
}

fn send_at(
    address: std::net::SocketAddr,
    url: &Url,
    message: &Message<'_>,
    timeout: Duration,
    limit: u64,
    binding: &crate::Binding,
) -> Result<Response, Error> {
    let stream = binding
        .connect_tcp(address, timeout)
        .map_err(|error| Error::Unreachable(error.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(timeout)))
        .map_err(|error| Error::Unreachable(error.to_string()))?;
    let default_port = if url.tls { 443 } else { 80 };
    let host_header = if url.port == default_port {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let length = if message.body.is_empty() && message.method == "GET" {
        String::new()
    } else {
        format!("Content-Length: {}\r\n", message.body.len())
    };
    let mut http_request = format!(
        "{} {} HTTP/1.0\r\nHost: {host_header}\r\nConnection: close\r\n\
         User-Agent: EmpireBT/{}\r\nAccept: */*\r\n{}{length}\r\n",
        message.method,
        message.target,
        env!("CARGO_PKG_VERSION"),
        message.headers,
    )
    .into_bytes();
    http_request.extend(message.body);
    // No response at all (timeout, reset, TLS failure) means unreachable.
    let raw = if url.tls {
        let name = rustls::pki_types::ServerName::try_from(url.host.clone())
            .map_err(|_| Error::Invalid("hostname is not valid for TLS".to_owned()))?;
        let connection = rustls::ClientConnection::new(tls_config(), name)
            .map_err(|error| Error::Invalid(format!("TLS setup failed: {error}")))?;
        exchange(
            rustls::StreamOwned::new(connection, stream),
            &http_request,
            limit,
        )
    } else {
        exchange(stream, &http_request, limit)
    }
    .map_err(Error::Unreachable)?;
    parse_response(&raw).map_err(Error::Invalid)
}

/// Fetches `url`, following redirects, and returns the body of a `200 OK`.
pub(crate) fn get(
    url: &str,
    timeout: Duration,
    limit: u64,
    binding: &crate::Binding,
) -> Result<Vec<u8>, Error> {
    let mut url = Url::parse(url, "").map_err(Error::Invalid)?;
    for _ in 0..=MAX_REDIRECTS {
        let response = request(&url, &url.path, timeout, limit, binding)?;
        match response.status {
            200 => return Ok(response.body),
            301 | 302 | 303 | 307 | 308 => {
                let location = response.header("location").ok_or_else(|| {
                    Error::Invalid(format!("HTTP {} without a Location", response.status))
                })?;
                url = url.join(location.trim()).map_err(Error::Invalid)?;
            }
            _ => {
                return Err(Error::Invalid(format!(
                    "HTTP status {}",
                    response.status_text
                )));
            }
        }
    }
    Err(Error::Invalid("too many redirects".to_owned()))
}

/// Downloads `url` (HTTP or HTTPS, following redirects) through
/// `binding`'s network interface: at most `limit` bytes, each request
/// within `timeout`.
pub fn download(
    url: &str,
    timeout: Duration,
    limit: u64,
    binding: &crate::Binding,
) -> Result<Vec<u8>, crate::BackendError> {
    get(url, timeout, limit, binding)
        .map_err(|error| crate::BackendError::Io(format!("{url}: {}", error.into_message())))
}

/// An HTTP response, for [`http_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A header's value, by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Sends one HTTP or HTTPS request (no redirects followed) through
/// `binding`'s network interface and reads at most `limit` bytes of the
/// response, for talking to other programs' web APIs.
pub fn http_request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
    limit: u64,
    binding: &crate::Binding,
) -> Result<HttpResponse, crate::BackendError> {
    let parsed = Url::parse(url, "").map_err(crate::BackendError::Io)?;
    let mut lines = String::new();
    for (name, value) in headers {
        if [name, value].iter().any(|text| text.contains(['\r', '\n'])) {
            return Err(crate::BackendError::Io(
                "a header cannot contain a line break".to_owned(),
            ));
        }
        lines.push_str(&format!("{name}: {value}\r\n"));
    }
    let message = Message {
        method,
        target: &parsed.path,
        headers: &lines,
        body,
    };
    send(&parsed, &message, timeout, limit, binding)
        .map(|response| HttpResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
        .map_err(|error| crate::BackendError::Io(format!("{url}: {}", error.into_message())))
}

/// Sends one request and reads the full response, tolerating servers that
/// close a TLS connection without a close-notify alert.
fn exchange(mut stream: impl Read + Write, request: &[u8], limit: u64) -> Result<Vec<u8>, String> {
    stream
        .write_all(request)
        .and_then(|_| stream.flush())
        .map_err(|error| error.to_string())?;
    // Read until the response is complete: by its Content-Length, or the
    // end of a chunked body, else until the server closes the connection.
    // Some servers keep the connection open even when asked to close it.
    let mut response = Vec::new();
    let mut buffer = [0; 16 * 1024];
    let mut stream = (&mut stream).take(limit);
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(response),
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if is_complete(&response) {
                    return Ok(response);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::UnexpectedEof && !response.is_empty() =>
            {
                return Ok(response);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Whether a chunked body has reached its final, empty chunk (and the
/// blank line after any trailers).
fn chunks_complete(mut body: &[u8]) -> bool {
    loop {
        let Some(line_end) = body.windows(2).position(|window| window == b"\r\n") else {
            return false;
        };
        let size = std::str::from_utf8(&body[..line_end])
            .ok()
            .and_then(|line| usize::from_str_radix(line.split(';').next()?.trim(), 16).ok());
        let Some(size) = size else {
            // Unreadable: let the parser report it once the server closes.
            return false;
        };
        body = &body[line_end + 2..];
        if size == 0 {
            return body.starts_with(b"\r\n")
                || body.windows(4).any(|window| window == b"\r\n\r\n");
        }
        if body.len() < size + 2 {
            return false;
        }
        body = &body[size + 2..];
    }
}

/// Whether `raw` holds a whole response, judged by its headers.
fn is_complete(raw: &[u8]) -> bool {
    let Some(header_end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
    let body = &raw[header_end + 4..];
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim() {
            "content-length" => {
                return value
                    .trim()
                    .parse::<usize>()
                    .is_ok_and(|length| body.len() >= length);
            }
            "transfer-encoding" if value.contains("chunked") => return chunks_complete(body),
            _ => {}
        }
    }
    false
}

fn tls_config() -> std::sync::Arc<rustls::ClientConfig> {
    static CONFIG: std::sync::OnceLock<std::sync::Arc<rustls::ClientConfig>> =
        std::sync::OnceLock::new();
    std::sync::Arc::clone(CONFIG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        std::sync::Arc::new(config)
    }))
}

/// Splits a raw response into status, headers, and the decoded body.
fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("response has no HTTP body")?;
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    if !status_line.starts_with("HTTP/1.") {
        return Err("response is not HTTP".to_owned());
    }
    let status_text = status_line
        .split_once(' ')
        .map_or("", |(_, rest)| rest)
        .to_owned();
    let status = status_text
        .split_whitespace()
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or("response has no status code")?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let mut response = Response {
        status,
        status_text,
        headers,
        body: Vec::new(),
    };
    let body = &raw[header_end + 4..];
    let chunked = response
        .header("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    response.body = if chunked {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };
    Ok(response)
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or("chunked response is truncated")?;
        let size_text = String::from_utf8_lossy(&body[..line_end]);
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| "chunked response has an invalid size")?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        let chunk = body.get(..size).ok_or("chunked response is truncated")?;
        decoded.extend_from_slice(chunk);
        body = body.get(size + 2..).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    /// A server that never closes the connection: the response ends by its
    /// length, or at the last chunk, not by the server hanging up.
    #[test]
    fn responses_end_without_the_server_closing() {
        use std::io::{Read, Write};
        for (body, encoding) in [
            ("hello", "Content-Length: 5"),
            ("5\r\nhello\r\n0\r\n\r\n", "Transfer-Encoding: chunked"),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                write!(stream, "HTTP/1.1 200 OK\r\n{encoding}\r\n\r\n{body}").unwrap();
                // Held open until the client is done.
                std::thread::sleep(Duration::from_secs(5));
            });
            let started = std::time::Instant::now();
            let response = super::http_request(
                "POST",
                &format!("http://127.0.0.1:{port}/login"),
                &[("Content-Type", "text/plain")],
                b"body",
                Duration::from_secs(10),
                1 << 20,
                &crate::Binding::any(),
            )
            .unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body, b"hello");
            assert!(started.elapsed() < Duration::from_secs(3), "{encoding}");
            drop(server);
        }
        assert!(!super::chunks_complete(b"5\r\nhello\r\n"));
        assert!(!super::chunks_complete(b"5\r\nhel"));
        assert!(super::chunks_complete(
            b"5\r\nhello\r\n0\r\nX-Trailer: 1\r\n\r\n"
        ));
        // Data that looks like the end is still data.
        assert!(!super::chunks_complete(b"7\r\n0\r\n\r\nab"));
    }

    use super::*;
    use std::net::TcpListener;

    fn url(tls: bool, host: &str, port: u16, path: &str) -> Url {
        Url {
            tls,
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        }
    }

    #[test]
    fn parses_urls() {
        assert_eq!(
            Url::parse("http://tracker.example:6969", "announce").unwrap(),
            url(false, "tracker.example", 6969, "/announce")
        );
        assert_eq!(
            Url::parse("http://tracker.example/a/announce?passkey=abc", "announce").unwrap(),
            url(false, "tracker.example", 80, "/a/announce?passkey=abc")
        );
        assert_eq!(
            Url::parse("https://tracker.example/announce", "").unwrap(),
            url(true, "tracker.example", 443, "/announce")
        );
        assert_eq!(
            Url::parse("http://[::1]/announce", "").unwrap(),
            url(false, "[::1]", 80, "/announce")
        );
        assert_eq!(
            Url::parse("https://site.example?id=7#top", "").unwrap(),
            url(true, "site.example", 443, "/?id=7")
        );
        assert!(Url::parse("ftp://site.example/a", "").is_err());
    }

    #[test]
    fn resolves_redirect_locations() {
        let base = Url::parse("https://site.example/dl/get.php?id=1", "").unwrap();
        assert_eq!(
            base.join("/files/a.torrent").unwrap(),
            url(true, "site.example", 443, "/files/a.torrent")
        );
        assert_eq!(
            base.join("a.torrent").unwrap(),
            url(true, "site.example", 443, "/dl/a.torrent")
        );
        assert_eq!(
            base.join("//cdn.example/a.torrent").unwrap(),
            url(true, "cdn.example", 443, "/a.torrent")
        );
        assert_eq!(
            base.join("http://other.example:8080/a").unwrap(),
            url(false, "other.example", 8080, "/a")
        );
    }

    #[test]
    fn decodes_http_bodies() {
        let plain = b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nd5:peers0:e";
        assert_eq!(parse_response(plain).unwrap().body, b"d5:peers0:e");
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
4\r\nd5:p\r\n7;ext=1\r\neers0:e\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap().body, b"d5:peers0:e");
        let missing = parse_response(b"HTTP/1.1 404 Not Found\r\n\r\n").unwrap();
        assert_eq!(
            (missing.status, missing.status_text.as_str()),
            (404, "404 Not Found")
        );
        assert!(parse_response(b"SSH-2.0-OpenSSH\r\n\r\n").is_err());
    }

    /// Serves each canned response to one connection, in order.
    fn serve(responses: Vec<String>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        port
    }

    #[test]
    fn follows_redirects() {
        let port = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: /real.torrent\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_owned(),
        ]);
        let body = get(
            &format!("http://127.0.0.1:{port}/get?id=1"),
            Duration::from_secs(5),
            1024,
            &crate::Binding::any(),
        )
        .unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn reports_error_statuses_and_unreachable_servers() {
        let port = serve(vec!["HTTP/1.1 404 Not Found\r\n\r\n".to_owned()]);
        let error = get(
            &format!("http://127.0.0.1:{port}/missing"),
            Duration::from_secs(5),
            1024,
            &crate::Binding::any(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            Error::Invalid("HTTP status 404 Not Found".to_owned())
        );

        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let error = get(
            &format!("http://127.0.0.1:{port}/"),
            Duration::from_secs(5),
            1024,
            &crate::Binding::any(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Unreachable(_)), "{error:?}");
    }
}
