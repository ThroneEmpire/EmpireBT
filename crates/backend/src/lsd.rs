//! Local Peer Discovery (BEP 14): finding peers on the local network.
//!
//! Clients multicast short HTTP-like messages naming the torrents they are
//! in and the port they accept peers on, to group 239.192.152.143, port
//! 6771. Hearing one for a torrent we have, we connect to the sender: a
//! download hands the address to its dialers, a seed dials it directly.
//! No tracker or DHT is involved, and transfers run at local speed.
//!
//! Every interface joins the group, not just the default route's: with a
//! VPN up, the default route is the tunnel, which carries no multicast.
//! IPv6 interfaces also join the IPv6 group, ff15::efc0:988f; peers found
//! there usually have link-local addresses, which keep the interface they
//! were heard on. The sockets share port 6771 with other clients on the
//! same machine.
//!
//! Sending follows the BEP's limit of one announce a minute. Each packs up
//! to 25 info hashes: downloads first (they need peers), seeds with what
//! room is left. Seeds rarely need announcing anyway, since a downloader's
//! announce makes us dial it. Private torrents (BEP 27) never take part.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::discovery::OnPeers;
use crate::{BackendError, Runtime};

/// The BEP 14 multicast groups and port, for IPv4 and IPv6.
const GROUP: Ipv4Addr = Ipv4Addr::new(239, 192, 152, 143);
const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff15, 0, 0, 0, 0, 0, 0xefc0, 0x988f);
const PORT: u16 = 6771;
/// Largest packet sent, as the BEP asks, to avoid fragmentation.
const MAX_PACKET: usize = 1400;
/// Info hashes per announce; the header lines around them leave room for
/// this many 52-byte `Infohash` lines within [`MAX_PACKET`].
const MAX_HASHES: usize = 25;
/// At most one announce a minute, to avoid multicast storms.
const ANNOUNCE_GAP: Duration = Duration::from_secs(60);
/// Each torrent is announced again this often, room permitting.
const REANNOUNCE: Duration = Duration::from_secs(5 * 60);
/// Interfaces come and go (Wi-Fi changes, a VPN connects); they are
/// listed again this often.
const RESCAN: Duration = Duration::from_secs(2 * 60);
/// How long the loop waits for a packet before checking its timers.
const POLL: Duration = Duration::from_millis(200);
/// The same peer for the same torrent is handed over once in this window:
/// a packet arrives once per joined interface, and peers re-announce.
const REPEAT_WINDOW: Duration = Duration::from_secs(60);
/// Packets accepted from one client (address and announced port) per
/// [`SOURCE_WINDOW`], so one noisy client cannot flood us. A well-behaved
/// one sends one a minute per interface.
const SOURCE_LIMIT: u32 = 20;
const SOURCE_WINDOW: Duration = Duration::from_secs(10);
/// Bound on remembered peers and sources before old entries are dropped.
const MAX_REMEMBERED: usize = 4096;

/// A running local peer discovery service, shared by a client's
/// downloads and its seeding runtime.
pub struct Lsd {
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for Lsd {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Lsd")
            .field("port", &self.shared.port)
            .finish_non_exhaustive()
    }
}

struct Shared {
    /// The port we accept peers on, as announced.
    port: u16,
    /// Interfaces announces may use.
    binding: crate::Binding,
    /// Sent with every announce, so ours are recognized when multicast
    /// loops them back.
    cookie: String,
    downloads: Mutex<HashMap<u64, Registration>>,
    next_id: AtomicU64,
    /// Seeds of this runtime are announced and dial peers that ask.
    runtime: Mutex<Option<Arc<Runtime>>>,
    shutdown: AtomicBool,
    /// Peers handed over so far, for display.
    found: AtomicU64,
    /// IPv4 interfaces currently in the group.
    interfaces: Mutex<Vec<Ipv4Addr>>,
    /// IPv6 interfaces in the IPv6 group, by index, with their addresses.
    interfaces_v6: Mutex<Vec<(u32, Vec<Ipv6Addr>)>>,
}

