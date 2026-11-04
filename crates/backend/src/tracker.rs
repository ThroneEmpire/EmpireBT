/// How long to wait before announcing again to a tracker that failed
/// `failures` times in a row, as libtorrent's `announce_infohash::failed`
/// with its default `tracker_backoff` of 250: 5 s plus 12.5 s times the
/// failures squared (about 17 s, 55 s, 2 min, 3.5 min, 5 min...), at most
/// an hour.
pub(crate) fn retry_delay(failures: u32) -> std::time::Duration {
    const MIN: u64 = 5;
    const MAX: u64 = 60 * 60;
    let failures = u64::from(failures.min(127));
    let seconds = MIN + failures * failures * MIN * 250 / 100;
    std::time::Duration::from_secs(seconds.min(MAX))
}

/// Announce interval when a tracker gives none.
pub(crate) const DEFAULT_ANNOUNCE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);
/// No tracker is announced to more often than this, whatever it asks:
/// libtorrent's `min_announce_interval`. Trackers that answer `interval
/// 0` or 60 would otherwise be asked every minute by every torrent.
pub(crate) const MIN_ANNOUNCE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

/// A tracker endpoint advertised by torrent metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    url: String,
    /// The network interface announces leave from.
    binding: crate::Binding,
}

fn parse_udp_url(url: &str) -> Result<(String, u16), crate::BackendError> {
    let rest = url
        .strip_prefix("udp://")
        .ok_or_else(|| tracker_error("tracker URL is not UDP"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| tracker_error("UDP tracker URL must include a port"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| tracker_error("tracker port is invalid"))?;
    if host.is_empty() {
        return Err(tracker_error("tracker URL has no hostname"));
    }
    Ok((host.to_owned(), port))
}

/// Attempts per UDP request; UDP datagrams are routinely lost, so a single
/// unanswered packet must not fail the whole announce (BEP 15).
const UDP_ATTEMPTS: u32 = 3;

/// How long a UDP tracker's connection ID is reused: BEP 15's minute, as
/// libtorrent's `udp_tracker_token_expiry`, so each announce is one round
/// trip, not two.
const CONNECTION_ID_LIFETIME: Duration = Duration::from_secs(60);

/// Connection IDs by tracker address, and when each was given.
type ConnectionIds = std::sync::Mutex<HashMap<SocketAddr, ([u8; 8], std::time::Instant)>>;

fn connection_ids() -> &'static ConnectionIds {
    static IDS: std::sync::OnceLock<ConnectionIds> = std::sync::OnceLock::new();
    IDS.get_or_init(Default::default)
}

/// A fresh connection ID from the tracker at `address` (BEP 15 `connect`).
fn connect_udp(socket: &UdpSocket, address: SocketAddr) -> Result<[u8; 8], Failure> {
    let transaction = transaction_id();
    let mut connect = Vec::with_capacity(16);
    connect.extend(0x41727101980_u64.to_be_bytes());
    connect.extend(0_u32.to_be_bytes());
    connect.extend(transaction.to_be_bytes());
    let response = udp_exchange(socket, &connect, 0, transaction, 16)?;
    let id: [u8; 8] = response[8..16].try_into().expect("16 bytes checked");
    if let Ok(mut ids) = connection_ids().lock() {
        // Expired ones go once there are many, so the map stays small.
        if ids.len() >= 1024 {
            ids.retain(|_, (_, at)| at.elapsed() < CONNECTION_ID_LIFETIME);
        }
        ids.insert(address, (id, std::time::Instant::now()));
    }
    Ok(id)
}

/// Runs `exchange` with a connection ID for `address`: a cached one while
/// it is fresh, else a new one. A cached one that fails is dropped and the
/// exchange tried once more with a new one, as the tracker may have
/// forgotten it sooner.
fn with_connection_id<T>(
    socket: &UdpSocket,
    address: SocketAddr,
    exchange: impl Fn([u8; 8]) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let cached = connection_ids().lock().ok().and_then(|ids| {
        ids.get(&address)
            .filter(|(_, at)| at.elapsed() < CONNECTION_ID_LIFETIME)
            .map(|(id, _)| *id)
    });
    if let Some(id) = cached {
        match exchange(id) {
            Ok(result) => return Ok(result),
            Err(_) => {
                if let Ok(mut ids) = connection_ids().lock() {
                    ids.remove(&address);
                }
            }
        }
    }
    exchange(connect_udp(socket, address)?)
}

/// What follows the host and port in a UDP tracker URL (`/announce`, a
/// passkey path, a query), sent with each announce as BEP 41's URLData
/// option, as libtorrent sends it: private UDP trackers tell users apart by
/// it. At most 255 bytes fit.
fn udp_request_string(url: &str) -> &[u8] {
    let rest = url.strip_prefix("udp://").unwrap_or(url);
    let path = rest.find(['/', '?']).map_or("", |at| &rest[at..]);
    let bytes = path.as_bytes();
    &bytes[..bytes.len().min(255)]
}

fn announce_udp_tracker(
    tracker: &Tracker,
    request: &AnnounceRequest,
    timeout: Duration,
) -> Result<TrackerResponse, Failure> {
    let (host, port) = parse_udp_url(tracker.url())?;
    let addresses = family_addresses(tracker, &format!("{host}:{port}"))?;
    let (tracker, request) = (tracker.clone(), request.clone());
    over_each_family(
        addresses,
        Arc::new(move |address| announce_udp_at(&tracker, address, &request, timeout)),
    )
}

/// One address per family (IPv4 first) that `host_port` resolves to and
/// the tracker's network interface can reach.
fn family_addresses(tracker: &Tracker, host_port: &str) -> Result<Vec<SocketAddr>, Failure> {
    let resolved: Vec<SocketAddr> = host_port
        .to_socket_addrs()
        .map_err(|error| unreachable(error.to_string()))?
        .collect();
    let pick = |v6: bool| {
        resolved
            .iter()
            .find(|address| address.is_ipv6() == v6 && tracker.binding.reaches(address))
            .copied()
    };
    let addresses: Vec<SocketAddr> = [pick(false), pick(true)].into_iter().flatten().collect();
    if addresses.is_empty() {
        return Err(unreachable(
            "tracker hostname resolved to no usable addresses",
        ));
    }
    Ok(addresses)
}

/// How long announces over the other address family get once one has
/// succeeded, so a network with broken IPv6 does not hold up every
/// announce until it times out.
const FAMILY_GRACE: Duration = Duration::from_secs(2);

type FamilyAnnounce = Arc<dyn Fn(SocketAddr) -> Result<TrackerResponse, Failure> + Send + Sync>;

/// Announces to each of `addresses` (a tracker's IPv4 and IPv6 ones) at
/// once and merges what they return: this is how a tracker learns both our
/// addresses, and we its peers of both families.
fn over_each_family(
    addresses: Vec<SocketAddr>,
    announce: FamilyAnnounce,
) -> Result<TrackerResponse, Failure> {
    if let [address] = addresses[..] {
        return announce(address);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    for address in &addresses {
        let (announce, sender, address) = (Arc::clone(&announce), sender.clone(), *address);
        std::thread::spawn(move || {
            let _ = sender.send(announce(address));
        });
    }
    drop(sender);
    let mut results = Vec::new();
    let mut deadline: Option<std::time::Instant> = None;
    while results.len() < addresses.len() {
        let received = match deadline {
            Some(deadline) => receiver
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .ok(),
            None => receiver.recv().ok(),
        };
        let Some(result) = received else { break };
        if result.is_ok() && deadline.is_none() {
            deadline = Some(std::time::Instant::now() + FAMILY_GRACE);
        }
        results.push(result);
    }
    merge_families(results)
}

/// One response from the families' announces: the peers of all that
/// succeeded. When all failed, a tracker's answer explains more than
/// silence.
fn merge_families(
    results: Vec<Result<TrackerResponse, Failure>>,
) -> Result<TrackerResponse, Failure> {
    let mut merged: Option<TrackerResponse> = None;
    let mut failures = Vec::new();
    for result in results {
        match (result, &mut merged) {
            (Ok(response), None) => merged = Some(response),
            (Ok(response), Some(merged)) => {
                for peer in response.peers {
                    if !merged.peers.contains(&peer) {
                        merged.peers.push(peer);
                    }
                }
                // Both describe the same swarm.
                merged.seeders = merged.seeders.max(response.seeders);
                merged.leechers = merged.leechers.max(response.leechers);
            }
            (Err(failure), _) => failures.push(failure),
        }
    }
    if let Some(merged) = merged {
        return Ok(merged);
    }
    failures.sort_by_key(|failure| failure.unreachable);
    Err(failures
        .into_iter()
        .next()
        .unwrap_or_else(|| unreachable("tracker could not be reached")))
}

fn announce_udp_at(
    tracker: &Tracker,
    address: SocketAddr,
    request: &AnnounceRequest,
    timeout: Duration,
) -> Result<TrackerResponse, Failure> {
    let socket = tracker
        .binding
        .udp_for(address)
        .map_err(|error| unreachable(error.to_string()))?;
    socket
        .connect(address)
        .map_err(|error| unreachable(error.to_string()))?;
    let attempt_timeout = (timeout / UDP_ATTEMPTS).max(Duration::from_secs(1));
    socket
        .set_read_timeout(Some(attempt_timeout))
        .map_err(|error| tracker_error(error.to_string()))?;

    let request_string = udp_request_string(tracker.url());
    let response = with_connection_id(&socket, address, |connection_id| {
        let transaction = transaction_id();
        let mut announce = Vec::with_capacity(100 + request_string.len());
        announce.extend(connection_id);
        announce.extend(1_u32.to_be_bytes());
        announce.extend(transaction.to_be_bytes());
        announce.extend(request.info_hash);
        announce.extend(request.peer_id);
        announce.extend(request.downloaded.to_be_bytes());
        announce.extend(request.left.to_be_bytes());
        announce.extend(request.uploaded.to_be_bytes());
        announce.extend(request.event.udp_code().to_be_bytes());
        // Our address (0: the one the packet came from), key, peers wanted.
        announce.extend(0_u32.to_be_bytes());
        announce.extend(request.key().to_be_bytes());
        announce.extend(request.num_want().to_be_bytes());
        announce.extend(request.port.to_be_bytes());
        if !request_string.is_empty() {
            // BEP 41 option 2, URLData.
            announce.push(2);
            announce.push(request_string.len() as u8);
            announce.extend(request_string);
        }
        udp_exchange(&socket, &announce, 1, transaction, 20)
    })?;
    let read = |at: usize| u64::from(u32::from_be_bytes(response[at..at + 4].try_into().unwrap()));
    let (interval, leechers, seeders) = (read(8), read(12), read(16));
    // Peers are returned in the address family the tracker was reached on.
    let mut decoded = if address.is_ipv6() {
        TrackerResponse::from_compact_ipv6(&response[20..], Some(interval), None)?
    } else {
        TrackerResponse::from_compact_peers(&response[20..], Some(interval), None)?
    };
    decoded.seeders = Some(seeders);
    decoded.leechers = Some(leechers);
    Ok(decoded)
}

/// Why a tracker announce failed: the tracker could not be reached at all,
/// or it answered with something unusable. Only the first says anything
/// about the tracker's health for other torrents.
#[derive(Debug)]
struct Failure {
    error: crate::BackendError,
    unreachable: bool,
}

impl From<crate::BackendError> for Failure {
    fn from(error: crate::BackendError) -> Self {
        Self {
            error,
            unreachable: false,
        }
    }
}

fn unreachable(message: impl Into<String>) -> Failure {
    Failure {
        error: tracker_error(message),
        unreachable: true,
    }
}

/// Network-level health of tracker hosts, shared by every torrent. Many
/// torrents list the same trackers, so a dead one is skipped everywhere for
/// a while instead of costing each torrent a full timeout.
mod health {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const FIRST_BACKOFF: Duration = Duration::from_secs(60);
    const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

    struct Health {
        failures: u32,
        retry_at: Instant,
    }

    fn hosts() -> &'static Mutex<HashMap<String, Health>> {
        static HOSTS: OnceLock<Mutex<HashMap<String, Health>>> = OnceLock::new();
        HOSTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// `scheme://host:port`, so paths and passkeys share one entry. It is
    /// also what logs name: never a URL's path or query, which carry
    /// private trackers' passkeys, nor credentials before an `@`.
    fn host_of(url: &str) -> String {
        let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
        let authority = rest.split(['/', '?']).next().unwrap_or(rest);
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        format!("{scheme}://{authority}")
    }

    /// How long until an unreachable tracker may be tried again.
    pub(super) fn retry_in(url: &str) -> Option<Duration> {
        let hosts = hosts().lock().ok()?;
        let health = hosts.get(&host_of(url))?;
        let now = Instant::now();
        (health.retry_at > now).then(|| health.retry_at - now)
    }

    pub(super) fn record(url: &str, reachable: bool) {
        let Ok(mut hosts) = hosts().lock() else {
            return;
        };
        let key = host_of(url);
        if reachable {
            if hosts.remove(&key).is_some() {
                tracing::info!(target: "tracker", "{key} is reachable again");
            }
            return;
        }
        let health = hosts.entry(key.clone()).or_insert(Health {
            failures: 0,
            retry_at: Instant::now(),
        });
        let backoff = FIRST_BACKOFF
            .saturating_mul(2_u32.saturating_pow(health.failures))
            .min(MAX_BACKOFF);
        // Said once when it goes down; every torrent then skips it.
        if health.failures == 0 {
            tracing::warn!(
                target: "tracker",
                "{key} is unreachable; every torrent skips it for {}s, then longer while it stays down",
                backoff.as_secs()
            );
        } else {
            tracing::debug!(target: "tracker", "{key} is still unreachable; next try in {}s", backoff.as_secs());
        }
        health.failures = health.failures.saturating_add(1);
        health.retry_at = Instant::now() + backoff;
    }
}

/// Sends a UDP tracker request, retrying on timeouts, and returns a
/// response matching the expected action and transaction.
fn udp_exchange(
    socket: &UdpSocket,
    packet: &[u8],
    action: u32,
    transaction: u32,
    minimum_length: usize,
) -> Result<Vec<u8>, Failure> {
    let mut buffer = [0; 4096];
    let mut last_error = None;
    for _ in 0..UDP_ATTEMPTS {
        socket
            .send(packet)
            .map_err(|error| unreachable(error.to_string()))?;
        let length = match socket.recv(&mut buffer) {
            Ok(length) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                last_error = Some(unreachable("UDP tracker did not respond"));
                continue;
            }
            Err(error) => return Err(unreachable(error.to_string())),
        };
        let response = &buffer[..length];
        if length < 8 || u32::from_be_bytes(response[4..8].try_into().unwrap()) != transaction {
            last_error = Some(tracker_error("UDP tracker sent an unrelated response").into());
            continue;
        }
        let received_action = u32::from_be_bytes(response[..4].try_into().unwrap());
        if received_action == 3 {
            return Err(tracker_error(format!(
                "tracker refused the announce: {}",
                String::from_utf8_lossy(&response[8..])
            ))
            .into());
        }
        if received_action != action || length < minimum_length {
            return Err(tracker_error("invalid UDP tracker response").into());
        }
        return Ok(response.to_vec());
    }
    Err(last_error.unwrap_or_else(|| unreachable("UDP tracker did not respond")))
}

/// A random transaction ID, as libtorrent's: one read off the clock could
/// be guessed by someone forging tracker replies (made-up peers, a huge
/// interval).
fn transaction_id() -> u32 {
    u32::from_be_bytes(crate::dht::random_bytes())
}

fn invalid(message: impl Into<String>) -> crate::BackendError {
    crate::BackendError::InvalidMetainfo(message.into())
}

#[derive(Debug)]
enum Value<'a> {
    Bytes(&'a [u8]),
    Integer(u64),
    List(Vec<Value<'a>>),
    Dictionary(std::collections::HashMap<&'a [u8], Value<'a>>),
}

impl<'a> Value<'a> {
    fn as_dictionary(
        &self,
    ) -> Result<&std::collections::HashMap<&'a [u8], Value<'a>>, crate::BackendError> {
        match self {
            Self::Dictionary(value) => Ok(value),
            _ => Err(invalid("tracker response root must be a dictionary")),
        }
    }

    fn as_bytes(&self) -> Result<&[u8], crate::BackendError> {
        match self {
            Self::Bytes(value) => Ok(value),
            _ => Err(invalid("tracker response field must be bytes")),
        }
    }

    fn as_text(&self) -> Result<String, crate::BackendError> {
        String::from_utf8(self.as_bytes()?.to_vec())
            .map_err(|_| invalid("tracker response text is not UTF-8"))
    }

    fn as_integer(&self) -> Result<u64, crate::BackendError> {
        match self {
            Self::Integer(value) => Ok(*value),
            _ => Err(invalid("tracker response interval must be an integer")),
        }
    }
}

struct BencodeParser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> BencodeParser<'a> {
    fn parse(&mut self) -> Result<Value<'a>, crate::BackendError> {
        match self.bytes.get(self.position) {
            Some(b'i') => {
                self.position += 1;
                let end = self.find(b'e')?;
                let number = std::str::from_utf8(&self.bytes[self.position..end])
                    .map_err(|_| invalid("tracker integer is invalid"))?
                    .parse()
                    .map_err(|_| invalid("tracker integer is invalid"))?;
                self.position = end + 1;
                Ok(Value::Integer(number))
            }
            Some(b'd') => {
                self.position += 1;
                let mut dictionary = std::collections::HashMap::new();
                while self.bytes.get(self.position) != Some(&b'e') {
                    let key = self.parse_bytes()?;
                    dictionary.insert(key, self.parse()?);
                }
                self.position += 1;
                Ok(Value::Dictionary(dictionary))
            }
            Some(b'l') => {
                self.position += 1;
                let mut values = Vec::new();
                while self.bytes.get(self.position) != Some(&b'e') {
                    values.push(self.parse()?);
                }
                self.position += 1;
                Ok(Value::List(values))
            }
            Some(b'0'..=b'9') => Ok(Value::Bytes(self.parse_bytes()?)),
            _ => Err(invalid("invalid tracker bencode")),
        }
    }

    fn parse_bytes(&mut self) -> Result<&'a [u8], crate::BackendError> {
        let separator = self
            .bytes
            .get(self.position..)
            .and_then(|remaining| remaining.iter().position(|byte| *byte == b':'))
            .ok_or_else(|| invalid("tracker byte string is missing its separator"))?;
        let end = self.position + separator;
        let length = std::str::from_utf8(&self.bytes[self.position..end])
            .map_err(|_| invalid("tracker byte string length is invalid"))?
            .parse::<usize>()
            .map_err(|_| invalid("tracker byte string length is invalid"))?;
        let start = end + 1;
        let finish = start
            .checked_add(length)
            .ok_or_else(|| invalid("tracker byte string is too large"))?;
        let value = self
            .bytes
            .get(start..finish)
            .ok_or_else(|| invalid("tracker byte string is truncated"))?;
        self.position = finish;
        Ok(value)
    }

    fn find(&self, byte: u8) -> Result<usize, crate::BackendError> {
        self.bytes
            .get(self.position..)
            .and_then(|remaining| remaining.iter().position(|candidate| *candidate == byte))
            .map(|offset| self.position + offset)
            .ok_or_else(|| invalid("tracker integer is unterminated"))
    }
}

