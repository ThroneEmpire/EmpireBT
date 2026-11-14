//! Message Stream Encryption (MSE), also called Protocol Encryption (PE).
//!
//! This is the Vuze/Azureus specification that every major client
//! implements. It never became a BEP. A Diffie-Hellman exchange (768-bit
//! prime, generator 2) gives both sides a shared secret `S`. The torrent's
//! info hash (`SKEY`) is sent only as `SHA1("req2", SKEY) xor
//! SHA1("req3", S)`, so an observer cannot see which torrent is requested.
//! The rest of the handshake is RC4-encrypted. Afterwards the BitTorrent
//! stream either stays RC4-encrypted or continues in plain text, whichever
//! both sides allow.
//!
//! ```text
//! A -> B  Ya, PadA
//! B -> A  Yb, PadB
//! A -> B  SHA1("req1", S), SHA1("req2", SKEY) ^ SHA1("req3", S),
//!         RC4(VC, crypto_provide, len(PadC), PadC, len(IA)), RC4(IA)
//! B -> A  RC4(VC, crypto_select, len(PadD), PadD), stream...
//! ```
//!
//! The pads are random lengths of 0 to 512 bytes, so the handshake has no
//! fixed size. `VC` is eight zero bytes that let A find where PadB ends.
//! `IA` is A's first payload, here its BitTorrent handshake.

use crate::BackendError;
use sha1::{Digest, Sha1};
use std::io::{Read, Write};

/// Whether peer connections use Message Stream Encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encryption {
    /// Outgoing connections try encryption first and fall back to plain
    /// text. Incoming peers may use either. This matches libtorrent's,
    /// qBittorrent's, and Deluge's defaults.
    #[default]
    Enabled,
    /// Every connection must be fully RC4-encrypted. Peers that cannot do
    /// that are refused.
    Required,
    /// Plain-text connections only. Encrypted handshakes are refused.
    Disabled,
}

impl Encryption {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Required => "required",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "enabled" => Some(Self::Enabled),
            "required" => Some(Self::Required),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

const CRYPTO_PLAINTEXT: u32 = 0x01;
const CRYPTO_RC4: u32 = 0x02;
const KEY_LENGTH: usize = 96;
const MAX_PAD: usize = 512;
/// The spec recommends 160-bit private keys.
const PRIVATE_KEY_LENGTH: usize = 20;
const VC: [u8; 8] = [0; 8];

/// RC4 keystreams for both directions of an encrypted connection.
#[derive(Clone)]
pub(crate) struct Ciphers {
    pub encrypt: Rc4,
    pub decrypt: Rc4,
}

impl std::fmt::Debug for Ciphers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Ciphers")
    }
}

/// Runs the initiator's side (A) on a freshly connected stream, sending
/// `initial` (the BitTorrent handshake) as the initial payload. Returns the
/// ciphers for the rest of the stream, or `None` if the peer chose plain
/// text.
pub(crate) fn initiate(
    stream: &mut (impl Read + Write),
    info_hash: &[u8; 20],
    initial: &[u8],
    allow_plaintext: bool,
) -> Result<Option<Ciphers>, BackendError> {
    let key = KeyPair::generate()?;
    let mut opening = key.public.to_vec();
    opening.extend(random_pad()?);
    write(stream, &opening)?;

    let mut their_public = [0; KEY_LENGTH];
    read(stream, &mut their_public)?;
    let secret = key.shared_secret(&their_public);
    let mut ciphers = Ciphers {
        encrypt: Rc4::new(&sha1(&[b"keyA", &secret, info_hash])),
        decrypt: Rc4::new(&sha1(&[b"keyB", &secret, info_hash])),
    };

    let provide = CRYPTO_RC4 | if allow_plaintext { CRYPTO_PLAINTEXT } else { 0 };
    let initial_length =
        u16::try_from(initial.len()).map_err(|_| invalid("initial payload too long"))?;
    let mut header = VC.to_vec();
    header.extend(provide.to_be_bytes());
    header.extend(0u16.to_be_bytes()); // no PadC
    header.extend(initial_length.to_be_bytes());
    header.extend(initial);
    ciphers.encrypt.apply(&mut header);
    let mut message = sha1(&[b"req1", &secret]).to_vec();
    message.extend(xor(sha1(&[b"req2", info_hash]), sha1(&[b"req3", &secret])));
    message.extend(header);
    write(stream, &message)?;

    // PadB has a random length, so scan for the encrypted VC that ends it.
    let mut expected = VC;
    ciphers.decrypt.clone().apply(&mut expected);
    let mut window = [0; 8];
    read(stream, &mut window)?;
    let mut scanned = 0;
    while window != expected {
        scanned += 1;
        if scanned > MAX_PAD {
            return Err(invalid("peer's encrypted handshake never synchronised"));
        }
        window.rotate_left(1);
        read(stream, &mut window[7..])?;
    }
    ciphers.decrypt.apply(&mut window);

    let mut reply = [0; 6];
    read(stream, &mut reply)?;
    ciphers.decrypt.apply(&mut reply);
    let select = u32::from_be_bytes(reply[..4].try_into().expect("four bytes"));
    let pad_length = usize::from(u16::from_be_bytes([reply[4], reply[5]]));
    if pad_length > MAX_PAD {
        return Err(invalid("peer's encryption padding is too long"));
    }
    let mut pad = vec![0; pad_length];
    read(stream, &mut pad)?;
    ciphers.decrypt.apply(&mut pad);
    match select {
        CRYPTO_RC4 => Ok(Some(ciphers)),
        CRYPTO_PLAINTEXT if allow_plaintext => Ok(None),
        _ => Err(invalid(
            "peer selected an encryption method we did not offer",
        )),
    }
}

