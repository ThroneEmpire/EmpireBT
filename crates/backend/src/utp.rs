//! uTP, the Micro Transport Protocol (BEP 29): BitTorrent over UDP.
//!
//! uTP gives a reliable, ordered byte stream like TCP, but its congestion
//! control (LEDBAT) backs off as soon as it sees queuing delay. Uploads
//! then leave room for everything else on the link. libtorrent-based
//! clients (qBittorrent, Deluge) and µTorrent try uTP before TCP, so
//! answering it avoids a TCP fallback delay on every connection.
//!
//! Every packet starts with a 20-byte header:
//!
//! ```text
//! type:4 version:4 | extension:8 | connection_id:16
//! timestamp_microseconds:32
//! timestamp_difference_microseconds:32
//! wnd_size:32
//! seq_nr:16 | ack_nr:16
//! ```
//!
//! Sequence numbers count packets, not bytes. `ack_nr` is the last packet
//! received in order. The selective ACK extension marks packets received
//! beyond a gap.
//!
//! Handshake numbering follows libutp and libtorrent. A state packet carries
//! the sequence number its sender will use next, without consuming it, so
//! the initiator sets `ack_nr` to the SYN-ACK's `seq_nr - 1`.
//!
//! [`UtpSocket`] shares one UDP port with the DHT (DHT messages start with
//! `d`, which is never a valid uTP header). [`UtpStream`] is a connection
//! with blocking reads and writes, or non-blocking ones plus a readiness
//! callback for the event-driven seeding server.

use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const VERSION: u8 = 1;
const HEADER_LENGTH: usize = 20;
const EXTENSION_SELECTIVE_ACK: u8 = 1;
/// Payload per packet, so datagrams fit common path MTUs.
const MAX_PAYLOAD: usize = 1380;
/// The congestion window never drops below one packet.
const MIN_WINDOW: usize = MAX_PAYLOAD;
const INITIAL_WINDOW: usize = 2 * MAX_PAYLOAD;
const MAX_WINDOW: usize = 4 << 20;
/// Receive buffer, advertised as `wnd_size`.
const RECEIVE_BUFFER: usize = 1 << 20;
/// Bytes a writer may queue beyond those in flight.
const SEND_BUFFER: usize = 1 << 20;
/// LEDBAT's target queuing delay.
const TARGET_DELAY: i64 = 100_000;
/// libutp's `MAX_CWND_INCREASE_BYTES_PER_RTT`.
const MAX_WINDOW_GAIN: f64 = 3000.0;
const INITIAL_TIMEOUT: Duration = Duration::from_millis(1000);
const MIN_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_TIMEOUT: Duration = Duration::from_secs(30);
/// Consecutive timeouts before a connection is given up: about a minute.
const MAX_TIMEOUTS: u32 = 6;
/// SYNs sent before connecting fails (1 + 2 + 4 seconds).
const MAX_SYN_TIMEOUTS: u32 = 2;
/// How far beyond `ack_nr` out-of-order packets are kept.
const REORDER_LIMIT: u16 = 1024;
/// How long a closed connection waits for its FIN to be acknowledged.
const LINGER: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(20);
/// Incoming connections waiting to be accepted.
const BACKLOG: usize = 256;

/// Whether peer connections may use uTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UtpMode {
    /// Peers may connect over uTP. Outgoing connections use TCP, falling
    /// back to uTP when TCP fails.
    #[default]
    Enabled,
    /// Outgoing connections try uTP first, as libtorrent does.
    Preferred,
    /// TCP only.
    Disabled,
}

impl UtpMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Preferred => "preferred",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "enabled" => Some(Self::Enabled),
            "preferred" => Some(Self::Preferred),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

// ----- packets -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Data = 0,
    Fin = 1,
    State = 2,
    Reset = 3,
    Syn = 4,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Packet {
    kind: Kind,
    connection_id: u16,
    timestamp: u32,
    timestamp_difference: u32,
    window: u32,
    seq_nr: u16,
    ack_nr: u16,
    selective_ack: Option<Vec<u8>>,
    payload: Vec<u8>,
}

impl Packet {
    fn encode(&self) -> Vec<u8> {
        self.encode_with(&self.payload)
    }

    /// Encodes the header followed by `payload` instead of `self.payload`.
    fn encode_with(&self, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_LENGTH + 2 + 32 + payload.len());
        bytes.push((self.kind as u8) << 4 | VERSION);
        bytes.push(if self.selective_ack.is_some() {
            EXTENSION_SELECTIVE_ACK
        } else {
            0
        });
        bytes.extend(self.connection_id.to_be_bytes());
        bytes.extend(self.timestamp.to_be_bytes());
        bytes.extend(self.timestamp_difference.to_be_bytes());
        bytes.extend(self.window.to_be_bytes());
        bytes.extend(self.seq_nr.to_be_bytes());
        bytes.extend(self.ack_nr.to_be_bytes());
        if let Some(mask) = &self.selective_ack {
            bytes.push(0); // no further extensions
            bytes.push(mask.len() as u8);
            bytes.extend(mask);
        }
        bytes.extend(payload);
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if !is_utp(bytes) {
            return None;
        }
        let kind = match bytes[0] >> 4 {
            0 => Kind::Data,
            1 => Kind::Fin,
            2 => Kind::State,
            3 => Kind::Reset,
            _ => Kind::Syn,
        };
        let u16_at = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]);
        let u32_at = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        // Each extension starts with the type of the one after it and its
        // own length. Unknown extensions are skipped.
        let mut next = bytes[1];
        let mut position = HEADER_LENGTH;
        let mut selective_ack = None;
        while next != 0 {
            let header = bytes.get(position..position + 2)?;
            let length = usize::from(header[1]);
            let data = bytes.get(position + 2..position + 2 + length)?;
            if next == EXTENSION_SELECTIVE_ACK {
                if length < 4 || length % 4 != 0 {
                    return None;
                }
                selective_ack = Some(data.to_vec());
            }
            next = header[0];
            position += 2 + length;
        }
        Some(Self {
            kind,
            connection_id: u16_at(2),
            timestamp: u32_at(4),
            timestamp_difference: u32_at(8),
            window: u32_at(12),
            seq_nr: u16_at(16),
            ack_nr: u16_at(18),
            selective_ack,
            payload: bytes[position..].to_vec(),
        })
    }
}

/// Whether a datagram is uTP rather than DHT (bencoded, starting with `d`).
pub(crate) fn is_utp(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LENGTH && bytes[0] & 0x0f == VERSION && bytes[0] >> 4 <= 4
}

