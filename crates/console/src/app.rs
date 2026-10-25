//! Dashboard state: the selection, the open dialog, and the actions behind
//! each key binding. The application itself runs in the `empirebt-app`
//! engine; the dashboard draws its snapshots and sends it commands.

use crate::input::PathInput;
use empirebt_app::{
    AddRequest, AddSource, Command, DhtState, Failure, Handle, InfoHash, Notice, PeerView, Reply,
    SeedStatus, SettingView, Snapshot, TorrentView,
};
use empirebt_backend::SettingKind;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a notice stays in the footer.
const NOTICE_LIFETIME: Duration = Duration::from_secs(6);
/// How often the Peers tab is refreshed.
const PEER_REFRESH: Duration = Duration::from_secs(1);

pub enum Mode {
    Normal,
    /// Editing a torrent's tags (comma-separated) or category.
    Label {
        torrent: InfoHash,
        kind: LabelKind,
        input: PathInput,
        error: Option<String>,
    },
    /// Choosing which torrents the list shows.
    Filter {
        selected: usize,
    },
    /// Choosing the order of the list.
    Sort {
        selected: usize,
    },
    Add {
        input: PathInput,
        /// The folder to save it in, the download directory at first.
        save: PathInput,
        /// Whether the folder field has the cursor.
        on_save: bool,
        error: Option<String>,
    },
    ConfirmRemove(InfoHash),
    Help,
    /// How to reach the web UI, shown when the dashboard opens with it.
    WebInfo,
    Settings {
        selected: usize,
        /// The value being typed, while a setting is being changed.
        editing: Option<PathInput>,
        error: Option<String>,
    },
}

/// How to reach the web UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebInfo {
    pub url: String,
    /// A one-time login link.
    pub link: String,
    /// The password made at this first start, if one was.
    pub password: Option<String>,
    /// The HTTPS certificate's SHA-256 fingerprint.
    pub fingerprint: Option<String>,
}

impl WebInfo {
    pub fn of(web: &empirebt_web::Web) -> Self {
        Self {
            url: web.url().to_owned(),
            link: web.login_link(),
            password: web.generated_password().map(str::to_owned),
            fingerprint: web.certificate_fingerprint().map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    Category,
    Tags,
}

/// Which torrents the list shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Category(String),
    Uncategorized,
    Tag(String),
    Untagged,
}

impl Filter {
    pub fn label(&self) -> String {
        match self {
            Self::All => "All torrents".to_owned(),
            Self::Category(name) => format!("Category: {name}"),
            Self::Uncategorized => "No category".to_owned(),
            Self::Tag(name) => format!("Tag: {name}"),
            Self::Untagged => "No tags".to_owned(),
        }
    }

    fn matches(&self, torrent: &TorrentView) -> bool {
        match self {
            Self::All => true,
            Self::Category(name) => torrent.category.as_ref() == Some(name),
            Self::Uncategorized => torrent.category.is_none(),
            Self::Tag(name) => torrent.tags.contains(name),
            Self::Untagged => torrent.tags.is_empty(),
        }
    }
}

/// The order of the list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// The order torrents were added in.
    #[default]
    Added,
    Name,
    Size,
    Progress,
    Down,
    Up,
    Peers,
    TimeLeft,
}

