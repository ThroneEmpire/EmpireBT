//! Magnet links (BEP 9): a torrent identified only by its info hash.

use std::net::{SocketAddr, ToSocketAddrs};

/// A parsed `magnet:` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagnetLink {
    /// The v1 info hash, or for a v2-only link the truncated v2 hash, which
    /// is what its swarm uses in handshakes.
    info_hash: [u8; 20],
    /// The SHA-256 v2 info hash (`urn:btmh:`), when the link has one.
    v2_hash: Option<[u8; 32]>,
    name: Option<String>,
    trackers: Vec<String>,
    peers: Vec<String>,
}

impl MagnetLink {
    /// Parses a magnet URI with a BitTorrent v1 info hash (`urn:btih:`, in
    /// 40-character hex or 32-character base32 form), a v2 info hash
    /// (`urn:btmh:` with a SHA-256 multihash), or both (hybrid torrents).
    pub fn parse(uri: &str) -> Result<Self, crate::BackendError> {
        let query = uri
            .trim()
            .strip_prefix("magnet:?")
            .ok_or_else(|| invalid("a magnet link starts with \"magnet:?\""))?;
        let mut info_hash = None;
        let mut v2_hash = None;
        let mut name = None;
        let mut trackers = Vec::new();
        let mut peers = Vec::new();
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            // Numbered keys such as `xt.1` and `tr.2` repeat a parameter.
            let key = key.split('.').next().unwrap_or(key);
            match key {
                "xt" => {
                    let value = percent_decode(value, false)?;
                    if let Some(hash) = value.strip_prefix("urn:btih:") {
                        info_hash = Some(parse_btih(hash)?);
                    } else if let Some(hash) = value.strip_prefix("urn:btmh:") {
                        v2_hash = Some(parse_btmh(hash)?);
                    }
                }
                "dn" => name = Some(percent_decode(value, true)?).filter(|name| !name.is_empty()),
                "tr" => {
                    let tracker = percent_decode(value, false)?;
                    if crate::Tracker::new(tracker.clone()).is_ok() && !trackers.contains(&tracker)
                    {
                        trackers.push(tracker);
                    }
                }
                "x" if pair.starts_with("x.pe=") => peers.push(percent_decode(value, false)?),
                _ => {}
            }
        }
        let info_hash = info_hash
            .or_else(|| v2_hash.map(|hash| hash[..20].try_into().expect("32-byte hash")))
            .ok_or_else(|| {
                invalid("magnet link has no BitTorrent info hash (urn:btih or urn:btmh)")
            })?;
        Ok(Self {
            info_hash,
            v2_hash,
            name,
            trackers,
            peers,
        })
    }

    /// The hash the swarm is joined with: the v1 hash, or the truncated v2
    /// hash for a v2-only link.
    pub fn info_hash(&self) -> &[u8; 20] {
        &self.info_hash
    }

    /// The full SHA-256 v2 info hash, when the link carries one.
    pub fn v2_hash(&self) -> Option<&[u8; 32]> {
        self.v2_hash.as_ref()
    }

    /// Whether the link names only a v2 hash, so the swarm and metadata are
    /// v2-only.
    pub fn is_v2_only(&self) -> bool {
        self.v2_hash
            .is_some_and(|hash| hash[..20] == self.info_hash)
    }

    /// The suggested display name (`dn`), if the link has one.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn trackers(&self) -> &[String] {
        &self.trackers
    }

    /// Peer addresses listed directly in the link (`x.pe`), resolved.
    pub fn peers(&self) -> Vec<SocketAddr> {
        self.peers
            .iter()
            .filter_map(|peer| peer.to_socket_addrs().ok())
            .flatten()
            .collect()
    }

    /// Formats the link back into a URI.
    pub fn to_uri(&self) -> String {
        let hash: String = self
            .info_hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut uri = if self.is_v2_only() {
            String::from("magnet:?")
        } else {
            format!("magnet:?xt=urn:btih:{hash}&")
        };
        if let Some(v2) = &self.v2_hash {
            let v2: String = v2.iter().map(|byte| format!("{byte:02x}")).collect();
            uri.push_str(&format!("xt=urn:btmh:1220{v2}&"));
        }
        uri.pop();
        if let Some(name) = &self.name {
            uri.push_str(&format!("&dn={}", percent_encode(name)));
        }
        for tracker in &self.trackers {
            uri.push_str(&format!("&tr={}", percent_encode(tracker)));
        }
        for peer in &self.peers {
            uri.push_str(&format!("&x.pe={}", percent_encode(peer)));
        }
        uri
    }
}