/// Whether a new incoming connection opened with a plain BitTorrent
/// handshake or an encrypted one, or it is too early to tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opening {
    Undecided,
    Plain,
    Encrypted,
}

pub(crate) fn classify(input: &[u8]) -> Opening {
    const PLAIN: &[u8] = b"\x13BitTorrent protocol";
    let length = input.len().min(PLAIN.len());
    if input[..length] != PLAIN[..length] {
        Opening::Encrypted
    } else if length == PLAIN.len() {
        Opening::Plain
    } else {
        Opening::Undecided
    }
}

/// The responder's side (B), driven by an event loop as bytes arrive.
pub(crate) struct Responder {
    allow_plaintext: bool,
    state: State,
}

enum State {
    AwaitingKey,
    Synchronising {
        secret: [u8; KEY_LENGTH],
        req1: [u8; 20],
    },
    AwaitingTorrent {
        secret: [u8; KEY_LENGTH],
    },
    AwaitingHeader {
        ciphers: Box<Ciphers>,
        info_hash: [u8; 20],
    },
    AwaitingPad {
        ciphers: Box<Ciphers>,
        info_hash: [u8; 20],
        provide: u32,
        length: usize,
    },
    AwaitingInitial {
        ciphers: Box<Ciphers>,
        info_hash: [u8; 20],
        provide: u32,
        length: usize,
    },
    Done,
}

/// A completed encrypted handshake.
pub(crate) struct Established {
    /// The torrent the peer asked for.
    pub info_hash: [u8; 20],
    /// `None` when the stream continues in plain text.
    pub ciphers: Option<Ciphers>,
}

impl Responder {
    pub fn new(allow_plaintext: bool) -> Self {
        Self {
            allow_plaintext,
            state: State::AwaitingKey,
        }
    }

