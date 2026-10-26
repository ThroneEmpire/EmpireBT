//! Torrent metadata exchange over the extension protocol (BEP 9, BEP 10):
//! how a magnet link becomes a full torrent.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};
use crate::{Handshake, PeerConnection, PeerMessage, TransferProgress};

/// The extension message id we assign to `ut_metadata`. Peers send their
/// metadata messages to this id.
pub(crate) const UT_METADATA: u8 = 1;
const PIECE_SIZE: usize = 16 * 1024;
/// Largest info dictionary accepted from a peer.
const MAX_METADATA_SIZE: usize = 16 * 1024 * 1024;

/// Our BEP 10 extension handshake, as seeders send it.
#[cfg(test)]
pub(crate) fn extension_handshake(metadata_size: Option<usize>) -> PeerMessage {
    extension_handshake_with(&ExtensionOffer {
        metadata_size,
        client_version: true,
        ..ExtensionOffer::default()
    })
}

/// What our extension handshake offers, beyond metadata exchange.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExtensionOffer {
    pub metadata_size: Option<usize>,
    /// Offer peer exchange (never for private torrents).
    pub pex: bool,
    /// The peer's address as we see it, so it can learn its public IP.
    pub your_ip: Option<std::net::IpAddr>,
    /// Our listen port, which a peer we connected to cannot otherwise
    /// know.
    pub listen_port: Option<u16>,
    /// Name the client and its version (`v`); not in anonymous mode.
    pub client_version: bool,
    /// Offer to relay hole punches (BEP 55; never for private torrents).
    pub holepunch: bool,
}

pub(crate) fn extension_handshake_with(offer: &ExtensionOffer) -> PeerMessage {
    let mut extensions = vec![(&b"ut_metadata"[..], Value::Int(UT_METADATA.into()))];
    if offer.pex {
        extensions.push((b"ut_pex", Value::Int(crate::pex::UT_PEX.into())));
    }
    if offer.holepunch {
        extensions.push((
            b"ut_holepunch",
            Value::Int(crate::holepunch::UT_HOLEPUNCH.into()),
        ));
    }
    let mut entries = vec![
        (
            b"m".to_vec(),
            Value::Dict(
                extensions
                    .into_iter()
                    .map(|(name, id)| (name.to_vec(), id))
                    .collect(),
            ),
        ),
        (b"reqq".to_vec(), Value::Int(250)),
    ];
    if offer.client_version {
        entries.push((
            b"v".to_vec(),
            Value::bytes(format!("EmpireBT {}", env!("CARGO_PKG_VERSION"))),
        ));
    }
    if let Some(size) = offer.metadata_size {
        entries.push((b"metadata_size".to_vec(), Value::Int(size as i64)));
    }
    if let Some(ip) = offer.your_ip {
        let bytes = match ip {
            std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
            std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        entries.push((b"yourip".to_vec(), Value::bytes(bytes)));
    }
    if let Some(port) = offer.listen_port {
        entries.push((b"p".to_vec(), Value::Int(port.into())));
    }
    PeerMessage::Extended {
        id: 0,
        payload: Value::Dict(entries.into_iter().collect()).encode(),
    }
}

/// What a peer's extension handshake told us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RemoteMetadata {
    /// The id the peer wants its `ut_metadata` messages sent to.
    pub(crate) message_id: Option<u8>,
    pub(crate) size: Option<usize>,
    /// The id for its `ut_pex` messages, if it exchanges peers.
    pub(crate) pex_id: Option<u8>,
    /// The id for its `ut_holepunch` messages (BEP 55), if it has them.
    pub(crate) holepunch_id: Option<u8>,
    /// The port it listens on (`p`).
    pub(crate) listen_port: Option<u16>,
    /// Our address as it sees us (`yourip`).
    pub(crate) your_ip: Option<std::net::IpAddr>,
    /// How many requests it queues (`reqq`).
    pub(crate) reqq: Option<usize>,
    /// The client it says it is (`v`), cleaned up for display.
    pub(crate) client: Option<String>,
}

