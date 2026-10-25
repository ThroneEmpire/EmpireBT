use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const MAX_BLOCK_LENGTH: u32 = 16 * 1024;
const HANDSHAKE_LENGTH: usize = 68;
const PROTOCOL_NAME: &[u8] = b"BitTorrent protocol";
/// BEP 10: bit 0x10 of reserved byte 5.
const EXTENSION_BIT: u8 = 0x10;
/// BEP 5: bit 0x01 of reserved byte 7.
const DHT_BIT: u8 = 0x01;
/// BEP 6, the Fast Extension: bit 0x04 of reserved byte 7.
const FAST_BIT: u8 = 0x04;
/// How long to wait for a peer to accept a connection. Many addresses from
/// trackers and the DHT are offline, so waiting long wastes a slot.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The fixed-size handshake exchanged before peer-wire messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    reserved: [u8; 8],
}

/// How to reach peers: whether to encrypt, and whether connections may use
/// uTP instead of TCP.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    encryption: crate::Encryption,
    utp: Option<Arc<crate::UtpSocket>>,
    prefer_utp: bool,
    /// Overrides [`CONNECT_TIMEOUT`] when set.
    dial_timeout: Option<Duration>,
    /// The network interface TCP connections leave from.
    binding: crate::Binding,
}

impl ConnectOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_encryption(mut self, encryption: crate::Encryption) -> Self {
        self.encryption = encryption;
        self
    }

    /// Lets connections use uTP over `socket`: after TCP fails, or first
    /// when preferred.
    pub fn with_utp(mut self, socket: Option<Arc<crate::UtpSocket>>) -> Self {
        self.utp = socket;
        self
    }

    /// Tries uTP before TCP, as libtorrent does by default.
    pub fn prefer_utp(mut self, prefer: bool) -> Self {
        self.prefer_utp = prefer;
        self
    }

    /// How long to wait for a peer to accept the connection and answer the
    /// handshake (default [`CONNECT_TIMEOUT`]).
    pub fn with_dial_timeout(mut self, timeout: Duration) -> Self {
        self.dial_timeout = Some(timeout);
        self
    }

    /// Connects only through this network interface.
    pub fn with_binding(mut self, binding: crate::Binding) -> Self {
        self.binding = binding;
        self
    }

    pub fn binding(&self) -> &crate::Binding {
        &self.binding
    }

    pub fn encryption(&self) -> crate::Encryption {
        self.encryption
    }
}

/// The byte stream under a peer connection.
#[derive(Debug)]
pub(crate) enum Transport {
    Tcp(TcpStream),
    Utp(crate::UtpStream),
}

impl Read for Transport {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buffer),
            Self::Utp(stream) => stream.read(buffer),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(data),
            Self::Utp(stream) => stream.write(data),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            Self::Utp(stream) => stream.flush(),
        }
    }
}

/// A connection to one BitTorrent peer, over TCP or uTP.
#[derive(Debug)]
pub struct PeerConnection {
    stream: Transport,
    peer_id: [u8; 20],
    supports_v2: bool,
    remote_reserved: [u8; 8],
    /// Present when the stream is RC4-encrypted (MSE).
    ciphers: Option<crate::mse::Ciphers>,
    /// The encryption handshake was used, even if the stream then went on
    /// in plain text.
    obfuscated: bool,
    /// Plain-text bytes read ahead of the current message.
    buffered: Vec<u8>,
}

impl PeerConnection {
    /// Connects to a peer over TCP and completes the BitTorrent handshake,
    /// trying an encrypted connection first (see [`Encryption::Enabled`]).
    ///
    /// [`Encryption::Enabled`]: crate::Encryption::Enabled
    pub fn connect(
        address: SocketAddr,
        handshake: &Handshake,
        timeout: Duration,
    ) -> Result<Self, crate::BackendError> {
        Self::connect_with(address, handshake, timeout, &ConnectOptions::default())
    }