impl Sort {
    pub const ALL: [Self; 8] = [
        Self::Added,
        Self::Name,
        Self::Size,
        Self::Progress,
        Self::Down,
        Self::Up,
        Self::Peers,
        Self::TimeLeft,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Progress => "Progress",
            Self::Down => "Download speed",
            Self::Up => "Upload speed",
            Self::Peers => "Peers",
            Self::TimeLeft => "Time left",
        }
    }

    /// Whether `a` comes before `b`: names A to Z, time left soonest
    /// first, and amounts (size, progress, speeds, peers) largest first.
    fn compare(self, a: &TorrentView, b: &TorrentView) -> std::cmp::Ordering {
        let amount = |view: &TorrentView| -> f64 {
            match self {
                Self::Size => view.size().unwrap_or(0) as f64,
                Self::Progress => view.fraction.unwrap_or(-1.0),
                Self::Down => view.down_rate,
                Self::Up => view.up_rate,
                Self::Peers => view.peers.unwrap_or(0) as f64,
                _ => 0.0,
            }
        };
        let time_left = |view: &TorrentView| {
            if view.down_rate >= 1.0 && view.remaining() > 0 {
                view.remaining() as f64 / view.down_rate
            } else {
                f64::INFINITY
            }
        };
        match self {
            Self::Added => std::cmp::Ordering::Equal,
            Self::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            Self::TimeLeft => time_left(a).total_cmp(&time_left(b)),
            _ => amount(b).total_cmp(&amount(a)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Overview,
    Files,
    Peers,
    Trackers,
}

impl DetailTab {
    pub const ALL: [Self; 4] = [Self::Overview, Self::Files, Self::Peers, Self::Trackers];

    pub fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Files => "Files",
            Self::Peers => "Peers",
            Self::Trackers => "Trackers",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn cycle(self, step: isize) -> Self {
        let next = (self.index() as isize + step).rem_euclid(Self::ALL.len() as isize);
        Self::ALL[next as usize]
    }
}

pub struct App {
    engine: Handle,
    /// The engine's latest snapshot, refreshed every tick.
    view: Arc<Snapshot>,
    /// A torrent to select once a snapshot shows it.
    select: Option<InfoHash>,
    pub filter: Filter,
    pub sort: Sort,
    /// Indexes into the snapshot's torrents of those the filter shows.
    visible: Vec<usize>,
    pub table: TableState,
    pub tab: DetailTab,
    pub mode: Mode,
    pub quit: bool,
    /// The web UI, when the dashboard serves one too.
    pub web: Option<WebInfo>,
    /// The selected torrent's peers while the Peers tab is open, and when
    /// they were fetched.
    peers: Option<(InfoHash, Vec<PeerView>, Instant)>,
}

impl App {
    pub fn new(engine: Handle) -> Self {
        let view = engine.snapshot();
        let mut table = TableState::default();
        if !view.torrents.is_empty() {
            table.select(Some(0));
        }
        Self {
            visible: (0..view.torrents.len()).collect(),
            engine,
            view,
            select: None,
            filter: Filter::All,
            sort: Sort::Added,
            table,
            tab: DetailTab::Overview,
            mode: Mode::Normal,
            quit: false,
            web: None,
            peers: None,
        }
    }

    /// The selected torrent's peers, while the Peers tab shows them.
    pub fn peers(&self) -> Option<&[PeerView]> {
        let hash = self.selected().and_then(|index| self.row(index))?.hash;
        match &self.peers {
            Some((of, peers, _)) if *of == hash => Some(peers),
            _ => None,
        }
    }

    /// Fetches the selected torrent's peers about once a second, while the
    /// Peers tab is open.
    fn refresh_peers(&mut self) {
        if self.tab != DetailTab::Peers {
            self.peers = None;
            return;
        }
        let Some(hash) = self
            .selected()
            .and_then(|index| self.row(index))
            .map(|row| row.hash)
        else {
            return;
        };
        let fresh = matches!(
            &self.peers,
            Some((of, _, at)) if *of == hash && at.elapsed() < PEER_REFRESH
        );
        if fresh {
            return;
        }
        if let Ok(Reply::Peers(mut peers)) = self.engine.call(Command::Peers(hash)) {
            // Who gets the most from us first, then who gives the most.
            peers.sort_by(|a, b| {
                let (a, b) = (&a.info, &b.info);
                b.up_rate
                    .total_cmp(&a.up_rate)
                    .then(b.down_rate.total_cmp(&a.down_rate))
                    .then(b.uploaded.cmp(&a.uploaded))
                    .then(b.downloaded.cmp(&a.downloaded))
            });
            self.peers = Some((hash, peers, Instant::now()));
        }
    }

    /// Shows how to reach the web UI, now and in the help.
    pub fn show_web(&mut self, web: WebInfo) {
        self.web = Some(web);
        self.mode = Mode::WebInfo;
    }

    pub fn download_dir(&self) -> &std::path::Path {
        &self.view.download_dir
    }

    pub fn listen_port(&self) -> u16 {
        self.view.network.listen_port
    }

    pub fn seed_status(&self) -> SeedStatus {
        self.view.network.seed_status.clone()
    }

    pub fn seeding_peers(&self) -> usize {
        self.view.network.seeding_peers
    }

    pub fn settings(&self) -> &[SettingView] {
        &self.view.settings
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.view.config_path
    }

    pub fn dht_state(&self) -> &DhtState {
        &self.view.network.dht
    }

    /// Whether peers can connect over uTP as well as TCP.
    pub fn utp_running(&self) -> bool {
        self.view.network.utp
    }

    /// The network interface connections are bound to, if one was chosen,
    /// and whether it is up.
    pub fn interface(&self) -> (Option<&str>, bool) {
        (
            self.view.network.interface.as_deref(),
            self.view.network.interface_up,
        )
    }

    pub fn lsd_running(&self) -> bool {
        self.view.network.lsd
    }

    pub fn dht_nodes(&self) -> Option<usize> {
        self.view.network.dht_nodes
    }

    /// Of [`App::dht_nodes`], those reached over IPv6.
    pub fn dht_ipv6_nodes(&self) -> usize {
        self.view.network.dht_ipv6_nodes
    }

    /// Torrents the filter shows.
    pub fn len(&self) -> usize {
        self.visible.len()
    }

    /// Every torrent, whatever the filter shows.
    pub fn total(&self) -> usize {
        self.view.torrents.len()
    }

    /// Every category and tag, for choosing a filter.
    pub fn filter_choices(&self) -> Vec<Filter> {
        let mut choices = vec![Filter::All];
        if !self.view.categories.is_empty() {
            choices.extend(self.view.categories.iter().cloned().map(Filter::Category));
            choices.push(Filter::Uncategorized);
        }
        if !self.view.tags.is_empty() {
            choices.extend(self.view.tags.iter().cloned().map(Filter::Tag));
            choices.push(Filter::Untagged);
        }
        choices
    }

    /// Known names for the label being edited, for hints and completion.
    pub fn known_labels(&self, kind: LabelKind) -> &[String] {
        match kind {
            LabelKind::Category => &self.view.categories,
            LabelKind::Tags => &self.view.tags,
        }
    }

    pub fn selected(&self) -> Option<usize> {
        self.table.selected().filter(|index| *index < self.len())
    }

    pub fn rows(&self) -> impl Iterator<Item = &TorrentView> {
        self.visible.iter().map(|index| &self.view.torrents[*index])
    }

    pub fn row(&self, index: usize) -> Option<&TorrentView> {
        self.visible
            .get(index)
            .map(|index| &self.view.torrents[*index])
    }

    /// The download and upload speed limits in bytes per second; 0 means
    /// unlimited.
    pub fn rate_limits(&self) -> (u64, u64) {
        self.view.rate_limits
    }

    pub fn totals(&self) -> (f64, f64) {
        self.view.totals
    }

    pub fn active_count(&self) -> usize {
        self.view.active_count
    }

    pub fn seeding_count(&self) -> usize {
        self.view.seeding_count
    }

    /// The newest notice, while it is recent enough to show.
    pub fn notice(&self) -> Option<&Notice> {
        self.view
            .notices
            .last()
            .filter(|notice| notice.at.elapsed() < NOTICE_LIFETIME)
    }

    // ----- event loop hooks -------------------------------------------------

    /// Takes the engine's latest snapshot.
    pub fn tick(&mut self) {
        self.refresh();
        self.refresh_peers();
    }

    /// Stops the engine: transfers stop and the session is saved.
    pub fn shutdown(&mut self) {
        self.engine.shutdown();
    }

    fn refresh(&mut self) {
        // The selected torrent stays selected while it is shown.
        let selected = self
            .selected()
            .and_then(|index| self.row(index))
            .map(|row| row.hash);
        self.view = self.engine.snapshot();
        // A filter on a removed category or tag shows everything again.
        let gone = match &self.filter {
            Filter::Category(name) => !self.view.categories.contains(name),
            Filter::Tag(name) => !self.view.tags.contains(name),
            _ => false,
        };
        if gone {
            self.filter = Filter::All;
        }
        self.visible = (0..self.view.torrents.len())
            .filter(|index| self.filter.matches(&self.view.torrents[*index]))
            .collect();
        // Ties keep the order torrents were added in.
        let (sort, torrents) = (self.sort, &self.view.torrents);
        self.visible
            .sort_by(|a, b| sort.compare(&torrents[*a], &torrents[*b]).then(a.cmp(b)));
        if let Some(index) = selected.and_then(|hash| self.visible_position(&hash)) {
            self.table.select(Some(index));
        }
        if let Some(hash) = self.select
            && let Some(index) = self.visible_position(&hash)
        {
            self.table.select(Some(index));
            self.select = None;
        }
        // Keep the selection on the list as torrents come and go.
        let count = self.len();
        match self.table.selected() {
            _ if count == 0 => self.table.select(None),
            Some(index) if index >= count => self.table.select(Some(count - 1)),
            None => self.table.select(Some(0)),
            Some(_) => {}
        }
    }

    /// Sends a command, then shows its effect at once.
    fn call(&mut self, command: Command) -> Result<Reply, Failure> {
        let result = self.engine.call(command);
        self.refresh();
        result
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match &mut self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Help | Mode::WebInfo => self.mode = Mode::Normal,
            Mode::Label { .. } => self.handle_label_key(key),
            Mode::Filter { .. } => self.handle_filter_key(key),
            Mode::Sort { .. } => self.handle_sort_key(key),
            Mode::Settings { .. } => self.handle_settings_key(key),
            Mode::ConfirmRemove(hash) => {
                let hash = *hash;
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        self.mode = Mode::Normal;
                        let _ = self.call(Command::Remove(hash));
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.mode = Mode::Normal;
                    }
                    _ => {}
                }
            }
            Mode::Add {
                input,
                save,
                on_save,
                error,
            } => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Up | KeyCode::Down => *on_save = !*on_save,
                    KeyCode::Enter => {
                        if input.text().trim().is_empty() {
                            *error = Some(
                                "Type a .torrent file path or URL, or paste a magnet link"
                                    .to_owned(),
                            );
                            *on_save = false;
                            return;
                        }
                        let source = AddSource::Link(input.text().to_owned());
                        let folder = save.text().trim();
                        let save_dir =
                            (!folder.is_empty()).then(|| std::path::PathBuf::from(folder));
                        match self.call(Command::AddWith(AddRequest {
                            save_dir,
                            ..AddRequest::new(source)
                        })) {
                            Ok(reply) => {
                                if let Reply::Added(hash) = reply {
                                    self.select_torrent(hash);
                                }
                                self.mode = Mode::Normal;
                            }
                            Err(failure) => {
                                if let Some(hash) = failure.torrent {
                                    self.select_torrent(hash);
                                }
                                if let Mode::Add { error, .. } = &mut self.mode {
                                    *error = Some(failure.message);
                                }
                            }
                        }
                    }
                    code => {
                        let field = if *on_save { save } else { input };
                        match code {
                            KeyCode::Tab => field.complete(),
                            KeyCode::Char('u') if control => field.clear(),
                            KeyCode::Char('w') if control => field.delete_word(),
                            KeyCode::Char(character) if !control => {
                                field.insert(character);
                                *error = None;
                            }
                            KeyCode::Backspace => field.backspace(),
                            KeyCode::Delete => field.delete(),
                            KeyCode::Left => field.left(),
                            KeyCode::Right => field.right(),
                            KeyCode::Home => field.home(),
                            KeyCode::End => field.end(),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    /// The add dialog, with `input` in it and the download directory as
    /// the folder.
    fn add_mode(&self, input: PathInput) -> Mode {
        let mut save = PathInput::default();
        save.paste(&self.download_dir().to_string_lossy());
        Mode::Add {
            input,
            save,
            on_save: false,
            error: None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::Add {
                input,
                save,
                on_save,
                error,
            } => {
                if *on_save { save } else { input }.paste(text);
                *error = None;
            }
            Mode::Label { input, error, .. } => {
                input.paste(text);
                *error = None;
            }
            Mode::Normal => {
                let mut input = PathInput::default();
                input.paste(text);
                self.mode = self.add_mode(input);
            }
            Mode::Settings {
                editing: Some(input),
                error,
                ..
            } => {
                input.paste(text);
                *error = None;
            }
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        let selected = self
            .selected()
            .and_then(|index| self.row(index))
            .map(|row| row.hash);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.tab = self.tab.cycle(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.tab = self.tab.cycle(-1),
            KeyCode::Char('1') => self.tab = DetailTab::Overview,
            KeyCode::Char('2') => self.tab = DetailTab::Files,
            KeyCode::Char('3') => self.tab = DetailTab::Peers,
            KeyCode::Char('4') => self.tab = DetailTab::Trackers,
            KeyCode::Char('a') | KeyCode::Char('o') => {
                self.mode = self.add_mode(PathInput::default());
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('S') => {
                let selected = Sort::ALL
                    .iter()
                    .position(|sort| *sort == self.sort)
                    .unwrap_or(0);
                self.mode = Mode::Sort { selected };
            }
            KeyCode::Char('f') => {
                let selected = self
                    .filter_choices()
                    .iter()
                    .position(|choice| *choice == self.filter)
                    .unwrap_or(0);
                self.mode = Mode::Filter { selected };
            }
            KeyCode::Esc if self.filter != Filter::All => self.set_filter(Filter::All),
            KeyCode::Char('c') | KeyCode::Char(',') => {
                self.mode = Mode::Settings {
                    selected: 0,
                    editing: None,
                    error: None,
                }
            }
            _ => {
                let Some(hash) = selected else {
                    return;
                };
                let command = match key.code {
                    KeyCode::Char('s') | KeyCode::Enter => Command::Start(hash),
                    KeyCode::Char('p') => Command::Pause(hash),
                    KeyCode::Char(' ') => Command::Toggle(hash),
                    KeyCode::Char('v') => Command::Recheck(hash),
                    KeyCode::Char('x') | KeyCode::Delete => {
                        self.mode = Mode::ConfirmRemove(hash);
                        return;
                    }
                    KeyCode::Char('t') => {
                        self.edit_label(hash, LabelKind::Tags);
                        return;
                    }
                    KeyCode::Char('C') => {
                        self.edit_label(hash, LabelKind::Category);
                        return;
                    }
                    _ => return,
                };
                let _ = self.call(command);
            }
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) {
        let count = self.view.settings.len();
        let Mode::Settings {
            selected,
            editing,
            error,
        } = &mut self.mode
        else {
            return;
        };
        let Some(item) = self.view.settings.get(*selected).cloned() else {
            return;
        };
        let setting = item.setting;
        if let Some(input) = editing {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => *editing = None,
                KeyCode::Enter => {
                    let text = input.text().to_owned();
                    let result = self.change_setting(setting.key, &text);
                    if let Mode::Settings { editing, error, .. } = &mut self.mode {
                        match result {
                            Ok(()) => {
                                *editing = None;
                                *error = None;
                            }
                            Err(message) => *error = Some(message),
                        }
                    }
                }
                KeyCode::Tab if setting.kind == SettingKind::Path => input.complete(),
                KeyCode::Char('u') if control => input.clear(),
                KeyCode::Char('w') if control => input.delete_word(),
                KeyCode::Char(character) if !control => {
                    input.insert(character);
                    *error = None;
                }
                KeyCode::Backspace => input.backspace(),
                KeyCode::Delete => input.delete(),
                KeyCode::Left => input.left(),
                KeyCode::Right => input.right(),
                KeyCode::Home => input.home(),
                KeyCode::End => input.end(),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                *error = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(count - 1);
                *error = None;
            }
            KeyCode::Home | KeyCode::Char('g') => *selected = 0,
            KeyCode::End | KeyCode::Char('G') => *selected = count - 1,
            KeyCode::Enter | KeyCode::Char(' ') if setting.kind == SettingKind::Bool => {
                let flipped = if item.value == "true" {
                    "false"
                } else {
                    "true"
                };
                self.change_setting_from_list(setting.key, flipped);
            }
            KeyCode::Enter | KeyCode::Char(' ')
                if matches!(setting.kind, SettingKind::Choice(_)) =>
            {
                let SettingKind::Choice(choices) = setting.kind else {
                    unreachable!("matched above");
                };
                let next = choices
                    .iter()
                    .position(|choice| *choice == item.value)
                    .map_or(choices[0], |index| choices[(index + 1) % choices.len()]);
                self.change_setting_from_list(setting.key, next);
            }
            KeyCode::Enter => {
                let mut input = PathInput::default();
                input.paste(&item.value);
                *editing = Some(input);
            }
            _ => {}
        }
    }

    /// Changes a setting picked in the list, showing a failure there.
    fn change_setting_from_list(&mut self, key: &str, value: &str) {
        if let Err(message) = self.change_setting(key, value)
            && let Mode::Settings { error, .. } = &mut self.mode
        {
            *error = Some(message);
        }
    }

    fn change_setting(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.call(Command::ChangeSetting {
            key: key.to_owned(),
            value: value.to_owned(),
        })
        .map(|_| ())
        .map_err(|failure| failure.message)
    }

    /// Opens the label dialog with the torrent's current labels.
    fn edit_label(&mut self, torrent: InfoHash, kind: LabelKind) {
        let Some(view) = self.view.torrent(&torrent) else {
            return;
        };
        let current = match kind {
            LabelKind::Category => view.category.clone().unwrap_or_default(),
            LabelKind::Tags => view.tags.join(", "),
        };
        let mut input = PathInput::default();
        input.paste(&current);
        self.mode = Mode::Label {
            torrent,
            kind,
            input,
            error: None,
        };
    }

    fn handle_label_key(&mut self, key: KeyEvent) {
        let Mode::Label {
            torrent,
            kind,
            input,
            error,
        } = &mut self.mode
        else {
            return;
        };
        let (torrent, kind) = (*torrent, *kind);
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let text = input.text().to_owned();
                let command = match kind {
                    LabelKind::Category => Command::SetCategory {
                        torrent,
                        category: Some(text.trim().to_owned()).filter(|name| !name.is_empty()),
                    },
                    LabelKind::Tags => Command::SetTags {
                        torrent,
                        tags: split_tags(&text),
                    },
                };
                match self.call(command) {
                    Ok(_) => self.mode = Mode::Normal,
                    Err(failure) => {
                        if let Mode::Label { error, .. } = &mut self.mode {
                            *error = Some(failure.message);
                        }
                    }
                }
            }
            KeyCode::Tab => {
                let known = match kind {
                    LabelKind::Category => &self.view.categories,
                    LabelKind::Tags => &self.view.tags,
                };
                if let Some(completed) = complete_label(input.text(), known, kind) {
                    input.clear();
                    input.paste(&completed);
                }
            }
            KeyCode::Char('u') if control => input.clear(),
            KeyCode::Char('w') if control => input.delete_word(),
            KeyCode::Char(character) if !control => {
                input.insert(character);
                *error = None;
            }
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            _ => {}
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        let choices = self.filter_choices();
        let Mode::Filter { selected } = &mut self.mode else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('f') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(choices.len() - 1);
            }
            KeyCode::Home | KeyCode::Char('g') => *selected = 0,
            KeyCode::End | KeyCode::Char('G') => *selected = choices.len() - 1,
            KeyCode::Enter | KeyCode::Char(' ') => {
                let filter = choices.get(*selected).cloned().unwrap_or_default();
                self.mode = Mode::Normal;
                self.set_filter(filter);
            }
            _ => {}
        }
    }

    fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.refresh();
    }

    fn handle_sort_key(&mut self, key: KeyEvent) {
        let Mode::Sort { selected } = &mut self.mode else {
            return;
        };
        let last = Sort::ALL.len() - 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('S') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => *selected = 0,
            KeyCode::End | KeyCode::Char('G') => *selected = last,
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.sort = Sort::ALL[*selected];
                self.mode = Mode::Normal;
                self.refresh();
            }
            _ => {}
        }
    }

    // ----- helpers ------------------------------------------------------------

    fn visible_position(&self, hash: &InfoHash) -> Option<usize> {
        self.visible
            .iter()
            .position(|index| self.view.torrents[*index].hash == *hash)
    }

    fn select_torrent(&mut self, hash: InfoHash) {
        match self.visible_position(&hash) {
            Some(index) => self.table.select(Some(index)),
            None => self.select = Some(hash),
        }
    }

    fn move_selection(&mut self, step: isize) {
        let count = self.len();
        if count == 0 {
            return;
        }
        let current = self.selected().unwrap_or(0) as isize;
        let next = current.saturating_add(step).clamp(0, count as isize - 1);
        self.table.select(Some(next as usize));
    }
}