impl Tracker {
    pub fn new(url: impl Into<String>) -> Result<Self, crate::BackendError> {
        let url = url.into();
        if url.trim().is_empty()
            || !(url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with("udp://"))
        {
            return Err(crate::BackendError::InvalidConfiguration(
                "tracker URL must use http, https, or udp".to_owned(),
            ));
        }
        Ok(Self {
            url,
            binding: crate::Binding::any(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Announces to a torrent's trackers, which are grouped in tiers (BEP 12).
#[derive(Debug, Default, Clone)]
pub struct TrackerClient {
    trackers: Vec<Tracker>,
    /// How many of `trackers`, in order, each tier holds.
    tiers: Vec<usize>,
}

impl TrackerClient {
    /// Each tracker in a tier of its own.
    pub fn new(trackers: Vec<Tracker>) -> Self {
        let tiers = vec![1; trackers.len()];
        Self { trackers, tiers }
    }

    pub fn trackers(&self) -> &[Tracker] {
        &self.trackers
    }

    /// Each URL in a tier of its own, such as a magnet link's trackers.
    pub fn from_urls(urls: impl IntoIterator<Item = String>) -> Result<Self, crate::BackendError> {
        Self::from_tiers(urls.into_iter().map(|url| vec![url]))
    }

    /// Trackers in tiers, as a torrent's `announce-list` has them. A
    /// tracker listed twice stays in its first tier.
    pub fn from_tiers(
        tiers: impl IntoIterator<Item = Vec<String>>,
    ) -> Result<Self, crate::BackendError> {
        let mut client = Self::default();
        for tier in tiers {
            let mut size = 0;
            for url in tier {
                let tracker = Tracker::new(url)?;
                if !client.trackers.contains(&tracker) {
                    client.trackers.push(tracker);
                    size += 1;
                }
            }
            if size > 0 {
                client.tiers.push(size);
            }
        }
        Ok(client)
    }

    /// Each tier on its own.
    pub fn tiers(&self) -> Vec<TrackerClient> {
        let mut rest = self.trackers.as_slice();
        self.tiers
            .iter()
            .map(|size| {
                let (tier, after) = rest.split_at(*size);
                rest = after;
                Self {
                    trackers: tier.to_vec(),
                    tiers: vec![*size],
                }
            })
            .collect()
    }

    /// Announces only through this network interface.
    pub fn with_binding(mut self, binding: &crate::Binding) -> Self {
        for tracker in &mut self.trackers {
            tracker.binding = binding.clone();
        }
        self
    }

    /// Announces to configured HTTP(S) trackers in order until one succeeds.
    pub fn announce_http(
        &self,
        request: &AnnounceRequest,
        timeout: Duration,
    ) -> Result<TrackerResponse, crate::BackendError> {
        let mut last_error = None;
        let mut found_http = false;
        for tracker in &self.trackers {
            if !is_http_url(tracker.url()) {
                continue;
            }
            found_http = true;
            match announce_tracker(tracker, request, timeout) {
                Ok(response) => return Ok(response),
                Err(error) => last_error = Some(error),
            }
        }
        if !found_http {
            return Err(tracker_error("no HTTP tracker is configured"));
        }
        Err(last_error.unwrap_or_else(|| tracker_error("all HTTP trackers failed")))
    }

    /// Announces to configured HTTP(S) or UDP trackers in order until one
    /// succeeds, starting with the one that last answered (BEP 12: a
    /// tracker that works moves to the front of its tier), so a dead first
    /// tracker does not cost every announce a timeout.
    pub fn announce(
        &self,
        request: &AnnounceRequest,
        timeout: Duration,
    ) -> Result<TrackerResponse, crate::BackendError> {
        let mut last_error = None;
        let preferred = tier_order::preferred(&self.trackers);
        let order = preferred.iter().chain(
            self.trackers
                .iter()
                .filter(|tracker| preferred.first() != Some(*tracker)),
        );
        for tracker in order {
            match announce_tracker(tracker, request, timeout) {
                Ok(response) => {
                    tier_order::worked(&self.trackers, tracker);
                    return Ok(response);
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| tracker_error("no supported tracker is configured")))
    }

    /// Announces to every tier concurrently, and within each tier to its
    /// trackers in order until one accepts (BEP 12, as qBittorrent does by
    /// default); returns each tier's result in order. Used where every
    /// tracker should learn about us, such as seeding.
    pub fn announce_all(
        &self,
        request: &AnnounceRequest,
        timeout: Duration,
    ) -> Vec<Result<TrackerResponse, crate::BackendError>> {
        let tiers = self.tiers();
        std::thread::scope(|scope| {
            let workers: Vec<_> = tiers
                .iter()
                .map(|tier| scope.spawn(move || tier.announce(request, timeout)))
                .collect();
            workers
                .into_iter()
                .map(|worker| {
                    worker
                        .join()
                        .unwrap_or_else(|_| Err(tracker_error("tracker announce panicked")))
                })
                .collect()
        })
    }
}

fn announce_tracker(
    tracker: &Tracker,
    request: &AnnounceRequest,
    timeout: Duration,
) -> Result<TrackerResponse, crate::BackendError> {
    // Our own interface being down says nothing about the tracker.
    tracker.binding.check()?;
    if let Some(wait) = health::retry_in(tracker.url()) {
        return Err(tracker_error(format!(
            "tracker was unreachable; retrying in {}s",
            wait.as_secs().max(1)
        )));
    }
    let result = if is_http_url(tracker.url()) {
        announce_http_tracker(tracker, request, timeout)
    } else if tracker.url().starts_with("udp://") {
        announce_udp_tracker(tracker, request, timeout)
    } else {
        Err(tracker_error("tracker URL scheme is not supported").into())
    };
    match result {
        Ok(response) => {
            health::record(tracker.url(), true);
            Ok(response)
        }
        Err(failure) => {
            if failure.unreachable {
                health::record(tracker.url(), false);
            } else {
                // It answered, so it is up, even if this torrent failed.
                health::record(tracker.url(), true);
            }
            Err(failure.error)
        }
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Largest tracker response accepted, so a misbehaving tracker cannot
/// exhaust memory.
const MAX_HTTP_RESPONSE: u64 = 4 * 1024 * 1024;

fn announce_http_tracker(
    tracker: &Tracker,
    request: &AnnounceRequest,
    timeout: Duration,
) -> Result<TrackerResponse, Failure> {
    let url = crate::http::Url::parse(tracker.url(), "announce")
        .map_err(|message| tracker_error(format!("tracker {message}")))?;
    // Private trackers embed credentials in the announce URL's own query.
    let separator = if url.path.contains('?') { '&' } else { '?' };
    let mut target = format!("{}{separator}{}", url.path, request.query());
    // What the tracker asked to be sent back (`tracker id`).
    if let Some(id) = tracker_ids::get(tracker.url(), &request.info_hash) {
        target.push_str("&trackerid=");
        target.push_str(&percent_encode(&id));
    }
    let addresses = family_addresses(tracker, &format!("{}:{}", url.host, url.port))?;
    let binding = tracker.binding.clone();
    let user_agent = crate::identity::user_agent_for(&request.peer_id);
    let response = over_each_family(
        addresses,
        Arc::new(move |address| {
            http_announce_at(address, &url, &target, user_agent, timeout, &binding)
        }),
    )?;
    if request.event == AnnounceEvent::Stopped {
        tracker_ids::forget(tracker.url(), &request.info_hash);
    } else if let Some(id) = &response.tracker_id {
        tracker_ids::set(tracker.url(), &request.info_hash, id.clone());
    }
    Ok(response)
}

/// The tracker that last answered in each tier of several trackers, tried
/// first next time (BEP 12), shared by every torrent with that tier.
mod tier_order {
    use super::Tracker;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn worked_last() -> &'static Mutex<HashMap<Vec<String>, String>> {
        static ORDER: OnceLock<Mutex<HashMap<Vec<String>, String>>> = OnceLock::new();
        ORDER.get_or_init(Default::default)
    }

    fn key(tier: &[Tracker]) -> Vec<String> {
        tier.iter()
            .map(|tracker| tracker.url().to_owned())
            .collect()
    }

    /// The tracker of `tier` that answered last, if it is not the first
    /// anyway.
    pub(super) fn preferred(tier: &[Tracker]) -> Vec<Tracker> {
        if tier.len() < 2 {
            return Vec::new();
        }
        let Ok(order) = worked_last().lock() else {
            return Vec::new();
        };
        order
            .get(&key(tier))
            .and_then(|url| tier.iter().find(|tracker| tracker.url() == url))
            .cloned()
            .into_iter()
            .collect()
    }

    pub(super) fn worked(tier: &[Tracker], tracker: &Tracker) {
        if tier.len() < 2 {
            return;
        }
        if let Ok(mut order) = worked_last().lock() {
            if order.len() >= 10_000 {
                order.clear();
            }
            order.insert(key(tier), tracker.url().to_owned());
        }
    }
}

/// The `tracker id` each tracker gave for each torrent, sent back with
/// later announces as BEP 3 asks and libtorrent does.
mod tracker_ids {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    type Ids = HashMap<(String, [u8; 20]), Vec<u8>>;

    fn ids() -> &'static Mutex<Ids> {
        static IDS: OnceLock<Mutex<Ids>> = OnceLock::new();
        IDS.get_or_init(Default::default)
    }

    pub(super) fn get(url: &str, info_hash: &[u8; 20]) -> Option<Vec<u8>> {
        ids()
            .lock()
            .ok()?
            .get(&(url.to_owned(), *info_hash))
            .cloned()
    }

    pub(super) fn set(url: &str, info_hash: &[u8; 20], id: Vec<u8>) {
        if let Ok(mut ids) = ids().lock() {
            // Bounded: torrents that left without `stopped` cannot pile up.
            if ids.len() >= 20_000 {
                ids.clear();
            }
            ids.insert((url.to_owned(), *info_hash), id);
        }
    }

    pub(super) fn forget(url: &str, info_hash: &[u8; 20]) {
        if let Ok(mut ids) = ids().lock() {
            ids.remove(&(url.to_owned(), *info_hash));
        }
    }
}

fn http_announce_at(
    address: SocketAddr,
    url: &crate::http::Url,
    target: &str,
    user_agent: &str,
    timeout: Duration,
    binding: &crate::Binding,
) -> Result<TrackerResponse, Failure> {
    let fetch = |address: SocketAddr, url: &crate::http::Url, target: &str| {
        crate::http::request_at(
            address,
            url,
            target,
            user_agent,
            timeout,
            MAX_HTTP_RESPONSE,
            binding,
        )
        .map_err(|error| match error {
            crate::http::Error::Unreachable(message) => unreachable(message),
            crate::http::Error::Invalid(message) => Failure::from(tracker_error(message)),
        })
    };
    let mut response = fetch(address, url, target)?;
    // A tracker that moved (to HTTPS, say) is followed, up to five times,
    // as libtorrent follows it.
    let mut redirects = 0;
    while matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        redirects += 1;
        if redirects > crate::http::MAX_REDIRECTS {
            return Err(tracker_error("tracker redirected too many times").into());
        }
        let location = response
            .header("location")
            .ok_or_else(|| tracker_error("tracker redirected without a Location"))?;
        let next = url
            .join(location.trim())
            .map_err(|message| tracker_error(format!("tracker redirect: {message}")))?;
        let address = format!("{}:{}", next.host, next.port)
            .to_socket_addrs()
            .map_err(|error| unreachable(error.to_string()))?
            .find(|candidate| binding.reaches(candidate))
            .ok_or_else(|| unreachable("tracker redirect resolved to no usable addresses"))?;
        response = fetch(address, &next, &next.path)?;
    }
    if response.status != 200 {
        return Err(tracker_error(format!(
            "tracker returned HTTP status {}",
            response.status_text
        ))
        .into());
    }
    Ok(TrackerResponse::from_bencode(&response.body)?)
}

/// A torrent's swarm as a tracker counts it, from a scrape (BEP 48, and
/// BEP 15 over UDP).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrapeStats {
    /// Peers with the whole torrent (`complete`).
    pub seeders: u64,
    /// Peers still downloading (`incomplete`).
    pub leechers: u64,
    /// Peers that ever finished it (`downloaded`).
    pub downloaded: u64,
}

/// Torrents asked about in one scrape; trackers limit how many they
/// answer, and the request URL grows with each.
pub const MAX_SCRAPE_HASHES: usize = 32;

/// The scrape URL BEP 48 derives from an announce URL: `announce` at the
/// start of the last path segment becomes `scrape`, the rest (a `.php`, a
/// passkey query) kept. `None` when the tracker cannot be scraped.
pub fn scrape_url(announce: &str) -> Option<String> {
    let (before_query, query) = match announce.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (announce, None),
    };
    let slash = before_query.rfind('/')?;
    // Not the `//` after the scheme.
    if before_query[..slash].ends_with(':') || before_query[..slash].ends_with('/') {
        return None;
    }
    let last = &before_query[slash + 1..];
    let rest = last.strip_prefix("announce")?;
    let mut url = format!("{}/scrape{rest}", &before_query[..slash]);
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    Some(url)
}

impl Tracker {
    /// Whether this tracker can be scraped: any UDP tracker, an HTTP one
    /// whose announce URL has a scrape URL.
    pub fn supports_scrape(&self) -> bool {
        self.url.starts_with("udp://")
            || (is_http_url(&self.url) && scrape_url(&self.url).is_some())
    }

    /// Asks the tracker for the swarm counts of up to
    /// [`MAX_SCRAPE_HASHES`] torrents at once, saying it is `user_agent`
    /// over HTTP; returns the counts of those it knows.
    pub fn scrape(
        &self,
        info_hashes: &[[u8; 20]],
        user_agent: &str,
        timeout: Duration,
    ) -> Result<HashMap<[u8; 20], ScrapeStats>, crate::BackendError> {
        let info_hashes = &info_hashes[..info_hashes.len().min(MAX_SCRAPE_HASHES)];
        self.binding.check()?;
        if let Some(wait) = health::retry_in(self.url()) {
            return Err(tracker_error(format!(
                "tracker was unreachable; retrying in {}s",
                wait.as_secs().max(1)
            )));
        }
        let result = if self.url.starts_with("udp://") {
            scrape_udp(self, info_hashes, timeout)
        } else {
            scrape_http(self, info_hashes, user_agent, timeout)
        };
        match result {
            Ok(stats) => {
                health::record(self.url(), true);
                Ok(stats)
            }
            Err(failure) => {
                health::record(self.url(), !failure.unreachable);
                Err(failure.error)
            }
        }
    }
}

fn scrape_http(
    tracker: &Tracker,
    info_hashes: &[[u8; 20]],
    user_agent: &str,
    timeout: Duration,
) -> Result<HashMap<[u8; 20], ScrapeStats>, Failure> {
    let scrape =
        scrape_url(tracker.url()).ok_or_else(|| tracker_error("this tracker cannot be scraped"))?;
    let url = crate::http::Url::parse(&scrape, "scrape")
        .map_err(|message| tracker_error(format!("tracker {message}")))?;
    let separator = if url.path.contains('?') { '&' } else { '?' };
    let hashes: Vec<String> = info_hashes
        .iter()
        .map(|hash| format!("info_hash={}", percent_encode(hash)))
        .collect();
    let target = format!("{}{separator}{}", url.path, hashes.join("&"));
    let mut last = None;
    for address in family_addresses(tracker, &format!("{}:{}", url.host, url.port))? {
        let response = match crate::http::request_at(
            address,
            &url,
            &target,
            user_agent,
            timeout,
            MAX_HTTP_RESPONSE,
            &tracker.binding,
        ) {
            Ok(response) => response,
            Err(crate::http::Error::Unreachable(message)) => {
                last = Some(unreachable(message));
                continue;
            }
            Err(crate::http::Error::Invalid(message)) => return Err(tracker_error(message).into()),
        };
        if response.status != 200 {
            return Err(tracker_error(format!(
                "tracker returned HTTP status {}",
                response.status_text
            ))
            .into());
        }
        return parse_scrape(&response.body).map_err(Failure::from);
    }
    Err(last.unwrap_or_else(|| unreachable("tracker could not be reached")))
}

/// A BEP 48 scrape response: `files`, each torrent's hash to its counts.
fn parse_scrape(body: &[u8]) -> Result<HashMap<[u8; 20], ScrapeStats>, crate::BackendError> {
    let response = crate::bencode::decode(body)?;
    if let Some(reason) = response.get_bytes(b"failure reason") {
        return Err(tracker_error(format!(
            "tracker refused the scrape: {}",
            String::from_utf8_lossy(reason)
        )));
    }
    let Some(crate::bencode::Value::Dict(files)) = response.get(b"files") else {
        return Err(tracker_error("scrape response has no files"));
    };
    let count = |file: &crate::bencode::Value, key: &[u8]| {
        file.get_int(key)
            .and_then(|count| u64::try_from(count).ok())
            .unwrap_or(0)
    };
    Ok(files
        .iter()
        .filter_map(|(hash, file)| {
            let hash = <[u8; 20]>::try_from(hash.as_slice()).ok()?;
            Some((
                hash,
                ScrapeStats {
                    seeders: count(file, b"complete"),
                    leechers: count(file, b"incomplete"),
                    downloaded: count(file, b"downloaded"),
                },
            ))
        })
        .collect())
}

/// A BEP 15 scrape: connect, then action 2 with the hashes; the answer
/// lists seeders, completed and leechers for each, in the order asked.
fn scrape_udp(
    tracker: &Tracker,
    info_hashes: &[[u8; 20]],
    timeout: Duration,
) -> Result<HashMap<[u8; 20], ScrapeStats>, Failure> {
    let (host, port) = parse_udp_url(tracker.url())?;
    let mut last = None;
    for address in family_addresses(tracker, &format!("{host}:{port}"))? {
        match scrape_udp_at(tracker, address, info_hashes, timeout) {
            Ok(stats) => return Ok(stats),
            Err(failure) if failure.unreachable => last = Some(failure),
            Err(failure) => return Err(failure),
        }
    }
    Err(last.unwrap_or_else(|| unreachable("tracker could not be reached")))
}

fn scrape_udp_at(
    tracker: &Tracker,
    address: SocketAddr,
    info_hashes: &[[u8; 20]],
    timeout: Duration,
) -> Result<HashMap<[u8; 20], ScrapeStats>, Failure> {
    let socket = tracker
        .binding
        .udp_for(address)
        .map_err(|error| unreachable(error.to_string()))?;
    socket
        .connect(address)
        .map_err(|error| unreachable(error.to_string()))?;
    let attempt_timeout = (timeout / UDP_ATTEMPTS).max(Duration::from_secs(1));
    socket
        .set_read_timeout(Some(attempt_timeout))
        .map_err(|error| tracker_error(error.to_string()))?;
    let response = with_connection_id(&socket, address, |connection_id| {
        let transaction = transaction_id();
        let mut scrape = connection_id.to_vec();
        scrape.extend(2_u32.to_be_bytes());
        scrape.extend(transaction.to_be_bytes());
        for hash in info_hashes {
            scrape.extend(hash);
        }
        udp_exchange(&socket, &scrape, 2, transaction, 8)
    })?;
    Ok(info_hashes
        .iter()
        .zip(response[8..].as_chunks::<12>().0)
        .map(|(hash, counts)| {
            let read = |at: usize| {
                u64::from(u32::from_be_bytes([
                    counts[at],
                    counts[at + 1],
                    counts[at + 2],
                    counts[at + 3],
                ]))
            };
            (
                *hash,
                ScrapeStats {
                    seeders: read(0),
                    downloaded: read(4),
                    leechers: read(8),
                },
            )
        })
        .collect())
}

fn tracker_error(message: impl Into<String>) -> crate::BackendError {
    crate::BackendError::Tracker(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announces_at_the_trackers_interval_as_libtorrent_does() {
        let next = |body: &[u8]| {
            TrackerResponse::from_bencode(body)
                .unwrap()
                .next_announce_in()
        };
        let minutes = |count: u64| std::time::Duration::from_secs(count * 60);
        // `interval`, not the `min interval` a client may at most use.
        assert_eq!(
            next(b"d8:intervali1800e12:min intervali300e5:peers0:e"),
            minutes(30)
        );
        // Never sooner than `min interval`, nor than five minutes.
        assert_eq!(
            next(b"d8:intervali600e12:min intervali900e5:peers0:e"),
            minutes(15)
        );
        assert_eq!(next(b"d8:intervali60e5:peers0:e"), minutes(5));
        assert_eq!(next(b"d8:intervali0e5:peers0:e"), minutes(5));
        // A tracker that asks for longer gets it.
        assert_eq!(next(b"d8:intervali7200e5:peers0:e"), minutes(120));
        assert_eq!(next(b"d5:peers0:e"), minutes(30), "none given");
    }
    use std::io::{Read, Write};

    #[test]
    #[ignore = "requires network access"]
    fn reaches_a_real_https_server() {
        let client =
            TrackerClient::from_urls(vec!["https://example.com/announce".to_owned()]).unwrap();
        let request = AnnounceRequest {
            info_hash: [1; 20],
            peer_id: [2; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 0,
            corrupt: 0,
            compact: true,
            event: AnnounceEvent::None,
        };
        let error = client
            .announce(&request, Duration::from_secs(10))
            .unwrap_err();
        assert!(
            matches!(&error, crate::BackendError::Tracker(message) if message.contains("HTTP status 4")),
            "{error}"
        );
    }

    /// An HTTP tracker on `address` that answers every announce with
    /// `peers` (compact IPv4 ones), or never answers when `None`.
    fn family_tracker(address: SocketAddr, peers: Option<&'static [u8]>) -> Option<SocketAddr> {
        let listener = std::net::TcpListener::bind(address).ok()?;
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for mut stream in listener.incoming().flatten() {
                let _ = stream.read(&mut [0; 2048]);
                match peers {
                    Some(peers) => {
                        let mut body =
                            format!("d8:intervali60e5:peers{}:", peers.len()).into_bytes();
                        body.extend(peers);
                        body.push(b'e');
                        let _ = stream
                            .write_all(&[b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), &body].concat());
                    }
                    None => held.push(stream),
                }
            }
        });
        Some(address)
    }

    fn http_family_announce(port: u16) -> FamilyAnnounce {
        let url =
            crate::http::Url::parse(&format!("http://tracker.test:{port}/announce"), "").unwrap();
        let target = format!("{}?info_hash=x", url.path);
        Arc::new(move |address| {
            http_announce_at(
                address,
                &url,
                &target,
                crate::identity::USER_AGENT,
                Duration::from_secs(10),
                &crate::Binding::any(),
            )
        })
    }

    #[test]
    fn announces_over_both_families_and_merges_their_peers() {
        let v4 = family_tracker(
            "127.0.0.1:0".parse().unwrap(),
            Some(&[10, 0, 0, 4, 0x1a, 0xe1]),
        )
        .unwrap();
        let Some(v6) = family_tracker(
            SocketAddr::from((Ipv6Addr::LOCALHOST, v4.port())),
            Some(&[10, 0, 0, 6, 0x1a, 0xe1]),
        ) else {
            eprintln!("skipped: no IPv6 here");
            return;
        };
        let response = over_each_family(vec![v4, v6], http_family_announce(v4.port())).unwrap();
        let mut peers = response.peers().to_vec();
        peers.sort();
        assert_eq!(
            peers,
            [
                "10.0.0.4:6881".parse().unwrap(),
                "10.0.0.6:6881".parse().unwrap()
            ]
        );
    }

    #[test]
    fn a_silent_family_does_not_hold_up_the_announce() {
        let v4 = family_tracker(
            "127.0.0.1:0".parse().unwrap(),
            Some(&[10, 0, 0, 4, 0x1a, 0xe1]),
        )
        .unwrap();
        let Some(v6) = family_tracker(SocketAddr::from((Ipv6Addr::LOCALHOST, v4.port())), None)
        else {
            eprintln!("skipped: no IPv6 here");
            return;
        };
        let started = std::time::Instant::now();
        let response = over_each_family(vec![v4, v6], http_family_announce(v4.port())).unwrap();
        assert_eq!(response.peers(), ["10.0.0.4:6881".parse().unwrap()]);
        // Not the 10 s timeout: the grace after the first answer.
        assert!(started.elapsed() < FAMILY_GRACE + Duration::from_secs(1));
    }

    #[test]
    fn our_interface_being_down_does_not_mark_trackers_dead() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce?down", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let _ = stream.read(&mut [0; 2048]);
                let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\nd8:intervali60e5:peers0:e");
            }
        });
        let request = AnnounceRequest {
            info_hash: [3; 20],
            peer_id: [4; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1,
            corrupt: 0,
            compact: true,
            event: AnnounceEvent::Started,
        };
        let down = TrackerClient::from_urls(vec![url.clone()])
            .unwrap()
            .with_binding(&crate::Binding::interface("empirebt-vpn0"));
        let error = down.announce(&request, Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("empirebt-vpn0"), "{error}");
        // Once the interface is back, the tracker is asked at once.
        let up = TrackerClient::from_urls(vec![url]).unwrap();
        up.announce(&request, Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn an_unreachable_tracker_is_skipped_for_other_torrents() {
        use std::net::TcpListener;
        // Accepts connections and never answers, like a dead tracker.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/announce?passkey=a",
            listener.local_addr().unwrap()
        );
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().take(4).collect();
            std::thread::sleep(Duration::from_secs(5));
            drop(held);
        });
        let announce = |url: &str, hash: u8| {
            let started = std::time::Instant::now();
            let client = TrackerClient::from_urls(vec![url.to_owned()]).unwrap();
            let request = AnnounceRequest {
                info_hash: [hash; 20],
                peer_id: [2; 20],
                port: 6881,
                uploaded: 0,
                downloaded: 0,
                left: 0,
                corrupt: 0,
                compact: true,
                event: AnnounceEvent::None,
            };
            let error = client
                .announce(&request, Duration::from_millis(500))
                .unwrap_err();
            (error.to_string(), started.elapsed())
        };

        let (_, first) = announce(&url, 1);
        assert!(first >= Duration::from_millis(400));
        // Another torrent, and another passkey on the same host, skip it.
        let other = url.replace("passkey=a", "passkey=b");
        let (message, second) = announce(&other, 2);
        assert!(second < Duration::from_millis(50), "{second:?}");
        assert!(message.contains("unreachable; retrying in"), "{message}");
    }

    #[test]
    fn accepts_supported_tracker_schemes() {
        assert!(Tracker::new("https://tracker.example/announce").is_ok());
        assert!(Tracker::new("udp://tracker.example:6969").is_ok());
        assert!(Tracker::new("file:///tmp/tracker").is_err());
    }

    /// An HTTP tracker that answers with no peers and forwards each
    /// request's target.
    fn recording_tracker() -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 2048];
                let size = stream.read(&mut request).unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..size]).into_owned();
                let body = b"d8:intervali900e5:peers0:e";
                let _ = stream.write_all(
                    &[
                        format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                            .as_bytes(),
                        body,
                    ]
                    .concat(),
                );
                if sender.send(text).is_err() {
                    return;
                }
            }
        });
        (url, receiver)
    }

    /// An HTTP tracker answering each connection with the next of
    /// `bodies` (as `status`, `extra headers`, body), recording requests.
    fn scripted_tracker(
        answers: Vec<(&'static str, String, Vec<u8>)>,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for ((status, headers, body), stream) in answers.into_iter().zip(listener.incoming()) {
                let Ok(mut stream) = stream else { return };
                let mut request = [0; 2048];
                let size = stream.read(&mut request).unwrap_or(0);
                let _ = stream.write_all(
                    &[
                        format!(
                            "HTTP/1.0 {status}\r\n{headers}Content-Length: {}\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                        &body,
                    ]
                    .concat(),
                );
                let _ = sender.send(String::from_utf8_lossy(&request[..size]).into_owned());
            }
        });
        (url, receiver)
    }

    fn ok(body: &[u8]) -> (&'static str, String, Vec<u8>) {
        ("200 OK", String::new(), body.to_vec())
    }

    #[test]
    fn backs_off_after_failures_as_libtorrent_does() {
        let delays: Vec<u64> = (1..=6)
            .map(|failures| retry_delay(failures).as_secs())
            .collect();
        assert_eq!(delays, [17, 55, 117, 205, 317, 455]);
        assert_eq!(retry_delay(1000).as_secs(), 3600, "at most an hour");
    }

    #[test]
    fn reads_answers_as_leniently_as_libtorrent() {
        // No peers at all: an answer, often to `stopped`.
        let response = TrackerResponse::from_bencode(b"d8:intervali1800ee").unwrap();
        assert!(response.peers().is_empty());
        // A tracker id to send back, and a warning that is not UTF-8.
        let response = TrackerResponse::from_bencode(
            b"d10:tracker id3:abc15:warning message2:\xff!5:peers0:e",
        )
        .unwrap();
        assert_eq!(response.tracker_id.as_deref(), Some(&b"abc"[..]));
        assert_eq!(response.warning(), Some("\u{fffd}!"));
    }

    #[test]
    fn sends_back_the_tracker_id_until_leaving() {
        let (url, requests) = scripted_tracker(vec![
            ok(b"d10:tracker id3:x y5:peers0:e"),
            ok(b"d5:peers0:e"),
            ok(b"d5:peers0:e"),
            ok(b"d5:peers0:e"),
        ]);
        let client = TrackerClient::from_urls([url]).unwrap();
        let timeout = Duration::from_secs(5);
        let next = || requests.recv_timeout(timeout).unwrap();
        client
            .announce(&request(AnnounceEvent::Started), timeout)
            .unwrap();
        assert!(!next().contains("trackerid"));
        client
            .announce(&request(AnnounceEvent::None), timeout)
            .unwrap();
        assert!(next().contains("&trackerid=x%20y "), "sent back");
        client
            .announce(&request(AnnounceEvent::Stopped), timeout)
            .unwrap();
        assert!(next().contains("&trackerid=x%20y "));
        client
            .announce(&request(AnnounceEvent::Started), timeout)
            .unwrap();
        assert!(!next().contains("trackerid"), "forgotten after stopped");
    }

    #[test]
    fn follows_a_tracker_that_moved() {
        let (moved_to, moved_requests) = scripted_tracker(vec![ok(
            b"d8:intervali1800e5:peers6:\x7f\x00\x00\x01\x1a\xe1e",
        )]);
        let (url, requests) = scripted_tracker(vec![(
            "301 Moved Permanently",
            format!("Location: {moved_to}?moved=1\r\n"),
            Vec::new(),
        )]);
        let response = TrackerClient::from_urls([url])
            .unwrap()
            .announce(&request(AnnounceEvent::Started), Duration::from_secs(5))
            .unwrap();
        assert_eq!(response.peers(), ["127.0.0.1:6881".parse().unwrap()]);
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        let followed = moved_requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(followed.starts_with("GET /announce?moved=1 "), "{followed}");
    }

    #[test]
    fn tries_the_tracker_that_answered_first_in_its_tier() {
        let refusing = || ok(b"d14:failure reason6:no wayee");
        let (first, first_requests) = scripted_tracker(vec![refusing(), refusing()]);
        let (second, second_requests) =
            scripted_tracker(vec![ok(b"d5:peers0:e"), ok(b"d5:peers0:e")]);
        let client = TrackerClient::from_tiers([vec![first, second]]).unwrap();
        let timeout = Duration::from_secs(5);
        client
            .announce(&request(AnnounceEvent::Started), timeout)
            .unwrap();
        client
            .announce(&request(AnnounceEvent::None), timeout)
            .unwrap();
        assert_eq!(
            first_requests.try_iter().count(),
            1,
            "asked once, then passed over"
        );
        assert_eq!(second_requests.try_iter().count(), 2);
    }

    /// The URL's path goes along as BEP 41's URLData option, and the
    /// connection ID is reused within its minute.
    #[test]
    fn announces_over_udp_with_the_url_path_and_one_connect() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let url = format!("udp://{}/announce/secret?x=1", socket.local_addr().unwrap());
        let tracker = std::thread::spawn(move || {
            let mut buffer = [0; 1024];
            let mut connects = 0;
            let mut options = Vec::new();
            for _ in 0..3 {
                let (length, from) = socket.recv_from(&mut buffer).unwrap();
                let action = u32::from_be_bytes(buffer[8..12].try_into().unwrap());
                let mut reply = action.to_be_bytes().to_vec();
                reply.extend(&buffer[12..16]);
                if length == 16 {
                    connects += 1;
                    reply.extend(7_u64.to_be_bytes());
                } else {
                    assert_eq!(&buffer[..8], &7_u64.to_be_bytes());
                    options.push(buffer[98..length].to_vec());
                    for count in [1800_u32, 0, 0] {
                        reply.extend(count.to_be_bytes());
                    }
                }
                socket.send_to(&reply, from).unwrap();
            }
            (connects, options)
        });
        let client = TrackerClient::from_urls([url]).unwrap();
        for event in [AnnounceEvent::Started, AnnounceEvent::None] {
            client
                .announce(&request(event), Duration::from_secs(5))
                .unwrap();
        }
        let (connects, options) = tracker.join().unwrap();
        assert_eq!(connects, 1);
        let mut expected = vec![2, 20];
        expected.extend(b"/announce/secret?x=1");
        assert_eq!(options, [expected.clone(), expected]);
    }

    fn request(event: AnnounceEvent) -> AnnounceRequest {
        AnnounceRequest {
            info_hash: [1; 20],
            peer_id: *b"-EM0100-abcdefghijkl",
            port: 6881,
            uploaded: 10,
            downloaded: 20,
            left: 30,
            corrupt: 40,
            compact: true,
            event,
        }
    }

    #[test]
    fn announces_with_libtorrents_parameters_in_its_order() {
        let started = request(AnnounceEvent::Started);
        assert_eq!(
            started.query(),
            format!(
                "info_hash=%01%01%01%01%01%01%01%01%01%01%01%01%01%01%01%01%01%01%01%01\
                 &peer_id=-EM0100-abcdefghijkl&port=6881&uploaded=10&downloaded=20&left=30\
                 &corrupt=40&key={:08X}&event=started&numwant=200&compact=1&no_peer_id=1",
                started.key()
            )
        );
        // Leaving asks for no peers; the key stays with the peer ID.
        let stopped = request(AnnounceEvent::Stopped);
        assert!(stopped.query().contains("&event=stopped&numwant=0&"));
        assert_eq!(stopped.key(), started.key());
        let mut other = request(AnnounceEvent::None);
        other.peer_id = *b"-EM0100-mnopqrstuvwx";
        assert_ne!(other.key(), started.key());
        assert!(!other.query().contains("event="));
    }

    #[test]
    fn announces_to_one_tracker_per_tier_and_to_every_tier() {
        let (first, first_heard) = recording_tracker();
        let (backup, backup_heard) = recording_tracker();
        let (other, other_heard) = recording_tracker();
        let client = TrackerClient::from_tiers([
            vec![first.clone(), backup.clone()],
            vec![other.clone(), first],
        ])
        .unwrap();
        assert_eq!(
            client.trackers().len(),
            3,
            "a repeat stays in its first tier"
        );
        let results = client.announce_all(&request(AnnounceEvent::Started), Duration::from_secs(5));
        assert_eq!(results.len(), 2, "one result per tier");
        assert!(results.iter().all(Result::is_ok));
        let wait = Duration::from_secs(2);
        assert!(first_heard.recv_timeout(wait).is_ok());
        assert!(other_heard.recv_timeout(wait).is_ok());
        assert!(
            backup_heard
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "the backup in the same tier would count the upload twice"
        );

        // When the first in a tier is down, the next one takes over.
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/announce", listener.local_addr().unwrap())
        };
        let client = TrackerClient::from_tiers([vec![dead, backup]]).unwrap();
        let results = client.announce_all(&request(AnnounceEvent::None), Duration::from_secs(5));
        assert!(results[0].is_ok(), "{results:?}");
        assert!(backup_heard.recv_timeout(wait).is_ok());
    }

    #[test]
    fn derives_scrape_urls_as_bep_48_says() {
        let examples = [
            (
                "http://example.com/announce",
                Some("http://example.com/scrape"),
            ),
            (
                "http://example.com/x/announce",
                Some("http://example.com/x/scrape"),
            ),
            (
                "http://example.com/announce.php",
                Some("http://example.com/scrape.php"),
            ),
            ("http://example.com/a", None),
            (
                "http://example.com/announce?x2%0644",
                Some("http://example.com/scrape?x2%0644"),
            ),
            (
                "http://example.com/announce?x=2/4",
                Some("http://example.com/scrape?x=2/4"),
            ),
            ("http://example.com/x%064announce", None),
            ("http://example.com", None),
            // A private tracker's passkey stays.
            (
                "https://tracker.example/abc123/announce",
                Some("https://tracker.example/abc123/scrape"),
            ),
        ];
        for (announce, scrape) in examples {
            assert_eq!(scrape_url(announce).as_deref(), scrape, "{announce}");
        }
        assert!(
            Tracker::new("udp://tracker.example:6969")
                .unwrap()
                .supports_scrape()
        );
        assert!(
            !Tracker::new("http://example.com/a")
                .unwrap()
                .supports_scrape()
        );
    }

    #[test]
    fn scrapes_http_trackers_for_several_torrents_at_once() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/announce?passkey=k",
            listener.local_addr().unwrap()
        );
        let (sender, asked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let size = stream.read(&mut request).unwrap();
            let _ = sender.send(String::from_utf8_lossy(&request[..size]).into_owned());
            let mut body = b"d5:filesd20:".to_vec();
            body.extend([1; 20]);
            body.extend(b"d8:completei5e10:downloadedi50e10:incompletei7ee20:");
            body.extend([2; 20]);
            body.extend(b"d8:completei0e10:downloadedi1e10:incompletei2eeee");
            let _ = stream.write_all(&[b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), &body].concat());
        });
        let tracker = Tracker::new(url).unwrap();
        let stats = tracker
            .scrape(
                &[[1; 20], [2; 20], [3; 20]],
                crate::USER_AGENT,
                Duration::from_secs(5),
            )
            .unwrap();
        let request = asked.recv().unwrap();
        assert!(
            request.starts_with("GET /scrape?passkey=k&info_hash=%01%01"),
            "{request}"
        );
        assert_eq!(request.matches("info_hash=").count(), 3);
        assert_eq!(
            stats[&[1; 20]],
            ScrapeStats {
                seeders: 5,
                leechers: 7,
                downloaded: 50
            }
        );
        assert_eq!(stats[&[2; 20]].leechers, 2);
        assert!(!stats.contains_key(&[3; 20]), "one it does not know");
    }

    #[test]
    fn scrapes_udp_trackers_as_bep_15_says() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let url = format!("udp://{}", socket.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut buffer = [0; 1024];
            // Connect.
            let (length, from) = socket.recv_from(&mut buffer).unwrap();
            assert_eq!(length, 16);
            let mut reply = 0_u32.to_be_bytes().to_vec();
            reply.extend(&buffer[12..16]);
            reply.extend(42_u64.to_be_bytes());
            socket.send_to(&reply, from).unwrap();
            // Scrape: connection id, action 2, transaction, two hashes.
            let (length, from) = socket.recv_from(&mut buffer).unwrap();
            assert_eq!(length, 16 + 2 * 20);
            assert_eq!(&buffer[..8], &42_u64.to_be_bytes());
            assert_eq!(&buffer[8..12], &2_u32.to_be_bytes());
            let mut reply = 2_u32.to_be_bytes().to_vec();
            reply.extend(&buffer[12..16]);
            for (seeders, completed, leechers) in [(9_u32, 90_u32, 4_u32), (1, 2, 3)] {
                reply.extend(seeders.to_be_bytes());
                reply.extend(completed.to_be_bytes());
                reply.extend(leechers.to_be_bytes());
            }
            socket.send_to(&reply, from).unwrap();
        });
        let stats = Tracker::new(url)
            .unwrap()
            .scrape(
                &[[1; 20], [2; 20]],
                crate::USER_AGENT,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(
            stats[&[1; 20]],
            ScrapeStats {
                seeders: 9,
                leechers: 4,
                downloaded: 90
            }
        );
        assert_eq!(stats[&[2; 20]].downloaded, 2);
    }

    #[test]
    fn deduplicates_tracker_urls() {
        let client = TrackerClient::from_urls(vec![
            "http://tracker.example/a".to_owned(),
            "http://tracker.example/a".to_owned(),
        ])
        .unwrap();
        assert_eq!(client.trackers().len(), 1);
    }

    #[test]
    fn builds_binary_safe_announce_query() {
        let request = AnnounceRequest {
            info_hash: [0; 20],
            peer_id: *b"-EM0001-123456789012",
            port: 6881,
            uploaded: 2,
            downloaded: 3,
            left: 4,
            corrupt: 0,
            compact: true,
            event: AnnounceEvent::None,
        };
        assert!(request.query().contains("info_hash=%00%00"));
        assert!(request.query().contains("compact=1"));
    }

    #[test]
    fn decodes_compact_ipv4_peers() {
        let response =
            TrackerResponse::from_compact_peers(&[127, 0, 0, 1, 0x1A, 0xE1], Some(1800), None)
                .unwrap();
        assert_eq!(response.peers(), &["127.0.0.1:6881".parse().unwrap()]);
        assert_eq!(response.interval(), Some(1800));
    }

    #[test]
    fn parses_bencoded_tracker_response() {
        let response =
            TrackerResponse::from_bencode(b"d8:intervali1800e5:peers6:\x7f\x00\x00\x01\x1a\xe1e")
                .unwrap();

        assert_eq!(response.interval(), Some(1800));
        assert_eq!(response.peers(), &["127.0.0.1:6881".parse().unwrap()]);
    }

    #[test]
    fn parses_dictionary_form_tracker_peers() {
        let response = TrackerResponse::from_bencode(
            b"d8:intervali60e5:peersl\
d2:ip9:127.0.0.14:porti6881eeee",
        )
        .unwrap();

        assert_eq!(response.peers(), &["127.0.0.1:6881".parse().unwrap()]);
    }

    #[test]
    fn accepts_a_valid_tracker_response_with_no_peers() {
        let response = TrackerResponse::from_bencode(b"d8:intervali60e5:peers0:e").unwrap();

        assert!(response.peers().is_empty());
        assert_eq!(response.interval(), Some(60));
    }

    #[test]
    fn parses_tracker_minimum_interval() {
        let response =
            TrackerResponse::from_bencode(b"d8:intervali60e12:min intervali120e5:peers0:e")
                .unwrap();

        assert_eq!(response.interval(), Some(60));
        assert_eq!(response.min_interval(), Some(120));
    }

    #[test]
    fn reports_tracker_failure() {
        let result = TrackerResponse::from_bencode(b"d14:failure reason11:temporarilye");

        assert!(matches!(
            result,
            Err(crate::BackendError::Tracker(message))
                if message.contains("temporarily")
        ));
    }

    #[test]
    fn announces_to_local_http_tracker() {
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /announce?"));
            assert!(request.contains("compact=1"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 33\r\n\r\n\
d8:intervali60e5:peers6:\x7f\x00\x00\x01\x1a\xe1e",
                )
                .unwrap();
        });

        let client =
            TrackerClient::from_urls(vec![format!("http://127.0.0.1:{port}/announce")]).unwrap();
        let request = AnnounceRequest {
            info_hash: [1; 20],
            peer_id: [2; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 10,
            corrupt: 0,
            compact: true,
            event: AnnounceEvent::None,
        };
        let response = client
            .announce_http(&request, Duration::from_secs(1))
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.peers(), &["127.0.0.1:6881".parse().unwrap()]);
        assert_eq!(response.interval(), Some(60));
    }

    #[test]
    fn fails_over_to_the_next_http_tracker() {
        use std::net::TcpListener;
        use std::thread;

        let unused = TcpListener::bind("127.0.0.1:0").unwrap();
        let unused_port = unused.local_addr().unwrap().port();
        drop(unused);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let working_port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
d8:intervali60e5:peers6:\x7f\x00\x00\x01\x1a\xe1e",
                )
                .unwrap();
        });

        let client = TrackerClient::from_urls(vec![
            format!("http://127.0.0.1:{unused_port}/announce"),
            format!("http://127.0.0.1:{working_port}/announce"),
        ])
        .unwrap();
        let request = AnnounceRequest {
            info_hash: [1; 20],
            peer_id: [2; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 10,
            corrupt: 0,
            compact: true,
            event: AnnounceEvent::None,
        };

        let response = client
            .announce_http(&request, Duration::from_secs(1))
            .unwrap();
        server.join().unwrap();
        assert_eq!(response.peers(), &["127.0.0.1:6881".parse().unwrap()]);
    }
}
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