/// Whether sequence number `a` comes before `b`, allowing for wrapping.
fn before(a: u16, b: u16) -> bool {
    (b.wrapping_sub(a) as i16) > 0
}

// ----- connection state machine --------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    SynSent,
    Connected,
    Closed,
}

/// A packet waiting to be acknowledged.
struct Sent {
    seq_nr: u16,
    kind: Kind,
    payload: Vec<u8>,
    sent_at: Instant,
    /// 0 until first sent.
    transmissions: u32,
    resend: bool,
}

/// The lowest one-way delay seen recently, the baseline for measuring
/// queuing delay. Kept as minimums over two one-minute periods.
struct BaseDelay {
    period_start: Instant,
    current: u32,
    previous: u32,
}

impl BaseDelay {
    fn new(now: Instant) -> Self {
        Self {
            period_start: now,
            current: u32::MAX,
            previous: u32::MAX,
        }
    }

    fn add(&mut self, sample: u32, now: Instant) {
        if now - self.period_start >= Duration::from_secs(60) {
            self.previous = self.current;
            self.current = u32::MAX;
            self.period_start = now;
        }
        self.current = self.current.min(sample);
    }

    fn get(&self) -> u32 {
        self.current.min(self.previous)
    }
}

/// One uTP connection, without any I/O: packets go in through
/// [`Core::on_packet`], and [`Core::flush`] returns datagrams to send.
struct Core {
    state: State,
    /// Why a connection closed abnormally.
    error: Option<ErrorKind>,
    send_id: u16,
    recv_id: u16,
    /// The sequence number of the next packet sent.
    seq_nr: u16,
    /// The last packet received in order.
    ack_nr: u16,
    epoch: Instant,

    in_flight: VecDeque<Sent>,
    /// Payload bytes in flight.
    cur_window: usize,
    max_window: f64,
    peer_window: usize,
    send_buffer: VecDeque<u8>,
    /// Some packet in flight is marked for resending.
    resend_pending: bool,
    closing: bool,
    fin_sent: bool,
    fin_acked: bool,

    /// Packets received beyond a gap, by sequence number.
    out_of_order: HashMap<u16, (Kind, Vec<u8>)>,
    out_of_order_bytes: usize,
    read_buffer: VecDeque<u8>,
    /// The peer's FIN arrived in order: no more data will come.
    eof: bool,
    ack_pending: bool,
    advertised_window: usize,

    /// Our receive time minus the peer's last timestamp, echoed back.
    reply_micros: u32,
    rtt: Option<(i64, i64)>,
    timeout: Duration,
    timeout_at: Option<Instant>,
    timeouts: u32,
    slow_start: bool,
    /// Slow start stops when the window reaches this.
    ssthresh: f64,
    base_delay: BaseDelay,
    last_ack_nr: u16,
    duplicate_acks: u32,
    last_cut: Option<Instant>,
}

impl Core {
    fn new(send_id: u16, recv_id: u16, seq_nr: u16, epoch: Instant, now: Instant) -> Self {
        Self {
            state: State::Connected,
            error: None,
            send_id,
            recv_id,
            seq_nr,
            ack_nr: 0,
            epoch,
            in_flight: VecDeque::new(),
            cur_window: 0,
            max_window: INITIAL_WINDOW as f64,
            peer_window: RECEIVE_BUFFER,
            send_buffer: VecDeque::new(),
            resend_pending: false,
            closing: false,
            fin_sent: false,
            fin_acked: false,
            out_of_order: HashMap::new(),
            out_of_order_bytes: 0,
            read_buffer: VecDeque::new(),
            eof: false,
            ack_pending: false,
            advertised_window: RECEIVE_BUFFER,
            reply_micros: 0,
            rtt: None,
            timeout: INITIAL_TIMEOUT,
            timeout_at: None,
            timeouts: 0,
            slow_start: true,
            ssthresh: MAX_WINDOW as f64,
            base_delay: BaseDelay::new(now),
            last_ack_nr: 0,
            duplicate_acks: 0,
            last_cut: None,
        }
    }

    /// Starts a connection: queues a SYN sent with `recv_id`.
    fn connect(recv_id: u16, seq_nr: u16, epoch: Instant, now: Instant) -> Self {
        let mut core = Self::new(recv_id.wrapping_add(1), recv_id, seq_nr, epoch, now);
        core.state = State::SynSent;
        core.queue(Kind::Syn, Vec::new(), now);
        core
    }

    /// Accepts a peer's SYN and queues the state packet answering it.
    fn accept(syn: &Packet, seq_nr: u16, epoch: Instant, now: Instant) -> Self {
        let mut core = Self::new(
            syn.connection_id,
            syn.connection_id.wrapping_add(1),
            seq_nr,
            epoch,
            now,
        );
        core.ack_nr = syn.seq_nr;
        core.last_ack_nr = syn.ack_nr;
        core.peer_window = syn.window as usize;
        core.reply_micros = core.micros(now).wrapping_sub(syn.timestamp);
        core.ack_pending = true;
        core
    }

    fn micros(&self, now: Instant) -> u32 {
        (now - self.epoch).as_micros() as u32
    }

    fn receive_window(&self) -> usize {
        RECEIVE_BUFFER.saturating_sub(self.read_buffer.len() + self.out_of_order_bytes)
    }

    fn queue(&mut self, kind: Kind, payload: Vec<u8>, now: Instant) {
        self.cur_window += payload.len();
        self.in_flight.push_back(Sent {
            seq_nr: self.seq_nr,
            kind,
            payload,
            sent_at: now,
            transmissions: 0,
            resend: false,
        });
        self.seq_nr = self.seq_nr.wrapping_add(1);
    }

    fn on_packet(&mut self, packet: &Packet, now: Instant) {
        if self.state == State::Closed {
            return;
        }
        if packet.kind == Kind::Reset {
            self.close_with(if self.state == State::SynSent {
                ErrorKind::ConnectionRefused
            } else {
                ErrorKind::ConnectionReset
            });
            return;
        }
        self.peer_window = packet.window as usize;
        if packet.timestamp != 0 {
            self.reply_micros = self.micros(now).wrapping_sub(packet.timestamp);
        }
        match (self.state, packet.kind) {
            (State::SynSent, Kind::State) => {
                self.ack_nr = packet.seq_nr.wrapping_sub(1);
                self.last_ack_nr = packet.ack_nr;
                self.state = State::Connected;
            }
            (State::SynSent, _) => return,
            // A repeated SYN: our answer was lost.
            (_, Kind::Syn) => {
                self.ack_pending = true;
                return;
            }
            _ => {}
        }
        self.process_acks(packet, now);
        if matches!(packet.kind, Kind::Data | Kind::Fin) {
            self.receive(packet);
        }
    }

