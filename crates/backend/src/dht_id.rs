//! DHT node IDs tied to the node's external IP address (BEP 42), and
//! learning that address from other nodes, as libtorrent does.
//!
//! A node's ID starts with 21 bits derived from its IP address, so a node
//! cannot pick an ID next to a torrent it wants to control: it would need
//! an address to match. The other bits are random, so the ID still
//! changes on every start, and it changes whenever the IP address does
//! (a VPN reconnecting elsewhere).
//!
//! Nodes say which address they see us at (`ip` in their responses); an
//! address is believed only when several nodes agree, so one lying node
//! cannot move us.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

/// A node ID whose first 21 bits derive from `ip` and whose last byte is
/// `r`, with the rest from `random` (BEP 42; libtorrent's
/// `generate_id_impl`).
pub(crate) fn secure_node_id(ip: IpAddr, r: u8, random: [u8; 20]) -> [u8; 20] {
    let crc = ip_hash(ip, r);
    let mut id = random;
    id[0] = (crc >> 24) as u8;
    id[1] = (crc >> 16) as u8;
    id[2] = ((crc >> 8) as u8 & 0xf8) | (random[2] & 0x07);
    id[19] = r;
    id
}

/// Whether `id` is one that `ip` may use (BEP 42). Local and private
/// addresses pass: their nodes cannot know their external address.
pub(crate) fn verify_node_id(id: &[u8; 20], ip: IpAddr) -> bool {
    if is_local(ip) {
        return true;
    }
    let crc = ip_hash(ip, id[19]);
    id[0] == (crc >> 24) as u8
        && id[1] == (crc >> 16) as u8
        && id[2] & 0xf8 == (crc >> 8) as u8 & 0xf8
}

/// CRC32C of the address masked as BEP 42 says, with `r`'s low 3 bits in
/// the top of the first byte.
fn ip_hash(ip: IpAddr, r: u8) -> u32 {
    const V4_MASK: [u8; 4] = [0x03, 0x0f, 0x3f, 0xff];
    const V6_MASK: [u8; 8] = [0x01, 0x03, 0x07, 0x0f, 0x1f, 0x3f, 0x7f, 0xff];
    let mut bytes = match ip {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets()[..8].to_vec(),
    };
    let mask: &[u8] = if ip.is_ipv4() { &V4_MASK } else { &V6_MASK };
    for (byte, mask) in bytes.iter_mut().zip(mask) {
        *byte &= mask;
    }
    bytes[0] |= (r & 0x07) << 5;
    crate::priority::crc32c(&bytes)
}

/// Addresses BEP 42 exempts: private, link-local, and loopback.
pub(crate) fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => is_local(IpAddr::V4(ip)),
            None => {
                ip.is_loopback()
                    // fc00::/7 (unique local) and fe80::/10 (link-local).
                    || ip.segments()[0] & 0xfe00 == 0xfc00
                    || ip.segments()[0] & 0xffc0 == 0xfe80
            }
        },
    }
}

/// Addresses tallied at most, so made-up ones cannot fill memory.
const MAX_CANDIDATES: usize = 40;
/// Voters a candidate may count.
const MAX_VOTERS: usize = 200;
/// Different nodes that must agree before a first address is believed.
const FIRST_AGREEMENT: usize = 2;
/// Different nodes that must agree before we move to another address,
/// and they must outnumber the current address's supporters two to one.
const CHANGE_AGREEMENT: usize = 10;

/// Our external address as other nodes report it, by vote: one vote per
/// node address, as libtorrent's `ip_voter` counts them.
#[derive(Debug, Default)]
pub(crate) struct ExternalIp {
    current: Option<IpAddr>,
    votes: HashMap<IpAddr, HashSet<IpAddr>>,
}

impl ExternalIp {
    /// The address believed now, if any.
    #[cfg(test)]
    pub(crate) fn current(&self) -> Option<IpAddr> {
        self.current
    }