pub(crate) fn parse_extension_handshake(
    payload: &[u8],
) -> Result<RemoteMetadata, crate::BackendError> {
    let handshake = bencode::decode(payload)?;
    let message_id = handshake
        .get(b"m")
        .and_then(|map| map.get_int(b"ut_metadata"))
        .and_then(|id| u8::try_from(id).ok())
        .filter(|id| *id != 0);
    let size = handshake
        .get_int(b"metadata_size")
        .and_then(|size| usize::try_from(size).ok());
    let pex_id = handshake
        .get(b"m")
        .and_then(|map| map.get_int(b"ut_pex"))
        .and_then(|id| u8::try_from(id).ok())
        .filter(|id| *id != 0);
    let holepunch_id = handshake
        .get(b"m")
        .and_then(|map| map.get_int(b"ut_holepunch"))
        .and_then(|id| u8::try_from(id).ok())
        .filter(|id| *id != 0);
    let listen_port = handshake
        .get_int(b"p")
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0);
    let your_ip = handshake
        .get_bytes(b"yourip")
        .and_then(crate::pex::parse_ip);
    let reqq = handshake
        .get_int(b"reqq")
        .and_then(|reqq| usize::try_from(reqq).ok())
        .filter(|reqq| *reqq > 0);
    // Shown to the user: printable characters only, and not too many.
    let client = handshake
        .get_bytes(b"v")
        .map(|bytes| {
            String::from_utf8_lossy(bytes)
                .chars()
                .filter(|character| !character.is_control())
                .take(48)
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .filter(|client| !client.is_empty());
    Ok(RemoteMetadata {
        message_id,
        size,
        pex_id,
        holepunch_id,
        listen_port,
        your_ip,
        reqq,
        client,
    })
}

/// Seeder side: answers one extension message from a peer. Returns the
/// replies to send. `remote` remembers the peer's handshake.
pub(crate) fn serve_extended(
    id: u8,
    payload: &[u8],
    info: Option<&[u8]>,
    remote: &mut RemoteMetadata,
) -> Result<Vec<PeerMessage>, crate::BackendError> {
    if id == 0 {
        *remote = parse_extension_handshake(payload)?;
        return Ok(Vec::new());
    }
    if id != UT_METADATA {
        return Ok(Vec::new());
    }
    let Some(reply_id) = remote.message_id else {
        return Ok(Vec::new());
    };
    let (message, _) = bencode::decode_prefix(payload)?;
    if message.get_int(b"msg_type") != Some(0) {
        return Ok(Vec::new());
    }
    let piece = message
        .get_int(b"piece")
        .and_then(|piece| usize::try_from(piece).ok());
    let reply = match (info, piece) {
        (Some(info), Some(piece)) if piece * PIECE_SIZE < info.len() => {
            let start = piece * PIECE_SIZE;
            let end = (start + PIECE_SIZE).min(info.len());
            let mut bytes = Value::dict([
                (b"msg_type", Value::Int(1)),
                (b"piece", Value::Int(piece as i64)),
                (b"total_size", Value::Int(info.len() as i64)),
            ])
            .encode();
            bytes.extend(&info[start..end]);
            bytes
        }
        _ => Value::dict([
            (b"msg_type", Value::Int(2)),
            (b"piece", Value::Int(piece.unwrap_or(0) as i64)),
        ])
        .encode(),
    };
    Ok(vec![PeerMessage::Extended {
        id: reply_id,
        payload: reply,
    }])
}

/// A torrent's metadata as fetched from a peer: the info dictionary and,
/// for v2 and hybrid torrents, the piece layers that live outside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub info: Vec<u8>,
    pub piece_layers: PieceLayers,
}

/// v2 piece layers: each multi-piece file's pieces root with its piece hashes.
pub type PieceLayers = Vec<([u8; 32], Vec<[u8; 32]>)>;

/// Downloads a torrent's info dictionary from one peer and verifies it
/// against the link's hash; for v2 and hybrid torrents, also fetches and
/// verifies the piece layers with BEP 52 hash requests.
pub fn fetch_metadata_from_peer(
    address: SocketAddr,
    link: &crate::MagnetLink,
    peer_id: [u8; 20],
    timeout: Duration,
) -> Result<Metadata, crate::BackendError> {
    fetch_metadata_from_peer_with(
        address,
        link,
        peer_id,
        timeout,
        &crate::ConnectOptions::default(),
    )
}