    fn process_acks(&mut self, packet: &Packet, now: Instant) {
        let mut bytes_acked = 0;
        let mut progressed = false;
        while let Some(front) = self.in_flight.front() {
            if front.transmissions == 0 || before(packet.ack_nr, front.seq_nr) {
                break;
            }
            let sent = self.in_flight.pop_front().expect("front exists");
            bytes_acked += self.acknowledge(&sent, now);
            progressed = true;
        }

        let mut loss = false;
        if let Some(mask) = &packet.selective_ack {
            // Bit 0 is ack_nr + 2; ack_nr + 1 is missing by definition.
            let base = packet.ack_nr.wrapping_add(2);
            let is_set = |seq: u16| {
                let offset = usize::from(seq.wrapping_sub(base));
                !before(seq, base)
                    && offset < mask.len() * 8
                    && mask[offset / 8] >> (offset % 8) & 1 == 1
            };
            // One pass from the newest packet back: remove those acked, and
            // mark as lost any with three acked packets sent after it.
            let mut acked_after = 0;
            let mut index = self.in_flight.len();
            while index > 0 {
                index -= 1;
                let sent = &mut self.in_flight[index];
                if sent.transmissions == 0 {
                    continue;
                }
                if is_set(sent.seq_nr) {
                    let sent = self.in_flight.remove(index).expect("index exists");
                    bytes_acked += self.acknowledge(&sent, now);
                    acked_after += 1;
                } else if acked_after >= 3 && !sent.resend {
                    sent.resend = true;
                    self.resend_pending = true;
                    loss = true;
                }
            }
        }

        if packet.kind == Kind::State && packet.ack_nr == self.last_ack_nr && !progressed {
            if self
                .in_flight
                .front()
                .is_some_and(|sent| sent.transmissions > 0)
            {
                self.duplicate_acks += 1;
                if self.duplicate_acks == 3 {
                    if let Some(front) = self.in_flight.front_mut() {
                        front.resend = true;
                        self.resend_pending = true;
                    }
                    loss = true;
                }
            }
        } else if progressed {
            self.duplicate_acks = 0;
        }
        self.last_ack_nr = packet.ack_nr;

        if bytes_acked > 0 || progressed {
            self.timeouts = 0;
            self.timeout_at = self
                .in_flight
                .front()
                .is_some_and(|sent| sent.transmissions > 0)
                .then(|| now + self.timeout);
        }
        if bytes_acked > 0 {
            self.grow_window(packet.timestamp_difference, bytes_acked, now);
        }
        if loss {
            self.on_loss(now);
        }
    }

    /// Accounts for an acknowledged packet; returns its payload size.
    fn acknowledge(&mut self, sent: &Sent, now: Instant) -> usize {
        // Only packets sent once give unambiguous round-trip samples.
        if sent.transmissions == 1 {
            self.sample_rtt((now - sent.sent_at).as_micros() as i64);
        }
        if sent.kind == Kind::Fin {
            self.fin_acked = true;
        }
        self.cur_window -= sent.payload.len();
        sent.payload.len()
    }

    fn sample_rtt(&mut self, sample: i64) {
        let (rtt, variance) = match self.rtt {
            None => (sample, sample / 2),
            Some((rtt, variance)) => {
                let delta = rtt - sample;
                (
                    rtt + (sample - rtt) / 8,
                    variance + (delta.abs() - variance) / 4,
                )
            }
        };
        self.rtt = Some((rtt, variance));
        self.timeout = Duration::from_micros((rtt + 4 * variance).max(0) as u64).max(MIN_TIMEOUT);
    }

    /// LEDBAT: grow the window while queuing delay is under the target,
    /// shrink it above. Slow start doubles it each round trip until the
    /// delay nears the target.
    fn grow_window(&mut self, delay_sample: u32, bytes_acked: usize, now: Instant) {
        let mut queuing = 0;
        if delay_sample != 0 {
            self.base_delay.add(delay_sample, now);
            let delay = delay_sample.wrapping_sub(self.base_delay.get());
            // A wrapped difference means the clocks moved; ignore it.
            if delay < 1 << 31 {
                queuing = i64::from(delay);
            }
        }
        let off_target = TARGET_DELAY - queuing;
        let delay_factor = off_target as f64 / TARGET_DELAY as f64;
        let acked = bytes_acked as f64;
        let window_factor = acked.min(self.max_window) / self.max_window.max(acked);
        let gain = MAX_WINDOW_GAIN * delay_factor * window_factor;
        if self.slow_start && off_target > TARGET_DELAY / 10 && self.max_window < self.ssthresh {
            self.max_window += acked.max(gain);
        } else {
            self.slow_start = false;
            self.max_window += gain;
        }
        self.max_window = self.max_window.clamp(MIN_WINDOW as f64, MAX_WINDOW as f64);
    }

    /// Halves the window, at most once per round trip.
    fn on_loss(&mut self, now: Instant) {
        let rtt = Duration::from_micros(self.rtt.map_or(0, |(rtt, _)| rtt.max(0) as u64));
        if self.last_cut.is_some_and(|cut| now - cut < rtt) {
            return;
        }
        self.last_cut = Some(now);
        self.slow_start = false;
        self.max_window = (self.max_window * 0.5).max(MIN_WINDOW as f64);
        self.ssthresh = self.max_window;
    }

    fn receive(&mut self, packet: &Packet) {
        self.ack_pending = true;
        let seq = packet.seq_nr;
        if !before(self.ack_nr, seq) || self.eof {
            return; // a duplicate of something already delivered
        }
        if seq.wrapping_sub(self.ack_nr) > REORDER_LIMIT
            || self.out_of_order_bytes + self.read_buffer.len() + packet.payload.len()
                > RECEIVE_BUFFER
            || self.out_of_order.contains_key(&seq)
        {
            return;
        }
        if seq == self.ack_nr.wrapping_add(1) && self.out_of_order.is_empty() {
            // The usual case: in order, with nothing waiting behind a gap.
            self.ack_nr = seq;
            self.read_buffer.extend(&packet.payload);
            self.eof = packet.kind == Kind::Fin;
            return;
        }
        self.out_of_order_bytes += packet.payload.len();
        self.out_of_order
            .insert(seq, (packet.kind, packet.payload.clone()));
        while let Some((kind, payload)) = self.out_of_order.remove(&self.ack_nr.wrapping_add(1)) {
            self.ack_nr = self.ack_nr.wrapping_add(1);
            self.out_of_order_bytes -= payload.len();
            self.read_buffer.extend(payload);
            if kind == Kind::Fin {
                self.eof = true;
                self.out_of_order.clear();
                self.out_of_order_bytes = 0;
                break;
            }
        }
    }