struct Registration {
    hashes: Vec<[u8; 20]>,
    on_peers: OnPeers,
}

/// A download taking part in local peer discovery; dropping it leaves.
pub(crate) struct LsdHandle {
    shared: Weak<Shared>,
    id: u64,
}

impl Drop for LsdHandle {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            lock(&shared.downloads).remove(&self.id);
        }
    }
}

impl Lsd {
    /// Starts announcing `port`, the port peers reach us on, and listening
    /// for other clients on the local network.
    pub fn start(port: u16) -> Result<Arc<Self>, BackendError> {
        Self::start_on(crate::Binding::any(), port)
    }

    /// Like [`Self::start`], announcing and listening on one network
    /// interface only.
    pub fn start_on(binding: crate::Binding, port: u16) -> Result<Arc<Self>, BackendError> {
        let socket = open_socket().map_err(|error| {
            BackendError::Io(format!("local peer discovery (UDP {PORT}): {error}"))
        })?;
        // IPv6 is a bonus: without it, IPv4 discovery still works.
        let socket_v6 = open_socket_v6().ok();
        let shared = Arc::new(Shared {
            port,
            binding,
            cookie: hex(&crate::dht::random_bytes::<8>()),
            downloads: Mutex::default(),
            next_id: AtomicU64::new(0),
            runtime: Mutex::default(),
            shutdown: AtomicBool::new(false),
            found: AtomicU64::new(0),
            interfaces: Mutex::default(),
            interfaces_v6: Mutex::default(),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("lsd".to_owned())
                .spawn(move || run(&shared, &socket, socket_v6.as_ref()))
                .map_err(|error| BackendError::Io(error.to_string()))?
        };
        Ok(Arc::new(Self {
            shared,
            thread: Mutex::new(Some(thread)),
        }))
    }

    /// Announces this runtime's public seeds, and dials local peers asking
    /// for them.
    pub fn set_runtime(&self, runtime: Option<Arc<Runtime>>) {
        *lock(&self.shared.runtime) = runtime;
    }

    /// Peers found on the local network so far.
    pub fn peers_found(&self) -> u64 {
        self.shared.found.load(Ordering::Relaxed)
    }

    /// How many network interfaces are listening for and sending
    /// announces.
    pub fn interface_count(&self) -> usize {
        lock(&self.shared.interfaces).len() + lock(&self.shared.interfaces_v6).len()
    }

    /// Announces a download's `hashes` and hands it the local peers that
    /// announce them, until the handle is dropped.
    pub(crate) fn join(&self, hashes: &[[u8; 20]], on_peers: OnPeers) -> LsdHandle {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.shared.downloads).insert(
            id,
            Registration {
                hashes: hashes.to_vec(),
                on_peers,
            },
        );
        LsdHandle {
            shared: Arc::downgrade(&self.shared),
            id,
        }
    }

    /// Stops announcing and listening.
    pub fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Lsd {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Binds the BEP 14 port, shared with other clients on this machine.
fn open_socket() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(all(
        unix,
        not(any(target_os = "solaris", target_os = "illumos", target_os = "cygwin"))
    ))]
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    // Messages stay on the local network; our own come back to us (and to
    // other clients on this machine) and are recognized by their cookie.
    socket.set_multicast_ttl_v4(1)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_read_timeout(Some(POLL))?;
    Ok(socket.into())
}

/// Binds the BEP 14 port for IPv6, shared like the IPv4 one.
fn open_socket_v6() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(true)?;
    socket.set_reuse_address(true)?;
    #[cfg(all(
        unix,
        not(any(target_os = "solaris", target_os = "illumos", target_os = "cygwin"))
    ))]
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, PORT)).into())?;
    socket.set_multicast_hops_v6(1)?;
    socket.set_multicast_loop_v6(true)?;
    socket.set_read_timeout(Some(POLL / 2))?;
    Ok(socket.into())
}