/// Lifecycle event reported with an announce.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// A regular re-announce.
    #[default]
    None,
    /// The first announce for a transfer.
    Started,
    /// The download just finished.
    Completed,
    /// The client is leaving the swarm.
    Stopped,
}

impl AnnounceEvent {
    fn http_name(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Started => Some("started"),
            Self::Completed => Some("completed"),
            Self::Stopped => Some("stopped"),
        }
    }

    fn udp_code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Completed => 1,
            Self::Started => 2,
            Self::Stopped => 3,
        }
    }
}

/// Parameters for a tracker announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceRequest {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    /// Payload bytes sent since the `started` announce.
    pub uploaded: u64,
    /// Payload bytes received and verified since the `started` announce:
    /// not data that was already on disk, nor corrupt pieces.
    pub downloaded: u64,
    /// Bytes still missing, of the whole torrent.
    pub left: u64,
    /// Bytes of pieces that failed verification since `started`.
    pub corrupt: u64,
    pub compact: bool,
    pub event: AnnounceEvent,
}

/// Peers asked of a tracker per announce, as libtorrent asks.
const NUM_WANT: u32 = 200;

impl AnnounceRequest {
    /// Builds the query string for an HTTP tracker announce, with the
    /// parameters in the order libtorrent sends them.
    pub fn query(&self) -> String {
        let event = self
            .event
            .http_name()
            .map(|event| format!("&event={event}"))
            .unwrap_or_default();
        format!(
            "info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&corrupt={}\
             &key={:08X}{event}&numwant={}&compact={}&no_peer_id=1",
            percent_encode(&self.info_hash),
            percent_encode(&self.peer_id),
            self.port,
            self.uploaded,
            self.downloaded,
            self.left,
            self.corrupt,
            self.key(),
            self.num_want(),
            u8::from(self.compact)
        )
    }

