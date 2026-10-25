//! Binding every socket to one network interface, as qBittorrent's
//! "network interface" option does: usually a VPN's, so that nothing leaves
//! over another connection if the VPN drops.
//!
//! The interface is named (`wg0`, `tun0`) or given by address, and looked
//! up each time a socket is made, so a VPN that reconnects is picked up
//! again. A name binds the interface's IPv4 address and its IPv6 one
//! (link-local addresses aside), whichever it has. While it has neither,
//! sockets fail instead of falling back to another interface: no
//! connection is better than one outside the VPN.
//!
//! Outgoing connections are bound to the interface's address and, where
//! the system allows (Linux, macOS), to the device itself, so routing
//! cannot send them out another way. Long-lived listening sockets (the
//! peer listener, the DHT and uTP) are bound to the address only: a
//! socket tied to a device stays dead after a VPN recreates its interface,
//! while one bound to the address works again once it comes back.
//!
//! Also here: listening sockets for each address family, and the canonical
//! form of peer addresses.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

/// Which network interface sockets use: any (the default), or one chosen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Binding {
    /// An interface name or an address; `None` for any interface.
    interface: Option<Arc<str>>,
}

/// A chosen interface's addresses, at most one per family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Addresses {
    v4: Option<Ipv4Addr>,
    v6: Option<Ipv6Addr>,
}

impl Binding {
    /// Sockets use whatever interface the system routes through.
    pub fn any() -> Self {
        Self::default()
    }

    /// Sockets use only this interface, by name (`wg0`) or address. An
    /// empty name means any interface.
    pub fn interface(interface: impl AsRef<str>) -> Self {
        let interface = interface.as_ref().trim();
        Self {
            interface: (!interface.is_empty()).then(|| Arc::from(interface)),
        }
    }

    /// The chosen interface, if any.
    pub fn name(&self) -> Option<&str> {
        self.interface.as_deref()
    }