    /// Consumes handshake bytes from the front of `input` and appends
    /// anything to send to `output`. `torrent` maps `SHA1("req2", hash)`
    /// to the info hash of a torrent we serve.
    ///
    /// Once established, `input` holds the rest of the peer's stream as
    /// plain text: its initial payload, then anything after it, decrypted.
    pub fn advance(
        &mut self,
        input: &mut Vec<u8>,
        output: &mut Vec<u8>,
        torrent: impl Fn(&[u8; 20]) -> Option<[u8; 20]>,
    ) -> Result<Option<Established>, BackendError> {
        loop {
            match std::mem::replace(&mut self.state, State::Done) {
                State::AwaitingKey => {
                    let Some(their_public) = input.get(..KEY_LENGTH) else {
                        self.state = State::AwaitingKey;
                        return Ok(None);
                    };
                    let key = KeyPair::generate()?;
                    let secret = key.shared_secret(their_public.try_into().expect("96 bytes"));
                    input.drain(..KEY_LENGTH);
                    output.extend(key.public);
                    output.extend(random_pad()?);
                    self.state = State::Synchronising {
                        secret,
                        req1: sha1(&[b"req1", &secret]),
                    };
                }
                State::Synchronising { secret, req1 } => {
                    match input.windows(20).position(|window| window == req1) {
                        Some(start) => {
                            input.drain(..start + 20);
                            self.state = State::AwaitingTorrent { secret };
                        }
                        None if input.len() >= MAX_PAD + 20 => {
                            return Err(invalid("encrypted handshake never synchronised"));
                        }
                        None => {
                            self.state = State::Synchronising { secret, req1 };
                            return Ok(None);
                        }
                    }
                }
                State::AwaitingTorrent { secret } => {
                    let Some(obfuscated) = input.get(..20) else {
                        self.state = State::AwaitingTorrent { secret };
                        return Ok(None);
                    };
                    let req2 = xor(
                        obfuscated.try_into().expect("20 bytes"),
                        sha1(&[b"req3", &secret]),
                    );
                    let info_hash = torrent(&req2)
                        .ok_or_else(|| invalid("peer asked for a torrent we are not seeding"))?;
                    input.drain(..20);
                    self.state = State::AwaitingHeader {
                        ciphers: Box::new(Ciphers {
                            encrypt: Rc4::new(&sha1(&[b"keyB", &secret, &info_hash])),
                            decrypt: Rc4::new(&sha1(&[b"keyA", &secret, &info_hash])),
                        }),
                        info_hash,
                    };
                }
                State::AwaitingHeader {
                    mut ciphers,
                    info_hash,
                } => {
                    if input.len() < 14 {
                        self.state = State::AwaitingHeader { ciphers, info_hash };
                        return Ok(None);
                    }
                    let mut header: Vec<u8> = input.drain(..14).collect();
                    ciphers.decrypt.apply(&mut header);
                    if header[..8] != VC {
                        return Err(invalid(
                            "encrypted handshake has a bad verification constant",
                        ));
                    }
                    let provide = u32::from_be_bytes(header[8..12].try_into().expect("4 bytes"));
                    let length = usize::from(u16::from_be_bytes([header[12], header[13]]));
                    if length > MAX_PAD {
                        return Err(invalid("peer's encryption padding is too long"));
                    }
                    self.state = State::AwaitingPad {
                        ciphers,
                        info_hash,
                        provide,
                        length,
                    };
                }
                State::AwaitingPad {
                    mut ciphers,
                    info_hash,
                    provide,
                    length,
                } => {
                    if input.len() < length + 2 {
                        self.state = State::AwaitingPad {
                            ciphers,
                            info_hash,
                            provide,
                            length,
                        };
                        return Ok(None);
                    }
                    let mut pad: Vec<u8> = input.drain(..length + 2).collect();
                    ciphers.decrypt.apply(&mut pad);
                    let initial = usize::from(u16::from_be_bytes([pad[length], pad[length + 1]]));
                    self.state = State::AwaitingInitial {
                        ciphers,
                        info_hash,
                        provide,
                        length: initial,
                    };
                }
                State::AwaitingInitial {
                    mut ciphers,
                    info_hash,
                    provide,
                    length,
                } => {
                    if input.len() < length {
                        self.state = State::AwaitingInitial {
                            ciphers,
                            info_hash,
                            provide,
                            length,
                        };
                        return Ok(None);
                    }
                    ciphers.decrypt.apply(&mut input[..length]);
                    // Prefer full encryption whenever the peer offers it.
                    let select = if provide & CRYPTO_RC4 != 0 {
                        CRYPTO_RC4
                    } else if provide & CRYPTO_PLAINTEXT != 0 && self.allow_plaintext {
                        CRYPTO_PLAINTEXT
                    } else {
                        return Err(invalid("peer offered no encryption method we allow"));
                    };
                    let mut reply = VC.to_vec();
                    reply.extend(select.to_be_bytes());
                    reply.extend(0u16.to_be_bytes()); // no PadD
                    ciphers.encrypt.apply(&mut reply);
                    output.extend(reply);
                    let ciphers = (select == CRYPTO_RC4).then(|| {
                        let mut ciphers = *ciphers;
                        ciphers.decrypt.apply(&mut input[length..]);
                        ciphers
                    });
                    return Ok(Some(Established { info_hash, ciphers }));
                }
                State::Done => return Err(invalid("encrypted handshake already finished")),
            }
        }
    }
}