    /// Identifies this client to trackers across address changes, as
    /// every client's `key` does. It follows the peer ID, which is random
    /// per run (and per network change), so it links nothing further.
    pub fn key(&self) -> u32 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        self.peer_id.hash(&mut hasher);
        hasher.finish() as u32
    }

    /// Peers wanted: none when leaving.
    fn num_want(&self) -> u32 {
        if self.event == AnnounceEvent::Stopped {
            0
        } else {
            NUM_WANT
        }
    }
}

/// Result of decoding a compact tracker response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerResponse {
    peers: Vec<SocketAddr>,
    interval: Option<u64>,
    min_interval: Option<u64>,
    warning: Option<String>,
    seeders: Option<u64>,
    leechers: Option<u64>,
    /// `tracker id`, to send back with later announces.
    tracker_id: Option<Vec<u8>>,
}

impl TrackerResponse {
    /// Decodes a bencoded HTTP tracker response.
    pub fn from_bencode(bytes: &[u8]) -> Result<Self, crate::BackendError> {
        let mut parser = BencodeParser { bytes, position: 0 };
        let root = parser.parse()?;
        if parser.position != bytes.len() {
            return Err(invalid("tracker response has trailing data"));
        }
        let dictionary = root.as_dictionary()?;
        if let Some(failure) = dictionary.get(b"failure reason".as_slice()) {
            return Err(tracker_error(format!(
                "tracker refused the announce: {}",
                String::from_utf8_lossy(failure.as_bytes()?)
            )));
        }
        let count = |key: &[u8]| {
            dictionary
                .get(key)
                .and_then(|value| value.as_integer().ok())
        };
        let (seeders, leechers) = (count(b"complete"), count(b"incomplete"));
        // As libtorrent reads them: a malformed optional field is left
        // out rather than failing the whole answer.
        let interval = count(b"interval");
        let min_interval = count(b"min interval");
        let text = |key: &[u8]| {
            dictionary
                .get(key)
                .and_then(|value| value.as_bytes().ok())
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        };
        let warning = text(b"warning message");
        let tracker_id = dictionary
            .get(b"tracker id".as_slice())
            .and_then(|value| value.as_bytes().ok())
            .filter(|id| !id.is_empty())
            .map(<[u8]>::to_vec);

        // No peers at all is an answer too (often to `stopped`, or for an
        // empty swarm); libtorrent accepts it.
        let mut peers = Vec::new();
        if let Some(value) = dictionary.get(b"peers".as_slice()) {
            match value {
                Value::Bytes(compact) => peers.extend(
                    Self::from_compact_peers_with_min_interval(
                        compact,
                        interval,
                        min_interval,
                        warning.clone(),
                    )?
                    .peers,
                ),
                Value::List(entries) => peers.extend(parse_peer_list(entries)?),
                _ => return Err(invalid("tracker peers field has an invalid type")),
            }
        }
        if let Some(value) = dictionary.get(b"peers6".as_slice()) {
            peers.extend(
                Self::from_compact_ipv6_with_min_interval(
                    value.as_bytes()?,
                    interval,
                    min_interval,
                    warning.clone(),
                )?
                .peers,
            );
        }
        Ok(Self {
            peers,
            interval,
            min_interval,
            warning,
            seeders,
            leechers,
            tracker_id,
        })
    }

