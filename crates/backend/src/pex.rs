//! BEP 11, peer exchange (`ut_pex`): connected peers tell each other about
//! the other peers they are connected to.
//!
//! Each message is a bencoded dictionary of contacts in compact form:
//! `added` and `dropped` for IPv4, `added6` and `dropped6` for IPv6, with
//! one flag byte per added contact in `added.f` and `added6.f`. Messages go
//! out at most once a minute, with at most 50 additions and 50 drops after
//! the first. Contacts from peers are untrusted, so the receiver checks and
//! limits them (see [`crate::swarm`]).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::bencode::{self, Value};

/// The extension message id we assign to `ut_pex`.
pub(crate) const UT_PEX: u8 = 2;
/// Contacts per list in any message but the first.
pub(crate) const MAX_PER_MESSAGE: usize = 50;

/// The peer prefers encrypted connections.
pub(crate) const PREFERS_ENCRYPTION: u8 = 0x01;
/// The peer is a seed (or only uploads).
pub(crate) const SEED: u8 = 0x02;
/// The peer supports uTP.
pub(crate) const SUPPORTS_UTP: u8 = 0x04;
/// We connected to the peer, so it accepts incoming connections.
pub(crate) const REACHABLE: u8 = 0x10;

/// One `ut_pex` message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PexMessage {
    pub added: Vec<(SocketAddr, u8)>,
    pub dropped: Vec<SocketAddr>,
}

impl PexMessage {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.dropped.is_empty()
    }

    pub fn encode(&self) -> Vec<u8> {
        let (mut added, mut flags, mut added6, mut flags6) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (address, flag) in &self.added {
            match address {
                SocketAddr::V4(_) => {
                    added.extend(compact(address));
                    flags.push(*flag);
                }
                SocketAddr::V6(_) => {
                    added6.extend(compact(address));
                    flags6.push(*flag);
                }
            }
        }
        let (mut dropped, mut dropped6) = (Vec::new(), Vec::new());
        for address in &self.dropped {
            match address {
                SocketAddr::V4(_) => dropped.extend(compact(address)),
                SocketAddr::V6(_) => dropped6.extend(compact(address)),
            }
        }
        Value::dict([
            (&b"added"[..], Value::bytes(added)),
            (b"added.f", Value::bytes(flags)),
            (b"added6", Value::bytes(added6)),
            (b"added6.f", Value::bytes(flags6)),
            (b"dropped", Value::bytes(dropped)),
            (b"dropped6", Value::bytes(dropped6)),
        ])
        .encode()
    }

    /// Decodes a message, skipping malformed contacts. Flags default to 0
    /// when a peer leaves them out.
    pub fn decode(payload: &[u8]) -> Result<Self, crate::BackendError> {
        let message = bencode::decode(payload)?;
        let list = |key: &[u8], size: usize| -> Vec<SocketAddr> {
            message
                .get_bytes(key)
                .map(|bytes| bytes.chunks_exact(size).filter_map(parse_compact).collect())
                .unwrap_or_default()
        };
        let flags = |key: &[u8]| message.get_bytes(key).unwrap_or_default().to_vec();
        let with_flags = |addresses: Vec<SocketAddr>, flags: Vec<u8>| {
            addresses
                .into_iter()
                .enumerate()
                .map(move |(index, address)| (address, flags.get(index).copied().unwrap_or(0)))
        };
        let mut added: Vec<(SocketAddr, u8)> =
            with_flags(list(b"added", 6), flags(b"added.f")).collect();
        added.extend(with_flags(list(b"added6", 18), flags(b"added6.f")));
        let mut dropped = list(b"dropped", 6);
        dropped.extend(list(b"dropped6", 18));
        Ok(Self { added, dropped })
    }
}

/// An address in compact form: 4 or 16 address bytes, then the port.
pub(crate) fn compact(address: &SocketAddr) -> Vec<u8> {
    let mut bytes = match address.ip() {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    };
    bytes.extend(address.port().to_be_bytes());
    bytes
}

fn parse_compact(bytes: &[u8]) -> Option<SocketAddr> {
    let (ip, port) = bytes.split_at(bytes.len() - 2);
    let ip = match ip.len() {
        4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(ip).ok()?)),
        16 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(ip).ok()?)),
        _ => return None,
    };
    let port = u16::from_be_bytes([port[0], port[1]]);
    (port != 0 && !ip.is_unspecified()).then_some(SocketAddr::new(ip, port))
}

/// An address from a peer's BEP 10 `yourip` field: 4 or 16 bytes.
pub(crate) fn parse_ip(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?))),
        16 => Some(IpAddr::V6(Ipv6Addr::from(
            <[u8; 16]>::try_from(bytes).ok()?,
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_both_address_families_with_flags() {
        let message = PexMessage {
            added: vec![
                // IPv4 contacts decode before IPv6 ones.
                ("10.0.0.1:6881".parse().unwrap(), SEED | REACHABLE),
                ("10.0.0.2:7000".parse().unwrap(), 0),
                ("[2001:db8::1]:51413".parse().unwrap(), SUPPORTS_UTP),
            ],
            dropped: vec![
                "10.0.0.3:6881".parse().unwrap(),
                "[2001:db8::2]:1".parse().unwrap(),
            ],
        };
        assert_eq!(PexMessage::decode(&message.encode()).unwrap(), message);
    }

    #[test]
    fn reads_libtorrent_style_messages_and_skips_bad_contacts() {
        // libtorrent's layout: added, added.f, dropped (and v6 variants).
        let mut added = vec![1, 2, 3, 4, 0x1a, 0xe1];
        added.extend([0, 0, 0, 0, 0, 80]); // unspecified address
        added.extend([9, 9, 9, 9]); // truncated
        let payload = Value::dict([
            (&b"added"[..], Value::bytes(added)),
            (b"added.f", Value::bytes([REACHABLE])),
            (b"dropped", Value::bytes([5, 6, 7, 8, 0, 1])),
        ])
        .encode();
        let message = PexMessage::decode(&payload).unwrap();
        assert_eq!(
            message.added,
            [("1.2.3.4:6881".parse().unwrap(), REACHABLE)]
        );
        assert_eq!(
            message.dropped,
            ["5.6.7.8:1".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn parses_yourip() {
        assert_eq!(
            parse_ip(&[203, 0, 113, 7]),
            Some(IpAddr::from([203, 0, 113, 7]))
        );
        assert_eq!(parse_ip(&[1, 2, 3]), None);
    }
}