    /// Connects to a peer with the given options. With
    /// [`Encryption::Enabled`], a peer that fails the encrypted handshake
    /// is retried in plain text on a new connection, as libtorrent does.
    /// With uTP available, a connection that fails before the BitTorrent
    /// handshake completes is retried over the other transport: uTP after
    /// TCP, or TCP after uTP when uTP is preferred. A peer that does not
    /// accept the connection, or does not answer our handshake, within
    /// [`CONNECT_TIMEOUT`] is not retried: most such addresses from
    /// trackers and the DHT are offline, and some networks accept
    /// connections that lead nowhere.
    ///
    /// [`Encryption::Enabled`]: crate::Encryption::Enabled
    pub fn connect_with(
        address: SocketAddr,
        handshake: &Handshake,
        timeout: Duration,
        options: &ConnectOptions,
    ) -> Result<Self, crate::BackendError> {
        use crate::Encryption;
        let io = |error: std::io::Error| crate::BackendError::Io(error.to_string());
        let dial_timeout = timeout.min(options.dial_timeout.unwrap_or(CONNECT_TIMEOUT));
        let unreachable = std::cell::Cell::new(false);
        // Dialing errors note whether the peer answered at all.
        let dial_error = |error: std::io::Error| {
            use std::io::ErrorKind;
            if matches!(
                error.kind(),
                ErrorKind::TimedOut
                    | ErrorKind::WouldBlock
                    | ErrorKind::HostUnreachable
                    | ErrorKind::NetworkUnreachable
            ) {
                unreachable.set(true);
            }
            io(error)
        };
        let open = |over_utp: bool| -> Result<Transport, crate::BackendError> {
            match (&options.utp, over_utp) {
                (Some(socket), true) => {
                    let mut stream = crate::UtpStream::connect(socket, address, dial_timeout)
                        .map_err(dial_error)?;
                    stream.set_read_timeout(Some(dial_timeout));
                    stream.set_write_timeout(Some(dial_timeout));
                    Ok(Transport::Utp(stream))
                }
                _ => {
                    let stream = options
                        .binding
                        .connect_tcp(address, dial_timeout)
                        .map_err(dial_error)?;
                    // The handshake gets the dialing deadline too.
                    stream
                        .set_read_timeout(Some(dial_timeout))
                        .and_then(|_| stream.set_write_timeout(Some(dial_timeout)))
                        .map_err(io)?;
                    Ok(Transport::Tcp(stream))
                }
            }
        };
        let plain = |mut stream: Transport| {
            stream.write_all(&handshake.encode()).map_err(io)?;
            Self::finish(stream, handshake, None, false)
        };
        let attempt = |over_utp: bool| match options.encryption {
            Encryption::Disabled => plain(open(over_utp)?),
            Encryption::Required | Encryption::Enabled => {
                let allow_plaintext = options.encryption == Encryption::Enabled;
                let mut stream = open(over_utp)?;
                let started = std::time::Instant::now();
                // The BitTorrent handshake rides along as the initial payload.
                match crate::mse::initiate(
                    &mut stream,
                    handshake.info_hash(),
                    &handshake.encode(),
                    allow_plaintext,
                ) {
                    Ok(ciphers) => Self::finish(stream, handshake, ciphers, true),
                    // A peer that refused encryption may accept plain text;
                    // one that never answered will not.
                    Err(_) if allow_plaintext && started.elapsed() < dial_timeout => {
                        drop(stream);
                        plain(open(over_utp)?)
                    }
                    Err(error) => Err(error),
                }
            }
        };
        let order: &[bool] = match (&options.utp, options.prefer_utp) {
            (None, _) => &[false],
            (Some(_), true) => &[true, false],
            (Some(_), false) => &[false, true],
        };
        let mut failure = None;
        for &over_utp in order {
            let started = std::time::Instant::now();
            match attempt(over_utp) {
                Ok(mut connection) => {
                    connection.set_read_timeout(timeout)?;
                    connection.set_write_timeout(timeout)?;
                    return Ok(connection);
                }
                Err(error) if unreachable.get() || started.elapsed() >= dial_timeout => {
                    return Err(error);
                }
                // Only transport failures are worth another transport.
                Err(error @ crate::BackendError::Io(_)) => failure = failure.or(Some(error)),
                Err(error) => return Err(error),
            }
        }
        Err(failure.expect("at least one transport was tried"))
    }

    /// The stream, its encryption, and anything read ahead of the next
    /// message, for an event loop to take over.
    pub(crate) fn into_parts(self) -> (Transport, Option<crate::mse::Ciphers>, bool, Vec<u8>) {
        (self.stream, self.ciphers, self.obfuscated, self.buffered)
    }

    /// Whether the connection runs over uTP rather than TCP.
    pub fn is_utp(&self) -> bool {
        matches!(self.stream, Transport::Utp(_))
    }