    pub fn from_compact_peers(
        peers: &[u8],
        interval: Option<u64>,
        warning: Option<String>,
    ) -> Result<Self, crate::BackendError> {
        Self::from_compact_peers_with_min_interval(peers, interval, None, warning)
    }

    fn from_compact_peers_with_min_interval(
        peers: &[u8],
        interval: Option<u64>,
        min_interval: Option<u64>,
        warning: Option<String>,
    ) -> Result<Self, crate::BackendError> {
        if peers.len() % 6 != 0 {
            return Err(crate::BackendError::InvalidMetainfo(
                "compact IPv4 peer list must be divisible by 6".to_owned(),
            ));
        }
        let addresses = peers
            .chunks_exact(6)
            .map(|peer| {
                SocketAddr::from((
                    Ipv4Addr::new(peer[0], peer[1], peer[2], peer[3]),
                    u16::from_be_bytes([peer[4], peer[5]]),
                ))
            })
            .collect();
        Ok(Self {
            peers: addresses,
            interval,
            min_interval,
            warning,
            seeders: None,
            leechers: None,
            tracker_id: None,
        })
    }

    pub fn from_compact_ipv6(
        peers: &[u8],
        interval: Option<u64>,
        warning: Option<String>,
    ) -> Result<Self, crate::BackendError> {
        Self::from_compact_ipv6_with_min_interval(peers, interval, None, warning)
    }