    /// The selective ACK mask for packets held beyond a gap, if any.
    fn selective_ack(&self) -> Option<Vec<u8>> {
        let base = self.ack_nr.wrapping_add(2);
        let highest = self
            .out_of_order
            .keys()
            .filter(|seq| !before(**seq, base))
            .map(|seq| usize::from(seq.wrapping_sub(base)))
            .max()?;
        let mut mask = vec![0u8; (highest / 32 + 1) * 4];
        for seq in self.out_of_order.keys() {
            if !before(*seq, base) {
                let offset = usize::from(seq.wrapping_sub(base));
                mask[offset / 8] |= 1 << (offset % 8);
            }
        }
        Some(mask)
    }

    fn read(&mut self, buffer: &mut [u8]) -> usize {
        let count = buffer.len().min(self.read_buffer.len());
        for (slot, byte) in buffer.iter_mut().zip(self.read_buffer.drain(..count)) {
            *slot = byte;
        }
        // Tell the peer once a closed or small window has reopened.
        let window = self.receive_window();
        if count > 0
            && (self.advertised_window < 4 * MAX_PAYLOAD
                || window >= self.advertised_window + RECEIVE_BUFFER / 4)
        {
            self.ack_pending = true;
        }
        count
    }

    fn write(&mut self, data: &[u8]) -> usize {
        if self.closing || self.state == State::Closed {
            return 0;
        }
        let room = SEND_BUFFER.saturating_sub(self.send_buffer.len() + self.cur_window);
        let count = room.min(data.len());
        self.send_buffer.extend(&data[..count]);
        count
    }

    fn close(&mut self) {
        self.closing = true;
    }

    fn close_with(&mut self, error: ErrorKind) {
        self.state = State::Closed;
        self.error = Some(error);
        self.in_flight.clear();
        self.cur_window = 0;
    }

    fn can_write(&self) -> bool {
        self.state == State::Connected
            && !self.closing
            && self.send_buffer.len() + self.cur_window < SEND_BUFFER
    }

    /// Handles timeouts. Everything in flight is presumed lost and resent,
    /// paced by a window cut to one packet that slow-starts back to half
    /// its old size. Each timeout doubles the next; repeated timeouts end
    /// the connection.
    fn tick(&mut self, now: Instant) {
        if self.state == State::Closed {
            return;
        }
        let Some(deadline) = self.timeout_at else {
            return;
        };
        if now < deadline {
            return;
        }
        self.timeouts += 1;
        let limit = if self.state == State::SynSent {
            MAX_SYN_TIMEOUTS
        } else {
            MAX_TIMEOUTS
        };
        if self.timeouts > limit {
            return self.close_with(ErrorKind::TimedOut);
        }
        self.ssthresh = (self.max_window / 2.0).max(2.0 * MIN_WINDOW as f64);
        self.max_window = MIN_WINDOW as f64;
        self.slow_start = true;
        for sent in &mut self.in_flight {
            if sent.transmissions > 0 {
                sent.resend = true;
                self.resend_pending = true;
            }
        }
        self.timeout = (self.timeout * 2).min(MAX_TIMEOUT);
        self.timeout_at = Some(now + self.timeout);
    }

    /// Builds the datagrams that are due: retransmissions, new data within
    /// the congestion and receive windows, a FIN once closed, and an ack
    /// when nothing else carries one.
    fn flush(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut datagrams = Vec::new();
        if self.state == State::Closed {
            return datagrams;
        }
        if self.state == State::Connected {
            let window = (self.max_window as usize).min(self.peer_window);
            // Always allow one packet in flight: it probes a zero window.
            while !self.send_buffer.is_empty() {
                let size = self.send_buffer.len().min(MAX_PAYLOAD);
                if self.cur_window > 0 && self.cur_window + size > window {
                    break;
                }
                let payload: Vec<u8> = self.send_buffer.drain(..size).collect();
                self.queue(Kind::Data, payload, now);
            }
            if self.closing && self.send_buffer.is_empty() && !self.fin_sent {
                self.fin_sent = true;
                self.queue(Kind::Fin, Vec::new(), now);
            }
        }
        let window = self.receive_window();
        let header = |core: &Self, kind: Kind, seq_nr: u16| Packet {
            kind,
            connection_id: if kind == Kind::Syn {
                core.recv_id
            } else {
                core.send_id
            },
            timestamp: core.micros(now),
            timestamp_difference: core.reply_micros,
            window: window as u32,
            seq_nr,
            ack_nr: if kind == Kind::Syn { 0 } else { core.ack_nr },
            selective_ack: if kind == Kind::Syn {
                None
            } else {
                core.selective_ack()
            },
            payload: Vec::new(),
        };
        let mut due = Vec::new();
        if self.resend_pending {
            // Packets presumed lost leave the network; resends refill the
            // window in order, so a timeout does not cause a burst.
            let window_limit = (self.max_window as usize).min(self.peer_window);
            let lost: usize = self
                .in_flight
                .iter()
                .filter(|sent| sent.resend)
                .map(|sent| sent.payload.len())
                .sum();
            let mut in_transit = self.cur_window - lost;
            let mut deferred = false;
            for (index, sent) in self.in_flight.iter().enumerate() {
                if sent.transmissions == 0 {
                    due.push(index);
                } else if sent.resend {
                    if in_transit == 0 || in_transit + sent.payload.len() <= window_limit {
                        in_transit += sent.payload.len();
                        due.push(index);
                    } else {
                        deferred = true;
                    }
                }
            }
            self.resend_pending = deferred;
        } else {
            // New packets wait at the back.
            let unsent = self
                .in_flight
                .iter()
                .rev()
                .take_while(|sent| sent.transmissions == 0)
                .count();
            due.extend(self.in_flight.len() - unsent..self.in_flight.len());
        }
        for index in due {
            let (kind, seq_nr) = (self.in_flight[index].kind, self.in_flight[index].seq_nr);
            let packet = header(self, kind, seq_nr);
            datagrams.push(packet.encode_with(&self.in_flight[index].payload));
            let sent = &mut self.in_flight[index];
            sent.transmissions += 1;
            sent.sent_at = now;
            sent.resend = false;
        }
        if self.ack_pending && datagrams.is_empty() && self.state == State::Connected {
            datagrams.push(header(self, Kind::State, self.seq_nr).encode());
        }
        if !datagrams.is_empty() {
            self.ack_pending = false;
            self.advertised_window = window;
        }
        // Sent packets always precede unsent ones.
        if self.timeout_at.is_none()
            && self
                .in_flight
                .front()
                .is_some_and(|sent| sent.transmissions > 0)
        {
            self.timeout_at = Some(now + self.timeout);
        }
        datagrams
    }

