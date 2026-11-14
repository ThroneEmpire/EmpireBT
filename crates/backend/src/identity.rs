//! How EmpireBT identifies itself to other peers.
//!
//! Peer IDs follow the Azureus style from BEP 20: `-EM` followed by four
//! version characters, a dash, and twelve random characters. `EM` is not
//! assigned to any client in BEP 20, libtorrent, or Transmission. (`EB`
//! would be read as EBit.) The version comes from the backend crate's own
//! version, so it cannot drift from the release.
//!
//! In anonymous mode a torrent uses a peer ID without the client code
//! ([`generate_anonymous_peer_id`]), and that peer ID decides the rest:
//! a torrent whose peer ID does not [identify](identifies) EmpireBT also
//! leaves the client name out of its extension handshake and announces
//! with a generic User-Agent.
//!
//! Disguised as qBittorrent ([`generate_qbittorrent_peer_id`]), a torrent
//! says everything qBittorrent 5.1.4 says instead: its peer ID, its
//! User-Agent, its client name in handshakes, and libtorrent's tag on DHT
//! messages. Only public torrents are ever disguised: private trackers
//! admit the clients they approve, and pretending to be another there is
//! what they ban.

/// The two-character client code in peer IDs and DHT messages.
pub const CLIENT_CODE: [u8; 2] = *b"EM";

/// The first eight bytes of every peer ID, e.g. `-EM0100-` for 0.1.0.
pub const PEER_ID_PREFIX: [u8; 8] = {
    let [a, b] = CLIENT_CODE;
    [
        b'-',
        a,
        b,
        version_char(parse(env!("CARGO_PKG_VERSION_MAJOR"))),
        version_char(parse(env!("CARGO_PKG_VERSION_MINOR"))),
        version_char(parse(env!("CARGO_PKG_VERSION_PATCH"))),
        b'0',
        b'-',
    ]
};

/// The BEP 5 `v` key on DHT messages: client code plus major and minor
/// version bytes.
pub(crate) const DHT_VERSION: [u8; 4] = {
    let [a, b] = CLIENT_CODE;
    [
        a,
        b,
        clamp_byte(parse(env!("CARGO_PKG_VERSION_MAJOR"))),
        clamp_byte(parse(env!("CARGO_PKG_VERSION_MINOR"))),
    ]
};

/// What trackers are told the client is.
pub const USER_AGENT: &str = concat!("EmpireBT/", env!("CARGO_PKG_VERSION"));

/// The User-Agent of anonymous torrents' announces: the one libtorrent's
/// anonymous mode uses, so they blend in with its users.
pub const ANONYMOUS_USER_AGENT: &str = "curl/7.81.0";

/// How public torrents present themselves (private ones always say they
/// are EmpireBT).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Identity {
    /// As EmpireBT.
    #[default]
    EmpireBt,
    /// As no client in particular (anonymous mode).
    Anonymous,
    /// As qBittorrent 5.1.4.
    QBittorrent,
}

impl Identity {
    /// The peer ID public torrents use instead of their own, if any: a
    /// new one each time, so runs cannot be linked.
    pub fn public_peer_id(self) -> Option<[u8; 20]> {
        match self {
            Self::EmpireBt => None,
            Self::Anonymous => Some(generate_anonymous_peer_id()),
            Self::QBittorrent => Some(generate_qbittorrent_peer_id()),
        }
    }

    /// The User-Agent public torrents announce and scrape with.
    pub fn user_agent(self) -> &'static str {
        match self {
            Self::EmpireBt => USER_AGENT,
            Self::Anonymous => ANONYMOUS_USER_AGENT,
            Self::QBittorrent => QBITTORRENT_USER_AGENT,
        }
    }

    /// The `v` key on DHT messages, if any.
    pub(crate) fn dht_version(self) -> Option<[u8; 4]> {
        match self {
            Self::EmpireBt => Some(DHT_VERSION),
            Self::Anonymous => None,
            Self::QBittorrent => Some(QBITTORRENT_DHT_VERSION),
        }
    }
}