/// [`fetch_metadata_from_peer`] with given connection options.
pub fn fetch_metadata_from_peer_with(
    address: SocketAddr,
    link: &crate::MagnetLink,
    peer_id: [u8; 20],
    timeout: Duration,
    options: &crate::ConnectOptions,
) -> Result<Metadata, crate::BackendError> {
    let mut handshake = Handshake::new(*link.info_hash(), peer_id)
        .with_extensions()
        .with_dht();
    // Joining on the truncated v2 hash is itself a v2 connection. On a v1
    // hash the v2 bit would invite a hybrid peer to upgrade the connection
    // to v2, which this client does not implement.
    if link.is_v2_only() {
        handshake = handshake.with_v2_support();
    }
    let mut connection = PeerConnection::connect_with(address, &handshake, timeout, options)?;
    if connection.peer_id() == &peer_id {
        return Err(invalid("connected to ourselves"));
    }
    if !connection.supports_extensions() {
        return Err(invalid("peer does not support the extension protocol"));
    }
    connection.send(&extension_handshake_with(&ExtensionOffer {
        client_version: crate::identity::identifies(&peer_id),
        ..ExtensionOffer::default()
    }))?;

    let mut remote = RemoteMetadata::default();
    let mut pieces: Vec<Option<Vec<u8>>> = Vec::new();
    let mut total_size = 0;
    // Bound the exchange so a peer trickling unrelated messages cannot keep
    // the fetch alive forever.
    for _ in 0..10_000 {
        let PeerMessage::Extended { id, payload } = connection.receive()? else {
            continue;
        };
        if id == 0 {
            remote = parse_extension_handshake(&payload)?;
            let message_id = remote
                .message_id
                .ok_or_else(|| invalid("peer does not offer torrent metadata"))?;
            total_size = remote
                .size
                .filter(|size| (1..=MAX_METADATA_SIZE).contains(size))
                .ok_or_else(|| invalid("peer reported an invalid metadata size"))?;
            pieces = vec![None; total_size.div_ceil(PIECE_SIZE)];
            for piece in 0..pieces.len() {
                connection.send(&PeerMessage::Extended {
                    id: message_id,
                    payload: Value::dict([
                        (b"msg_type", Value::Int(0)),
                        (b"piece", Value::Int(piece as i64)),
                    ])
                    .encode(),
                })?;
            }
            continue;
        }
        if id != UT_METADATA || remote.message_id.is_none() {
            continue;
        }
        let (message, used) = bencode::decode_prefix(&payload)?;
        match message.get_int(b"msg_type") {
            Some(1) => {}
            Some(2) => return Err(invalid("peer refused to share the metadata")),
            _ => continue,
        }
        let index = message
            .get_int(b"piece")
            .and_then(|piece| usize::try_from(piece).ok())
            .filter(|piece| *piece < pieces.len())
            .ok_or_else(|| invalid("peer sent a metadata piece out of range"))?;
        let data = &payload[used..];
        let expected = (total_size - index * PIECE_SIZE).min(PIECE_SIZE);
        if data.len() != expected {
            return Err(invalid("peer sent a metadata piece of the wrong size"));
        }
        pieces[index] = Some(data.to_vec());
        if pieces.iter().all(Option::is_some) {
            let info: Vec<u8> = pieces.into_iter().flatten().flatten().collect();
            if !matches_link(&info, link) {
                return Err(invalid(
                    "peer sent metadata that does not match the info hash",
                ));
            }
            let piece_layers = if link.is_v2_only() {
                fetch_piece_layers(&mut connection, &info)?
            } else {
                // Peers drop a second connection with the same peer id, so
                // close this one before reconnecting on the v2 hash.
                drop(connection);
                fetch_hybrid_layers(address, &info, peer_id, timeout, options)?
            };
            return Ok(Metadata { info, piece_layers });
        }
    }
    Err(invalid("peer did not finish sending the metadata"))
}

/// Tries up to `parallel` peers at a time and returns the first verified
/// info dictionary. Returns as soon as one peer succeeds; the remaining
/// attempts stop at their next peer boundary.
pub fn fetch_metadata(
    peers: &[SocketAddr],
    link: &crate::MagnetLink,
    peer_id: [u8; 20],
    timeout: Duration,
    parallel: usize,
    progress: &TransferProgress,
    options: &crate::ConnectOptions,
) -> Result<Metadata, crate::BackendError> {
    if peers.is_empty() {
        return Err(crate::BackendError::Tracker(
            "no peers found to fetch the metadata from yet".to_owned(),
        ));
    }
    let queue = Arc::new(Mutex::new(peers.iter().copied().collect::<VecDeque<_>>()));
    let done = Arc::new(AtomicBool::new(false));
    let (sender, results) = mpsc::channel();
    let workers = parallel.clamp(1, peers.len());
    for _ in 0..workers {
        let (queue, done, sender, link, options) = (
            Arc::clone(&queue),
            Arc::clone(&done),
            sender.clone(),
            link.clone(),
            options.clone(),
        );
        std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let Some(address) = queue.lock().ok().and_then(|mut queue| queue.pop_front())
                else {
                    return;
                };
                let result =
                    fetch_metadata_from_peer_with(address, &link, peer_id, timeout, &options);
                if sender.send(result).is_err() {
                    return;
                }
            }
        });
    }
    drop(sender);
    let mut last_error = None;
    loop {
        if progress.is_cancelled() {
            done.store(true, Ordering::Release);
            return Err(crate::BackendError::Cancelled);
        }
        match results.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(info)) => {
                done.store(true, Ordering::Release);
                return Ok(info);
            }
            Ok(Err(error)) => {
                progress.record_error(&error);
                last_error = Some(error);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(
                    last_error.unwrap_or_else(|| invalid("no peer could provide the metadata"))
                );
            }
        }
    }
}