    /// Whether a closed stream has finished: its FIN was acknowledged and
    /// the peer closed too.
    fn finished(&self) -> bool {
        self.state == State::Closed || (self.fin_acked && self.eof)
    }
}

// ----- socket --------------------------------------------------------------

type Notify = Arc<dyn Fn() + Send + Sync>;

/// One connection's state, shared by its stream and the socket's threads.
struct Entry {
    core: Mutex<Core>,
    changed: Condvar,
    /// Threads blocked on `changed`. Counted under the `core` lock, so
    /// waking can skip the system call when nobody waits.
    waiters: std::sync::atomic::AtomicUsize,
    notify: Mutex<Option<Notify>>,
    address: SocketAddr,
    /// When the stream was dropped; the connection lingers to finish.
    dropped_at: Mutex<Option<Instant>>,
}

impl Entry {
    fn new(core: Core, address: SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            core: Mutex::new(core),
            changed: Condvar::new(),
            waiters: std::sync::atomic::AtomicUsize::new(0),
            notify: Mutex::new(None),
            address,
            dropped_at: Mutex::new(None),
        })
    }

    /// Blocks until woken or `timeout` passes.
    fn wait<'a>(&self, core: MutexGuard<'a, Core>, timeout: Duration) -> MutexGuard<'a, Core> {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let core = self
            .changed
            .wait_timeout(core, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
        self.waiters.fetch_sub(1, Ordering::SeqCst);
        core
    }

    /// Wakes blocked readers and writers and the event loop.
    fn wake(&self) {
        if self.waiters.load(Ordering::SeqCst) > 0 {
            self.changed.notify_all();
        }
        let notify = lock(&self.notify).clone();
        if let Some(notify) = notify {
            notify();
        }
    }
}

struct Listener {
    queue: VecDeque<Arc<Entry>>,
    notify: Notify,
    id: u64,
}

pub(crate) struct Shared {
    /// One socket per address family, on the same port.
    sockets: Vec<UdpSocket>,
    epoch: Instant,
    /// Keyed by peer address and the connection id its packets carry.
    connections: Mutex<HashMap<(SocketAddr, u16), Arc<Entry>>>,
    listener: Mutex<Option<Listener>>,
    next_listener: AtomicU64,
    shutdown: AtomicBool,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("utp::Shared")
    }
}

/// Connections that datagrams of one batch reached. Their acks and data
/// are sent, and their readers woken, once when the batch is finished,
/// rather than once per datagram.
#[derive(Default)]
pub(crate) struct Batch {
    touched: Vec<Arc<Entry>>,
}

impl Shared {
    /// Sends what the batch's datagrams made due, and wakes readers.
    pub(crate) fn finish(&self, batch: Batch) {
        let now = Instant::now();
        for entry in batch.touched {
            self.flush(&entry, now);
            entry.wake();
        }
    }

    /// Handles one datagram that [`is_utp`] accepted, as part of `batch`.
    pub(crate) fn handle_in_batch(&self, bytes: &[u8], from: SocketAddr, batch: &mut Batch) {
        let Some(packet) = Packet::decode(bytes) else {
            return;
        };
        let now = Instant::now();
        if packet.kind == Kind::Syn {
            let key = (from, packet.connection_id.wrapping_add(1));
            let existing = lock(&self.connections).get(&key).cloned();
            if let Some(entry) = existing {
                return self.deliver(&entry, &packet, now, batch);
            }
            let mut listener = lock(&self.listener);
            let Some(listener) = listener.as_mut().filter(|l| l.queue.len() < BACKLOG) else {
                return self.reset(&packet, from);
            };
            let core = Core::accept(&packet, random_u16(), self.epoch, now);
            let entry = Entry::new(core, from);
            lock(&self.connections).insert(key, Arc::clone(&entry));
            self.flush(&entry, now);
            listener.queue.push_back(entry);
            (listener.notify)();
            return;
        }
        let entry = {
            let connections = lock(&self.connections);
            let found = connections.get(&(from, packet.connection_id)).cloned();
            // A reset may carry either of the connection's ids.
            if found.is_none() && packet.kind == Kind::Reset {
                connections
                    .get(&(from, packet.connection_id.wrapping_add(1)))
                    .or_else(|| connections.get(&(from, packet.connection_id.wrapping_sub(1))))
                    .cloned()
            } else {
                found
            }
        };
        match entry {
            Some(entry) => self.deliver(&entry, &packet, now, batch),
            None if packet.kind != Kind::Reset => self.reset(&packet, from),
            None => {}
        }
    }

    fn deliver(&self, entry: &Arc<Entry>, packet: &Packet, now: Instant, batch: &mut Batch) {
        lock(&entry.core).on_packet(packet, now);
        if !batch
            .touched
            .iter()
            .any(|touched| Arc::ptr_eq(touched, entry))
        {
            batch.touched.push(Arc::clone(entry));
        }
    }

    fn flush(&self, entry: &Entry, now: Instant) {
        let datagrams = lock(&entry.core).flush(now);
        self.send(&datagrams, entry.address);
    }

    fn send(&self, datagrams: &[Vec<u8>], to: SocketAddr) {
        let Some(socket) = self.socket_for(&to) else {
            return;
        };
        for datagram in datagrams {
            let _ = socket.send_to(datagram, to);
        }
    }

    /// The socket of `to`'s address family, if there is one.
    fn socket_for(&self, to: &SocketAddr) -> Option<&UdpSocket> {
        self.sockets.iter().find(|socket| {
            socket
                .local_addr()
                .is_ok_and(|local| local.is_ipv6() == to.is_ipv6())
        })
    }

    fn reset(&self, packet: &Packet, to: SocketAddr) {
        let reset = Packet {
            kind: Kind::Reset,
            connection_id: packet.connection_id,
            timestamp: (Instant::now() - self.epoch).as_micros() as u32,
            timestamp_difference: 0,
            window: 0,
            seq_nr: random_u16(),
            ack_nr: packet.seq_nr,
            selective_ack: None,
            payload: Vec::new(),
        };
        self.send(&[reset.encode()], to);
    }