    /// Reads and checks the remote handshake once ours is sent.
    fn finish(
        stream: Transport,
        handshake: &Handshake,
        ciphers: Option<crate::mse::Ciphers>,
        obfuscated: bool,
    ) -> Result<Self, crate::BackendError> {
        let mut connection = Self {
            stream,
            peer_id: [0; 20],
            supports_v2: false,
            remote_reserved: [0; 8],
            ciphers,
            obfuscated,
            buffered: Vec::new(),
        };
        let received = connection.read_exact(HANDSHAKE_LENGTH)?;
        let remote = Handshake::decode(&received)?;
        if !remote.matches_info_hash(handshake.info_hash()) {
            return Err(crate::BackendError::InvalidPeerMessage(
                "peer handshake has a different info hash".to_owned(),
            ));
        }
        // Our own address can come back from trackers, the DHT, or a NAT.
        if remote.peer_id() == handshake.peer_id() {
            return Err(crate::BackendError::InvalidPeerMessage(
                "connected to ourselves".to_owned(),
            ));
        }
        connection.peer_id = *remote.peer_id();
        connection.supports_v2 = remote.supports_v2();
        connection.remote_reserved = *remote.reserved();
        Ok(connection)
    }

    /// Whether the connection is RC4-encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.ciphers.is_some()
    }

    /// Whether the encryption handshake was used, even if the stream then
    /// went on in plain text.
    pub fn is_obfuscated(&self) -> bool {
        self.obfuscated
    }

    /// Wraps an already-handshaken stream, useful for accepted inbound peers.
    pub fn from_stream(
        mut stream: TcpStream,
        handshake: &Handshake,
        timeout: Duration,
    ) -> Result<Self, crate::BackendError> {
        use crate::mse::{Opening, Responder};
        let io = |error: std::io::Error| crate::BackendError::Io(error.to_string());
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|_| stream.set_write_timeout(Some(timeout)))
            .map_err(io)?;
        // Peers open with either a plain handshake or an encrypted one.
        let mut input = Vec::new();
        while crate::mse::classify(&input) == Opening::Undecided {
            read_more(&mut stream, &mut input)?;
        }
        let mut ciphers = None;
        let obfuscated = crate::mse::classify(&input) == Opening::Encrypted;
        if obfuscated {
            let info_hash = *handshake.info_hash();
            let mut responder = Responder::new(true);
            loop {
                let mut output = Vec::new();
                let established = responder.advance(&mut input, &mut output, |req2| {
                    (*req2 == crate::mse::obfuscated_hash(&info_hash)).then_some(info_hash)
                })?;
                stream.write_all(&output).map_err(io)?;
                if let Some(established) = established {
                    ciphers = established.ciphers;
                    break;
                }
                read_more(&mut stream, &mut input)?;
            }
        }
        let mut connection = Self {
            stream: Transport::Tcp(stream),
            peer_id: [0; 20],
            supports_v2: false,
            remote_reserved: [0; 8],
            ciphers,
            obfuscated,
            buffered: input,
        };
        let received = connection.read_exact(HANDSHAKE_LENGTH)?;
        let remote = Handshake::decode(&received)?;
        if !remote.matches_info_hash(handshake.info_hash()) {
            return Err(crate::BackendError::InvalidPeerMessage(
                "peer handshake has a different info hash".to_owned(),
            ));
        }
        connection.send_bytes(handshake.encode().to_vec())?;
        connection.peer_id = *remote.peer_id();
        connection.supports_v2 = remote.supports_v2();
        connection.remote_reserved = *remote.reserved();
        Ok(connection)
    }

    pub fn peer_id(&self) -> &[u8; 20] {
        &self.peer_id
    }

    pub fn supports_v2(&self) -> bool {
        self.supports_v2
    }

    /// Whether the remote handshake advertised the BEP 10 extension protocol.
    pub fn supports_extensions(&self) -> bool {
        self.remote_reserved[5] & EXTENSION_BIT != 0
    }

    /// Whether the remote handshake advertised BEP 5 DHT support.
    pub fn supports_dht(&self) -> bool {
        self.remote_reserved[7] & DHT_BIT != 0
    }

    /// Whether the remote handshake advertised the BEP 6 Fast Extension.
    /// It is in use only if ours did too.
    pub fn supports_fast(&self) -> bool {
        self.remote_reserved[7] & FAST_BIT != 0
    }

    pub fn send(&mut self, message: &PeerMessage) -> Result<(), crate::BackendError> {
        self.send_bytes(message.encode()?)
    }

    fn send_bytes(&mut self, mut bytes: Vec<u8>) -> Result<(), crate::BackendError> {
        if let Some(ciphers) = &mut self.ciphers {
            ciphers.encrypt.apply(&mut bytes);
        }
        self.stream
            .write_all(&bytes)
            .map_err(|error| crate::BackendError::Io(error.to_string()))
    }

    /// How long [`PeerConnection::poll`] waits for data.
    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), crate::BackendError> {
        match &mut self.stream {
            Transport::Tcp(stream) => stream
                .set_read_timeout(Some(timeout))
                .map_err(|error| crate::BackendError::Io(error.to_string())),
            Transport::Utp(stream) => {
                stream.set_read_timeout(Some(timeout));
                Ok(())
            }
        }
    }

    fn set_write_timeout(&mut self, timeout: Duration) -> Result<(), crate::BackendError> {
        match &mut self.stream {
            Transport::Tcp(stream) => stream
                .set_write_timeout(Some(timeout))
                .map_err(|error| crate::BackendError::Io(error.to_string())),
            Transport::Utp(stream) => {
                stream.set_write_timeout(Some(timeout));
                Ok(())
            }
        }
    }

    /// Waits up to the read timeout for one message. Returns `None` if no
    /// complete message arrived in time; a partial one stays buffered.
    pub fn poll(&mut self) -> Result<Option<PeerMessage>, crate::BackendError> {
        if let Some(message) = self.take_buffered()? {
            return Ok(Some(message));
        }
        let mut chunk = [0; 32 * 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err(crate::BackendError::Io(
                "peer closed the connection".to_owned(),
            )),
            Ok(read) => {
                if let Some(ciphers) = &mut self.ciphers {
                    ciphers.decrypt.apply(&mut chunk[..read]);
                }
                self.buffered.extend_from_slice(&chunk[..read]);
                self.take_buffered()
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(crate::BackendError::Io(error.to_string())),
        }
    }

    /// A complete message from the read-ahead buffer, if one is there.
    fn take_buffered(&mut self) -> Result<Option<PeerMessage>, crate::BackendError> {
        let Some(prefix) = self.buffered.get(..4) else {
            return Ok(None);
        };
        let length = u32::from_be_bytes(prefix.try_into().expect("four bytes")) as usize;
        if length > 1 << 20 {
            return Err(invalid("peer message exceeds maximum size"));
        }
        if self.buffered.len() < 4 + length {
            return Ok(None);
        }
        let bytes: Vec<u8> = self.buffered.drain(..4 + length).collect();
        PeerMessage::decode(&bytes).map(Some)
    }

    pub fn receive(&mut self) -> Result<PeerMessage, crate::BackendError> {
        let prefix = self.read_exact(4)?;
        let length = u32::from_be_bytes(
            prefix
                .as_slice()
                .try_into()
                .expect("length prefix is four bytes"),
        );
        if length > 1 << 20 {
            return Err(invalid("peer message exceeds maximum size"));
        }
        let mut bytes = prefix.to_vec();
        bytes.extend(self.read_exact(length as usize)?);
        PeerMessage::decode(&bytes)
    }

    fn read_exact(&mut self, length: usize) -> Result<Vec<u8>, crate::BackendError> {
        let ahead = self.buffered.len().min(length);
        let mut bytes: Vec<u8> = self.buffered.drain(..ahead).collect();
        if bytes.len() < length {
            let mut rest = read_exact(&mut self.stream, length - bytes.len())?;
            if let Some(ciphers) = &mut self.ciphers {
                ciphers.decrypt.apply(&mut rest);
            }
            bytes.extend(rest);
        }
        Ok(bytes)
    }
}