/// IPv6 interfaces that can reach a local network: index, name, and their
/// IPv6 addresses (link-local ones included, as LSD uses them).
fn local_interfaces_v6() -> Vec<(u32, String, Vec<Ipv6Addr>)> {
    let mut found: Vec<(u32, String, Vec<Ipv6Addr>)> = Vec::new();
    for interface in if_addrs::get_if_addrs().unwrap_or_default() {
        let (IpAddr::V6(ip), Some(index)) = (interface.ip(), interface.index) else {
            continue;
        };
        if interface.is_loopback()
            || matches!(
                interface.oper_status,
                if_addrs::IfOperStatus::Down | if_addrs::IfOperStatus::NotPresent
            )
        {
            continue;
        }
        match found.iter_mut().find(|(known, _, _)| *known == index) {
            Some((_, _, addresses)) => addresses.push(ip),
            None => found.push((index, interface.name, vec![ip])),
        }
    }
    found
}

/// IPv4 addresses of the interfaces that can reach a local network.
fn local_interfaces() -> Vec<Ipv4Addr> {
    let mut found: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| {
            !interface.is_loopback()
                && !interface.is_link_local()
                && !matches!(
                    interface.oper_status,
                    if_addrs::IfOperStatus::Down | if_addrs::IfOperStatus::NotPresent
                )
        })
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

fn run(shared: &Shared, socket: &UdpSocket, socket_v6: Option<&UdpSocket>) {
    let mut schedule = Schedule::default();
    let mut inbox = Inbox::default();
    let mut seeds = Seeds::default();
    let mut last_scan: Option<Instant> = None;
    let mut buffer = [0; 2048];
    while !shared.shutdown.load(Ordering::Acquire) {
        let now = Instant::now();
        if last_scan.is_none_or(|scanned| now - scanned >= RESCAN) {
            last_scan = Some(now);
            rejoin(shared, socket);
            if let Some(socket_v6) = socket_v6 {
                rejoin_v6(shared, socket_v6);
            }
        }
        let runtime = lock(&shared.runtime).clone();
        seeds.refresh(runtime.as_ref());
        let downloads: Vec<[u8; 20]> = lock(&shared.downloads)
            .values()
            .flat_map(|registration| registration.hashes.iter().copied())
            .collect();
        let picked = schedule.pick(now, &downloads, &seeds.hashes);
        if !picked.is_empty() {
            let packet = encode(
                shared.port,
                &shared.cookie,
                &picked,
                &format!("{GROUP}:{PORT}"),
            );
            let socket = socket2::SockRef::from(socket);
            for interface in lock(&shared.interfaces).iter() {
                if socket.set_multicast_if_v4(interface).is_ok() {
                    let _ = socket.send_to(&packet, &SocketAddr::from((GROUP, PORT)).into());
                }
            }
            if let Some(socket_v6) = socket_v6 {
                let packet = encode(
                    shared.port,
                    &shared.cookie,
                    &picked,
                    &format!("[{GROUP_V6}]:{PORT}"),
                );
                let socket = socket2::SockRef::from(socket_v6);
                for (index, _) in lock(&shared.interfaces_v6).iter() {
                    let group = SocketAddrV6::new(GROUP_V6, PORT, 0, *index);
                    if socket.set_multicast_if_v6(*index).is_ok() {
                        let _ = socket.send_to(&packet, &SocketAddr::V6(group).into());
                    }
                }
            }
        }
        // Timeouts, and errors we can only wait out, end each receive.
        for socket in std::iter::once(socket).chain(socket_v6) {
            if let Ok((length, source)) = socket.recv_from(&mut buffer) {
                handle(
                    shared,
                    &buffer[..length],
                    source,
                    &mut inbox,
                    &seeds,
                    runtime.as_ref(),
                );
            }
        }
    }
}