/// qBittorrent 5.1.4's peer ID prefix: libtorrent's fingerprint for `qB`
/// 5.1.4.0.
pub const QBITTORRENT_PEER_ID_PREFIX: [u8; 8] = *b"-qB5140-";

/// qBittorrent's User-Agent and handshake client name.
pub const QBITTORRENT_USER_AGENT: &str = "qBittorrent/5.1.4";

/// The `v` key libtorrent puts on DHT messages: `LT` and its major and
/// minor version; qBittorrent 5.1 ships with libtorrent 2.0.
pub(crate) const QBITTORRENT_DHT_VERSION: [u8; 4] = *b"LT\x02\x00";

/// A peer ID as qBittorrent 5.1.4 makes one: its prefix, then twelve
/// characters from libtorrent's `url_random` set.
pub fn generate_qbittorrent_peer_id() -> [u8; 20] {
    const PRINTABLE: &[u8] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-_.!~*()";
    let mut id = [0; 20];
    id[..8].copy_from_slice(&QBITTORRENT_PEER_ID_PREFIX);
    let random: [u8; 12] = crate::discovery::dht::random_bytes();
    for (byte, value) in id[8..].iter_mut().zip(random) {
        *byte = PRINTABLE[usize::from(value) % PRINTABLE.len()];
    }
    id
}

/// Whether a peer ID is a disguised torrent's.
pub fn disguised(peer_id: &[u8; 20]) -> bool {
    peer_id.starts_with(&QBITTORRENT_PEER_ID_PREFIX)
}

/// The client name a torrent with `peer_id` gives in its extension
/// handshake (`v`): ours, qBittorrent's when disguised, none when
/// anonymous.
pub fn handshake_name(peer_id: &[u8; 20]) -> Option<&'static str> {
    if identifies(peer_id) {
        Some(concat!("EmpireBT ", env!("CARGO_PKG_VERSION")))
    } else if disguised(peer_id) {
        Some(QBITTORRENT_USER_AGENT)
    } else {
        None
    }
}

/// Generates a peer ID: [`PEER_ID_PREFIX`] followed by twelve random
/// lowercase letters and digits. Printable IDs keep tracker logs and
/// clients that display peer IDs readable.
pub fn generate_peer_id() -> [u8; 20] {
    let mut id = [0; 20];
    id[..8].copy_from_slice(&PEER_ID_PREFIX);
    fill_random(&mut id[8..]);
    id
}

/// Generates a peer ID that names no client: twenty random lowercase
/// letters and digits.
pub fn generate_anonymous_peer_id() -> [u8; 20] {
    let mut id = [0; 20];
    fill_random(&mut id);
    id
}

/// Whether a peer ID says it is EmpireBT's, which anonymous ones do not.
pub fn identifies(peer_id: &[u8; 20]) -> bool {
    peer_id.starts_with(&PEER_ID_PREFIX)
}

/// The User-Agent to announce a torrent with its `peer_id`.
pub fn user_agent_for(peer_id: &[u8; 20]) -> &'static str {
    if identifies(peer_id) {
        USER_AGENT
    } else if disguised(peer_id) {
        QBITTORRENT_USER_AGENT
    } else {
        ANONYMOUS_USER_AGENT
    }
}

/// Fills `id` with random lowercase letters and digits.
fn fill_random(id: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    // RandomState is keyed from the operating system's random source.
    let state = std::collections::hash_map::RandomState::new();
    for (chunk, salt) in id.chunks_mut(8).zip(0u64..) {
        let mut hasher = state.build_hasher();
        hasher.write_u64(salt);
        hasher.write_u32(std::process::id());
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
        );
        for (byte, value) in chunk.iter_mut().zip(hasher.finish().to_le_bytes()) {
            *byte = ALPHABET[usize::from(value) % ALPHABET.len()];
        }
    }
}