/// Tags typed as a comma-separated list, without empty entries.
fn split_tags(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Completes the name being typed (for tags, the last in the list) to the
/// first known name it starts, ignoring case.
fn complete_label(text: &str, known: &[String], kind: LabelKind) -> Option<String> {
    let (before, typed) = match kind {
        LabelKind::Tags => match text.rfind(',') {
            Some(comma) => (&text[..=comma], &text[comma + 1..]),
            None => ("", text),
        },
        LabelKind::Category => ("", text),
    };
    let typed = typed.trim_start().to_lowercase();
    let found = known
        .iter()
        .find(|name| name.to_lowercase().starts_with(&typed) && name.to_lowercase() != typed)?;
    let separator = if before.is_empty() { "" } else { " " };
    Some(format!("{before}{separator}{found}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use empirebt_app::Store;
    use std::io::{Read, Write};

    /// A dashboard over a fresh data directory that adds torrents paused.
    fn dashboard(root: &std::path::Path) -> App {
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
        App::new(Handle::start(store, config, client_config, Vec::new()).unwrap())
    }

    /// Serves `body` once over HTTP, behind a redirect as torrent sites do.
    fn serve_torrent(body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let responses = [
                b"HTTP/1.1 302 Found\r\nLocation: /file.torrent\r\n\r\n".to_vec(),
                [b"HTTP/1.1 200 OK\r\n\r\n".as_slice(), &body].concat(),
            ];
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = stream.read(&mut [0; 1024]);
                stream.write_all(&response).unwrap();
            }
        });
        format!("http://{address}/download?id=1")
    }

    fn press(app: &mut App, keys: &str) {
        for character in keys.chars() {
            let code = match character {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                '\t' => KeyCode::Tab,
                character => KeyCode::Char(character),
            };
            app.handle_key(KeyEvent::from(code));
        }
    }

    fn add_magnet(app: &mut App, byte: u8) {
        let hex: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
        app.call(Command::Add(format!(
            "magnet:?xt=urn:btih:{hex}&dn=torrent-{byte}"
        )))
        .unwrap();
    }

    /// The dashboard as drawn on a terminal of this size.
    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tags_categories_and_filters_from_the_keyboard() {
        let root =
            std::env::temp_dir().join(format!("empirebt-console-labels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut app = dashboard(&root);
        add_magnet(&mut app, 1);
        add_magnet(&mut app, 2);
        app.table.select(Some(0));

        // Tags for the first torrent, a category typed with completion.
        press(&mut app, "tiso, favorite\n");
        assert!(matches!(app.mode, Mode::Normal));
        press(&mut app, "CLinux\n");
        app.table.select(Some(1));
        press(&mut app, "tfav\t\n");
        assert_eq!(app.row(1).unwrap().tags, ["favorite"], "completed");
        app.table.select(Some(0));
        assert_eq!(app.row(0).unwrap().tags, ["favorite", "iso"]);
        assert_eq!(app.row(0).unwrap().category.as_deref(), Some("Linux"));

        let drawn = screen(&mut app, 160, 30);
        assert!(
            drawn.contains("torrent-1  Linux  #favorite #iso"),
            "{drawn}"
        );
        // On a narrow terminal labels are cut before the name is.
        let drawn = screen(&mut app, 80, 24);
        assert!(drawn.contains("torrent-1"), "{drawn}");
        // The detail pane shows them in full, on small terminals too.
        let drawn = screen(&mut app, 160, 30);
        assert!(
            drawn.contains("Labels    Linux  ·  tags favorite, iso"),
            "{drawn}"
        );
        let drawn = screen(&mut app, 100, 24);
        assert!(drawn.contains("Labels    Linux"), "{drawn}");
        assert!(
            drawn.contains("torrent-1  Linux  #favorite"),
            "names keep room:\n{drawn}"
        );

        // Filter by tag: only the first torrent is shown.
        let choices = app.filter_choices();
        let iso = choices
            .iter()
            .position(|choice| *choice == Filter::Tag("iso".to_owned()))
            .unwrap();
        press(&mut app, "f");
        assert!(matches!(app.mode, Mode::Filter { .. }));
        let drawn = screen(&mut app, 120, 30);
        assert!(drawn.contains("Category: Linux"), "{drawn}");
        for _ in 0..iso {
            press(&mut app, "j");
        }
        press(&mut app, "\n");
        assert_eq!(app.filter, Filter::Tag("iso".to_owned()));
        assert_eq!((app.len(), app.total()), (1, 2));
        assert_eq!(app.row(0).unwrap().name, "torrent-1");
        let drawn = screen(&mut app, 120, 30);
        assert!(drawn.contains("Tag: iso 1 of 2"), "{drawn}");

        // A filter that matches nothing says so; Esc shows everything.
        app.filter = Filter::Untagged;
        app.tick();
        assert_eq!(app.len(), 0);
        let drawn = screen(&mut app, 120, 30);
        assert!(drawn.contains("No torrents match this filter"), "{drawn}");
        press(&mut app, "\x1b");
        assert_eq!((app.filter.clone(), app.len()), (Filter::All, 2));

        // Bad names are explained in the dialog. (Typing and pasting
        // already drop control characters.)
        press(&mut app, "C");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.handle_paste(&"x".repeat(empirebt_app::MAX_NAME + 1));
        press(&mut app, "\n");
        assert!(matches!(app.mode, Mode::Label { error: Some(_), .. }));
        let drawn = screen(&mut app, 120, 30);
        assert!(drawn.contains("Known: Linux"), "{drawn}");
        app.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn splits_and_completes_label_names() {
        assert_eq!(split_tags(" a, b ,,c "), ["a", "b", "c"]);
        assert!(split_tags(" , ").is_empty());
        let known = ["Favorite".to_owned(), "iso".to_owned()];
        assert_eq!(
            complete_label("iso, fa", &known, LabelKind::Tags).as_deref(),
            Some("iso, Favorite")
        );
        assert_eq!(
            complete_label("i", &known, LabelKind::Tags).as_deref(),
            Some("iso")
        );
        assert_eq!(complete_label("zz", &known, LabelKind::Tags), None);
        assert_eq!(
            complete_label("fav", &known, LabelKind::Category).as_deref(),
            Some("Favorite")
        );
    }

    /// `S` picks the order; the selection stays on its torrent.
    #[test]
    fn sorts_the_list_and_keeps_the_selection() {
        let root =
            std::env::temp_dir().join(format!("empirebt-console-sort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut app = dashboard(&root);
        for (byte, name) in [(1u8, "zebra"), (2, "apple"), (3, "mango")] {
            let hex: String = [byte; 20].iter().map(|b| format!("{b:02x}")).collect();
            app.call(Command::Add(format!("magnet:?xt=urn:btih:{hex}&dn={name}")))
                .unwrap();
        }
        app.tick();
        let names = |app: &App| app.rows().map(|row| row.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&app), ["zebra", "apple", "mango"]);
        app.table.select(Some(0)); // zebra

        app.handle_key(KeyEvent::from(KeyCode::Char('S')));
        assert!(matches!(app.mode, Mode::Sort { selected: 0 }));
        app.handle_key(KeyEvent::from(KeyCode::Down)); // Name
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.sort, Sort::Name);
        assert_eq!(names(&app), ["apple", "mango", "zebra"]);
        assert_eq!(app.row(app.selected().unwrap()).unwrap().name, "zebra");
        app.engine.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    /// The add dialog's second field picks the folder; ↑↓ move between
    /// the fields, and typing goes to the one with the cursor.
    #[test]
    fn adds_a_torrent_to_the_folder_typed_in_the_dialog() {
        let root =
            std::env::temp_dir().join(format!("empirebt-console-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut torrent =
            b"d4:infod6:lengthi5e4:name9:hello.txt12:piece lengthi16384e6:pieces20:".to_vec();
        torrent.extend([7; 20]);
        torrent.extend(b"ee");
        let file = root.join("hello.torrent");
        std::fs::write(&file, torrent).unwrap();

        let mut app = dashboard(&root);
        app.handle_key(KeyEvent::from(KeyCode::Char('a')));
        app.handle_paste(&file.to_string_lossy());
        app.handle_key(KeyEvent::from(KeyCode::Down));
        let Mode::Add { save, .. } = &app.mode else {
            panic!("the add dialog is not open");
        };
        assert_eq!(save.text(), root.join("downloads").to_string_lossy());
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let folder = root.join("elsewhere");
        app.handle_paste(&folder.to_string_lossy());
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Normal));
        let torrent = &app.engine.snapshot().torrents[0];
        assert_eq!(torrent.save_dir, folder);
        app.engine.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn adds_a_torrent_from_a_url_in_the_background() {
        let root =
            std::env::temp_dir().join(format!("empirebt-console-url-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut torrent =
            b"d4:infod6:lengthi5e4:name9:hello.txt12:piece lengthi16384e6:pieces20:".to_vec();
        torrent.extend([7; 20]);
        torrent.extend(b"ee");
        let url = serve_torrent(torrent);

        let mut app = dashboard(&root);
        let mut input = PathInput::default();
        input.paste(&url);
        app.mode = app.add_mode(input);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        // The dialog closes at once; the download finishes in the background.
        assert!(matches!(app.mode, Mode::Normal));
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.len() == 0 && Instant::now() < deadline {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.len(), 1);
        assert_eq!(app.row(0).unwrap().name, "hello.txt");
        assert_eq!(app.selected(), Some(0));
        app.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }
}