/// Joins the IPv6 group on interfaces that appeared, and forgets those
/// gone.
fn rejoin_v6(shared: &Shared, socket: &UdpSocket) {
    let current: Vec<(u32, Vec<Ipv6Addr>)> = local_interfaces_v6()
        .into_iter()
        .filter(|(_, name, addresses)| {
            let addresses: Vec<IpAddr> = addresses.iter().map(|ip| IpAddr::V6(*ip)).collect();
            shared.binding.allows_interface(name, &addresses)
        })
        .map(|(index, _, addresses)| (index, addresses))
        .collect();
    let mut joined = lock(&shared.interfaces_v6);
    for (index, _) in joined.iter() {
        if !current.iter().any(|(known, _)| known == index) {
            let _ = socket.leave_multicast_v6(&GROUP_V6, *index);
        }
    }
    joined.retain(|(index, _)| current.iter().any(|(known, _)| known == index));
    for (index, addresses) in current {
        match joined.iter_mut().find(|(known, _)| *known == index) {
            Some((_, known)) => *known = addresses,
            None => {
                if socket.join_multicast_v6(&GROUP_V6, index).is_ok() {
                    joined.push((index, addresses));
                }
            }
        }
    }
}

/// Joins the group on interfaces that appeared, and forgets those gone.
fn rejoin(shared: &Shared, socket: &UdpSocket) {
    let current: Vec<Ipv4Addr> = local_interfaces()
        .into_iter()
        .filter(|ip| shared.binding.permits(IpAddr::V4(*ip)))
        .collect();
    let mut joined = lock(&shared.interfaces);
    for interface in joined.iter() {
        if !current.contains(interface) {
            let _ = socket.leave_multicast_v4(&GROUP, interface);
        }
    }
    joined.retain(|interface| current.contains(interface));
    for interface in current {
        if !joined.contains(&interface) && socket.join_multicast_v4(&GROUP, &interface).is_ok() {
            joined.push(interface);
        }
    }
}