/// `SHA1("req2", info_hash)`, which identifies a torrent in an encrypted
/// handshake.
pub(crate) fn obfuscated_hash(info_hash: &[u8; 20]) -> [u8; 20] {
    sha1(&[b"req2", info_hash])
}

/// The RC4 stream cipher, with the first 1024 bytes of keystream discarded
/// as MSE requires.
#[derive(Clone)]
pub(crate) struct Rc4 {
    state: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    pub fn new(key: &[u8]) -> Self {
        let mut state: [u8; 256] = std::array::from_fn(|index| index as u8);
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
            state.swap(i, usize::from(j));
        }
        let mut cipher = Self { state, i: 0, j: 0 };
        cipher.apply(&mut [0; 1024]);
        cipher
    }

    /// Encrypts or decrypts `data` in place.
    pub fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.state[usize::from(self.i)]);
            self.state.swap(usize::from(self.i), usize::from(self.j));
            let index =
                self.state[usize::from(self.i)].wrapping_add(self.state[usize::from(self.j)]);
            *byte ^= self.state[usize::from(index)];
        }
    }
}

// ----- Diffie-Hellman ------------------------------------------------------

/// The MSE prime: 768 bits, as 12 little-endian 64-bit limbs.
const LIMBS: usize = 12;
type Number = [u64; LIMBS];

const PRIME: Number = from_hex(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563",
);
const GENERATOR: u64 = 2;

struct KeyPair {
    private: [u8; PRIVATE_KEY_LENGTH],
    public: [u8; KEY_LENGTH],
}

impl KeyPair {
    fn generate() -> Result<Self, BackendError> {
        let mut private = [0; PRIVATE_KEY_LENGTH];
        fill_random(&mut private)?;
        let mut generator = [0; LIMBS];
        generator[0] = GENERATOR;
        let public = to_bytes(&mod_pow(&generator, &private));
        Ok(Self { private, public })
    }

    fn shared_secret(&self, their_public: &[u8; KEY_LENGTH]) -> [u8; KEY_LENGTH] {
        let mut base = from_bytes(their_public);
        if !less_than(&base, &PRIME) {
            subtract(&mut base, &PRIME);
        }
        to_bytes(&mod_pow(&base, &self.private))
    }
}

/// `base ^ exponent mod PRIME` by Montgomery multiplication.
fn mod_pow(base: &Number, exponent: &[u8]) -> Number {
    let montgomery = Montgomery::get();
    let mut one = [0; LIMBS];
    one[0] = 1;
    let base = montgomery.multiply(base, &montgomery.r_squared);
    let mut result = montgomery.multiply(&one, &montgomery.r_squared);
    for byte in exponent {
        for bit in (0..8).rev() {
            result = montgomery.multiply(&result, &result);
            if byte >> bit & 1 == 1 {
                result = montgomery.multiply(&result, &base);
            }
        }
    }
    montgomery.multiply(&result, &one)
}

struct Montgomery {
    /// `-PRIME^-1 mod 2^64`.
    inverse: u64,
    /// `2^1536 mod PRIME`, which moves numbers into Montgomery form.
    r_squared: Number,
}

impl Montgomery {
    fn get() -> &'static Self {
        static INSTANCE: std::sync::OnceLock<Montgomery> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(|| {
            let mut inverse = 1u64;
            for _ in 0..6 {
                inverse = inverse.wrapping_mul(2u64.wrapping_sub(PRIME[0].wrapping_mul(inverse)));
            }
            let mut r_squared = [0; LIMBS];
            r_squared[0] = 1;
            for _ in 0..2 * 64 * LIMBS {
                let carry = r_squared[LIMBS - 1] >> 63;
                for index in (1..LIMBS).rev() {
                    r_squared[index] = r_squared[index] << 1 | r_squared[index - 1] >> 63;
                }
                r_squared[0] <<= 1;
                if carry == 1 || !less_than(&r_squared, &PRIME) {
                    subtract(&mut r_squared, &PRIME);
                }
            }
            Self {
                inverse: inverse.wrapping_neg(),
                r_squared,
            }
        })
    }

    /// `a * b / 2^768 mod PRIME`, for `a` and `b` below `PRIME`.
    fn multiply(&self, a: &Number, b: &Number) -> Number {
        let mut t = [0u64; LIMBS + 2];
        for &digit in b {
            let mut carry = 0u128;
            for j in 0..LIMBS {
                let sum = u128::from(t[j]) + u128::from(a[j]) * u128::from(digit) + carry;
                t[j] = sum as u64;
                carry = sum >> 64;
            }
            let sum = u128::from(t[LIMBS]) + carry;
            t[LIMBS] = sum as u64;
            t[LIMBS + 1] = (sum >> 64) as u64;

            let m = t[0].wrapping_mul(self.inverse);
            let mut carry = (u128::from(t[0]) + u128::from(m) * u128::from(PRIME[0])) >> 64;
            for j in 1..LIMBS {
                let sum = u128::from(t[j]) + u128::from(m) * u128::from(PRIME[j]) + carry;
                t[j - 1] = sum as u64;
                carry = sum >> 64;
            }
            let sum = u128::from(t[LIMBS]) + carry;
            t[LIMBS - 1] = sum as u64;
            t[LIMBS] = t[LIMBS + 1] + (sum >> 64) as u64;
            t[LIMBS + 1] = 0;
        }
        let mut result: Number = t[..LIMBS].try_into().expect("12 limbs");
        if t[LIMBS] != 0 || !less_than(&result, &PRIME) {
            subtract(&mut result, &PRIME);
        }
        result
    }
}