    /// The chosen interface's addresses, or `None` for any interface. An
    /// error while it is missing or has no usable address. An interface
    /// with several addresses of a family is bound by its first.
    fn resolve(&self) -> Result<Option<Addresses>, crate::BackendError> {
        let Some(interface) = self.name() else {
            return Ok(None);
        };
        let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
        let present = |ip: IpAddr| interfaces.iter().any(|candidate| candidate.ip() == ip);
        let addresses = match interface.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) if present(ip.into()) => Addresses {
                v4: Some(ip),
                v6: None,
            },
            Ok(IpAddr::V6(ip)) if present(ip.into()) => Addresses {
                v4: None,
                v6: Some(ip),
            },
            Ok(_) => return Err(unavailable(interface)),
            Err(_) => {
                let own = || {
                    interfaces
                        .iter()
                        .filter(|candidate| candidate.name == interface)
                        .map(|candidate| candidate.ip())
                };
                Addresses {
                    v4: own().find_map(|ip| match ip {
                        IpAddr::V4(ip) => Some(ip),
                        IpAddr::V6(_) => None,
                    }),
                    // Link-local addresses need a scope and reach only the
                    // local link; they are no use for peers.
                    v6: own().find_map(|ip| match ip {
                        IpAddr::V6(ip) if !is_link_local(&ip) => Some(ip),
                        _ => None,
                    }),
                }
            }
        };
        if addresses.v4.is_none() && addresses.v6.is_none() {
            return Err(unavailable(interface));
        }
        Ok(Some(addresses))
    }

    /// Whether the chosen interface is up: always for any interface.
    pub(crate) fn check(&self) -> Result<(), crate::BackendError> {
        self.resolve().map(|_| ())
    }

    /// Whether sockets can be made now: always for any interface, and
    /// while the chosen one is up with an address.
    pub fn is_up(&self) -> bool {
        self.check().is_ok()
    }

    /// Whether traffic may use the interface with this address.
    pub(crate) fn permits(&self, ip: IpAddr) -> bool {
        match self.resolve() {
            Ok(None) => true,
            Ok(Some(addresses)) => match ip {
                IpAddr::V4(ip) => addresses.v4 == Some(ip),
                IpAddr::V6(ip) => addresses.v6 == Some(ip),
            },
            Err(_) => false,
        }
    }

    /// Whether traffic may use the interface `name`, which has `addresses`:
    /// any for any interface; by name, the interface of that name; by
    /// address, the interface that has it.
    pub(crate) fn allows_interface(&self, name: &str, addresses: &[IpAddr]) -> bool {
        match self.name() {
            None => true,
            Some(chosen) => match chosen.parse::<IpAddr>() {
                Ok(address) => addresses.contains(&address),
                Err(_) => chosen == name,
            },
        }
    }

    /// Local addresses for listening sockets on `port`, IPv4 first: both
    /// families on every interface, or the chosen interface's addresses.
    pub fn listen_addresses(&self, port: u16) -> Result<Vec<SocketAddr>, crate::BackendError> {
        Ok(match self.resolve()? {
            None => vec![
                SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)),
                SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
            ],
            Some(addresses) => addresses
                .v4
                .map(IpAddr::V4)
                .into_iter()
                .chain(addresses.v6.map(IpAddr::V6))
                .map(|ip| SocketAddr::new(ip, port))
                .collect(),
        })
    }

    /// Connects to `address` over TCP from the chosen interface.
    pub(crate) fn connect_tcp(
        &self,
        address: SocketAddr,
        timeout: Duration,
    ) -> std::io::Result<TcpStream> {
        let Some(socket) = self.outgoing(address, socket2::Type::STREAM)? else {
            return TcpStream::connect_timeout(&address, timeout);
        };
        socket.connect_timeout(&address.into(), timeout)?;
        Ok(socket.into())
    }

    /// A UDP socket on the chosen interface for talking to `address`.
    pub(crate) fn udp_for(&self, address: SocketAddr) -> std::io::Result<UdpSocket> {
        match self.outgoing(address, socket2::Type::DGRAM)? {
            Some(socket) => Ok(socket.into()),
            None if address.is_ipv6() => UdpSocket::bind("[::]:0"),
            None => UdpSocket::bind("0.0.0.0:0"),
        }
    }

    /// Whether `address` can be reached from the chosen interface: any
    /// address for any interface, else one of a family it has.
    pub(crate) fn reaches(&self, address: &SocketAddr) -> bool {
        match self.resolve() {
            Ok(None) => true,
            Ok(Some(addresses)) => match address {
                SocketAddr::V4(_) => addresses.v4.is_some(),
                SocketAddr::V6(_) => addresses.v6.is_some(),
            },
            Err(_) => false,
        }
    }

    /// An unconnected socket bound to the chosen interface, or `None` for
    /// any interface.
    fn outgoing(
        &self,
        address: SocketAddr,
        kind: socket2::Type,
    ) -> std::io::Result<Option<socket2::Socket>> {
        let Some(interface) = self.name() else {
            return Ok(None);
        };
        let Ok(Some(addresses)) = self.resolve() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                unavailable_message(interface),
            ));
        };
        let local = match address {
            SocketAddr::V4(_) => addresses.v4.map(IpAddr::V4),
            SocketAddr::V6(_) => addresses.v6.map(IpAddr::V6),
        };
        let Some(local) = local else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "network interface {interface} has no {} address for {address}",
                    if address.is_ipv6() { "IPv6" } else { "IPv4" }
                ),
            ));
        };
        let socket = socket2::Socket::new(socket2::Domain::for_address(address), kind, None)?;
        if interface.parse::<IpAddr>().is_err() {
            bind_device(&socket, interface, address.is_ipv6());
        }
        socket.bind(&SocketAddr::new(local, 0).into())?;
        Ok(Some(socket))
    }
}

/// Binds a listening socket of `kind` on `address`. IPv6 sockets take IPv6
/// only, so an IPv4 socket can share the port, and peers' addresses never
/// come as IPv4-mapped IPv6 ones.
pub(crate) fn listen_socket(
    address: SocketAddr,
    kind: socket2::Type,
) -> std::io::Result<socket2::Socket> {
    let socket = socket2::Socket::new(socket2::Domain::for_address(address), kind, None)?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    if kind == socket2::Type::STREAM {
        // Restarting must not wait out connections in TIME_WAIT.
        #[cfg(unix)]
        socket.set_reuse_address(true)?;
    }
    socket.bind(&address.into())?;
    Ok(socket)
}