/// One Azureus version character: `0`-`9`, then `A`-`Z` for 10-35, as
/// libtorrent and Transmission decode them.
const fn version_char(part: u32) -> u8 {
    match part {
        0..=9 => b'0' + part as u8,
        10..=35 => b'A' + (part - 10) as u8,
        _ => b'Z',
    }
}

const fn clamp_byte(part: u32) -> u8 {
    if part > 255 { 255 } else { part as u8 }
}

const fn parse(digits: &str) -> u32 {
    let bytes = digits.as_bytes();
    let mut value = 0;
    let mut index = 0;
    while index < bytes.len() {
        value = value * 10 + (bytes[index] - b'0') as u32;
        index += 1;
    }
    value
}

/// Azureus-style client codes (BEP 20 and what clients use in practice).
const CLIENTS: &[(&[u8; 2], &str)] = &[
    (b"AG", "Ares"),
    (b"AZ", "Vuze"),
    (b"BC", "BitComet"),
    (b"BI", "BiglyBT"),
    (b"BT", "BitTorrent"),
    (b"BW", "BitWombat"),
    (b"DE", "Deluge"),
    (b"EM", "EmpireBT"),
    (b"FD", "Free Download Manager"),
    (b"FW", "FrostWire"),
    (b"KT", "KTorrent"),
    (b"LT", "libtorrent"),
    (b"lt", "rTorrent"),
    (b"PI", "PicoTorrent"),
    (b"qB", "qBittorrent"),
    (b"SD", "Thunder"),
    (b"TL", "Tribler"),
    (b"TR", "Transmission"),
    (b"TX", "Tixati"),
    (b"UM", "µTorrent for Mac"),
    (b"UT", "µTorrent"),
    (b"UW", "µTorrent Web"),
    (b"WW", "WebTorrent"),
    (b"XL", "Xunlei"),
];

