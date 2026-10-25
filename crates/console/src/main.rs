mod app;
mod format;
mod input;
mod logging;
mod ui;

use app::App;
use clap::{Parser, Subcommand};
use empirebt_app::{Handle, Store};
use empirebt_backend::{ClientConfig, ConfigFile, MagnetLink, TorrentInfo, TransferState};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::execute;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// How often the dashboard redraws and samples transfer rates.
const TICK: Duration = Duration::from_millis(250);

/// Environment variables that are not settings.
const DATA_DIR_VARIABLE: &str = "EMPIREBT_DATA_DIR";
const PASSWORD_VARIABLE: &str = "EMPIREBT_WEB_PASSWORD";
const PASSWORD_FILE_VARIABLE: &str = "EMPIREBT_WEB_PASSWORD_FILE";

#[derive(Parser)]
#[command(
    name = "empirebt",
    version,
    about = "A BitTorrent client for the terminal",
    long_about = "A BitTorrent client for the terminal.\n\nRun without a command to open the \
                  interactive dashboard. Your torrent list is saved between runs."
)]
struct Cli {
    /// Where downloaded files are stored, for this run only.
    #[arg(long, short = 'd', global = true, value_name = "DIR")]
    download_dir: Option<PathBuf>,

    /// Where the torrent list and settings are kept.
    #[arg(long, global = true, value_name = "DIR", env = DATA_DIR_VARIABLE)]
    data_dir: Option<PathBuf>,

    /// TCP port for incoming peer connections, for this run only.
    #[arg(long, short = 'p', global = true)]
    port: Option<u16>,

    /// Maximum simultaneous peer connections per torrent, for this run only.
    #[arg(long, global = true, value_parser = clap::value_parser!(u16).range(1..))]
    max_peers: Option<u16>,

    /// Maximum peer connections across all torrents, for this run only.
    #[arg(
        long,
        global = true,
        alias = "max-inbound",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    max_connections: Option<u32>,

    /// Total upload speed limit in KiB/s (0 for unlimited), for this run
    /// only.
    #[arg(long, global = true, value_name = "KIB_PER_SEC")]
    upload_limit: Option<u64>,

    /// Total download speed limit in KiB/s (0 for unlimited), for this run
    /// only.
    #[arg(long, global = true, value_name = "KIB_PER_SEC")]
    download_limit: Option<u64>,

    /// Do not use the DHT this run; trackers only.
    #[arg(long, global = true)]
    no_dht: bool,

    /// Do not look for peers on the local network this run.
    #[arg(long, global = true)]
    no_lsd: bool,

    /// Use only this network interface (a name like wg0, or an IPv4
    /// address) for every connection, for this run only. While it is down,
    /// nothing connects.
    #[arg(long, global = true, value_name = "NAME|IP")]
    interface: Option<String>,

    /// DHT bootstrap node (host:port) for this run, replacing the configured
    /// nodes. Repeat for several nodes.
    #[arg(long = "dht-bootstrap", global = true, value_name = "HOST:PORT")]
    dht_bootstrap: Vec<String>,

    /// Seconds to wait on an unresponsive peer or tracker, for this run only.
    #[arg(long, global = true, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    peer_timeout: Option<u64>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the interactive dashboard (the default).
    Ui {
        /// Also serve the web UI (see the [web] settings).
        #[arg(long)]
        web: bool,
    },
    /// Run without the dashboard, serving the web UI, until stopped with
    /// Ctrl-C.
    Serve,
    /// Manage access to the web UI.
    Web {
        #[command(subcommand)]
        action: WebAction,
    },
    /// Add .torrent files (paths or URLs) or magnet links without opening the dashboard.
    Add {
        #[arg(required = true, value_name = "FILE|MAGNET")]
        sources: Vec<String>,
    },
    /// Print the torrent list.
    List,
    /// Remove a torrent from the list by its number. Files stay on disk.
    Remove {
        #[arg(value_name = "NUMBER")]
        index: usize,
    },
    /// Show settings, or change one with `config set <key> <value>`.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Exit successfully if the web UI of a running EmpireBT accepts
    /// connections (for container health checks).
    Health,
    /// Bring torrents over from another client.
    Import {
        #[command(subcommand)]
        from: ImportFrom,
    },
}

