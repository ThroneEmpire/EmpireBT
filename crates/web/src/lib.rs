//! The EmpireBT web UI: a browser interface to the same engine the console
//! drives, served from inside EmpireBT.
//!
//! It listens on 127.0.0.1 unless told otherwise; any other address is
//! served over HTTPS only, with a generated certificate when none is
//! configured, unless `web.https = off` says a reverse proxy adds HTTPS. Signing in is always required: the first start makes a
//! password, and [`Web::login_link`] makes one-time links. Scripts can use
//! API tokens instead (see [`create_token`]). See `api` for what each
//! request is checked for.

mod api;
mod assets;
mod auth;
mod server;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use empirebt_app::{Handle, Store};
use empirebt_backend::ConfigFile;
use tokio::sync::watch;

/// Shortest password accepted by [`set_password`].
pub const MIN_PASSWORD: usize = 8;

/// Where and how the web UI listens, from the `[web]` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSettings {
    pub address: IpAddr,
    pub port: u16,
    /// A PEM certificate and key; without them, HTTPS uses a generated
    /// certificate (or plain HTTP on a loopback address).
    pub tls: Option<(PathBuf, PathBuf)>,
    /// Host names besides `localhost` the web UI answers to.
    pub allowed_hosts: Vec<String>,
    /// Plain HTTP on every address (`web.https = off`), for a reverse proxy
    /// in front that adds HTTPS.
    pub plain_http: bool,
}

impl WebSettings {
    pub fn from_config(config: &ConfigFile) -> Result<Self, String> {
        let text = |key: &str| config.get(key).unwrap_or_default().trim().to_owned();
        let address = text("web.address")
            .parse()
            .map_err(|_| "web.address must be an IP address".to_owned())?;
        let port = text("web.port")
            .parse()
            .map_err(|_| "web.port must be a port number".to_owned())?;
        let tls = match (text("web.tls_cert"), text("web.tls_key")) {
            (certificate, key) if certificate.is_empty() && key.is_empty() => None,
            (certificate, key) if certificate.is_empty() || key.is_empty() => {
                return Err("set both web.tls_cert and web.tls_key, or neither".to_owned());
            }
            (certificate, key) => Some((PathBuf::from(certificate), PathBuf::from(key))),
        };
        let plain_http = text("web.https") == "off";
        if plain_http && tls.is_some() {
            return Err(
                "web.https is off, but web.tls_cert is set; clear one or the other".to_owned(),
            );
        }
        let allowed_hosts = text("web.allowed_hosts")
            .split(',')
            .map(|host| host.trim().to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect();
        Ok(Self {
            address,
            port,
            tls,
            allowed_hosts,
            plain_http,
        })
    }
}

/// A running web UI. Dropping it stops the server.
pub struct Web {
    url: String,
    state: Arc<api::WebState>,
    stop: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
    generated_password: Option<String>,
    fingerprint: Option<String>,
}

impl Web {
    /// Starts serving `engine` as `settings` say. The first start makes a
    /// password, available once from [`Web::generated_password`].
    pub fn start(engine: Handle, store: &Store, settings: WebSettings) -> Result<Self, String> {
        let auth_path = store.web_auth_path();
        let mut auth_file = auth::AuthFile::load(&auth_path)?;
        let generated_password = match auth_file.password {
            Some(_) => None,
            None => {
                let password = auth::generate_password();
                auth_file.password = Some(auth::hash_password(&password)?);
                auth_file.save(&auth_path)?;
                Some(password)
            }
        };
        let tls = match &settings.tls {
            Some((certificate, key)) => Some(server::load_tls(certificate, key)?),
            // Plain HTTP never leaves this computer, unless a reverse proxy
            // in front adds HTTPS.
            None if settings.address.is_loopback() || settings.plain_http => None,
            None => {
                let (certificate, key) = store.web_certificate_paths();
                let mut names = vec!["localhost".to_owned()];
                if !settings.address.is_unspecified() {
                    names.push(settings.address.to_string());
                }
                names.extend(settings.allowed_hosts.iter().cloned());
                server::ensure_self_signed(&certificate, &key, names)?;
                Some(server::load_tls(&certificate, &key)?)
            }
        };
        let address = SocketAddr::new(settings.address, settings.port);
        let listener = std::net::TcpListener::bind(address)
            .and_then(|listener| {
                listener.set_nonblocking(true)?;
                Ok(listener)
            })
            .map_err(|error| format!("web UI on {address}: {error}"))?;
        let port = listener
            .local_addr()
            .map_or(settings.port, |local| local.port());
        let mut hosts = vec!["localhost".to_owned()];
        hosts.extend(settings.allowed_hosts.iter().cloned());
        let (stop, stopped) = watch::channel(false);
        let state = Arc::new(api::WebState::new(
            engine,
            auth_file,
            hosts,
            tls.is_some(),
            stopped.clone(),
        ));
        let router = api::router(Arc::clone(&state));
        let fingerprint = tls.as_ref().map(|tls| tls.fingerprint.clone());
        let acceptor = tls.map(|tls| tls.acceptor);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("web")
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let thread = std::thread::Builder::new()
            .name("web".to_owned())
            .spawn(move || {
                runtime.block_on(async move {
                    match tokio::net::TcpListener::from_std(listener) {
                        Ok(listener) => server::run(listener, acceptor, router, stopped).await,
                        Err(error) => eprintln!("web UI: {error}"),
                    }
                });
            })
            .map_err(|error| error.to_string())?;
        let scheme = if fingerprint.is_some() {
            "https"
        } else {
            "http"
        };
        let host = match settings.address {
            address if address.is_unspecified() => "localhost".to_owned(),
            IpAddr::V6(address) => format!("[{address}]"),
            IpAddr::V4(address) => address.to_string(),
        };
        Ok(Self {
            url: format!("{scheme}://{host}:{port}"),
            state,
            stop,
            thread: Some(thread),
            generated_password,
            fingerprint,
        })
    }