/// Binds UDP sockets on `addresses`, one per family. IPv4 must work; an
/// IPv6 socket that cannot be had (no IPv6 on this system) is left out.
pub(crate) fn udp_sockets(addresses: &[SocketAddr]) -> std::io::Result<Vec<UdpSocket>> {
    let mut sockets: Vec<UdpSocket> = Vec::new();
    for address in addresses {
        // Port 0 picks a free port once; the other families share it.
        let mut address = *address;
        if address.port() == 0
            && let Some(first) = sockets.first().and_then(|socket| socket.local_addr().ok())
        {
            address.set_port(first.port());
        }
        match listen_socket(address, socket2::Type::DGRAM) {
            Ok(socket) => {
                // uTP's congestion window arrives in bursts; the default
                // buffer (about 200 KiB on Linux) overflows long before the
                // window fills, and each drop halves the window or costs a
                // 500 ms timeout. The kernel caps these at its own limit.
                let _ = socket.set_recv_buffer_size(UDP_BUFFER);
                let _ = socket.set_send_buffer_size(UDP_BUFFER);
                sockets.push(socket.into());
            }
            Err(error) if address.is_ipv4() || addresses.len() == 1 => return Err(error),
            Err(_) => {}
        }
    }
    Ok(sockets)
}

/// Kernel buffer asked for on each UDP socket, both ways.
const UDP_BUFFER: usize = 8 << 20;
/// Datagrams handled before a batch is finished, even if more are queued.
const RECEIVE_BATCH: usize = 64;
/// How often a waiting receive loop checks for shutdown.
const SHUTDOWN_CHECK: Duration = Duration::from_millis(250);

/// Reads datagrams from `socket` until `shutdown`, in batches: once one
/// arrives, those already queued behind it are read too before `finished`
/// runs, so work done per batch (acks, wakeups) is paid once rather than
/// per datagram. Leaves the socket non-blocking.
pub(crate) fn receive_batches(
    socket: &UdpSocket,
    shutdown: &std::sync::atomic::AtomicBool,
    mut datagram: impl FnMut(&[u8], SocketAddr),
    mut finished: impl FnMut(),
) -> std::io::Result<()> {
    use std::io::ErrorKind;
    socket.set_nonblocking(true)?;
    let mut source = mio::net::UdpSocket::from_std(socket.try_clone()?);
    let mut poll = mio::Poll::new()?;
    poll.registry()
        .register(&mut source, mio::Token(0), mio::Interest::READABLE)?;
    let mut events = mio::Events::with_capacity(4);
    let mut buffer = vec![0; 64 * 1024];
    while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
        match poll.poll(&mut events, Some(SHUTDOWN_CHECK)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        // Readiness is edge-triggered: read until the queue is empty.
        let mut pending = 0;
        loop {
            match source.recv_from(&mut buffer) {
                Ok((length, from)) => {
                    datagram(&buffer[..length], from);
                    pending += 1;
                    if pending == RECEIVE_BATCH {
                        finished();
                        pending = 0;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                // An ICMP error for an earlier send (a closed port) is
                // reported once; the socket keeps working.
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::Interrupted
                            | ErrorKind::ConnectionRefused
                            | ErrorKind::ConnectionReset
                    ) => {}
                Err(_) => break,
            }
        }
        if pending > 0 {
            finished();
        }
    }
    Ok(())
}

/// The canonical form of a peer's address: IPv4-mapped IPv6 addresses as
/// the IPv4 addresses they are.
pub(crate) fn canonical(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(ip) => SocketAddr::from((ip, v6.port())),
            None => address,
        },
        SocketAddr::V4(_) => address,
    }
}

/// `fe80::/10`.
pub(crate) fn is_link_local(ip: &Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

/// Ties a socket to a device by name where the system allows it. Without
/// the privilege some systems ask for, the address binding still applies.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
fn bind_device(socket: &socket2::Socket, interface: &str, _v6: bool) {
    let _ = socket.bind_device(Some(interface.as_bytes()));
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos"
))]
fn bind_device(socket: &socket2::Socket, interface: &str, v6: bool) {
    let index = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .find(|candidate| candidate.name == interface)
        .and_then(|candidate| candidate.index)
        .and_then(std::num::NonZeroU32::new);
    if index.is_some() {
        let _ = if v6 {
            socket.bind_device_by_index_v6(index)
        } else {
            socket.bind_device_by_index_v4(index)
        };
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "fuchsia",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos"
)))]
fn bind_device(_socket: &socket2::Socket, _interface: &str, _v6: bool) {}

fn unavailable(interface: &str) -> crate::BackendError {
    crate::BackendError::Io(unavailable_message(interface))
}