    /// Runs timeouts and removes finished connections.
    fn tick(&self) {
        let now = Instant::now();
        let entries: Vec<((SocketAddr, u16), Arc<Entry>)> = lock(&self.connections)
            .iter()
            .map(|(key, entry)| (*key, Arc::clone(entry)))
            .collect();
        for (key, entry) in entries {
            let dropped_at = *lock(&entry.dropped_at);
            let (datagrams, closed, finished) = {
                let mut core = lock(&entry.core);
                let was_open = core.state != State::Closed;
                core.tick(now);
                let datagrams = core.flush(now);
                let closed = was_open && core.state == State::Closed;
                let finished =
                    dropped_at.is_some_and(|dropped| core.finished() || now - dropped >= LINGER);
                (datagrams, closed, finished)
            };
            self.send(&datagrams, entry.address);
            if closed {
                entry.wake();
            }
            if finished {
                lock(&self.connections).remove(&key);
            }
        }
    }

    fn receive_loop(&self, index: usize) {
        let batch = std::cell::RefCell::new(Batch::default());
        let _ = crate::net::receive_batches(
            &self.sockets[index],
            &self.shutdown,
            |bytes, from| {
                if is_utp(bytes) {
                    self.handle_in_batch(bytes, from, &mut batch.borrow_mut());
                }
            },
            || self.finish(batch.take()),
        );
    }

    fn tick_loop(&self) {
        while !self.shutdown.load(Ordering::Acquire) {
            std::thread::sleep(TICK);
            self.tick();
        }
    }
}

/// A UDP port carrying uTP connections, in both directions.
pub struct UtpSocket {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for UtpSocket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UtpSocket")
            .field("local_addr", &self.local_addr().ok())
            .finish()
    }
}

impl UtpSocket {
    /// Binds a UDP port of its own. Use [`UtpSocket::on_dht`] instead when
    /// the DHT already holds the port.
    pub fn bind(address: SocketAddr) -> Result<Arc<Self>, crate::BackendError> {
        Self::bind_all(&[address])
    }

    /// Binds a UDP socket on each of `addresses` (from
    /// [`crate::Binding::listen_addresses`]): one per address family, on
    /// one port. IPv6 is left out where the system has none.
    pub fn bind_all(addresses: &[SocketAddr]) -> Result<Arc<Self>, crate::BackendError> {
        let port = addresses.first().map_or(0, SocketAddr::port);
        let sockets = crate::net::udp_sockets(addresses)
            .map_err(|error| crate::BackendError::Io(format!("uTP port {port}: {error}")))?;
        let count = sockets.len();
        let socket = Self::start(sockets);
        for index in 0..count {
            let shared = Arc::clone(&socket.shared);
            let receiver = std::thread::spawn(move || shared.receive_loop(index));
            lock(&socket.threads).push(receiver);
        }
        Ok(Arc::new(socket))
    }

    /// Shares the DHT's UDP sockets: the DHT hands uTP datagrams over.
    pub fn on_dht(dht: &crate::Dht) -> Result<Arc<Self>, crate::BackendError> {
        let socket = Self::start(dht.udp_sockets()?);
        dht.route_utp(Arc::clone(&socket.shared));
        Ok(Arc::new(socket))
    }

    fn start(sockets: Vec<UdpSocket>) -> Self {
        let shared = Arc::new(Shared {
            sockets,
            epoch: Instant::now(),
            connections: Mutex::new(HashMap::new()),
            listener: Mutex::new(None),
            next_listener: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
        });
        let ticker = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || shared.tick_loop())
        };
        Self {
            shared,
            threads: Mutex::new(vec![ticker]),
        }
    }

    /// The address of the first socket (IPv4 where there is one).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.shared
            .sockets
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no uTP socket"))?
            .local_addr()
    }

    /// Open connections, including closed ones still finishing.
    pub fn connections(&self) -> usize {
        lock(&self.shared.connections).len()
    }

    /// Accepts incoming connections until the listener is dropped.
    /// `notify` is called whenever one is waiting. Only one listener is
    /// active at a time; a new one replaces the old.
    pub(crate) fn listen(&self, notify: Notify) -> UtpListener {
        let id = self.shared.next_listener.fetch_add(1, Ordering::Relaxed);
        *lock(&self.shared.listener) = Some(Listener {
            queue: VecDeque::new(),
            notify,
            id,
        });
        UtpListener {
            shared: Arc::clone(&self.shared),
            id,
        }
    }

    /// Stops the socket's threads. Open streams fail.
    pub fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        for thread in lock(&self.threads).drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for UtpSocket {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
    }
}

/// Incoming connections for one server.
pub(crate) struct UtpListener {
    shared: Arc<Shared>,
    id: u64,
}

impl UtpListener {
    pub fn accept(&self) -> Option<UtpStream> {
        let entry = lock(&self.shared.listener)
            .as_mut()
            .filter(|listener| listener.id == self.id)?
            .queue
            .pop_front()?;
        Some(UtpStream::new(Arc::clone(&self.shared), entry))
    }
}

impl Drop for UtpListener {
    fn drop(&mut self) {
        let mut listener = lock(&self.shared.listener);
        if listener
            .as_ref()
            .is_some_and(|listener| listener.id == self.id)
        {
            let waiting = listener.take().map(|listener| listener.queue);
            drop(listener);
            // Close connections nobody accepted.
            for entry in waiting.into_iter().flatten() {
                drop(UtpStream::new(Arc::clone(&self.shared), entry));
            }
        }
    }
}

/// One uTP connection: a reliable, ordered byte stream like `TcpStream`.
pub struct UtpStream {
    shared: Arc<Shared>,
    entry: Arc<Entry>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    nonblocking: bool,
}

impl std::fmt::Debug for UtpStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UtpStream")
            .field("peer", &self.entry.address)
            .finish()
    }
}

impl UtpStream {
    fn new(shared: Arc<Shared>, entry: Arc<Entry>) -> Self {
        Self {
            shared,
            entry,
            read_timeout: None,
            write_timeout: None,
            nonblocking: false,
        }
    }