/// Appends whatever the peer has sent, waiting for at least one byte.
fn read_more(stream: &mut impl Read, input: &mut Vec<u8>) -> Result<(), crate::BackendError> {
    let mut buffer = [0; 4096];
    match stream.read(&mut buffer) {
        Ok(0) => Err(crate::BackendError::Io(
            "peer closed the connection".to_owned(),
        )),
        Ok(read) => {
            input.extend_from_slice(&buffer[..read]);
            Ok(())
        }
        Err(error) => Err(crate::BackendError::Io(error.to_string())),
    }
}

fn read_exact(stream: &mut impl Read, length: usize) -> Result<Vec<u8>, crate::BackendError> {
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| crate::BackendError::Io(error.to_string()))?;
    Ok(bytes)
}

impl Handshake {
    pub fn new(info_hash: [u8; 20], peer_id: [u8; 20]) -> Self {
        Self {
            info_hash,
            peer_id,
            reserved: [0; 8],
        }
    }

    /// Enables a reserved protocol extension bit.
    ///
    /// Bit numbering starts at the least-significant bit of the last
    /// reserved byte, matching the wire representation used by BEP 10.
    pub fn with_extension_bit(mut self, byte: usize, bit: u8) -> Result<Self, crate::BackendError> {
        if byte >= self.reserved.len() || bit >= 8 {
            return Err(invalid("reserved extension bit is out of range"));
        }
        self.reserved[byte] |= 1 << bit;
        Ok(self)
    }