    /// Counts `voter` saying we are at `ip`; returns the new address when
    /// this vote changes what we believe.
    pub(crate) fn vote(&mut self, ip: IpAddr, voter: IpAddr) -> Option<IpAddr> {
        // Local addresses say nothing about our external one, and a voter
        // reached over another family cannot know this one.
        if ip.is_unspecified() || is_local(ip) || ip.is_ipv4() != voter.is_ipv4() {
            return None;
        }
        if !self.votes.contains_key(&ip) && self.votes.len() >= MAX_CANDIDATES {
            return None;
        }
        let voters = self.votes.entry(ip).or_default();
        if voters.len() < MAX_VOTERS {
            voters.insert(voter);
        }
        let count = voters.len();
        let adopt = match self.current {
            None => count >= FIRST_AGREEMENT,
            Some(current) if current == ip => false,
            Some(current) => {
                let supporting = self.votes.get(&current).map_or(0, HashSet::len);
                count >= CHANGE_AGREEMENT && count > 2 * supporting
            }
        };
        if !adopt {
            return None;
        }
        self.current = Some(ip);
        // Fresh counts for the next change.
        self.votes.clear();
        Some(ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_the_ids_in_bep_42s_examples() {
        // (IP, rand, the first three bytes of the ID; byte 2's low bits are
        // random, so only its top 5 bits are fixed).
        let examples: [(&str, u8, [u8; 3]); 5] = [
            ("124.31.75.21", 1, [0x5f, 0xbf, 0xbf]),
            ("21.75.31.124", 86, [0x5a, 0x3c, 0xe9]),
            ("65.23.51.170", 22, [0xa5, 0xd4, 0x32]),
            ("84.124.73.14", 65, [0x1b, 0x03, 0x21]),
            ("43.213.53.83", 90, [0xe5, 0x6f, 0x6c]),
        ];
        for (ip, r, prefix) in examples {
            let ip: IpAddr = ip.parse().unwrap();
            let id = secure_node_id(ip, r, [0xff; 20]);
            assert_eq!(id[..2], prefix[..2], "{ip}");
            assert_eq!(id[2] & 0xf8, prefix[2] & 0xf8, "{ip}");
            assert_eq!(id[19], r);
            assert!(verify_node_id(&id, ip));
            assert!(!verify_node_id(&id, "1.2.3.4".parse().unwrap()));
        }
    }

    #[test]
    fn makes_and_checks_ipv6_ids_and_lets_local_addresses_pass() {
        let ip: IpAddr = "2001:db8:1234:5678::1".parse().unwrap();
        let id = secure_node_id(ip, 3, [7; 20]);
        assert!(verify_node_id(&id, ip));
        assert!(!verify_node_id(&id, "2001:db8:9999::1".parse().unwrap()));
        for local in [
            "192.168.1.2",
            "10.0.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "fe80::1",
            "fd00::1",
        ] {
            assert!(verify_node_id(&[0; 20], local.parse().unwrap()), "{local}");
        }
    }

    #[test]
    fn believes_an_address_only_when_nodes_agree() {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        let mut external = ExternalIp::default();
        assert_eq!(external.vote(ip("203.0.113.5"), ip("198.51.100.1")), None);
        // The same voter again does not count twice.
        assert_eq!(external.vote(ip("203.0.113.5"), ip("198.51.100.1")), None);
        assert_eq!(
            external.vote(ip("203.0.113.5"), ip("198.51.100.2")),
            Some(ip("203.0.113.5"))
        );
        // Local addresses and other families are not votes.
        assert_eq!(external.vote(ip("192.168.1.9"), ip("198.51.100.3")), None);
        assert_eq!(external.vote(ip("2001:db8::1"), ip("198.51.100.3")), None);
        // A few liars do not move us; a clear majority does.
        external.vote(ip("203.0.113.5"), ip("198.51.100.9"));
        for voter in 1..CHANGE_AGREEMENT as u8 {
            let said = external.vote(ip("192.0.2.77"), IpAddr::from([100, 64, 0, voter]));
            assert_eq!(said, None, "after {voter} votes");
        }
        let moved = external.vote(ip("192.0.2.77"), ip("100.64.0.200"));
        assert_eq!(moved, Some(ip("192.0.2.77")));
        assert_eq!(external.current(), Some(ip("192.0.2.77")));
    }
}