fn unavailable_message(interface: &str) -> String {
    format!("network interface {interface} is not available; not connecting")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const MISSING: &str = "empirebt-missing0";

    /// An interface whose address (its first IPv4 one, which binding by
    /// name uses) is not a loopback address, with that address.
    fn other_local_address() -> Option<(Ipv4Addr, String)> {
        if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .find_map(|interface| {
                let ip = Binding::interface(&interface.name).resolve().ok()??.v4?;
                (!ip.is_loopback()).then_some((ip, interface.name))
            })
    }

    #[test]
    fn resolves_interfaces_by_name_or_address() {
        assert_eq!(Binding::any().resolve().unwrap(), None);
        assert_eq!(Binding::interface("  ").name(), None);
        let v4_only = Addresses {
            v4: Some(Ipv4Addr::LOCALHOST),
            v6: None,
        };
        assert_eq!(
            Binding::interface("127.0.0.1").resolve().unwrap(),
            Some(v4_only)
        );
        // The loopback interface by name has both 127.0.0.1-style and ::1
        // addresses where IPv6 is on.
        let loopback = if_addrs::get_if_addrs()
            .unwrap()
            .into_iter()
            .find(|interface| interface.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
            .unwrap();
        let by_name = Binding::interface(&loopback.name)
            .resolve()
            .unwrap()
            .unwrap();
        assert!(by_name.v4.is_some());
        if Binding::interface("::1").is_up() {
            assert_eq!(by_name.v6, Some(Ipv6Addr::LOCALHOST));
            let listen = Binding::interface("::1").listen_addresses(6881).unwrap();
            assert_eq!(listen, vec!["[::1]:6881".parse().unwrap()]);
        }
        assert_eq!(Binding::any().listen_addresses(6881).unwrap().len(), 2);

        let missing = Binding::interface(MISSING);
        assert!(!missing.is_up());
        let error = missing.listen_addresses(6881).unwrap_err();
        assert!(error.to_string().contains(MISSING), "{error}");
        // An address no interface has is just as missing.
        assert!(!Binding::interface("192.0.2.77").is_up());
        assert!(!Binding::interface("2001:db8::77").is_up());
    }

    #[test]
    fn maps_ipv4_mapped_addresses_to_ipv4() {
        let mapped: SocketAddr = "[::ffff:192.0.2.1]:6881".parse().unwrap();
        assert_eq!(canonical(mapped), "192.0.2.1:6881".parse().unwrap());
        let v6: SocketAddr = "[2001:db8::1]:6881".parse().unwrap();
        assert_eq!(canonical(v6), v6);
        assert!(is_link_local(&"fe80::1".parse().unwrap()));
        assert!(!is_link_local(&"2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn connections_leave_from_the_chosen_interface() {
        let Some((ip, name)) = other_local_address() else {
            eprintln!("skipped: no network interface besides loopback");
            return;
        };
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // By address: even a connection to 127.0.0.1, which would otherwise
        // come from 127.0.0.1, leaves from the chosen address.
        let by_address = Binding::interface(ip.to_string());
        let _stream = by_address
            .connect_tcp(
                SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(listener.accept().unwrap().1.ip(), IpAddr::V4(ip));
        let udp = by_address
            .udp_for(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .unwrap();
        assert_eq!(udp.local_addr().unwrap().ip(), IpAddr::V4(ip));

        // By name: connections through the interface work...
        let by_name = Binding::interface(&name);
        let _stream = by_name
            .connect_tcp(SocketAddr::from((ip, port)), Duration::from_secs(5))
            .unwrap();
        assert_eq!(listener.accept().unwrap().1.ip(), IpAddr::V4(ip));
        // ...and where sockets can be tied to the device, nothing reachable
        // only another way gets through, as with a VPN that is down.
        if cfg!(target_os = "linux") {
            let outside = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            assert!(
                by_name
                    .connect_tcp(outside, Duration::from_secs(1))
                    .is_err()
            );
        }
    }

    #[test]
    fn nothing_connects_while_the_interface_is_missing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let target = listener.local_addr().unwrap();
        let missing = Binding::interface(MISSING);
        let error = missing
            .connect_tcp(target, Duration::from_secs(1))
            .unwrap_err();
        assert!(error.to_string().contains(MISSING), "{error}");
        assert!(missing.udp_for(target).is_err());
        std::thread::sleep(Duration::from_millis(100));
        assert!(listener.accept().is_err(), "no connection was attempted");
    }
}