    /// Advertises support for the BitTorrent v2 peer protocol.
    ///
    /// BEP 52 assigns the fourth most-significant bit of the final reserved
    /// byte to the v2 capability flag.
    pub fn with_v2_support(mut self) -> Self {
        self.reserved[7] |= 1 << 4;
        self
    }

    pub fn supports_v2(&self) -> bool {
        self.reserved[7] & (1 << 4) != 0
    }

    /// Whether this handshake advertises the BEP 6 Fast Extension.
    pub fn supports_fast(&self) -> bool {
        self.reserved[7] & FAST_BIT != 0
    }

    /// Whether this handshake advertises the BEP 10 extension protocol.
    pub fn supports_extensions(&self) -> bool {
        self.reserved[5] & EXTENSION_BIT != 0
    }

    /// Advertises the BEP 10 extension protocol (used for metadata exchange).
    pub fn with_extensions(mut self) -> Self {
        self.reserved[5] |= EXTENSION_BIT;
        self
    }

    /// Advertises the BEP 6 Fast Extension.
    pub fn with_fast(mut self) -> Self {
        self.reserved[7] |= FAST_BIT;
        self
    }

    /// Advertises BEP 5 DHT support.
    pub fn with_dht(mut self) -> Self {
        self.reserved[7] |= DHT_BIT;
        self
    }

    pub fn info_hash(&self) -> &[u8; 20] {
        &self.info_hash
    }

    pub fn peer_id(&self) -> &[u8; 20] {
        &self.peer_id
    }

    pub fn reserved(&self) -> &[u8; 8] {
        &self.reserved
    }

    pub fn encode(&self) -> [u8; HANDSHAKE_LENGTH] {
        let mut encoded = [0; HANDSHAKE_LENGTH];
        encoded[0] = PROTOCOL_NAME.len() as u8;
        encoded[1..20].copy_from_slice(PROTOCOL_NAME);
        encoded[20..28].copy_from_slice(&self.reserved);
        encoded[28..48].copy_from_slice(&self.info_hash);
        encoded[48..68].copy_from_slice(&self.peer_id);
        encoded
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, crate::BackendError> {
        if bytes.len() != HANDSHAKE_LENGTH {
            return Err(invalid("handshake must be exactly 68 bytes"));
        }
        if bytes[0] != PROTOCOL_NAME.len() as u8 || &bytes[1..20] != PROTOCOL_NAME {
            return Err(invalid("unsupported peer protocol"));
        }
        Ok(Self {
            info_hash: bytes[28..48]
                .try_into()
                .expect("handshake info hash length"),
            peer_id: bytes[48..68].try_into().expect("handshake peer id length"),
            reserved: bytes[20..28].try_into().expect("handshake reserved length"),
        })
    }

    pub fn matches_info_hash(&self, expected: &[u8; 20]) -> bool {
        &self.info_hash == expected
    }
}

/// Messages exchanged after the BitTorrent handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerMessage {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have {
        piece_index: u32,
    },
    Bitfield(Vec<u8>),
    Request {
        piece_index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        piece_index: u32,
        begin: u32,
        block: Vec<u8>,
    },
    Cancel {
        piece_index: u32,
        begin: u32,
        length: u32,
    },
    HashRequest {
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
    },
    Hashes {
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: Vec<[u8; 32]>,
        hashes: Vec<[u8; 32]>,
    },
    HashReject {
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
    },
    /// BEP 5: the sender's DHT port.
    Port(u16),
    /// BEP 6: suggests a piece to download.
    SuggestPiece {
        piece_index: u32,
    },
    /// BEP 6: instead of a bitfield, the sender has every piece.
    HaveAll,
    /// BEP 6: instead of a bitfield, the sender has no pieces.
    HaveNone,
    /// BEP 6: the sender will not answer this request.
    RejectRequest {
        piece_index: u32,
        begin: u32,
        length: u32,
    },
    /// BEP 6: the sender will serve this piece even while choking us.
    AllowedFast {
        piece_index: u32,
    },
    /// BEP 10 extension message; id 0 is the extension handshake.
    Extended {
        id: u8,
        payload: Vec<u8>,
    },
    /// A message id this client does not implement. Peers must ignore
    /// these rather than disconnect.
    Unknown {
        id: u8,
        payload: Vec<u8>,
    },
}