/// A client's name and version from its peer id, when it follows a known
/// style: Azureus (`-qB4650-` is qBittorrent 4.6.5) or Mainline
/// (`M7-4-0--` is BitTorrent 7.4.0). Peers that name themselves in the
/// extension handshake are better shown by that name.
pub fn client_name(peer_id: &[u8; 20]) -> Option<String> {
    if peer_id[0] == b'-' && peer_id[7] == b'-' {
        let code = [peer_id[1], peer_id[2]];
        let name = CLIENTS
            .iter()
            .find(|(known, _)| **known == code)
            .map(|(_, name)| (*name).to_owned())
            .or_else(|| {
                code.iter()
                    .all(u8::is_ascii_alphanumeric)
                    .then(|| String::from_utf8_lossy(&code).into_owned())
            })?;
        let mut parts: Vec<String> = peer_id[3..7]
            .iter()
            .map(|byte| match byte {
                b'0'..=b'9' => (byte - b'0').to_string(),
                // Some clients count past 9 in letters.
                b'A'..=b'Z' => (byte - b'A' + 10).to_string(),
                b'a'..=b'z' => (byte - b'a' + 36).to_string(),
                _ => String::new(),
            })
            .collect();
        if parts.iter().any(String::is_empty) {
            return Some(name);
        }
        // A last zero is a build number nobody reads.
        while parts.len() > 2 && parts.last().is_some_and(|part| part == "0") {
            parts.pop();
        }
        return Some(format!("{name} {}", parts.join(".")));
    }
    // Mainline: a letter, then digits separated by dashes.
    if peer_id[0] == b'M' && peer_id[1].is_ascii_digit() {
        let text = String::from_utf8_lossy(&peer_id[1..8]);
        let version: Vec<&str> = text
            .split('-')
            .filter(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
            .collect();
        if !version.is_empty() {
            return Some(format!("BitTorrent {}", version.join(".")));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Disguised as qBittorrent 5.1.4, everything says so, as it would:
    /// the peer ID's prefix and its libtorrent-style random part, the
    /// User-Agent, the handshake name, and libtorrent's DHT tag.
    #[test]
    fn a_disguise_says_what_qbittorrent_says() {
        let id = generate_qbittorrent_peer_id();
        assert_eq!(&id[..8], b"-qB5140-");
        assert!(
            id[8..]
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.!~*()".contains(byte))
        );
        assert_ne!(id, generate_qbittorrent_peer_id());
        assert!(disguised(&id) && !identifies(&id));
        assert_eq!(client_name(&id).as_deref(), Some("qBittorrent 5.1.4"));
        assert_eq!(user_agent_for(&id), "qBittorrent/5.1.4");
        assert_eq!(handshake_name(&id), Some("qBittorrent/5.1.4"));
        assert_eq!(Identity::QBittorrent.dht_version(), Some(*b"LT\x02\x00"));
        assert_eq!(Identity::Anonymous.dht_version(), None);
        assert_eq!(Identity::EmpireBt.dht_version(), Some(DHT_VERSION));
        assert!(
            Identity::QBittorrent
                .public_peer_id()
                .is_some_and(|id| disguised(&id))
        );
        assert_eq!(Identity::EmpireBt.public_peer_id(), None);
    }

    #[test]
    fn names_clients_from_their_peer_ids() {
        let id = |prefix: &[u8]| {
            let mut id = [b'x'; 20];
            id[..prefix.len()].copy_from_slice(prefix);
            id
        };
        assert_eq!(
            client_name(&id(b"-qB4650-")).as_deref(),
            Some("qBittorrent 4.6.5")
        );
        assert_eq!(
            client_name(&id(b"-TR4050-")).as_deref(),
            Some("Transmission 4.0.5")
        );
        assert_eq!(
            client_name(&id(b"-LT20A0-")).as_deref(),
            Some("libtorrent 2.0.10")
        );
        assert_eq!(
            client_name(&id(b"-UT3550-")).as_deref(),
            Some("µTorrent 3.5.5")
        );
        assert_eq!(client_name(&id(b"-ZZ1200-")).as_deref(), Some("ZZ 1.2"));
        assert_eq!(
            client_name(&id(b"M7-4-0--")).as_deref(),
            Some("BitTorrent 7.4.0")
        );
        assert_eq!(
            client_name(&generate_peer_id())
                .as_deref()
                .map(|name| name.starts_with("EmpireBT ")),
            Some(true)
        );
        assert_eq!(client_name(&[0; 20]), None);
    }

    #[test]
    fn peer_id_prefix_encodes_the_crate_version() {
        let expected = format!(
            "-EM{}{}{}0-",
            env!("CARGO_PKG_VERSION_MAJOR"),
            env!("CARGO_PKG_VERSION_MINOR"),
            env!("CARGO_PKG_VERSION_PATCH")
        );
        assert_eq!(&PEER_ID_PREFIX, expected.as_bytes());
    }

    #[test]
    fn peer_ids_are_tagged_printable_and_distinct() {
        let first = generate_peer_id();
        let second = generate_peer_id();
        assert!(identifies(&first));
        assert_eq!(user_agent_for(&first), USER_AGENT);
        // Anonymous ones name no client, and differ every time.
        let anonymous = generate_anonymous_peer_id();
        assert!(!identifies(&anonymous));
        assert_eq!(client_name(&anonymous), None, "{anonymous:?}");
        assert_eq!(user_agent_for(&anonymous), ANONYMOUS_USER_AGENT);
        assert!(anonymous.iter().all(u8::is_ascii_alphanumeric));
        assert_ne!(anonymous, generate_anonymous_peer_id());
        assert!(first.starts_with(&PEER_ID_PREFIX));
        assert!(first[8..].iter().all(u8::is_ascii_alphanumeric));
        assert_ne!(first, second);
    }

    #[test]
    fn version_parts_above_nine_use_letters() {
        assert_eq!(version_char(7), b'7');
        assert_eq!(version_char(10), b'A');
        assert_eq!(version_char(35), b'Z');
        assert_eq!(version_char(99), b'Z');
        assert_eq!(parse("12"), 12);
    }
}