fn less_than(a: &Number, b: &Number) -> bool {
    for index in (0..LIMBS).rev() {
        if a[index] != b[index] {
            return a[index] < b[index];
        }
    }
    false
}

/// `a -= b`, wrapping modulo `2^768`.
fn subtract(a: &mut Number, b: &Number) {
    let mut borrow = false;
    for index in 0..LIMBS {
        let (difference, first) = a[index].overflowing_sub(b[index]);
        let (difference, second) = difference.overflowing_sub(u64::from(borrow));
        a[index] = difference;
        borrow = first || second;
    }
}

fn from_bytes(bytes: &[u8; KEY_LENGTH]) -> Number {
    std::array::from_fn(|limb| {
        let end = KEY_LENGTH - 8 * limb;
        u64::from_be_bytes(bytes[end - 8..end].try_into().expect("8 bytes"))
    })
}

fn to_bytes(number: &Number) -> [u8; KEY_LENGTH] {
    let mut bytes = [0; KEY_LENGTH];
    for (limb, value) in number.iter().enumerate() {
        let end = KEY_LENGTH - 8 * limb;
        bytes[end - 8..end].copy_from_slice(&value.to_be_bytes());
    }
    bytes
}

const fn from_hex(hex: &str) -> Number {
    let digits = hex.as_bytes();
    assert!(digits.len() == 2 * KEY_LENGTH);
    let mut number = [0; LIMBS];
    let mut index = 0;
    while index < digits.len() {
        let digit = match digits[index] {
            b'0'..=b'9' => digits[index] - b'0',
            b'A'..=b'F' => digits[index] - b'A' + 10,
            _ => panic!("invalid hex digit"),
        };
        // Digit `index` from the most significant end.
        let position = digits.len() - 1 - index;
        number[position / 16] |= (digit as u64) << (4 * (position % 16));
        index += 1;
    }
    number
}

// ----- helpers -------------------------------------------------------------

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn xor(a: [u8; 20], b: [u8; 20]) -> [u8; 20] {
    std::array::from_fn(|index| a[index] ^ b[index])
}

pub(crate) fn fill_random(bytes: &mut [u8]) -> Result<(), BackendError> {
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new()
        .fill(bytes)
        .map_err(|_| BackendError::Io("the system random number generator failed".to_owned()))
}

/// Random bytes of a random length from 0 to 512.
fn random_pad() -> Result<Vec<u8>, BackendError> {
    let mut length = [0; 2];
    fill_random(&mut length)?;
    let mut pad = vec![0; usize::from(u16::from_be_bytes(length)) % (MAX_PAD + 1)];
    fill_random(&mut pad)?;
    Ok(pad)
}

fn read(stream: &mut impl Read, buffer: &mut [u8]) -> Result<(), BackendError> {
    stream
        .read_exact(buffer)
        .map_err(|error| BackendError::Io(error.to_string()))
}

fn write(stream: &mut impl Write, bytes: &[u8]) -> Result<(), BackendError> {
    stream
        .write_all(bytes)
        .map_err(|error| BackendError::Io(error.to_string()))
}

