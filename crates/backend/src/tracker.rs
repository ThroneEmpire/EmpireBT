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

    let transaction = transaction_id();
    let mut connect = Vec::with_capacity(16);
    connect.extend(0x41727101980_u64.to_be_bytes());
    connect.extend(0_u32.to_be_bytes());
    connect.extend(transaction.to_be_bytes());
    let response = udp_exchange(&socket, &connect, 0, transaction, 16)?;
    let connection_id = response[8..16].to_vec();

    let transaction = transaction_id().wrapping_add(1);
    let mut announce = Vec::with_capacity(98);
    announce.extend(&connection_id);
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
    let response = udp_exchange(&socket, &announce, 1, transaction, 20)?;
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

fn transaction_id() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    (nanos as u64 ^ (nanos >> 32) as u64) as u32
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
    /// succeeds.
    pub fn announce(
        &self,
        request: &AnnounceRequest,
        timeout: Duration,
    ) -> Result<TrackerResponse, crate::BackendError> {
        let mut last_error = None;
        for tracker in &self.trackers {
            match announce_tracker(tracker, request, timeout) {
                Ok(response) => return Ok(response),
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
    let target = format!("{}{separator}{}", url.path, request.query());
    let addresses = family_addresses(tracker, &format!("{}:{}", url.host, url.port))?;
    let binding = tracker.binding.clone();
    let user_agent = crate::identity::user_agent_for(&request.peer_id);
    over_each_family(
        addresses,
        Arc::new(move |address| {
            http_announce_at(address, &url, &target, user_agent, timeout, &binding)
        }),
    )
}

fn http_announce_at(
    address: SocketAddr,
    url: &crate::http::Url,
    target: &str,
    user_agent: &str,
    timeout: Duration,
    binding: &crate::Binding,
) -> Result<TrackerResponse, Failure> {
    let response = crate::http::request_at(
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
        crate::http::Error::Invalid(message) => tracker_error(message).into(),
    })?;
    if response.status != 200 {
        return Err(tracker_error(format!(
            "tracker returned HTTP status {}",
            response.status_text
        ))
        .into());
    }
    Ok(TrackerResponse::from_bencode(&response.body)?)
}

fn tracker_error(message: impl Into<String>) -> crate::BackendError {
    crate::BackendError::Tracker(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
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
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
                failure.as_text()?
            )));
        }
        let count = |key: &[u8]| {
            dictionary
                .get(key)
                .and_then(|value| value.as_integer().ok())
        };
        let (seeders, leechers) = (count(b"complete"), count(b"incomplete"));
        let interval = dictionary
            .get(b"interval".as_slice())
            .map(Value::as_integer)
            .transpose()?;
        let min_interval = dictionary
            .get(b"min interval".as_slice())
            .map(Value::as_integer)
            .transpose()?;
        let warning = dictionary
            .get(b"warning message".as_slice())
            .map(Value::as_text)
            .transpose()?;

        let mut peers = Vec::new();
        let mut peers_field_present = false;
        if let Some(value) = dictionary.get(b"peers".as_slice()) {
            peers_field_present = true;
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
            peers_field_present = true;
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
        if !peers_field_present {
            return Err(invalid("tracker response does not contain peers"));
        }
        Ok(Self {
            peers,
            interval,
            min_interval,
            warning,
            seeders,
            leechers,
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