    /// Connects to a peer, waiting up to `timeout` for it to answer.
    pub fn connect(socket: &UtpSocket, address: SocketAddr, timeout: Duration) -> io::Result<Self> {
        let shared = Arc::clone(&socket.shared);
        let now = Instant::now();
        let entry = {
            let mut connections = lock(&shared.connections);
            let recv_id = loop {
                let id = random_u16();
                if !connections.contains_key(&(address, id)) {
                    break id;
                }
            };
            let entry = Entry::new(
                Core::connect(recv_id, random_u16(), shared.epoch, now),
                address,
            );
            connections.insert((address, recv_id), Arc::clone(&entry));
            entry
        };
        shared.flush(&entry, now);
        let stream = Self::new(shared, entry);
        let deadline = now + timeout;
        let mut core = lock(&stream.entry.core);
        loop {
            match core.state {
                State::Connected => break,
                State::Closed => {
                    return Err(core.error.unwrap_or(ErrorKind::ConnectionRefused).into());
                }
                State::SynSent => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                core.close_with(ErrorKind::TimedOut);
                return Err(ErrorKind::TimedOut.into());
            }
            core = stream.entry.wait(core, remaining);
        }
        drop(core);
        Ok(stream)
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.entry.address
    }

    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    /// In non-blocking mode, reads and writes that cannot proceed fail
    /// with `WouldBlock`, and the callback from [`UtpStream::set_notify`]
    /// says when to retry.
    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        self.nonblocking = nonblocking;
    }

    /// Calls `notify` whenever the stream may have become readable or
    /// writable, or closed.
    pub(crate) fn set_notify(&self, notify: Notify) {
        *lock(&self.entry.notify) = Some(notify);
    }

    /// Waits for `entry.changed` until `deadline`, failing on timeout.
    fn wait<'a>(
        &'a self,
        core: MutexGuard<'a, Core>,
        deadline: Option<Instant>,
    ) -> io::Result<MutexGuard<'a, Core>> {
        if self.nonblocking {
            return Err(ErrorKind::WouldBlock.into());
        }
        let remaining = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        if remaining.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        Ok(self.entry.wait(core, remaining))
    }
}

impl Read for UtpStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let deadline = self.read_timeout.map(|timeout| Instant::now() + timeout);
        let mut core = lock(&self.entry.core);
        loop {
            let count = core.read(buffer);
            if count > 0 || buffer.is_empty() {
                let datagrams = core.flush(Instant::now());
                drop(core);
                self.shared.send(&datagrams, self.entry.address);
                return Ok(count);
            }
            if core.eof {
                return Ok(0);
            }
            if core.state == State::Closed {
                return Err(core.error.unwrap_or(ErrorKind::ConnectionAborted).into());
            }
            core = self.wait(core, deadline)?;
        }
    }
}

