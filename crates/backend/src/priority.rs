//! BEP 40, canonical peer priority: a ranking of peer pairs that every
//! client computes the same way.
//!
//! The priority of a pair is a CRC-32C of both addresses, partly masked so
//! that nearby addresses do not rank alike. Both sides of a pair compute
//! the same value, so preferring high-priority peers spreads connections
//! across the swarm instead of clustering. An attacker also cannot pick
//! addresses that rank high with everyone. Details follow libtorrent's
//! reference implementation: addresses are ordered before masking, and IPv6
//! masking starts after the first differing byte (never before byte 6).

use std::net::{IpAddr, SocketAddr};

/// The priority of a connection between `a` and `b`, or `None` if they are
/// of different address families.
pub(crate) fn peer_priority(a: SocketAddr, b: SocketAddr) -> Option<u32> {
    if a.ip() == b.ip() {
        let (low, high) = if a.port() <= b.port() {
            (a.port(), b.port())
        } else {
            (b.port(), a.port())
        };
        let mut bytes = low.to_be_bytes().to_vec();
        bytes.extend(high.to_be_bytes());
        return Some(crc32c(&bytes));
    }
    match (a.ip(), b.ip()) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let (mut first, mut second) = if a <= b {
                (a.octets(), b.octets())
            } else {
                (b.octets(), a.octets())
            };
            let mask: [u8; 4] = if first[..2] != second[..2] {
                [0xff, 0xff, 0x55, 0x55]
            } else if first[..3] != second[..3] {
                [0xff, 0xff, 0xff, 0x55]
            } else {
                [0xff; 4]
            };
            for index in 0..4 {
                first[index] &= mask[index];
                second[index] &= mask[index];
            }
            Some(crc32c(&[first, second].concat()))
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let (mut first, mut second) = if a <= b {
                (a.octets(), b.octets())
            } else {
                (b.octets(), a.octets())
            };
            let mut offset = None;
            for index in 0..16 {
                match offset {
                    None if first[index] != second[index] => offset = Some((index + 1).max(5)),
                    Some(start) if index > start => {
                        first[index] &= 0x55;
                        second[index] &= 0x55;
                    }
                    _ => {}
                }
            }
            Some(crc32c(&[first, second].concat()))
        }
        _ => None,
    }
}

/// CRC-32C (Castagnoli), as used by BEP 40.
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0; 256];
        let mut index = 0;
        while index < 256 {
            let mut value = index as u32;
            let mut bit = 0;
            while bit < 8 {
                value = if value & 1 == 1 {
                    value >> 1 ^ 0x82f6_3b78
                } else {
                    value >> 1
                };
                bit += 1;
            }
            table[index] = value;
            index += 1;
        }
        table
    };
    !bytes.iter().fold(!0, |crc, byte| {
        TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ crc >> 8
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(address: &str, port: u16) -> SocketAddr {
        SocketAddr::new(address.parse().unwrap(), port)
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn crc32c_matches_the_standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn matches_the_bep_40_examples() {
        let client = at("123.213.32.10", 0);
        assert_eq!(
            peer_priority(client, at("98.76.54.32", 0)),
            Some(0xec2d_7224)
        );
        assert_eq!(
            peer_priority(client, at("123.213.32.234", 0)),
            Some(0x9956_8189)
        );
        assert_eq!(
            peer_priority(at("98.76.54.32", 0), client),
            Some(0xec2d_7224),
            "both sides agree"
        );
    }

    #[test]
    fn matches_libtorrents_ipv4_vectors() {
        let cases = [
            ("230.12.123.3", 0x4d2, "230.12.123.3", 0x12c, "012c04d2"),
            (
                "230.12.123.1",
                0x4d2,
                "230.12.123.3",
                0x12c,
                "e60c7b01e60c7b03",
            ),
            (
                "230.12.23.1",
                0x4d2,
                "230.12.123.3",
                0x12c,
                "e60c1701e60c7b01",
            ),
            (
                "230.120.23.1",
                0x4d2,
                "230.12.123.3",
                0x12c,
                "e60c5101e6781501",
            ),
        ];
        for (a, a_port, b, b_port, buffer) in cases {
            assert_eq!(
                peer_priority(at(a, a_port), at(b, b_port)),
                Some(crc32c(&hex(buffer))),
                "{a} {b}"
            );
        }
    }

    #[test]
    fn matches_libtorrents_ipv6_vectors() {
        let all = "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";
        let cases = [
            (
                "ffff:0fff:ffff:ffff:ffff:ffff:ffff:ffff",
                "ffff0fffffff55555555555555555555ffffffffffff55555555555555555555",
            ),
            (
                "ffff:ffff:0fff:ffff:ffff:ffff:ffff:ffff",
                "ffffffff0fff55555555555555555555ffffffffffff55555555555555555555",
            ),
            (
                "ffff:ffff:ff0f:ffff:ffff:ffff:ffff:ffff",
                "ffffffffff0fff555555555555555555ffffffffffffff555555555555555555",
            ),
            (
                "ffff:ffff:ffff:0fff:ffff:ffff:ffff:ffff",
                "ffffffffffff0fff5555555555555555ffffffffffffffff5555555555555555",
            ),
        ];
        for (other, buffer) in cases {
            assert_eq!(
                peer_priority(at(all, 0x4d2), at(other, 0x12c)),
                Some(crc32c(&hex(buffer))),
                "{other}"
            );
        }
        assert_eq!(
            peer_priority(at(all, 0x12c), at(all, 0x4d2)),
            Some(crc32c(&hex("012c04d2")))
        );
        assert_eq!(peer_priority(at(all, 1), at("1.2.3.4", 1)), None);
    }
}