impl PeerMessage {
    /// Encodes a peer-wire message, excluding the initial protocol handshake.
    pub fn encode(&self) -> Result<Vec<u8>, crate::BackendError> {
        let (message_id, payload): (Option<u8>, Vec<u8>) = match self {
            Self::KeepAlive => return Ok(0u32.to_be_bytes().to_vec()),
            Self::Choke => (Some(0), Vec::new()),
            Self::Unchoke => (Some(1), Vec::new()),
            Self::Interested => (Some(2), Vec::new()),
            Self::NotInterested => (Some(3), Vec::new()),
            Self::Have { piece_index } => (Some(4), piece_index.to_be_bytes().to_vec()),
            Self::Bitfield(bits) => (Some(5), bits.clone()),
            Self::Request {
                piece_index,
                begin,
                length,
            }
            | Self::Cancel {
                piece_index,
                begin,
                length,
            }
            | Self::RejectRequest {
                piece_index,
                begin,
                length,
            } => {
                if *length == 0 || *length > MAX_BLOCK_LENGTH {
                    return Err(invalid("request block length must be 1..=16384"));
                }
                let id = match self {
                    Self::Request { .. } => 6,
                    Self::Cancel { .. } => 8,
                    _ => 16,
                };
                let mut payload = Vec::with_capacity(12);
                payload.extend(piece_index.to_be_bytes());
                payload.extend(begin.to_be_bytes());
                payload.extend(length.to_be_bytes());
                (Some(id), payload)
            }
            Self::Piece {
                piece_index,
                begin,
                block,
            } => {
                if block.is_empty() || block.len() > MAX_BLOCK_LENGTH as usize {
                    return Err(invalid("piece block length must be 1..=16384"));
                }
                let mut payload = Vec::with_capacity(8 + block.len());
                payload.extend(piece_index.to_be_bytes());
                payload.extend(begin.to_be_bytes());
                payload.extend(block);
                (Some(7), payload)
            }
            Self::HashRequest {
                pieces_root,
                base_layer,
                index,
                length,
                proof_layers,
            } => {
                validate_hash_request(*length)?;
                let mut payload = Vec::with_capacity(48);
                payload.extend(pieces_root);
                payload.extend(base_layer.to_be_bytes());
                payload.extend(index.to_be_bytes());
                payload.extend(length.to_be_bytes());
                payload.extend(proof_layers.to_be_bytes());
                (Some(21), payload)
            }
            Self::Hashes {
                pieces_root,
                base_layer,
                index,
                length,
                proof_layers,
                hashes,
            } => {
                validate_hash_request(*length)?;
                if hashes.len() != *length as usize {
                    return Err(invalid("hashes response has an invalid hash count"));
                }
                if proof_layers.len() > 64 {
                    return Err(invalid("hashes response has too many proof layers"));
                }
                let mut payload = Vec::with_capacity(48 + (proof_layers.len() + hashes.len()) * 32);
                payload.extend(pieces_root);
                payload.extend(base_layer.to_be_bytes());
                payload.extend(index.to_be_bytes());
                payload.extend(length.to_be_bytes());
                payload.extend((proof_layers.len() as u32).to_be_bytes());
                for hash in hashes.iter().chain(proof_layers) {
                    payload.extend(hash);
                }
                (Some(22), payload)
            }
            Self::HashReject {
                pieces_root,
                base_layer,
                index,
                length,
                proof_layers,
            } => {
                validate_hash_request(*length)?;
                let mut payload = Vec::with_capacity(48);
                payload.extend(pieces_root);
                payload.extend(base_layer.to_be_bytes());
                payload.extend(index.to_be_bytes());
                payload.extend(length.to_be_bytes());
                payload.extend(proof_layers.to_be_bytes());
                (Some(23), payload)
            }
            Self::Port(port) => (Some(9), port.to_be_bytes().to_vec()),
            Self::SuggestPiece { piece_index } => (Some(13), piece_index.to_be_bytes().to_vec()),
            Self::HaveAll => (Some(14), Vec::new()),
            Self::HaveNone => (Some(15), Vec::new()),
            Self::AllowedFast { piece_index } => (Some(17), piece_index.to_be_bytes().to_vec()),
            Self::Extended { id, payload } => {
                let mut bytes = Vec::with_capacity(1 + payload.len());
                bytes.push(*id);
                bytes.extend(payload);
                (Some(20), bytes)
            }
            Self::Unknown { id, payload } => (Some(*id), payload.clone()),
        };
        let id = message_id.expect("all non-keepalive messages have an id");
        let length = 1usize + payload.len();
        let length = u32::try_from(length).map_err(|_| invalid("peer message is too large"))?;
        let mut encoded = length.to_be_bytes().to_vec();
        encoded.push(id);
        encoded.extend(payload);
        Ok(encoded)
    }