    fn from_compact_ipv6_with_min_interval(
        peers: &[u8],
        interval: Option<u64>,
        min_interval: Option<u64>,
        warning: Option<String>,
    ) -> Result<Self, crate::BackendError> {
        if peers.len() % 18 != 0 {
            return Err(crate::BackendError::InvalidMetainfo(
                "compact IPv6 peer list must be divisible by 18".to_owned(),
            ));
        }
        let addresses = peers
            .chunks_exact(18)
            .map(|peer| {
                let address = Ipv6Addr::from(<[u8; 16]>::try_from(&peer[..16]).unwrap());
                SocketAddr::from((address, u16::from_be_bytes([peer[16], peer[17]])))
            })
            .collect();
        Ok(Self {
            peers: addresses,
            interval,
            min_interval,
            warning,
            seeders: None,
            leechers: None,
            tracker_id: None,
        })
    }

    pub fn peers(&self) -> &[SocketAddr] {
        &self.peers
    }

    pub fn interval(&self) -> Option<u64> {
        self.interval
    }

    pub fn min_interval(&self) -> Option<u64> {
        self.min_interval
    }

    /// When to announce again after this response, as libtorrent decides:
    /// the tracker's `interval` (30 minutes if it gives none), never
    /// sooner than its `min interval` or than [`MIN_ANNOUNCE_INTERVAL`].
    /// `min interval` is the least a client may wait, not how often to
    /// announce.
    pub fn next_announce_in(&self) -> std::time::Duration {
        let seconds = |value: Option<u64>| value.map(std::time::Duration::from_secs);
        seconds(self.interval)
            .unwrap_or(DEFAULT_ANNOUNCE_INTERVAL)
            .max(seconds(self.min_interval).unwrap_or_default())
            .max(MIN_ANNOUNCE_INTERVAL)
    }

    /// Swarm seeders reported by the tracker, when it reports them.
    pub fn seeders(&self) -> Option<u64> {
        self.seeders
    }

    /// Swarm leechers reported by the tracker, when it reports them.
    pub fn leechers(&self) -> Option<u64> {
        self.leechers
    }

    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }
}

fn percent_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' => {
                (*byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn parse_peer_list(entries: &[Value<'_>]) -> Result<Vec<SocketAddr>, crate::BackendError> {
    entries
        .iter()
        .map(|entry| {
            let dictionary = entry.as_dictionary()?;
            let ip = dictionary
                .get(b"ip".as_slice())
                .ok_or_else(|| invalid("tracker peer is missing its IP address"))?
                .as_text()?;
            let port = dictionary
                .get(b"port".as_slice())
                .ok_or_else(|| invalid("tracker peer is missing its port"))?
                .as_integer()?;
            let port =
                u16::try_from(port).map_err(|_| invalid("tracker peer port is out of range"))?;
            let address = format!("{ip}:{port}")
                .parse()
                .map_err(|_| invalid("tracker peer IP address is invalid"))?;
            Ok(address)
        })
        .collect()
}