#[derive(Subcommand)]
enum ImportFrom {
    /// From a running qBittorrent, through its Web UI (read only): every
    /// torrent where its files are, with its category, tags, state, and
    /// progress. Shows what it will do and asks first.
    Qbittorrent {
        /// qBittorrent's Web UI address.
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
        /// Web UI username; leave out if qBittorrent lets this computer in
        /// without one.
        #[arg(long)]
        username: Option<String>,
        /// Web UI password; asked for when a username is given without
        /// one. Also read from EMPIREBT_QBITTORRENT_PASSWORD.
        #[arg(long, env = "EMPIREBT_QBITTORRENT_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Rewrite paths qBittorrent reports, for a qBittorrent that sees
        /// the files elsewhere (such as in Docker): --map /downloads=/srv/torrents.
        /// Repeat for several.
        #[arg(long = "map", value_name = "FROM=TO")]
        maps: Vec<String>,
        /// Check every torrent's data instead of trusting qBittorrent's
        /// progress.
        #[arg(long)]
        recheck: bool,
        /// Only show what would be imported.
        #[arg(long)]
        dry_run: bool,
        /// Import without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum WebAction {
    /// Set the web UI password (typed, or read from standard input).
    Password,
    /// Make an API token for scripts, sent as `Authorization: Bearer <token>`.
    Token {
        /// What the token is for, to tell tokens apart.
        name: String,
    },
    /// Revoke every API token.
    RevokeTokens,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Change a setting, for example `config set network.port 51413`.
    Set { key: String, value: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let store = Store::open(
        cli.data_dir
            .clone()
            .unwrap_or_else(empirebt_app::default_data_dir),
    )?;
    let mut config_file = store.config()?;
    if let Some(Command::Config { action }) = &cli.command {
        return run_config(&mut config_file, action.as_ref());
    }
    // The web UI's password and tokens live in their own file, which a
    // running EmpireBT reads when its web UI starts; they need no lock.
    if let Some(Command::Web { action }) = cli.command {
        return run_web(&store, action);
    }
    let from_environment = apply_environment(&mut config_file, std::env::vars())?;
    if let Some(Command::Health) = &cli.command {
        return check_health(&config_file);
    }
    // The dashboard owns the terminal, so it logs to a file; everything
    // else to standard error. One-off commands (add, list, import, …) say
    // only what went wrong, unless EMPIREBT_LOG_LEVEL asks for more.
    let dashboard = matches!(cli.command, None | Some(Command::Ui { .. }));
    let one_off = !dashboard && !matches!(cli.command, Some(Command::Serve));
    let level = if one_off && !config_file.is_overridden("log.level") {
        "warn".to_owned()
    } else {
        config_file.get("log.level").unwrap_or_default()
    };
    let log_path = store.log_path();
    let destination = if dashboard {
        logging::Destination::File(&log_path)
    } else {
        logging::Destination::Stderr
    };
    if let Some(problem) = logging::start(&level, destination) {
        eprintln!("warning: {problem}");
    }
    let (config, mut overridden) = apply_overrides(
        config_file.client_config().map_err(|e| e.to_string())?,
        &cli,
    );
    for key in from_environment {
        if !overridden.contains(&key) {
            overridden.push(key);
        }
    }
    // One process per data directory: a dashboard and a script must not
    // both write the session.
    let _lock = store.lock()?;
    let command = cli.command.unwrap_or(Command::Ui { web: false });
    match command {
        Command::Ui { web } => {
            if !std::io::stdout().is_terminal() {
                return Err("the dashboard needs a terminal; try `empirebt list`".to_owned());
            }
            let web_settings = web
                .then(|| empirebt_web::WebSettings::from_config(&config_file))
                .transpose()?;
            if web {
                password_from_environment(&store)?;
            }
            let engine = Handle::start(store.clone(), config_file, config, overridden)?;
            let mut app = App::new(engine.clone());
            let _web = match web_settings {
                Some(settings) => {
                    match empirebt_web::Web::start(engine.clone(), &store, settings) {
                        Ok(web) => {
                            app.show_web(app::WebInfo::of(&web));
                            Some(web)
                        }
                        Err(error) => {
                            engine.shutdown();
                            return Err(error);
                        }
                    }
                }
                None => None,
            };
            return run_dashboard(app);
        }
        Command::Serve => {
            let settings = empirebt_web::WebSettings::from_config(&config_file)?;
            password_from_environment(&store)?;
            let engine = Handle::start(store.clone(), config_file, config, overridden)?;
            let mut web = match empirebt_web::Web::start(engine.clone(), &store, settings) {
                Ok(web) => web,
                Err(error) => {
                    engine.shutdown();
                    return Err(error);
                }
            };
            print_web_access(&web);
            println!("Press Ctrl-C to stop.");
            empirebt_web::wait_for_interrupt();
            println!("Stopping…");
            web.stop();
            engine.shutdown();
            return Ok(());
        }
        _ => {}
    }
    if let Command::Import {
        from:
            ImportFrom::Qbittorrent {
                url,
                username,
                password,
                maps,
                recheck,
                dry_run,
                yes,
            },
    } = &command
    {
        return import_qbittorrent(
            &store,
            &config,
            QbittorrentImport {
                url,
                username: username.as_deref(),
                password: password.as_deref(),
                maps,
                recheck: *recheck,
                dry_run: *dry_run,
                yes: *yes,
            },
        );
    }
    let (mut client, warnings) = store.load(config.clone())?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    let binding = config.binding();

    match command {
        Command::Ui { .. }
        | Command::Serve
        | Command::Web { .. }
        | Command::Health
        | Command::Import { .. } => {
            unreachable!("handled above")
        }
        Command::Add { sources } => {
            // Like the dashboard; the dashboard starts these when it opens.
            let start_on_add = config_file.get("console.start_on_add").as_deref() != Some("false");
            let mut failed = false;
            for source in sources {
                if source.starts_with("magnet:") {
                    match MagnetLink::parse(&source) {
                        Ok(link) if client.torrent_index(link.info_hash()).is_some() => {
                            println!("already added: {}", link.name().unwrap_or(&source));
                        }
                        Ok(link) => {
                            let index = client.add_magnet(link);
                            if start_on_add {
                                let _ = client.mark_started(index);
                            }
                            println!("added #{index}: {}", client.torrents()[index].name());
                        }
                        Err(error) => {
                            eprintln!("cannot add magnet link: {error}");
                            failed = true;
                        }
                    }
                    continue;
                }
                let path = PathBuf::from(&source);
                let parsed = if TorrentInfo::is_url(&source) {
                    println!("downloading {source}");
                    TorrentInfo::from_url_with(&source, Duration::from_secs(20), &binding)
                } else {
                    TorrentInfo::from_file(&path)
                };
                match parsed {
                    Ok(info) if client.torrent_index(info.info_hash()).is_some() => {
                        println!("already added: {}", info.name());
                    }
                    Ok(info) => {
                        let name = info.name().to_owned();
                        let index = client.add_torrent(info);
                        if start_on_add {
                            let _ = client.mark_started(index);
                        }
                        println!("added #{index}: {name}");
                    }
                    Err(error) => {
                        eprintln!("cannot add {}: {error}", path.display());
                        failed = true;
                    }
                }
            }
            store.save(&client)?;
            if failed {
                return Err("some torrents could not be added".to_owned());
            }
            Ok(())
        }
        Command::List => {
            print_list(&client);
            Ok(())
        }
        Command::Config { .. } => unreachable!("handled before loading the session"),
        Command::Remove { index } => {
            let torrent = client
                .remove_torrent(index)
                .map_err(|_| format!("there is no torrent #{index}; see `empirebt list`"))?;
            store.save(&client)?;
            println!("removed {} (files kept on disk)", torrent.name());
            Ok(())
        }
    }
}

struct QbittorrentImport<'a> {
    url: &'a str,
    username: Option<&'a str>,
    password: Option<&'a str>,
    maps: &'a [String],
    recheck: bool,
    dry_run: bool,
    yes: bool,
}

/// Plans an import from qBittorrent, shows it, and carries it out once
/// confirmed.
fn import_qbittorrent(
    store: &Store,
    config: &ClientConfig,
    request: QbittorrentImport<'_>,
) -> Result<(), String> {
    use empirebt_app::qbittorrent;
    let maps = request
        .maps
        .iter()
        .map(|map| {
            map.split_once('=')
                .filter(|(from, to)| !from.is_empty() && !to.is_empty())
                .map(|(from, to)| (from.to_owned(), PathBuf::from(to)))
                .ok_or_else(|| format!("--map takes FROM=TO, not {map}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let password = match (request.username, request.password) {
        (Some(_), None) => Some(read_secret("qBittorrent password: ")?),
        (_, password) => password.map(str::to_owned),
    };
    let connection = qbittorrent::Connection::open(
        request.url,
        request.username,
        password.as_deref(),
        config.binding(),
    )?;
    let options = qbittorrent::Options {
        maps,
        recheck: request.recheck,
    };
    let plan = qbittorrent::plan(&connection, &options, store, config)?;
    print_import_plan(&plan);
    let importing = plan
        .torrents
        .iter()
        .filter(|torrent| torrent.skip.is_none())
        .count();
    if request.dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }
    if importing == 0 && plan.categories.is_empty() && plan.tags.is_empty() {
        println!("\nNothing to import.");
        return Ok(());
    }
    if !request.yes {
        if !std::io::stdin().is_terminal() {
            return Err("add --yes to import without being asked".to_owned());
        }
        print!(
            "\nImport {importing} torrent{}? qBittorrent is left as it is. [y/N] ",
            if importing == 1 { "" } else { "s" }
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing was changed.");
            return Ok(());
        }
    }
    let imported = qbittorrent::import(&plan, store, config)?;
    println!(
        "Imported {} torrent{}, {} new categor{}, {} new tag{}.",
        imported.torrents,
        if imported.torrents == 1 { "" } else { "s" },
        imported.categories,
        if imported.categories == 1 { "y" } else { "ies" },
        imported.tags,
        if imported.tags == 1 { "" } else { "s" },
    );
    println!(
        "Before opening EmpireBT, stop these torrents in qBittorrent (or close it), so the two \
         do not share the same files and port."
    );
    Ok(())
}

fn print_import_plan(plan: &empirebt_app::qbittorrent::Plan) {
    println!("{}\n", plan.source);
    if !plan.categories.is_empty() {
        let categories: Vec<String> = plan
            .categories
            .iter()
            .map(|(name, folder)| match folder {
                Some(folder) => format!("{name} ({})", folder.display()),
                None => name.clone(),
            })
            .collect();
        println!("Categories: {}", categories.join(", "));
    }
    if !plan.tags.is_empty() {
        let tags: Vec<&str> = plan.tags.iter().map(String::as_str).collect();
        println!("Tags: {}", tags.join(", "));
    }
    println!();
    for torrent in &plan.torrents {
        if let Some(reason) = &torrent.skip {
            println!("  ✕ {}: skipped, {reason}", torrent.name);
            continue;
        }
        let state = match torrent.state {
            TransferState::Seeding | TransferState::Complete => "seeding".to_owned(),
            TransferState::Downloading => "downloading".to_owned(),
            TransferState::Paused => "paused".to_owned(),
        };
        let progress = match torrent.progress() {
            Some(progress) if progress < 1.0 => format!(" at {:.0}%", progress * 100.0),
            Some(_) => String::new(),
            None => " (checked when started)".to_owned(),
        };
        let mut line = format!(
            "  {} {}: {state}{progress}",
            if torrent.warnings.is_empty() {
                "✓"
            } else {
                "!"
            },
            torrent.name
        );
        if let Some(folder) = &torrent.folder {
            line.push_str(&format!(", in {}", folder.display()));
        }
        if let Some(category) = &torrent.category {
            line.push_str(&format!(", category {category}"));
        }
        if !torrent.tags.is_empty() {
            line.push_str(&format!(", tags {}", torrent.tags.join(", ")));
        }
        println!("{line}");
        for warning in &torrent.warnings {
            println!("      {warning}");
        }
    }
    let skipped = plan
        .torrents
        .iter()
        .filter(|torrent| torrent.skip.is_some())
        .count();
    println!(
        "\n{} to import, {skipped} skipped.",
        plan.torrents.len() - skipped
    );
}

/// Applies `EMPIREBT_<SECTION>_<NAME>` environment variables (for example
/// `EMPIREBT_NETWORK_UPLOAD_LIMIT=500`) on top of the settings file for
/// this run, without saving them; returns the keys they override.
/// Unknown `EMPIREBT_` variables are warned about, since they are likely
/// typos.
fn apply_environment(
    config: &mut ConfigFile,
    variables: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<&'static str>, String> {
    let names: Vec<(String, &'static str)> = config
        .settings()
        .into_iter()
        .map(|setting| (environment_name(setting.key), setting.key))
        .collect();
    let mut applied = Vec::new();
    for (name, value) in variables {
        if !name.starts_with("EMPIREBT_")
            || [DATA_DIR_VARIABLE, PASSWORD_VARIABLE, PASSWORD_FILE_VARIABLE]
                .contains(&name.as_str())
        {
            continue;
        }
        match names.iter().find(|(candidate, _)| *candidate == name) {
            Some((_, key)) => {
                config
                    .set_override(key, &value)
                    .map_err(|error| format!("{name}: {error}"))?;
                applied.push(*key);
            }
            None => eprintln!("warning: {name} is not an EmpireBT setting; ignoring it"),
        }
    }
    Ok(applied)
}

/// `network.upload_limit` → `EMPIREBT_NETWORK_UPLOAD_LIMIT`.
fn environment_name(key: &str) -> String {
    format!("EMPIREBT_{}", key.replace('.', "_").to_ascii_uppercase())
}

/// Sets the web UI password from `EMPIREBT_WEB_PASSWORD`, or from the file
/// `EMPIREBT_WEB_PASSWORD_FILE` names (a Docker secret), when either is set.
fn password_from_environment(store: &Store) -> Result<(), String> {
    let (variable, password) = match (
        std::env::var(PASSWORD_VARIABLE),
        std::env::var_os(PASSWORD_FILE_VARIABLE),
    ) {
        (Ok(password), _) => (PASSWORD_VARIABLE, password),
        (Err(_), Some(path)) => (
            PASSWORD_FILE_VARIABLE,
            std::fs::read_to_string(&path)
                .map_err(|error| format!("{PASSWORD_FILE_VARIABLE}: {error}"))?
                .trim_end_matches(['\r', '\n'])
                .to_owned(),
        ),
        (Err(_), None) => return Ok(()),
    };
    empirebt_web::set_password(store, &password).map_err(|error| format!("{variable}: {error}"))
}

/// Connects to the web UI's port, as a container health check.
fn check_health(config: &ConfigFile) -> Result<(), String> {
    let settings = empirebt_web::WebSettings::from_config(config)?;
    let address = match settings.address {
        std::net::IpAddr::V4(address) if address.is_unspecified() => {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
        std::net::IpAddr::V6(address) if address.is_unspecified() => {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
        address => address,
    };
    let target = std::net::SocketAddr::new(address, settings.port);
    std::net::TcpStream::connect_timeout(&target, Duration::from_secs(3))
        .map_err(|error| format!("the web UI on {target} is not answering: {error}"))?;
    println!("ok");
    Ok(())
}

/// Applies command-line flags on top of the settings file for this run,
/// returning the keys they override.
fn apply_overrides(mut config: ClientConfig, cli: &Cli) -> (ClientConfig, Vec<&'static str>) {
    let mut overridden = Vec::new();
    if let Some(dir) = &cli.download_dir {
        config = config.with_download_directory(std::path::absolute(dir).unwrap_or(dir.clone()));
        overridden.push("storage.download_dir");
    }
    if let Some(port) = cli.port {
        config = config.with_listen_port(port);
        overridden.push("network.port");
    }
    if let Some(peers) = cli.max_peers {
        config = config.with_max_peer_connections(usize::from(peers));
        overridden.push("network.max_peers");
    }
    if let Some(connections) = cli.max_connections {
        config = config.with_max_connections(connections as usize);
        overridden.push("network.max_connections");
    }
    if let Some(kib) = cli.upload_limit {
        config = config.with_upload_limit(kib.saturating_mul(1024));
        overridden.push("network.upload_limit");
    }
    if let Some(kib) = cli.download_limit {
        config = config.with_download_limit(kib.saturating_mul(1024));
        overridden.push("network.download_limit");
    }
    if let Some(seconds) = cli.peer_timeout {
        config = config.with_peer_timeout(Duration::from_secs(seconds));
        overridden.push("network.peer_timeout");
    }
    if let Some(interface) = &cli.interface {
        config = config.with_interface(Some(interface.clone()));
        overridden.push("network.interface");
    }
    if cli.no_lsd {
        config = config.with_lsd(false);
        overridden.push("network.lsd");
    }
    if cli.no_dht {
        config = config.with_dht(false);
        overridden.push("dht.enabled");
    }
    if !cli.dht_bootstrap.is_empty() {
        config = config.with_dht_bootstrap(cli.dht_bootstrap.clone());
        overridden.push("dht.bootstrap");
    }
    (config, overridden)
}

fn run_config(config: &mut ConfigFile, action: Option<&ConfigAction>) -> Result<(), String> {
    match action {
        None => {
            println!("Settings file: {}\n", config.path().display());
            let settings = config.settings();
            let width = settings
                .iter()
                .map(|setting| setting.key.len())
                .max()
                .unwrap_or(0);
            let terminal = std::io::stdout().is_terminal();
            for setting in settings {
                println!(
                    "  {:<width$}  {}",
                    setting.key,
                    config.get(setting.key).unwrap_or_default()
                );
                let description = if terminal {
                    format!("\x1b[2m{}\x1b[0m", setting.description)
                } else {
                    setting.description.to_owned()
                };
                println!("  {:<width$}  {description}", "");
            }
            println!("\nChange one with: empirebt config set <key> <value>");
            Ok(())
        }
        Some(ConfigAction::Set { key, value }) => {
            config.set(key, value).map_err(|error| error.to_string())?;
            config.save().map_err(|error| error.to_string())?;
            println!("{key} = {}", config.get(key).unwrap_or_default());
            Ok(())
        }
    }
}

/// Tells how to reach and sign in to the web UI.
fn print_web_access(web: &empirebt_web::Web) {
    println!("Web UI: {}", web.url());
    println!(
        "Sign in with this one-time link (valid for 10 minutes):\n  {}",
        web.login_link()
    );
    if let Some(password) = web.generated_password() {
        println!(
            "First start: the web UI password is {password}\n  (change it with `empirebt web password`)"
        );
    }
    if let Some(fingerprint) = web.certificate_fingerprint() {
        println!("HTTPS certificate fingerprint (SHA-256):\n  {fingerprint}");
    }
}

fn run_web(store: &Store, action: WebAction) -> Result<(), String> {
    match action {
        WebAction::Password => {
            let password = read_secret("New web UI password: ")?;
            let again = if std::io::stdin().is_terminal() {
                read_secret("Again: ")?
            } else {
                password.clone()
            };
            if password != again {
                return Err("the passwords differ".to_owned());
            }
            empirebt_web::set_password(store, &password)?;
            println!(
                "Saved. A running web UI uses it from its next restart; browsers signed in \
                 now stay signed in until then."
            );
        }
        WebAction::Token { name } => {
            let token = empirebt_web::create_token(store, &name)?;
            println!("{token}");
            eprintln!("Shown once. Send it as `Authorization: Bearer <token>`.");
        }
        WebAction::RevokeTokens => {
            let count = empirebt_web::revoke_tokens(store)?;
            println!(
                "Revoked {count} token{}.",
                if count == 1 { "" } else { "s" }
            );
        }
    }
    Ok(())
}

/// Reads a line without showing it when typed at a terminal; from a pipe,
/// reads it as is.
fn read_secret(prompt: &str) -> Result<String, String> {
    use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    use std::io::Write;
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_owned());
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    ratatui::crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
    let mut secret = String::new();
    let result = loop {
        match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err("cancelled".to_owned());
                }
                KeyCode::Char(character) => secret.push(character),
                KeyCode::Backspace => {
                    secret.pop();
                }
                KeyCode::Esc => break Err("cancelled".to_owned()),
                _ => {}
            },
            Ok(_) => {}
            Err(error) => break Err(error.to_string()),
        }
    };
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
    eprintln!();
    result.map(|()| secret)
}

fn run_dashboard(mut app: App) -> Result<(), String> {
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = event_loop(&mut terminal, &mut app);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    app.shutdown();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<(), String> {
    while !app.quit {
        app.tick();
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map_err(|error| error.to_string())?;
        if event::poll(TICK).map_err(|error| error.to_string())? {
            // Drain everything queued so held keys and pastes stay responsive.
            loop {
                match event::read().map_err(|error| error.to_string())? {
                    Event::Key(key) => app.handle_key(key),
                    Event::Paste(text) => app.handle_paste(&text),
                    _ => {}
                }
                if app.quit || !event::poll(Duration::ZERO).map_err(|error| error.to_string())? {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn print_list(client: &empirebt_backend::Client) {
    if client.torrents().is_empty() {
        println!("No torrents yet. Add one with `empirebt add <file.torrent>`.");
        return;
    }
    let color = std::io::stdout().is_terminal();
    let name_width = client
        .torrents()
        .iter()
        .map(|torrent| torrent.name().chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 48);
    println!("{:>3}  {:<name_width$}  {:>10}  STATE", "#", "NAME", "SIZE");
    for (index, torrent) in client.torrents().iter().enumerate() {
        let size = torrent
            .info()
            .map(|info| format::bytes(info.total_length()))
            .unwrap_or_else(|| "magnet".to_owned());
        let (label, ansi) = match torrent.state() {
            TransferState::Downloading => ("Downloading", "94"),
            TransferState::Seeding | TransferState::Complete => ("Seeding", "92"),
            TransferState::Paused => ("Paused", "90"),
        };
        let mut name: String = torrent.name().chars().take(name_width).collect();
        if torrent.name().chars().count() > name_width {
            name.pop();
            name.push('…');
        }
        let state = if color {
            format!("\x1b[{ansi}m{label}\x1b[0m")
        } else {
            label.to_owned()
        };
        println!("{index:>3}  {name:<name_width$}  {size:>10}  {state}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// Docker passes settings as `EMPIREBT_<SECTION>_<NAME>` variables;
    /// they apply for the run, other variables are left alone, and a bad
    /// value is an error naming the variable.
    #[test]
    fn settings_come_from_environment_variables() {
        let dir = std::env::temp_dir().join(format!("empirebt-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.clone()).unwrap();
        let mut config = store.config().unwrap();
        let variables = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect::<Vec<_>>()
        };
        let applied = apply_environment(
            &mut config,
            variables(&[
                ("EMPIREBT_NETWORK_UPLOAD_LIMIT", "500"),
                ("EMPIREBT_WEB_ADDRESS", "0.0.0.0"),
                ("EMPIREBT_WEB_HTTPS", "off"),
                ("EMPIREBT_STORAGE_DOWNLOAD_DIR", "/downloads"),
                ("EMPIREBT_DATA_DIR", "/data"),
                ("EMPIREBT_WEB_PASSWORD", "not a setting"),
                ("EMPIREBT_NO_SUCH_THING", "1"),
                ("HOME", "/root"),
            ]),
        )
        .unwrap();
        assert_eq!(
            applied,
            [
                "network.upload_limit",
                "web.address",
                "web.https",
                "storage.download_dir"
            ]
        );
        assert_eq!(config.client_config().unwrap().upload_limit(), 500 * 1024);
        let web = empirebt_web::WebSettings::from_config(&config).unwrap();
        assert!(web.address.is_unspecified());
        assert!(web.plain_http);

        let error = apply_environment(&mut config, variables(&[("EMPIREBT_NETWORK_PORT", "x")]))
            .unwrap_err();
        assert!(error.starts_with("EMPIREBT_NETWORK_PORT"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