    /// Decodes exactly one length-prefixed peer-wire message.
    pub fn decode(bytes: &[u8]) -> Result<Self, crate::BackendError> {
        if bytes.len() < 4 {
            return Err(invalid("message is missing its length prefix"));
        }
        let declared = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        if declared == 0 {
            if bytes.len() != 4 {
                return Err(invalid("keep-alive message has trailing bytes"));
            }
            return Ok(Self::KeepAlive);
        }
        if declared > 1 << 20 || bytes.len() != declared + 4 {
            return Err(invalid("message length does not match payload"));
        }
        let payload = &bytes[5..];
        let read_u32 = |offset: usize| -> Result<u32, crate::BackendError> {
            payload
                .get(offset..offset + 4)
                .and_then(|value| value.try_into().ok())
                .map(u32::from_be_bytes)
                .ok_or_else(|| invalid("message payload is truncated"))
        };
        match bytes[4] {
            0 if payload.is_empty() => Ok(Self::Choke),
            1 if payload.is_empty() => Ok(Self::Unchoke),
            2 if payload.is_empty() => Ok(Self::Interested),
            3 if payload.is_empty() => Ok(Self::NotInterested),
            4 if payload.len() == 4 => Ok(Self::Have {
                piece_index: read_u32(0)?,
            }),
            5 => Ok(Self::Bitfield(payload.to_vec())),
            6 | 8 | 16 if payload.len() == 12 => {
                let (piece_index, begin, length) = (read_u32(0)?, read_u32(4)?, read_u32(8)?);
                let message = match bytes[4] {
                    6 => Self::Request {
                        piece_index,
                        begin,
                        length,
                    },
                    8 => Self::Cancel {
                        piece_index,
                        begin,
                        length,
                    },
                    _ => Self::RejectRequest {
                        piece_index,
                        begin,
                        length,
                    },
                };
                message.encode()?;
                Ok(message)
            }
            13 if payload.len() == 4 => Ok(Self::SuggestPiece {
                piece_index: read_u32(0)?,
            }),
            14 if payload.is_empty() => Ok(Self::HaveAll),
            15 if payload.is_empty() => Ok(Self::HaveNone),
            17 if payload.len() == 4 => Ok(Self::AllowedFast {
                piece_index: read_u32(0)?,
            }),
            7 if payload.len() >= 9 => Ok(Self::Piece {
                piece_index: read_u32(0)?,
                begin: read_u32(4)?,
                block: payload[8..].to_vec(),
            }),
            21 | 23 if payload.len() == 48 => {
                let message = if bytes[4] == 21 {
                    Self::HashRequest {
                        pieces_root: payload[..32].try_into().expect("root length"),
                        base_layer: read_u32(32)?,
                        index: read_u32(36)?,
                        length: read_u32(40)?,
                        proof_layers: read_u32(44)?,
                    }
                } else {
                    Self::HashReject {
                        pieces_root: payload[..32].try_into().expect("root length"),
                        base_layer: read_u32(32)?,
                        index: read_u32(36)?,
                        length: read_u32(40)?,
                        proof_layers: read_u32(44)?,
                    }
                };
                message.encode()?;
                Ok(message)
            }
            22 if payload.len() >= 112 && (payload.len() - 48) % 32 == 0 => {
                let proof_count = read_u32(44)? as usize;
                let hash_bytes = &payload[48..];
                let hash_count = read_u32(40)? as usize;
                if hash_count > hash_bytes.len() / 32
                    || proof_count > hash_bytes.len() / 32 - hash_count
                {
                    return Err(invalid("hashes response proof count is invalid"));
                }
                let hashes_end = hash_count * 32;
                let hashes = hash_bytes[..hashes_end]
                    .chunks_exact(32)
                    .map(|hash| hash.try_into().expect("hash length"))
                    .collect();
                let proof_layers = hash_bytes[hashes_end..]
                    .chunks_exact(32)
                    .map(|hash| hash.try_into().expect("hash length"))
                    .collect();
                let message = Self::Hashes {
                    pieces_root: payload[..32].try_into().expect("root length"),
                    base_layer: read_u32(32)?,
                    index: read_u32(36)?,
                    length: read_u32(40)?,
                    proof_layers,
                    hashes,
                };
                message.encode()?;
                Ok(message)
            }
            9 if payload.len() == 2 => Ok(Self::Port(u16::from_be_bytes([payload[0], payload[1]]))),
            20 if !payload.is_empty() => Ok(Self::Extended {
                id: payload[0],
                payload: payload[1..].to_vec(),
            }),
            0..=9 | 20..=23 => Err(invalid("malformed peer message")),
            id => Ok(Self::Unknown {
                id,
                payload: payload.to_vec(),
            }),
        }
    }
}