/// Whether fetched metadata is the torrent the link names: by SHA-1 for a
/// v1 hash, by SHA-256 for a v2 hash, and by both when both are given.
fn matches_link(info: &[u8], link: &crate::MagnetLink) -> bool {
    let v1_ok = link.is_v2_only() || {
        let actual: [u8; 20] = Sha1::digest(info).into();
        &actual == link.info_hash()
    };
    let v2_ok = link.v2_hash().is_none_or(|expected| {
        let actual: [u8; 32] = sha2::Sha256::digest(info).into();
        &actual == expected
    });
    v1_ok && v2_ok
}

/// A hybrid torrent's piece layers. Hash requests are only valid on v2
/// connections, so this connects again using the torrent's v2 hash, which
/// the info dictionary now provides. v1 torrents need no layers.
fn fetch_hybrid_layers(
    address: SocketAddr,
    info: &[u8],
    peer_id: [u8; 20],
    timeout: Duration,
    options: &crate::ConnectOptions,
) -> Result<PieceLayers, crate::BackendError> {
    let needs_layers = crate::TorrentInfo::v2_files_of_info(info)?
        .iter()
        .any(|file| file.length() > 0);
    if !needs_layers {
        return Ok(Vec::new());
    }
    let v2: [u8; 32] = sha2::Sha256::digest(info).into();
    let handshake = Handshake::new(v2[..20].try_into().expect("32-byte hash"), peer_id)
        .with_extensions()
        .with_v2_support();
    let mut connection = PeerConnection::connect_with(address, &handshake, timeout, options)?;
    fetch_piece_layers(&mut connection, info)
}

/// Hashes requested per BEP 52 hash request (the protocol's maximum).
const MAX_HASHES_PER_REQUEST: usize = 512;

/// Requests the piece layer of every multi-piece v2 file and verifies each
/// response against the file's Merkle root. Single-piece files need no
/// layer; v1 torrents need none at all.
fn fetch_piece_layers(
    connection: &mut PeerConnection,
    info: &[u8],
) -> Result<PieceLayers, crate::BackendError> {
    let files = crate::TorrentInfo::v2_files_of_info(info)?;
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let piece_length = bencode::decode(info)?
        .get_int(b"piece length")
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| invalid("metadata has no piece length"))?;
    let base_layer = (piece_length / (16 * 1024)).trailing_zeros();
    let mut layers: PieceLayers = Vec::new();
    for file in &files {
        let count = file.piece_count(piece_length);
        if count < 2 || layers.iter().any(|(root, _)| root == file.pieces_root()) {
            continue;
        }
        let width = count.next_power_of_two();
        let step = width.min(MAX_HASHES_PER_REQUEST);
        // Uncles needed to reach the root from each requested range.
        let proof_layers = (width / step).trailing_zeros();
        let mut layer = vec![[0; 32]; count];
        for index in (0..width).step_by(step).filter(|index| *index < count) {
            connection.send(&PeerMessage::HashRequest {
                pieces_root: *file.pieces_root(),
                base_layer,
                index: index as u32,
                length: step as u32,
                proof_layers,
            })?;
            let hashes = loop {
                match connection.receive()? {
                    PeerMessage::Hashes {
                        pieces_root,
                        index: answered,
                        hashes,
                        proof_layers: proofs,
                        ..
                    } if pieces_root == *file.pieces_root() && answered as usize == index => {
                        if !crate::verify_hashes_response(
                            pieces_root,
                            base_layer,
                            answered,
                            &hashes,
                            &proofs,
                        ) {
                            return Err(invalid("peer sent piece hashes that fail their proof"));
                        }
                        break hashes;
                    }
                    PeerMessage::HashReject { pieces_root, .. }
                        if pieces_root == *file.pieces_root() =>
                    {
                        return Err(invalid("peer refused to share piece hashes"));
                    }
                    _ => {}
                }
            };
            // Entries past the last piece are padding.
            for (offset, hash) in hashes.into_iter().enumerate() {
                if let Some(slot) = layer.get_mut(index + offset) {
                    *slot = hash;
                }
            }
        }
        layers.push((*file.pieces_root(), layer));
    }
    Ok(layers)
}