/// Hands the peer in an announce to the downloads and seeds that want it.
fn handle(
    shared: &Shared,
    packet: &[u8],
    source: SocketAddr,
    inbox: &mut Inbox,
    seeds: &Seeds,
    runtime: Option<&Arc<Runtime>>,
) {
    let Some(announce) = parse(packet) else {
        return;
    };
    if announce.cookie.as_deref() == Some(shared.cookie.as_str()) {
        return; // our own, looped back
    }
    // A client without cookies on one of our addresses with our port is us.
    let ours = match source.ip() {
        IpAddr::V4(ip) => lock(&shared.interfaces).contains(&ip),
        IpAddr::V6(ip) => lock(&shared.interfaces_v6)
            .iter()
            .any(|(_, addresses)| addresses.contains(&ip)),
    };
    if announce.port == shared.port && ours {
        return;
    }
    let now = Instant::now();
    // A link-local address only works with the interface it was heard on.
    let peer = match source {
        SocketAddr::V6(source) => SocketAddr::V6(SocketAddrV6::new(
            *source.ip(),
            announce.port,
            0,
            source.scope_id(),
        )),
        SocketAddr::V4(source) => SocketAddr::from((*source.ip(), announce.port)),
    };
    if !inbox.accept(peer, now) {
        return;
    }
    for hash in announce.hashes {
        if inbox.repeated(hash, peer, now) {
            continue;
        }
        let mut handed = false;
        for registration in lock(&shared.downloads).values() {
            if registration.hashes.contains(&hash) {
                (registration.on_peers)(&[peer]);
                handed = true;
            }
        }
        if !handed
            && seeds.set.contains(&hash)
            && let Some(runtime) = runtime
        {
            handed = runtime.dial(peer, &hash);
        }
        // Only what was handed over counts as seen: an announce that came
        // just before its download started is taken when repeated.
        if handed {
            inbox.handed(hash, peer, now);
            shared.found.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The public seeds of the runtime, refreshed only when it changes.
#[derive(Default)]
struct Seeds {
    tracked: Option<(Arc<Runtime>, u64)>,
    hashes: Vec<[u8; 20]>,
    set: HashSet<[u8; 20]>,
}

impl Seeds {
    fn refresh(&mut self, runtime: Option<&Arc<Runtime>>) {
        let unchanged = match (runtime, &self.tracked) {
            (Some(current), Some((previous, revision))) => {
                Arc::ptr_eq(current, previous) && current.revision() == *revision
            }
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        self.hashes = runtime
            .map(|runtime| runtime.seeding_torrents())
            .unwrap_or_default()
            .iter()
            .filter(|info| !info.is_private())
            .flat_map(|info| info.swarm_hashes())
            .collect();
        self.set = self.hashes.iter().copied().collect();
        self.tracked = runtime.map(|runtime| (Arc::clone(runtime), runtime.revision()));
    }
}

/// Which torrents go in each announce, and when one may be sent.
#[derive(Debug, Default)]
struct Schedule {
    last_sent: Option<Instant>,
    /// When each torrent is next due; absent until first announced.
    next: HashMap<[u8; 20], Instant>,
}

impl Schedule {
    /// The hashes for an announce to send now, if one may be sent: at most
    /// one a minute, downloads before seeds, and within each, torrents never
    /// announced first, then those waiting longest. Empty when nothing is
    /// due.
    fn pick(&mut self, now: Instant, downloads: &[[u8; 20]], seeds: &[[u8; 20]]) -> Vec<[u8; 20]> {
        if self
            .last_sent
            .is_some_and(|last| now.saturating_duration_since(last) < ANNOUNCE_GAP)
        {
            return Vec::new();
        }
        let active: HashSet<&[u8; 20]> = downloads.iter().chain(seeds).collect();
        self.next.retain(|hash, _| active.contains(hash));
        let downloading: HashSet<&[u8; 20]> = downloads.iter().collect();
        let mut due: Vec<(bool, Option<Instant>, [u8; 20])> = active
            .into_iter()
            .map(|hash| {
                (
                    !downloading.contains(hash),
                    self.next.get(hash).copied(),
                    *hash,
                )
            })
            .filter(|(_, next, _)| next.is_none_or(|next| next <= now))
            .collect();
        // Downloads (`false`) first; never announced (`None`) first.
        due.sort_unstable();
        let picked: Vec<[u8; 20]> = due
            .into_iter()
            .take(MAX_HASHES)
            .map(|(_, _, hash)| hash)
            .collect();
        for hash in &picked {
            self.next.insert(*hash, now + REANNOUNCE);
        }
        if !picked.is_empty() {
            self.last_sent = Some(now);
        }
        picked
    }
}

/// Filters incoming announces: per-source rate limits and repeats.
#[derive(Debug, Default)]
struct Inbox {
    sources: HashMap<SocketAddr, (Instant, u32)>,
    recent: HashMap<([u8; 20], SocketAddr), Instant>,
}

impl Inbox {
    /// Whether another packet from the client at `source` is accepted now.
    fn accept(&mut self, source: SocketAddr, now: Instant) -> bool {
        if self.sources.len() >= MAX_REMEMBERED {
            self.sources
                .retain(|_, (since, _)| now.saturating_duration_since(*since) < SOURCE_WINDOW);
        }
        let (since, count) = self.sources.entry(source).or_insert((now, 0));
        if now.saturating_duration_since(*since) >= SOURCE_WINDOW {
            *since = now;
            *count = 0;
        }
        *count += 1;
        *count <= SOURCE_LIMIT
    }

    /// Whether `peer` announcing `hash` was handed over just now.
    fn repeated(&self, hash: [u8; 20], peer: SocketAddr, now: Instant) -> bool {
        self.recent
            .get(&(hash, peer))
            .is_some_and(|at| now.saturating_duration_since(*at) < REPEAT_WINDOW)
    }

    fn handed(&mut self, hash: [u8; 20], peer: SocketAddr, now: Instant) {
        if self.recent.len() >= MAX_REMEMBERED {
            self.recent
                .retain(|_, at| now.saturating_duration_since(*at) < REPEAT_WINDOW);
        }
        self.recent.insert((hash, peer), now);
    }
}

/// A parsed announce.
#[derive(Debug, PartialEq, Eq)]
struct Announce {
    port: u16,
    hashes: Vec<[u8; 20]>,
    cookie: Option<String>,
}

/// Builds an announce. The BEP's example ends with an extra blank line,
/// as libtorrent's announces do.
fn encode(port: u16, cookie: &str, hashes: &[[u8; 20]], host: &str) -> Vec<u8> {
    let mut text = format!("BT-SEARCH * HTTP/1.1\r\nHost: {host}\r\nPort: {port}\r\n");
    for hash in hashes {
        text.push_str(&format!("Infohash: {}\r\n", hex(hash)));
    }
    text.push_str(&format!("cookie: {cookie}\r\n\r\n\r\n"));
    text.into_bytes()
}

/// Reads an announce, tolerating bare line feeds, any header case, and
/// unknown headers. Info hashes that are not 40 hex digits are skipped.
fn parse(packet: &[u8]) -> Option<Announce> {
    if packet.len() > 2 * MAX_PACKET {
        return None;
    }
    let text = std::str::from_utf8(packet).ok()?;
    let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
    if lines.next()?.trim() != "BT-SEARCH * HTTP/1.1" {
        return None;
    }
    let (mut port, mut hashes, mut cookie) = (None, Vec::new(), None);
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "port" => port = Some(value.parse::<u16>().ok().filter(|port| *port != 0)?),
            "infohash" => hashes.extend(unhex(value)),
            "cookie" => cookie = Some(value.to_owned()),
            _ => {}
        }
    }
    (!hashes.is_empty()).then_some(Announce {
        port: port?,
        hashes,
        cookie,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 20]> {
    if text.len() != 40 || !text.is_ascii() {
        return None;
    }
    let mut hash = [0; 20];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn hash(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    #[test]
    fn announces_round_trip() {
        let packet = encode(51413, "c00k1e", &[hash(1), hash(2)], "239.192.152.143:6771");
        let text = String::from_utf8(packet.clone()).unwrap();
        assert!(text.starts_with(
            "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 51413\r\nInfohash: 0101"
        ));
        assert!(text.ends_with("cookie: c00k1e\r\n\r\n\r\n"));
        assert_eq!(
            parse(&packet),
            Some(Announce {
                port: 51413,
                hashes: vec![hash(1), hash(2)],
                cookie: Some("c00k1e".to_owned()),
            })
        );
    }

    #[test]
    fn a_full_announce_fits_in_one_packet() {
        let hashes: Vec<[u8; 20]> = (0..MAX_HASHES as u8).map(hash).collect();
        // The IPv6 group makes the longer Host line.
        let host = format!("[{GROUP_V6}]:{PORT}");
        let packet = encode(u16::MAX, &hex(&[0xff; 8]), &hashes, &host);
        assert!(packet.len() <= MAX_PACKET, "{} bytes", packet.len());
        assert_eq!(parse(&packet).unwrap().hashes.len(), MAX_HASHES);
    }

    #[test]
    fn reads_announces_from_other_clients() {
        // Bare line feeds, other header cases, unknown headers, upper-case
        // hex, no cookie, and one bad hash among good ones.
        let packet = format!(
            "BT-SEARCH * HTTP/1.1\nHOST: 239.192.152.143:6771\nport: 6881\nX-Extra: yes\n\
             infohash: {}\nInfohash: not-a-hash\n\n",
            "AB".repeat(20)
        );
        assert_eq!(
            parse(packet.as_bytes()),
            Some(Announce {
                port: 6881,
                hashes: vec![[0xab; 20]],
                cookie: None,
            })
        );
    }

    #[test]
    fn rejects_malformed_announces() {
        let good = format!("Infohash: {}\r\n", "ab".repeat(20));
        for packet in [
            format!("M-SEARCH * HTTP/1.1\r\nPort: 6881\r\n{good}\r\n"),
            format!("BT-SEARCH * HTTP/1.1\r\n{good}\r\n"),
            format!("BT-SEARCH * HTTP/1.1\r\nPort: 0\r\n{good}\r\n"),
            format!("BT-SEARCH * HTTP/1.1\r\nPort: 70000\r\n{good}\r\n"),
            "BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\nInfohash: 12\r\n\r\n".to_owned(),
        ] {
            assert_eq!(parse(packet.as_bytes()), None, "{packet:?}");
        }
        assert_eq!(parse(&[0xff, 0xfe]), None);
    }

    #[test]
    fn sends_at_most_one_announce_a_minute_downloads_first() {
        let mut schedule = Schedule::default();
        let start = Instant::now();
        let seeds: Vec<[u8; 20]> = (100..140).map(hash).collect();
        // The first announce goes at once: the download, then seeds.
        let first = schedule.pick(start, &[hash(1)], &seeds);
        assert_eq!(first.len(), MAX_HASHES);
        assert_eq!(first[0], hash(1));
        // Nothing more within the minute, even for a new download.
        assert!(
            schedule
                .pick(start + Duration::from_secs(59), &[hash(1), hash(2)], &seeds)
                .is_empty()
        );
        // Next minute: the new download, then the seeds not yet announced.
        let minute = start + Duration::from_secs(60);
        let second = schedule.pick(minute, &[hash(1), hash(2)], &seeds);
        assert_eq!(second[0], hash(2));
        assert_eq!(second.len(), 1 + seeds.len() - (MAX_HASHES - 1));
        assert!(!second.contains(&hash(1)), "announced a minute ago");
        // Nothing is due for a while; then everything comes round again,
        // downloads first.
        assert!(
            schedule
                .pick(
                    start + Duration::from_secs(120),
                    &[hash(1), hash(2)],
                    &seeds
                )
                .is_empty()
        );
        let again = schedule.pick(start + REANNOUNCE, &[hash(1), hash(2)], &seeds);
        assert_eq!(again[0], hash(1));
    }

    #[test]
    fn forgets_torrents_that_left() {
        let mut schedule = Schedule::default();
        let start = Instant::now();
        schedule.pick(start, &[hash(1)], &[hash(2)]);
        schedule.pick(start + REANNOUNCE, &[], &[hash(2)]);
        assert!(!schedule.next.contains_key(&hash(1)));
    }

    #[test]
    fn limits_noisy_sources_and_repeats() {
        let mut inbox = Inbox::default();
        let now = Instant::now();
        let source = SocketAddr::from((Ipv4Addr::new(192, 168, 1, 20), 6881));
        for _ in 0..SOURCE_LIMIT {
            assert!(inbox.accept(source, now));
        }
        assert!(!inbox.accept(source, now));
        // Another client, even on the same machine, has its own allowance.
        assert!(inbox.accept(
            SocketAddr::from((Ipv4Addr::new(192, 168, 1, 20), 6882)),
            now
        ));
        assert!(inbox.accept(source, now + SOURCE_WINDOW));

        let peer = SocketAddr::from((Ipv4Addr::new(192, 168, 1, 20), 6881));
        // Not handed over yet (no download wanted it): not a repeat.
        assert!(!inbox.repeated(hash(1), peer, now));
        inbox.handed(hash(1), peer, now);
        assert!(inbox.repeated(hash(1), peer, now + Duration::from_secs(1)));
        assert!(!inbox.repeated(hash(2), peer, now));
        assert!(!inbox.repeated(hash(1), peer, now + REPEAT_WINDOW));
    }

    /// A running service on this machine, or `None` where multicast is not
    /// available (no network interface, or a sandbox without it).
    pub(crate) fn running(port: u16) -> Option<Arc<Lsd>> {
        let lsd = Lsd::start(port).ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while lsd.interface_count() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        (lsd.interface_count() > 0).then_some(lsd)
    }

    /// Collects the peers a registration is handed.
    fn collector() -> (OnPeers, Arc<Mutex<Vec<SocketAddr>>>) {
        let found = Arc::new(Mutex::new(Vec::new()));
        let on_peers: OnPeers = {
            let found = Arc::clone(&found);
            Arc::new(move |peers: &[SocketAddr]| lock(&found).extend_from_slice(peers))
        };
        (on_peers, found)
    }

    fn wait_for(found: &Mutex<Vec<SocketAddr>>, port: u16) -> Option<SocketAddr> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(peer) = lock(found).iter().find(|peer| peer.port() == port) {
                return Some(*peer);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn two_clients_on_one_network_find_each_other() {
        let (Some(first), Some(second)) = (running(40001), running(40002)) else {
            eprintln!("skipped: multicast is not available here");
            return;
        };
        let torrent = crate::dht::random_bytes::<20>();
        let (on_first, found_by_first) = collector();
        let (on_second, found_by_second) = collector();
        let _first = first.join(&[torrent], on_first);
        let _second = second.join(&[torrent], on_second);
        assert!(
            wait_for(&found_by_first, 40002).is_some(),
            "first never heard second"
        );
        assert!(
            wait_for(&found_by_second, 40001).is_some(),
            "second never heard first"
        );
        // Neither hands a download its own announces.
        assert!(
            lock(&found_by_first)
                .iter()
                .all(|peer| peer.port() != 40001)
        );
        assert!(first.peers_found() >= 1);
    }

    /// Multicasts an announce as another client would.
    pub(crate) fn announce_as_peer(port: u16, hashes: &[[u8; 20]]) {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.set_multicast_loop_v4(true).unwrap();
        let packet = encode(port, "other-client", hashes, &format!("{GROUP}:{PORT}"));
        for interface in local_interfaces() {
            let socket = socket2::SockRef::from(&socket);
            if socket.set_multicast_if_v4(&interface).is_ok() {
                let _ = socket.send_to(&packet, &SocketAddr::from((GROUP, PORT)).into());
            }
        }
    }

    /// Multicasts an announce over IPv6 as another client would; false
    /// when no interface could send one.
    fn announce_as_peer_v6(port: u16, hashes: &[[u8; 20]]) -> bool {
        let Ok(socket) = UdpSocket::bind("[::]:0") else {
            return false;
        };
        let _ = socket.set_multicast_loop_v6(true);
        let packet = encode(
            port,
            "other-client",
            hashes,
            &format!("[{GROUP_V6}]:{PORT}"),
        );
        let socket = socket2::SockRef::from(&socket);
        let mut sent = false;
        for (index, _, _) in local_interfaces_v6() {
            let group = SocketAddrV6::new(GROUP_V6, PORT, 0, index);
            if socket.set_multicast_if_v6(index).is_ok()
                && socket
                    .send_to(&packet, &SocketAddr::V6(group).into())
                    .is_ok()
            {
                sent = true;
            }
        }
        sent
    }

    #[test]
    fn finds_peers_announcing_over_ipv6() {
        let Some(lsd) = running(40006) else {
            eprintln!("skipped: multicast is not available here");
            return;
        };
        if lock(&lsd.shared.interfaces_v6).is_empty() {
            eprintln!("skipped: no IPv6 multicast interface here");
            return;
        }
        let torrent = crate::dht::random_bytes();
        let (on_peers, found) = collector();
        let _download = lsd.join(&[torrent], on_peers);
        let deadline = Instant::now() + Duration::from_secs(5);
        let peer = loop {
            assert!(
                announce_as_peer_v6(40007, &[torrent]),
                "no IPv6 interface sent"
            );
            std::thread::sleep(Duration::from_millis(200));
            if let Some(peer) = lock(&found).iter().find(|peer| peer.is_ipv6()) {
                break *peer;
            }
            assert!(Instant::now() < deadline, "never heard the IPv6 announce");
        };
        assert_eq!(peer.port(), 40007);
        // A link-local peer keeps the interface it was heard on.
        if let SocketAddr::V6(peer) = peer
            && crate::net::is_link_local(peer.ip())
        {
            assert_ne!(peer.scope_id(), 0);
        }
    }

    #[test]
    fn ignores_announces_for_torrents_we_do_not_have() {
        let Some(lsd) = running(40003) else {
            eprintln!("skipped: multicast is not available here");
            return;
        };
        let (wanted, unknown) = (crate::dht::random_bytes(), crate::dht::random_bytes());
        let (on_peers, found) = collector();
        let _download = lsd.join(&[wanted], on_peers);
        announce_as_peer(40004, &[unknown]);
        announce_as_peer(40005, &[wanted]);
        assert!(wait_for(&found, 40005).is_some());
        assert!(lock(&found).iter().all(|peer| peer.port() != 40004));
    }
}