impl Write for UtpStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let deadline = self.write_timeout.map(|timeout| Instant::now() + timeout);
        let mut core = lock(&self.entry.core);
        loop {
            if core.state == State::Closed || core.closing {
                return Err(core.error.unwrap_or(ErrorKind::BrokenPipe).into());
            }
            if data.is_empty() {
                return Ok(0);
            }
            if core.can_write() {
                let count = core.write(data);
                let datagrams = core.flush(Instant::now());
                drop(core);
                self.shared.send(&datagrams, self.entry.address);
                return Ok(count);
            }
            core = self.wait(core, deadline)?;
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for UtpStream {
    /// Sends the remaining data and a FIN in the background.
    fn drop(&mut self) {
        let now = Instant::now();
        let datagrams = {
            let mut core = lock(&self.entry.core);
            core.close();
            core.flush(now)
        };
        self.shared.send(&datagrams, self.entry.address);
        *lock(&self.entry.notify) = None;
        *lock(&self.entry.dropped_at) = Some(now);
    }
}

fn random_u16() -> u16 {
    let mut bytes = [0; 2];
    crate::mse::fill_random(&mut bytes).unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        bytes = (nanos as u16).to_le_bytes();
    });
    u16::from_le_bytes(bytes)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(kind: Kind) -> Packet {
        Packet {
            kind,
            connection_id: 0x1234,
            timestamp: 1,
            timestamp_difference: 2,
            window: 3,
            seq_nr: 4,
            ack_nr: 5,
            selective_ack: None,
            payload: b"data".to_vec(),
        }
    }

    #[test]
    fn packets_round_trip_with_selective_acks() {
        let mut original = packet(Kind::Data);
        assert_eq!(Packet::decode(&original.encode()), Some(original.clone()));
        original.selective_ack = Some(vec![0b101, 0, 0, 0x80]);
        let encoded = original.encode();
        assert_eq!(encoded[0], 0x01);
        assert_eq!(encoded[1], EXTENSION_SELECTIVE_ACK);
        assert_eq!(Packet::decode(&encoded), Some(original));
    }

    #[test]
    fn skips_unknown_extensions() {
        let mut bytes = packet(Kind::State).encode();
        let payload = bytes.split_off(HEADER_LENGTH);
        bytes[1] = 7; // an unknown extension, then a selective ACK
        bytes.extend([EXTENSION_SELECTIVE_ACK, 2, 0xaa, 0xbb]);
        bytes.extend([0, 4, 1, 0, 0, 0]);
        bytes.extend(payload);
        let decoded = Packet::decode(&bytes).unwrap();
        assert_eq!(decoded.selective_ack, Some(vec![1, 0, 0, 0]));
        assert_eq!(decoded.payload, b"data");
    }

    #[test]
    fn tells_utp_from_dht_messages() {
        assert!(is_utp(&packet(Kind::Syn).encode()));
        assert!(!is_utp(
            b"d1:ad2:id20:aaaaaaaaaaaaaaaaaaaae1:q4:ping1:t2:aa1:y1:qe"
        ));
        assert!(!is_utp(&[0x51; 20])); // type 5 does not exist
    }

    #[test]
    fn compares_wrapping_sequence_numbers() {
        assert!(before(1, 2));
        assert!(before(65_535, 0));
        assert!(!before(2, 1));
        assert!(!before(7, 7));
    }

    /// Two connection cores exchanging packets through a link that drops,
    /// duplicates, and reorders them, with simulated time.
    struct Link {
        a: Core,
        b: Core,
        now: Instant,
        state: u64,
        loss_percent: u64,
    }

    impl Link {
        fn new(loss_percent: u64) -> Self {
            let epoch = Instant::now();
            let mut a = Core::connect(100, 5, epoch, epoch);
            let syn = Packet::decode(&a.flush(epoch)[0]).unwrap();
            assert_eq!(syn.kind, Kind::Syn);
            assert_eq!(syn.connection_id, 100);
            let b = Core::accept(&syn, 60_000, epoch, epoch);
            Self {
                a,
                b,
                now: epoch,
                state: 0x2545_f491_4f6c_dd1d,
                loss_percent,
            }
        }

        fn random(&mut self) -> u64 {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            self.state
        }

        /// Moves packets both ways for one simulated step of 5 ms.
        fn step(&mut self) {
            self.now += Duration::from_millis(5);
            let now = self.now;
            self.a.tick(now);
            self.b.tick(now);
            let mut to_b = self.a.flush(now);
            let mut to_a = self.b.flush(now);
            for datagrams in [&mut to_b, &mut to_a] {
                datagrams.retain(|_| true);
                let mut kept = Vec::new();
                for datagram in datagrams.drain(..) {
                    let roll = self.random() % 100;
                    if roll < self.loss_percent {
                        continue;
                    }
                    if roll < self.loss_percent + 3 {
                        kept.push(datagram.clone()); // duplicated
                    }
                    kept.push(datagram);
                }
                if kept.len() > 1 && self.random().is_multiple_of(4) {
                    kept.swap(0, 1);
                }
                *datagrams = kept;
            }
            for datagram in to_b {
                self.b.on_packet(&Packet::decode(&datagram).unwrap(), now);
            }
            for datagram in to_a {
                self.a.on_packet(&Packet::decode(&datagram).unwrap(), now);
            }
        }
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| (index * 7 + index / 251) as u8)
            .collect()
    }

    fn transfer(loss_percent: u64, length: usize) {
        let mut link = Link::new(loss_percent);
        let data = pattern(length);
        let (mut written, mut received) = (0, Vec::new());
        let mut buffer = vec![0; 64 * 1024];
        for _ in 0..200_000 {
            if written < data.len() {
                written += link.a.write(&data[written..]);
            } else {
                link.a.close();
            }
            link.step();
            loop {
                let count = link.b.read(&mut buffer);
                if count == 0 {
                    break;
                }
                received.extend_from_slice(&buffer[..count]);
            }
            if link.b.eof && link.a.fin_acked {
                break;
            }
        }
        assert_eq!(link.a.state, State::Connected, "{:?}", link.a.error);
        assert!(link.b.eof, "the FIN never arrived");
        assert!(link.a.fin_acked, "the FIN was never acknowledged");
        assert_eq!(received.len(), data.len());
        assert!(received == data, "data was corrupted");
    }

    #[test]
    fn transfers_over_a_clean_link() {
        transfer(0, 500_000);
    }

    #[test]
    fn transfers_over_a_lossy_reordering_link() {
        transfer(10, 300_000);
    }

    #[test]
    fn connecting_times_out_without_an_answer() {
        let epoch = Instant::now();
        let mut core = Core::connect(1, 1, epoch, epoch);
        let mut now = epoch;
        let mut syns = 0;
        while core.state != State::Closed && now - epoch < Duration::from_secs(60) {
            syns += core.flush(now).len();
            now += Duration::from_millis(100);
            core.tick(now);
        }
        assert_eq!(core.error, Some(ErrorKind::TimedOut));
        assert_eq!(syns, 1 + MAX_SYN_TIMEOUTS as usize);
        assert!(now - epoch < Duration::from_secs(10));
    }

    #[test]
    fn streams_carry_data_both_ways_over_real_sockets() {
        let server = UtpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let client = UtpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (sender, accepted) = std::sync::mpsc::channel();
        let listener = server.listen(Arc::new(move || {
            let _ = sender.send(());
        }));
        let address = server.local_addr().unwrap();
        let data = pattern(2_000_000);

        let expected = data.clone();
        let echo = std::thread::spawn(move || {
            accepted.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut stream = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut received = Vec::new();
            stream.read_to_end(&mut received).unwrap();
            assert!(received == expected, "server received corrupted data");
            stream.write_all(b"thanks").unwrap();
        });

        let mut stream = UtpStream::connect(&client, address, Duration::from_secs(5)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10)));
        stream.set_write_timeout(Some(Duration::from_secs(10)));
        stream.write_all(&data).unwrap();
        // Half-close by hand: mark closing so the FIN follows the data.
        lock(&stream.entry.core).close();
        stream.shared.flush(&stream.entry, Instant::now());
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"thanks");
        echo.join().unwrap();
    }

    #[test]
    fn shares_the_dht_port() {
        let dht = crate::Dht::start(0, Vec::new(), None).unwrap();
        let server = UtpSocket::on_dht(&dht).unwrap();
        let (sender, accepted) = std::sync::mpsc::channel();
        let listener = server.listen(Arc::new(move || {
            let _ = sender.send(());
        }));
        let port = server.local_addr().unwrap().port();
        let client = UtpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = UtpStream::connect(&client, address, Duration::from_secs(5)).unwrap();
        stream.write_all(b"over the DHT's port").unwrap();
        accepted.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut incoming = listener.accept().unwrap();
        incoming.set_read_timeout(Some(Duration::from_secs(5)));
        let mut received = [0; 19];
        incoming.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"over the DHT's port");
        dht.stop();
    }

    #[test]
    fn carries_connections_over_both_families_on_one_port() {
        let both: [SocketAddr; 2] = ["127.0.0.1:0".parse().unwrap(), "[::1]:0".parse().unwrap()];
        let server = UtpSocket::bind_all(&both).unwrap();
        if server.shared.sockets.len() < 2 {
            eprintln!("skipped: no IPv6 here");
            return;
        }
        let client = UtpSocket::bind_all(&both).unwrap();
        let (sender, accepted) = std::sync::mpsc::channel();
        let listener = server.listen(Arc::new(move || {
            let _ = sender.send(());
        }));
        let port = server.local_addr().unwrap().port();
        for address in [
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
        ] {
            let mut stream = UtpStream::connect(&client, address, Duration::from_secs(5)).unwrap();
            stream.write_all(b"family").unwrap();
            accepted.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut incoming = listener.accept().unwrap();
            assert_eq!(incoming.peer_addr().is_ipv6(), address.is_ipv6());
            incoming.set_read_timeout(Some(Duration::from_secs(5)));
            let mut received = [0; 6];
            incoming.read_exact(&mut received).unwrap();
            assert_eq!(&received, b"family");
        }
    }

    #[test]
    fn connecting_to_a_port_without_a_listener_is_refused() {
        let server = UtpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let client = UtpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let started = Instant::now();
        let error = UtpStream::connect(
            &client,
            server.local_addr().unwrap(),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionRefused);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