fn invalid(message: &str) -> crate::BackendError {
    crate::BackendError::InvalidPeerMessage(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn anonymous_handshakes_leave_out_the_client_name() {
        let client = |offer: &ExtensionOffer| {
            let PeerMessage::Extended { payload, .. } = extension_handshake_with(offer) else {
                panic!("not an extended message");
            };
            parse_extension_handshake(&payload).unwrap().client
        };
        let named = client(&ExtensionOffer {
            client_version: true,
            ..ExtensionOffer::default()
        });
        assert!(named.is_some_and(|name| name.starts_with("EmpireBT")));
        assert_eq!(client(&ExtensionOffer::default()), None);
    }

    /// Serves `info` over ut_metadata to one peer, like a seeder would.
    fn metadata_peer(info: Vec<u8>, info_hash: [u8; 20]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let handshake = Handshake::new(info_hash, [8; 20]).with_extensions();
            let mut connection =
                PeerConnection::from_stream(stream, &handshake, Duration::from_secs(5)).unwrap();
            // Unrelated traffic first: the fetcher must ignore it.
            connection.send(&PeerMessage::Bitfield(vec![0])).unwrap();
            connection.send(&PeerMessage::Port(6881)).unwrap();
            connection
                .send(&extension_handshake(Some(info.len())))
                .unwrap();
            let mut remote = RemoteMetadata::default();
            while let Ok(message) = connection.receive() {
                if let PeerMessage::Extended { id, payload } = message {
                    for reply in serve_extended(id, &payload, Some(&info), &mut remote).unwrap() {
                        connection.send(&reply).unwrap();
                    }
                }
            }
        });
        address
    }

    fn link_for(hash: &[u8; 20]) -> crate::MagnetLink {
        let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        crate::MagnetLink::parse(&format!("magnet:?xt=urn:btih:{hex}")).unwrap()
    }

    #[test]
    fn fetches_and_verifies_multi_piece_metadata() {
        // Larger than one 16 KiB metadata piece.
        let mut info = b"d4:name4:test12:piece lengthi16384e6:pieces".to_vec();
        let pieces = vec![7_u8; 20 * 1000];
        info.extend(format!("{}:", pieces.len()).as_bytes());
        info.extend(&pieces);
        info.extend(b"6:lengthi16384000ee");
        let info_hash: [u8; 20] = Sha1::digest(&info).into();
        let address = metadata_peer(info.clone(), info_hash);

        let fetched = fetch_metadata_from_peer(
            address,
            &link_for(&info_hash),
            [9; 20],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(fetched.info, info);
        assert!(
            fetched.piece_layers.is_empty(),
            "v1 metadata has no piece layers"
        );
    }

    #[test]
    fn rejects_metadata_with_the_wrong_hash() {
        let info = b"d4:name4:teste".to_vec();
        let address = metadata_peer(info, [1; 20]);
        let error = fetch_metadata_from_peer(
            address,
            &link_for(&[1; 20]),
            [9; 20],
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn reads_the_private_flag() {
        let public = crate::TorrentInfo::from_metadata(
            b"d6:lengthi4e4:name4:test12:piece lengthi4e6:pieces20:\
\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03e",
            &[],
        )
        .unwrap();
        let private = crate::TorrentInfo::from_metadata(
            b"d6:lengthi4e4:name4:test12:piece lengthi4e6:pieces20:\
\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x037:privatei1ee",
            &[],
        )
        .unwrap();
        assert!(!public.is_private());
        assert!(private.is_private());
    }

    #[test]
    fn builds_a_torrent_from_fetched_metadata() {
        let info = b"d6:lengthi4e4:name4:test12:piece lengthi4e6:pieces20:\
\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03e";
        let trackers = vec![
            "udp://a.example:1".to_owned(),
            "http://b.example/announce".to_owned(),
        ];
        let torrent = crate::TorrentInfo::from_metadata(info, &trackers).unwrap();
        let expected: [u8; 20] = Sha1::digest(info).into();
        assert_eq!(torrent.info_hash(), &expected);
        assert_eq!(torrent.tracker_urls(), trackers);
        assert_eq!(torrent.info_bytes(), Some(&info[..]));
        let reparsed = crate::TorrentInfo::from_bytes(torrent.source_bytes()).unwrap();
        assert_eq!(reparsed.info_hash(), &expected);
    }
}