    /// Where the web UI is, such as `http://127.0.0.1:8080`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// A link that signs in once, within ten minutes. The token travels
    /// in the fragment, so it never reaches server logs or other sites.
    pub fn login_link(&self) -> String {
        format!("{}/#login={}", self.url, self.state.links.create())
    }

    /// The password made at this first start, if one was.
    pub fn generated_password(&self) -> Option<&str> {
        self.generated_password.as_deref()
    }

    /// The HTTPS certificate's SHA-256 fingerprint, to compare with what
    /// the browser shows.
    pub fn certificate_fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }

    /// Stops serving: open requests get a moment to finish, live update
    /// streams end.
    pub fn stop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Sets the web UI password. Signed-in browsers stay signed in until the
/// web UI restarts.
pub fn set_password(store: &Store, password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(format!("use at least {MIN_PASSWORD} characters"));
    }
    let path = store.web_auth_path();
    let mut file = auth::AuthFile::load(&path)?;
    file.password = Some(auth::hash_password(password)?);
    file.save(&path)
}

/// Makes an API token for scripts, sent as `Authorization: Bearer
/// <token>`. Only its hash is kept, so it is shown this once.
pub fn create_token(store: &Store, name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) || name.len() > 64 {
        return Err("name the token: up to 64 characters, no tabs or line breaks".to_owned());
    }
    let path = store.web_auth_path();
    let mut file = auth::AuthFile::load(&path)?;
    let token = auth::random_token();
    file.tokens
        .push((name.to_owned(), auth::token_hash(&token)));
    file.save(&path)?;
    Ok(token)
}

/// Revokes every API token; returns how many there were.
pub fn revoke_tokens(store: &Store) -> Result<usize, String> {
    let path = store.web_auth_path();
    let mut file = auth::AuthFile::load(&path)?;
    let count = file.tokens.len();
    file.tokens.clear();
    file.save(&path)?;
    Ok(count)
}

/// Waits until the process is asked to stop (Ctrl-C, or SIGTERM on Unix).
pub fn wait_for_interrupt() {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut terminate = signal(SignalKind::terminate()).ok();
            let terminated = async {
                match terminate.as_mut() {
                    Some(signal) => {
                        signal.recv().await;
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminated => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    });
}