fn validate_hash_request(length: u32) -> Result<(), crate::BackendError> {
    if !(2..=512).contains(&length) || !length.is_power_of_two() {
        return Err(invalid(
            "hash request length must be a power of two from 2 to 512",
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> crate::BackendError {
    crate::BackendError::InvalidPeerMessage(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_message_round_trips() {
        let message = PeerMessage::Request {
            piece_index: 3,
            begin: 16_384,
            length: 16_384,
        };

        assert_eq!(
            PeerMessage::decode(&message.encode().unwrap()).unwrap(),
            message
        );
    }

    #[test]
    fn hash_messages_round_trip() {
        let request = PeerMessage::HashRequest {
            pieces_root: [3; 32],
            base_layer: 0,
            index: 0,
            length: 2,
            proof_layers: 3,
        };
        assert_eq!(
            PeerMessage::decode(&request.encode().unwrap()).unwrap(),
            request
        );

        let response = PeerMessage::Hashes {
            pieces_root: [3; 32],
            base_layer: 0,
            index: 0,
            length: 2,
            proof_layers: vec![[4; 32]],
            hashes: vec![[5; 32], [6; 32]],
        };
        assert_eq!(
            PeerMessage::decode(&response.encode().unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn handshake_round_trips_and_preserves_extension_flags() {
        let handshake = Handshake::new([1; 20], [2; 20])
            .with_extension_bit(5, 2)
            .expect("valid extension bit");

        let decoded = Handshake::decode(&handshake.encode()).expect("valid handshake");

        assert_eq!(decoded, handshake);
        assert!(decoded.matches_info_hash(&[1; 20]));
    }

    #[test]
    fn handshake_round_trips_v2_capability() {
        let handshake = Handshake::new([1; 20], [2; 20]).with_v2_support();
        let decoded = Handshake::decode(&handshake.encode()).unwrap();
        assert!(decoded.supports_v2());
    }

    #[test]
    fn handshake_rejects_wrong_protocol() {
        let mut bytes = Handshake::new([0; 20], [0; 20]).encode();
        bytes[19] = b'X';

        assert!(matches!(
            Handshake::decode(&bytes),
            Err(crate::BackendError::InvalidPeerMessage(_))
        ));
    }

    #[test]
    fn peer_connection_performs_handshake_and_transfers_message() {
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let expected = Handshake::new([7; 20], [1; 20]);
        let remote = Handshake::new([7; 20], [2; 20]);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut connection =
                PeerConnection::from_stream(stream, &remote, Duration::from_secs(1)).unwrap();
            connection.receive().unwrap()
        });

        let mut connection =
            PeerConnection::connect(address, &expected, Duration::from_secs(1)).unwrap();
        connection
            .send(&PeerMessage::Interested)
            .expect("message should send");

        assert_eq!(server.join().unwrap(), PeerMessage::Interested);
        assert_eq!(connection.peer_id(), &[2; 20]);
    }

    #[test]
    fn refuses_a_connection_to_ourselves() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let ours = Handshake::new([7; 20], [1; 20]);
        let inbound = ours.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = PeerConnection::from_stream(stream, &inbound, Duration::from_secs(1));
        });

        let error = PeerConnection::connect(address, &ours, Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("ourselves"), "{error}");
        server.join().unwrap();
    }
}