fn invalid(message: &str) -> BackendError {
    BackendError::InvalidPeerMessage(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn rc4_matches_reference_keystream() {
        // RFC 6229, 40-bit key 0x0102030405: keystream at offsets 0 and 1024.
        let mut raw = Rc4 {
            state: std::array::from_fn(|index| index as u8),
            i: 0,
            j: 0,
        };
        let key = [1, 2, 3, 4, 5];
        let mut j = 0u8;
        for i in 0..256 {
            j = j
                .wrapping_add(raw.state[i])
                .wrapping_add(key[i % key.len()]);
            raw.state.swap(i, usize::from(j));
        }
        let mut first = [0; 16];
        raw.apply(&mut first);
        assert_eq!(hex(&first), "b2396305f03dc027ccc3524a0a1118a8");

        let mut after_discard = [0; 16];
        Rc4::new(&key).apply(&mut after_discard);
        assert_eq!(hex(&after_discard), "30abbcc7c20b01609f23ee2d5f6bb7df");
    }

    #[test]
    fn modular_exponentiation_matches_reference_values() {
        let mut two = [0; LIMBS];
        two[0] = 2;
        assert_eq!(to_bytes(&mod_pow(&two, &[1]))[KEY_LENGTH - 1], 2);
        // pow(2, 65537, P) from Python.
        assert_eq!(
            hex(&to_bytes(&mod_pow(&two, &[1, 0, 1]))),
            "90bebd74b81b239d8cd10f49edd7e2644e6869fa9e17960705045171439fde1d\
             1cde8700fd439210ccb2344b22b088facde58058d5061f125ac133b0b0a2d3d0\
             3d042eeb805beaca763afb81369faf9cacd9e985dd69277b74ea3d1baa6bea09"
        );
    }

    #[test]
    fn both_sides_derive_the_same_secret() {
        let a = KeyPair::generate().unwrap();
        let b = KeyPair::generate().unwrap();
        assert_eq!(a.shared_secret(&b.public), b.shared_secret(&a.public));
        assert_ne!(a.public, b.public);
    }

    #[test]
    fn classifies_connection_openings() {
        assert_eq!(classify(b"\x13BitTor"), Opening::Undecided);
        assert_eq!(classify(b"\x13BitTorrent protocol\0"), Opening::Plain);
        assert_eq!(classify(b"\x13BitTorrent protocoX"), Opening::Encrypted);
        assert_eq!(classify(&[0xaf, 0x6b]), Opening::Encrypted);
    }

    /// Runs the initiator on a thread against the responder fed through a
    /// real socket, as the reactor does.
    fn handshake(
        allow_plaintext_a: bool,
        allow_plaintext_b: bool,
    ) -> (Option<Ciphers>, Established, Vec<u8>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let info_hash = [9; 20];
        let initiator = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            let ciphers = initiate(&mut stream, &info_hash, b"hello", allow_plaintext_a).unwrap();
            (stream, ciphers)
        });
        let (mut stream, _) = listener.accept().unwrap();
        let mut responder = Responder::new(allow_plaintext_b);
        let mut input = Vec::new();
        let established = loop {
            let mut buffer = [0; 4096];
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "initiator hung up");
            input.extend_from_slice(&buffer[..read]);
            let mut output = Vec::new();
            let result = responder
                .advance(&mut input, &mut output, |req2| {
                    (*req2 == obfuscated_hash(&info_hash)).then_some(info_hash)
                })
                .unwrap();
            stream.write_all(&output).unwrap();
            if let Some(established) = result {
                break established;
            }
        };
        let (_stream, ciphers) = initiator.join().unwrap();
        (ciphers, established, input)
    }

    #[test]
    fn negotiates_full_encryption() {
        let (a, b, initial) = handshake(true, true);
        assert_eq!(b.info_hash, [9; 20]);
        assert_eq!(initial, b"hello");
        let (mut a, mut b) = (a.expect("RC4 selected"), b.ciphers.expect("RC4 selected"));
        let mut message = *b"piece data";
        a.encrypt.apply(&mut message);
        assert_ne!(&message, b"piece data");
        b.decrypt.apply(&mut message);
        assert_eq!(&message, b"piece data");
        b.encrypt.apply(&mut message);
        a.decrypt.apply(&mut message);
        assert_eq!(&message, b"piece data");
    }

    #[test]
    fn requires_rc4_when_plain_text_is_not_allowed() {
        let (a, b, _) = handshake(false, true);
        assert!(a.is_some() && b.ciphers.is_some());
    }
}