fn parse_btih(hash: &str) -> Result<[u8; 20], crate::BackendError> {
    let bytes = match hash.len() {
        40 => (0..40)
            .step_by(2)
            .map(|at| u8::from_str_radix(&hash[at..at + 2], 16).ok())
            .collect::<Option<Vec<u8>>>(),
        32 => base32_decode(hash),
        _ => None,
    };
    bytes
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| invalid("magnet info hash must be 40 hex or 32 base32 characters"))
}

/// A `btmh` multihash: `0x12` (SHA-256), `0x20` (32 bytes), then the hash.
fn parse_btmh(text: &str) -> Result<[u8; 32], crate::BackendError> {
    let hex = text
        .strip_prefix("1220")
        .filter(|hash| hash.len() == 64)
        .ok_or_else(|| invalid("magnet v2 hash must be a SHA-256 multihash (1220 + 64 hex)"))?;
    (0..64)
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| invalid("magnet v2 hash is not valid hex"))
}

fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0_u64;
    let mut bit_count = 0;
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    for character in text.bytes() {
        let value = match character.to_ascii_uppercase() {
            letter @ b'A'..=b'Z' => letter - b'A',
            digit @ b'2'..=b'7' => digit - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(value);
        bit_count += 5;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push((bits >> bit_count) as u8);
        }
    }
    Some(out)
}

fn percent_decode(text: &str, plus_is_space: bool) -> Result<String, crate::BackendError> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = text
                    .get(index + 1..index + 3)
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| invalid("magnet link has an invalid percent escape"))?;
                out.push(hex);
                index += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| invalid("magnet link text is not valid UTF-8"))
}

fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn invalid(message: &str) -> crate::BackendError {
    crate::BackendError::InvalidMetainfo(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_magnets_with_name_and_trackers() {
        let link = MagnetLink::parse(
            "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a\
             &dn=Ubuntu+24.04%20Desktop&tr=udp%3A%2F%2Ftracker.example%3A1337\
             &tr=http://t.example/announce&tr=ftp://bad&x.pe=127.0.0.1:6881",
        )
        .unwrap();
        assert_eq!(link.info_hash()[..2], [0xc1, 0x2f]);
        assert_eq!(link.name(), Some("Ubuntu 24.04 Desktop"));
        assert_eq!(
            link.trackers(),
            ["udp://tracker.example:1337", "http://t.example/announce"]
        );
        assert_eq!(link.peers(), ["127.0.0.1:6881".parse().unwrap()]);
        assert_eq!(MagnetLink::parse(&link.to_uri()).unwrap(), link);
    }

    #[test]
    fn parses_base32_hashes() {
        let hex = MagnetLink::parse("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a")
            .unwrap();
        let base32 =
            MagnetLink::parse("magnet:?xt=urn:btih:YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK").unwrap();
        assert_eq!(hex.info_hash(), base32.info_hash());
    }

    #[test]
    fn parses_v2_and_hybrid_links() {
        let v2 = "f313a03980674d917d839855a78cd968aec12058bc9427275f01c36cfa7a0c20";
        let link = MagnetLink::parse(&format!("magnet:?xt=urn:btmh:1220{v2}&dn=album")).unwrap();
        assert!(link.is_v2_only());
        assert_eq!(link.v2_hash().unwrap()[0], 0xf3);
        assert_eq!(link.info_hash()[..], link.v2_hash().unwrap()[..20]);
        assert_eq!(MagnetLink::parse(&link.to_uri()).unwrap(), link);

        let hybrid = MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:c087c5e73e789022f30a44f00eb7c7194e6aa76e&xt=urn:btmh:1220{v2}"
        ))
        .unwrap();
        assert!(!hybrid.is_v2_only());
        assert_eq!(hybrid.info_hash()[0], 0xc0);
        assert_eq!(MagnetLink::parse(&hybrid.to_uri()).unwrap(), hybrid);
    }

    #[test]
    fn rejects_links_without_a_v1_hash() {
        assert!(MagnetLink::parse("https://example.com").is_err());
        assert!(MagnetLink::parse("magnet:?dn=nothing").is_err());
        assert!(MagnetLink::parse("magnet:?xt=urn:btih:abc").is_err());
        assert!(MagnetLink::parse("magnet:?xt=urn:btmh:1220abcd").is_err());
    }
}
