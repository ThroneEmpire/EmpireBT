//! BEP 55, the hole-punch extension (`ut_holepunch`): a peer connected to
//! two others that cannot reach each other (both behind a NAT) introduces
//! them, and they connect to each other over uTP at the same moment.
//!
//! EmpireBT is a relay: asked by one peer to introduce it to another it is
//! connected to, it sends both a `connect` naming the other, as libtorrent
//! does. It does not punch holes itself yet; `connect` messages sent to it
//! are ignored, which the BEP allows.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The extension message id we assign to `ut_holepunch`.
pub(crate) const UT_HOLEPUNCH: u8 = 3;

/// The relay is not connected to the peer asked for.
pub(crate) const NOT_CONNECTED: u32 = 0x02;
/// The peer asked for does not support the extension.
pub(crate) const NO_SUPPORT: u32 = 0x03;
/// The peer asked for is the relay itself.
pub(crate) const NO_SELF: u32 = 0x04;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// From the initiator to the relay: introduce me to `address`.
    Rendezvous,
    /// From the relay to both peers: connect to `address`.
    Connect,
    /// From the relay to the initiator: it could not, for `error`.
    Failed,
}

/// One `ut_holepunch` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Message {
    pub kind: Kind,
    pub address: SocketAddr,
    pub error: u32,
}

impl Message {
    pub(crate) fn new(kind: Kind, address: SocketAddr, error: u32) -> Self {
        Self {
            kind,
            address,
            error,
        }
    }

    /// Message type, address type, address, port, and error code, each
    /// big-endian. libtorrent leaves the error code out except in
    /// `failed` messages, so it is optional when reading.
    pub(crate) fn decode(payload: &[u8]) -> Option<Self> {
        let (&kind, rest) = payload.split_first()?;
        let kind = match kind {
            0 => Kind::Rendezvous,
            1 => Kind::Connect,
            2 => Kind::Failed,
            _ => return None,
        };
        let (&family, rest) = rest.split_first()?;
        let (ip, rest): (IpAddr, &[u8]) = match family {
            0 => (
                Ipv4Addr::from(<[u8; 4]>::try_from(rest.get(..4)?).ok()?).into(),
                &rest[4..],
            ),
            1 => (
                Ipv6Addr::from(<[u8; 16]>::try_from(rest.get(..16)?).ok()?).into(),
                &rest[16..],
            ),
            _ => return None,
        };
        let port = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?);
        let error = rest.get(2..6).map_or(0, |bytes| {
            u32::from_be_bytes(bytes.try_into().unwrap_or_default())
        });
        Some(Self::new(kind, SocketAddr::new(ip, port), error))
    }

    /// Written in full, error code included, as the BEP lays it out.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let kind = match self.kind {
            Kind::Rendezvous => 0,
            Kind::Connect => 1,
            Kind::Failed => 2,
        };
        let mut bytes = vec![kind];
        match crate::net::canonical(self.address).ip() {
            IpAddr::V4(ip) => {
                bytes.push(0);
                bytes.extend(ip.octets());
            }
            IpAddr::V6(ip) => {
                bytes.push(1);
                bytes.extend(ip.octets());
            }
        }
        bytes.extend(self.address.port().to_be_bytes());
        bytes.extend(self.error.to_be_bytes());
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_messages_as_bep_55_lays_them_out() {
        let message = Message::new(Kind::Connect, "203.0.113.7:51413".parse().unwrap(), 0);
        let bytes = message.encode();
        assert_eq!(
            bytes,
            [1, 0, 203, 0, 113, 7, 0xc8, 0xd5, 0, 0, 0, 0],
            "type, IPv4, address, port, error"
        );
        assert_eq!(Message::decode(&bytes), Some(message));
        // libtorrent's, without the error code.
        assert_eq!(Message::decode(&bytes[..8]), Some(message));
        let failed = Message::new(
            Kind::Failed,
            "[2001:db8::1]:6881".parse().unwrap(),
            NO_SUPPORT,
        );
        assert_eq!(failed.encode().len(), 2 + 16 + 2 + 4);
        assert_eq!(Message::decode(&failed.encode()), Some(failed));
        assert_eq!(
            Message::decode(&[9, 0, 1, 2, 3, 4, 0, 1]),
            None,
            "unknown type"
        );
        assert_eq!(
            Message::decode(&[0, 2, 1, 2, 3, 4, 0, 1]),
            None,
            "unknown family"
        );
        assert_eq!(Message::decode(&[0, 0, 1, 2]), None, "cut short");
    }
}
