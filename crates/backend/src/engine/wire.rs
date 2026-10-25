//! The peer protocol for one connection, as a state machine without I/O.
//!
//! [`PeerWire`] takes in what a peer sends ([`PeerWire::on_message`]) and
//! the passing of time ([`PeerWire::tick`]), and asks for [`Action`]s:
//! messages to send, blocks to read, pieces to check, hash requests to
//! answer. Whoever drives it performs the actions and reports results back
//! ([`PeerWire::block_read`], [`PeerWire::piece_checked`]). The same
//! protocol code then serves both a blocking connection and an event loop,
//! for downloading and seeding alike.
//!
//! The protocol, per connection:
//! - We download pieces the peer has and we need, keeping about two seconds
//!   of requests in flight across several pieces.
//! - We upload verified pieces it asks for while the choker lets it (see
//!   [`crate::swarm`]), and announce pieces as they are verified.
//! - With the Fast Extension (BEP 6), which both handshakes must advertise:
//!   `have_all` and `have_none` may replace the bitfield, and every request
//!   gets a block or a `reject`, so a choke no longer discards outstanding
//!   requests. Each side may also download its allowed fast pieces while
//!   choked, and suggested pieces are tried first.
//! - With the extension protocol (BEP 10) we send the address we see the
//!   peer at and our listen port, and serve metadata to peers fetching a
//!   magnet link. Unless the torrent is private (BEP 27) we also exchange
//!   peers (BEP 11): a full list once the peer's extension handshake
//!   arrives, then changes at most once a minute.
//! - Connections neither side needs close, keep-alives go out, silent peers
//!   are dropped, and a peer that stops sending requested blocks is
//!   snubbed.
//!
//! Blocks we did not request are ignored rather than closing the
//! connection, since an end-game cancel can cross a block in flight.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::active::{ActiveTorrent, Checked, Download, has_bit, invalid_peer};
use crate::PeerMessage;

/// What the driver of a [`PeerWire`] must do.
pub(crate) enum Action {
    Send(PeerMessage),
    /// Read a block of a verified piece; report it with
    /// [`PeerWire::block_read`].
    ReadBlock {
        piece_index: u32,
        begin: u32,
        length: u32,
    },
    /// Check and store a completed piece with
    /// [`ActiveTorrent::check_piece`]; report it with
    /// [`PeerWire::piece_checked`].
    CheckPiece {
        download: Download,
        data: Vec<u8>,
    },
    /// Answer a BEP 52 hash request with [`ActiveTorrent::answer_hashes`]
    /// and send the reply.
    AnswerHashes(PeerMessage),
}

/// Whether a connection should continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    /// Neither side needs it any more, or a duplicate replaced it.
    Finished,
}

/// What we know about a connection when it starts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Link {
    pub(crate) address: SocketAddr,
    pub(crate) key: crate::swarm::PeerKey,
    /// We connected to the peer, rather than it to us.
    pub(crate) outgoing: bool,
    /// Its handshake advertised the Fast Extension (ours always does).
    pub(crate) fast: bool,
    /// Its handshake advertised the extension protocol.
    pub(crate) extensions: bool,
    pub(crate) utp: bool,
    pub(crate) encrypted: bool,
}

pub(crate) struct PeerWire {
    torrent: Arc<ActiveTorrent>,
    link: Link,
    private: bool,
    tie_breaker: usize,
    /// The pieces the peer has, once it said, and how many that is.
    bitfield: Option<Vec<u8>>,
    have: usize,
    /// How far into the swarm's verified pieces we have announced.
    have_cursor: usize,
    /// Pieces this peer may download from us while choked.
    granted: Vec<u32>,
    /// Pieces the peer lets us download while choked, and suggests.
    allowed: Vec<u32>,
    suggested: VecDeque<u32>,
    remote: crate::metadata::RemoteMetadata,
    pex: Pex,
    pex_flags: u8,
    state: State,
    /// Pieces in flight from this peer, oldest first.
    downloads: Vec<Download>,
    /// Block reads requested from the driver and not yet reported.
    reads_in_flight: usize,
}

impl PeerWire {
    pub(crate) fn new(torrent: Arc<ActiveTorrent>, link: Link, now: Instant) -> Self {
        let tie_breaker = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            link.address.hash(&mut hasher);
            hasher.finish() as usize
        };
        let granted = if link.fast {
            crate::fast::allowed_fast_set(
                link.address.ip(),
                &torrent.info.peer_info_hash(),
                torrent.piece_count(),
                crate::fast::ALLOWED_FAST_COUNT,
            )
        } else {
            Vec::new()
        };
        torrent.connection_opened();
        torrent
            .swarm
            .describe(link.key, link.outgoing, link.utp, link.encrypted);
        let mut pex_flags = 0;
        if link.utp {
            pex_flags |= crate::pex::SUPPORTS_UTP;
        }
        if link.encrypted {
            pex_flags |= crate::pex::PREFERS_ENCRYPTION;
        }
        Self {
            private: torrent.info.is_private(),
            have_cursor: 0,
            tie_breaker,
            bitfield: None,
            have: 0,
            granted,
            allowed: Vec::new(),
            suggested: VecDeque::new(),
            remote: crate::metadata::RemoteMetadata::default(),
            pex: Pex::default(),
            pex_flags,
            state: State::new(now),
            downloads: Vec::new(),
            reads_in_flight: 0,
            torrent,
            link,
        }
    }

    /// The messages that open a connection: what we have, the peer's
    /// allowed fast pieces, and our extension handshake.
    pub(crate) fn start(&mut self, out: &mut Vec<Action>) -> Result<(), crate::BackendError> {
        let torrent = Arc::clone(&self.torrent);
        // Haves from here on reach this peer; the bitfield covers the rest.
        self.have_cursor = torrent.swarm.have_count();
        let (ours, complete) = {
            let pieces = torrent.pieces()?;
            (pieces.complete_bitfield(), pieces.is_complete())
        };
        let empty = ours.iter().all(|byte| *byte == 0);
        match (self.link.fast, empty, complete) {
            (true, true, _) => out.push(Action::Send(PeerMessage::HaveNone)),
            (true, _, true) => out.push(Action::Send(PeerMessage::HaveAll)),
            (_, false, _) => out.push(Action::Send(PeerMessage::Bitfield(ours))),
            (false, true, _) => {}
        }
        for index in &self.granted {
            if torrent.has_piece(*index as usize) {
                out.push(Action::Send(PeerMessage::AllowedFast {
                    piece_index: *index,
                }));
            }
        }
        if self.link.extensions {
            let offer = crate::metadata::ExtensionOffer {
                metadata_size: torrent.info.info_bytes().map(<[u8]>::len),
                pex: !self.private,
                your_ip: Some(self.link.address.ip()),
                listen_port: torrent.swarm.listen_port(),
            };
            out.push(Action::Send(crate::metadata::extension_handshake_with(
                &offer,
            )));
        }
        // A peer we connected to is reachable at that address.
        if self.link.outgoing {
            torrent.swarm.set_pex(
                self.link.key,
                self.link.address,
                self.pex_flags | crate::pex::REACHABLE,
            );
        }
        Ok(())
    }

    /// Everything that happens with time rather than in reply to a
    /// message: announcing pieces, interest and choking, requests,
    /// uploads, timeouts, peer exchange, and keep-alives.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        out: &mut Vec<Action>,
    ) -> Result<Flow, crate::BackendError> {
        let torrent = Arc::clone(&self.torrent);
        let key = self.link.key;
        if torrent.swarm.is_evicted(key) {
            return Ok(Flow::Finished);
        }

        // Announce pieces verified through other connections.
        for index in torrent.swarm.haves_since(&mut self.have_cursor) {
            if !self.peer_has(index as usize) {
                self.send(out, PeerMessage::Have { piece_index: index }, now);
                if self.granted.contains(&index) {
                    self.send(out, PeerMessage::AllowedFast { piece_index: index }, now);
                }
            }
            self.state.interest_changed = true;
        }
        if self.state.interest_changed {
            self.state.interest_changed = false;
            let seed = self
                .bitfield
                .as_deref()
                .is_some_and(|bits| (0..torrent.piece_count()).all(|index| has_bit(bits, index)));
            if seed {
                torrent.swarm.update_pex_flags(key, crate::pex::SEED, 0);
            }
            let wanted = self
                .bitfield
                .as_ref()
                .is_some_and(|bits| torrent.pieces().is_ok_and(|pieces| pieces.wants_any(bits)));
            if wanted != self.state.am_interested {
                self.state.am_interested = wanted;
                self.report_download_state();
                let message = if wanted {
                    PeerMessage::Interested
                } else {
                    PeerMessage::NotInterested
                };
                self.send(out, message, now);
            }
        }
        self.update_choke(now, out);
        self.request_pieces(now, out)?;
        // A peer that stops sending requested blocks is snubbed: its pieces
        // go to other peers, and it gets no requests for a while. It is
        // kept only if it still wants data from us.
        let waiting = self
            .downloads
            .iter()
            .any(|download| !download.outstanding.is_empty());
        if waiting && now - self.state.last_block > torrent.timeout {
            self.downloads.clear();
            self.state.snubbed_until = Some(now + SNUB_COOLDOWN);
            if !self.state.peer_interested {
                return Err(invalid_peer("peer stopped sending requested blocks"));
            }
        }

        self.pump_uploads(out);

        // Connections that neither side needs make room for others.
        if self.state.am_interested
            || self.state.peer_interested
            || !self.downloads.is_empty()
            || !self.state.uploads.is_empty()
            || self.reads_in_flight > 0
        {
            self.state.idle_since = None;
        } else if now - *self.state.idle_since.get_or_insert(now) >= IDLE_LIMIT {
            return Ok(Flow::Finished);
        }
        if now - self.state.last_received >= SILENCE_LIMIT {
            return Err(invalid_peer("peer went silent"));
        }
        if let Some(id) = self.remote.pex_id.filter(|_| !self.private)
            && self.pex.next.is_some_and(|next| now >= next)
        {
            let message = self.pex.changes(&torrent.swarm.pex_contacts(key));
            if message.is_empty() {
                // The first message waits until there is someone to name.
                self.pex.next = Some(
                    now + if self.pex.sent_any {
                        PEX_INTERVAL
                    } else {
                        PEX_RETRY
                    },
                );
            } else {
                let payload = message.encode();
                self.send(out, PeerMessage::Extended { id, payload }, now);
                self.pex.sent_any = true;
                self.pex.next = Some(now + PEX_INTERVAL);
            }
        }
        if now - self.state.last_sent >= KEEP_ALIVE_INTERVAL {
            self.send(out, PeerMessage::KeepAlive, now);
        }
        Ok(Flow::Continue)
    }

    /// Chokes or unchokes the peer as the swarm's choker decided.
    fn update_choke(&mut self, now: Instant, out: &mut Vec<Action>) {
        let unchoke =
            self.state.peer_interested && self.torrent.swarm.is_unchoked(self.link.key, now);
        if unchoke != self.state.am_choking {
            return;
        }
        self.state.am_choking = !unchoke;
        let message = if unchoke {
            PeerMessage::Unchoke
        } else {
            PeerMessage::Choke
        };
        self.send(out, message, now);
        // Choking drops the peer's queued requests (BEP 3). With the Fast
        // Extension each is rejected, after the choke, except those for
        // allowed fast pieces.
        if self.state.am_choking {
            for request in std::mem::take(&mut self.state.uploads) {
                if !self.link.fast {
                    continue;
                }
                if self.granted.contains(&request.0) {
                    self.state.uploads.push_back(request);
                } else {
                    out.push(Action::Send(reject(request)));
                }
            }
        }
    }

    /// Keeps enough requests in flight to cover a couple of seconds at this
    /// peer's rate, across as many pieces as that takes, so the pipeline
    /// never drains between pieces. While choked, only its allowed fast
    /// pieces may be requested.
    fn request_pieces(
        &mut self,
        now: Instant,
        out: &mut Vec<Action>,
    ) -> Result<(), crate::BackendError> {
        let torrent = Arc::clone(&self.torrent);
        self.state.update_rate(now);
        let depth = self.state.request_depth(self.remote.reqq);
        let snubbed = self.state.snubbed_until.is_some_and(|until| now < until);
        let choked = self.state.peer_choking;
        // In end-game another peer may finish a piece first.
        let mut position = 0;
        while position < self.downloads.len() {
            if torrent.has_piece(self.downloads[position].index) {
                for request in self.downloads.remove(position).outstanding {
                    out.push(Action::Send(PeerMessage::Cancel {
                        piece_index: request.piece_index,
                        begin: request.begin,
                        length: request.length,
                    }));
                }
            } else {
                position += 1;
            }
        }
        if !self.state.am_interested || snubbed || (choked && self.allowed.is_empty()) {
            return Ok(());
        }
        let allowed = &self.allowed;
        let requestable =
            |download: &Download| !choked || allowed.contains(&(download.index as u32));
        while let Some(bits) = self.bitfield.as_deref() {
            let queued: usize = self
                .downloads
                .iter()
                .filter(|download| requestable(download))
                .map(|download| download.outstanding.len() + download.unsent.len())
                .sum();
            if queued >= depth || self.downloads.len() >= MAX_PIECES_PER_PEER {
                break;
            }
            let next = if choked {
                torrent.pick_piece(bits, self.tie_breaker, allowed, true)?
            } else {
                let preferred: Vec<u32> = self.suggested.iter().copied().collect();
                torrent.pick_piece(bits, self.tie_breaker, &preferred, false)?
            };
            match next {
                // End-game offers pieces already in flight; one copy per
                // connection is enough.
                Some(download)
                    if !self
                        .downloads
                        .iter()
                        .any(|other| other.index == download.index) =>
                {
                    self.downloads.push(download);
                }
                Some(_) => break,
                None => {
                    if self.downloads.is_empty() && !choked {
                        self.state.interest_changed = true;
                    }
                    break;
                }
            }
        }
        let mut outstanding: usize = self
            .downloads
            .iter()
            .map(|download| download.outstanding.len())
            .sum();
        for download in self.downloads.iter_mut() {
            if !requestable(download) {
                continue;
            }
            while outstanding < depth
                && let Some(request) = download.unsent.pop_front()
            {
                // Waiting time starts when requests go out.
                if outstanding == 0 {
                    self.state.last_block = now;
                }
                out.push(Action::Send(PeerMessage::Request {
                    piece_index: request.piece_index,
                    begin: request.begin,
                    length: request.length,
                }));
                download.outstanding.push(request);
                outstanding += 1;
                self.state.last_sent = now;
            }
        }
        Ok(())
    }

    /// Handles one message from the peer.
    pub(crate) fn on_message(
        &mut self,
        message: PeerMessage,
        now: Instant,
        out: &mut Vec<Action>,
    ) -> Result<Flow, crate::BackendError> {
        let torrent = Arc::clone(&self.torrent);
        let key = self.link.key;
        let fast = self.link.fast;
        self.state.last_received = now;
        match message {
            PeerMessage::Bitfield(bits) if self.bitfield.is_none() => {
                validate_bitfield(&bits, torrent.piece_count())?;
                torrent.pieces()?.add_peer_pieces(&bits);
                self.have = (0..torrent.piece_count())
                    .filter(|index| has_bit(&bits, *index))
                    .count();
                self.bitfield = Some(bits);
                self.state.interest_changed = true;
                self.report_pieces();
            }
            PeerMessage::Extended { id: 0, payload } => {
                self.remote = crate::metadata::parse_extension_handshake(&payload)?;
                torrent.swarm.set_client(key, self.remote.client.clone());
                if let Some(ip) = self.remote.your_ip {
                    torrent.swarm.vote_external(self.link.address, ip);
                }
                // A peer that connected to us can be reached on its listen
                // port, if it told us one.
                if !self.link.outgoing
                    && let Some(port) = self.remote.listen_port
                {
                    torrent.swarm.set_pex(
                        key,
                        SocketAddr::new(self.link.address.ip(), port),
                        self.pex_flags,
                    );
                }
                if self.remote.pex_id.is_some() && !self.private && self.pex.next.is_none() {
                    self.pex.next = Some(now);
                }
            }
            PeerMessage::Extended {
                id: crate::metadata::UT_METADATA,
                payload,
            } => {
                let replies = crate::metadata::serve_extended(
                    crate::metadata::UT_METADATA,
                    &payload,
                    torrent.info.info_bytes(),
                    &mut self.remote,
                )?;
                out.extend(replies.into_iter().map(Action::Send));
            }
            PeerMessage::Extended {
                id: crate::pex::UT_PEX,
                payload,
            } if !self.private => {
                // More than one message per interval is ignored.
                if self
                    .pex
                    .last_received
                    .is_none_or(|last| now - last >= PEX_INTERVAL / 2)
                {
                    self.pex.last_received = Some(now);
                    let message = crate::pex::PexMessage::decode(&payload)?;
                    torrent.swarm.discover(self.link.address, &message.added);
                }
            }
            PeerMessage::HaveAll
            | PeerMessage::HaveNone
            | PeerMessage::SuggestPiece { .. }
            | PeerMessage::RejectRequest { .. }
            | PeerMessage::AllowedFast { .. }
                if !fast =>
            {
                return Err(invalid_peer(
                    "peer sent a Fast Extension message without negotiating it",
                ));
            }
            PeerMessage::HaveAll if self.bitfield.is_none() => {
                let bits = full_bitfield(torrent.piece_count());
                torrent.pieces()?.add_peer_pieces(&bits);
                self.bitfield = Some(bits);
                self.have = torrent.piece_count();
                self.state.interest_changed = true;
                self.report_pieces();
            }
            PeerMessage::HaveNone if self.bitfield.is_none() => {
                self.bitfield = Some(vec![0; torrent.piece_count().div_ceil(8)]);
                self.report_pieces();
            }
            PeerMessage::SuggestPiece { piece_index } => {
                if (piece_index as usize) < torrent.piece_count()
                    && !self.suggested.contains(&piece_index)
                {
                    self.suggested.push_front(piece_index);
                    self.suggested.truncate(MAX_SUGGESTIONS);
                }
            }
            PeerMessage::AllowedFast { piece_index } => {
                if (piece_index as usize) < torrent.piece_count()
                    && !self.allowed.contains(&piece_index)
                    && self.allowed.len() < MAX_ALLOWED_FAST
                {
                    self.allowed.push(piece_index);
                }
            }
            PeerMessage::RejectRequest {
                piece_index,
                begin,
                length,
            } => {
                let Some((index, position)) = self.find_request(piece_index, begin, length) else {
                    return Ok(Flow::Continue);
                };
                let request = self.downloads[index].outstanding.swap_remove(position);
                if self.state.peer_choking {
                    // Asked again once unchoked.
                    self.downloads[index].unsent.push_front(request);
                } else {
                    // An unchoked peer refusing a block will not serve the
                    // piece; another peer can.
                    self.downloads.remove(index);
                }
            }
            PeerMessage::Have { piece_index } => {
                self.record_have(piece_index)?;
                self.state.interest_changed = true;
            }
            PeerMessage::Choke => {
                self.state.peer_choking = true;
                self.report_download_state();
                // Without the Fast Extension outstanding requests are
                // discarded, so the claim is released for other peers. With
                // it, each is answered with a block or a reject.
                if !fast {
                    self.downloads.clear();
                }
            }
            PeerMessage::Unchoke => {
                self.state.peer_choking = false;
                self.report_download_state();
            }
            PeerMessage::Interested => {
                self.state.peer_interested = true;
                torrent.swarm.set_interested(key, true);
                // With a slot free, unchoke at once rather than next tick.
                self.update_choke(now, out);
            }
            PeerMessage::NotInterested => {
                self.state.peer_interested = false;
                for request in std::mem::take(&mut self.state.uploads) {
                    if fast {
                        out.push(Action::Send(reject(request)));
                    }
                }
                torrent.swarm.set_interested(key, false);
            }
            PeerMessage::Request {
                piece_index,
                begin,
                length,
            } => {
                // Requests while choked, for pieces we lack, or beyond the
                // queue limit are refused: ignored, or with the Fast
                // Extension rejected. A choked peer may still get its
                // allowed fast pieces while it has only a few.
                let starting = self.bitfield.as_deref().map_or(0, |bits| {
                    bits.iter().map(|byte| byte.count_ones()).sum::<u32>()
                }) < crate::fast::ALLOWED_FAST_COUNT as u32;
                let permitted = !self.state.am_choking
                    || (fast && starting && self.granted.contains(&piece_index));
                if permitted
                    && self.state.uploads.len() < MAX_QUEUED_UPLOADS
                    && torrent.valid_upload(piece_index, begin, length)
                {
                    self.state.uploads.push_back((piece_index, begin, length));
                } else if fast {
                    out.push(Action::Send(reject((piece_index, begin, length))));
                }
            }
            PeerMessage::Cancel {
                piece_index,
                begin,
                length,
            } => {
                let request = (piece_index, begin, length);
                let queued = self.state.uploads.len();
                self.state.uploads.retain(|queued| *queued != request);
                // With the Fast Extension a cancelled request is still
                // answered; one already sent was answered by its block.
                if fast && self.state.uploads.len() < queued {
                    out.push(Action::Send(reject(request)));
                }
            }
            PeerMessage::Piece {
                piece_index,
                begin,
                block,
            } => {
                // Only replies to outstanding requests count, so a late or
                // duplicate block is never stored twice.
                let found = self
                    .downloads
                    .iter()
                    .enumerate()
                    .find_map(|(index, download)| {
                        download
                            .outstanding
                            .iter()
                            .position(|request| {
                                request.piece_index == piece_index
                                    && request.begin == begin
                                    && request.length as usize == block.len()
                            })
                            .map(|position| (index, position))
                    });
                let Some((index, position)) = found else {
                    return Ok(Flow::Continue);
                };
                self.downloads[index].outstanding.swap_remove(position);
                self.state.last_block = now;
                self.state.snubbed_until = None;
                self.state.window_bytes += block.len() as u64;
                torrent.progress.add_received(block.len() as u64);
                torrent.swarm.record_download(key, block.len() as u64);
                if let Some(data) =
                    self.downloads[index]
                        .blocks
                        .receive(piece_index, begin, block)?
                {
                    let download = self.downloads.remove(index);
                    out.push(Action::CheckPiece { download, data });
                }
            }
            PeerMessage::HashRequest { .. } => out.push(Action::AnswerHashes(message)),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Asks for queued uploads to be read, a few at a time. Everything
    /// queued may be sent, since a choke removes what it no longer allows.
    /// Drivers call this when the connection can take more output.
    pub(crate) fn pump_uploads(&mut self, out: &mut Vec<Action>) {
        while self.reads_in_flight < MAX_READS_IN_FLIGHT
            && let Some((piece_index, begin, length)) = self.state.uploads.pop_front()
        {
            self.reads_in_flight += 1;
            out.push(Action::ReadBlock {
                piece_index,
                begin,
                length,
            });
        }
    }

    /// A block read for [`Action::ReadBlock`]; sends it.
    pub(crate) fn block_read(
        &mut self,
        piece_index: u32,
        begin: u32,
        block: Vec<u8>,
        now: Instant,
        out: &mut Vec<Action>,
    ) {
        self.reads_in_flight = self.reads_in_flight.saturating_sub(1);
        self.torrent
            .swarm
            .record_upload(self.link.key, block.len() as u64);
        self.send(
            out,
            PeerMessage::Piece {
                piece_index,
                begin,
                block,
            },
            now,
        );
    }

    /// The outcome of [`Action::CheckPiece`]. An address that keeps sending
    /// corrupt pieces is banned, which ends the connection.
    pub(crate) fn piece_checked(
        &mut self,
        index: usize,
        checked: Checked,
    ) -> Result<(), crate::BackendError> {
        let torrent = &self.torrent;
        let address = self.link.address;
        match checked {
            Checked::Corrupt => {
                // The claim was dropped, so the piece is downloaded again,
                // from whichever peer gets it next.
                let error = invalid_peer(format!("{address} sent a corrupt copy of piece {index}"));
                torrent.progress.record_error(&error);
                if torrent.swarm.piece_corrupt(address) {
                    return Err(invalid_peer(format!(
                        "banned {address} for sending corrupt pieces"
                    )));
                }
            }
            Checked::Stored => {
                torrent.swarm.piece_verified(address);
                torrent.swarm.add_have(index as u32);
            }
            Checked::AlreadyHad => torrent.swarm.piece_verified(address),
        }
        Ok(())
    }

    fn send(&mut self, out: &mut Vec<Action>, message: PeerMessage, now: Instant) {
        out.push(Action::Send(message));
        self.state.last_sent = now;
    }

    fn peer_has(&self, index: usize) -> bool {
        self.bitfield
            .as_deref()
            .is_some_and(|bits| has_bit(bits, index))
    }

    fn find_request(&self, piece_index: u32, begin: u32, length: u32) -> Option<(usize, usize)> {
        self.downloads
            .iter()
            .enumerate()
            .find_map(|(index, download)| {
                download
                    .outstanding
                    .iter()
                    .position(|request| {
                        (request.piece_index, request.begin, request.length)
                            == (piece_index, begin, length)
                    })
                    .map(|position| (index, position))
            })
    }

    fn record_have(&mut self, piece_index: u32) -> Result<(), crate::BackendError> {
        let count = self.torrent.piece_count();
        let piece_index = usize::try_from(piece_index)
            .ok()
            .filter(|index| *index < count)
            .ok_or_else(|| invalid_peer("peer advertised a piece outside the torrent"))?;
        let bits = self
            .bitfield
            .get_or_insert_with(|| vec![0; count.div_ceil(8)]);
        if !has_bit(bits, piece_index) {
            bits[piece_index / 8] |= 1 << (7 - piece_index % 8);
            self.torrent.pieces()?.add_peer_piece(piece_index);
            self.have += 1;
            self.report_pieces();
        }
        Ok(())
    }

    /// Tells the swarm, for display, how much the peer has.
    fn report_pieces(&self) {
        self.torrent
            .swarm
            .set_pieces(self.link.key, self.have, self.torrent.piece_count());
    }

    /// Tells the swarm, for display, whether we download from the peer.
    fn report_download_state(&self) {
        self.torrent.swarm.set_download_state(
            self.link.key,
            self.state.peer_choking,
            self.state.am_interested,
        );
    }
}

impl Drop for PeerWire {
    /// The peer's pieces no longer count as available for rarest-first
    /// selection, and its pieces in flight are released.
    fn drop(&mut self) {
        if let (Some(bits), Ok(mut pieces)) = (&self.bitfield, self.torrent.pieces()) {
            pieces.remove_peer_pieces(bits);
        }
        self.downloads.clear();
        self.torrent.swarm.leave(self.link.key);
        self.torrent.connection_closed();
    }
}

/// Peer exchange with one connection.
#[derive(Default)]
struct Pex {
    /// The contacts this peer has been told about.
    told: std::collections::HashSet<SocketAddr>,
    /// When the next message is due, once the peer supports PEX.
    next: Option<Instant>,
    sent_any: bool,
    last_received: Option<Instant>,
}

impl Pex {
    /// The message bringing the peer up to date with `contacts`: the whole
    /// list the first time, then at most 50 additions and 50 drops.
    fn changes(&mut self, contacts: &[(SocketAddr, u8)]) -> crate::pex::PexMessage {
        let limit = if self.sent_any {
            crate::pex::MAX_PER_MESSAGE
        } else {
            usize::MAX
        };
        let current: std::collections::HashSet<SocketAddr> =
            contacts.iter().map(|(address, _)| *address).collect();
        let added: Vec<(SocketAddr, u8)> = contacts
            .iter()
            .filter(|(address, _)| !self.told.contains(address))
            .take(limit)
            .copied()
            .collect();
        let dropped: Vec<SocketAddr> = self
            .told
            .iter()
            .filter(|address| !current.contains(address))
            .take(limit)
            .copied()
            .collect();
        for (address, _) in &added {
            self.told.insert(*address);
        }
        for address in &dropped {
            self.told.remove(address);
        }
        crate::pex::PexMessage { added, dropped }
    }
}

/// The protocol state of one connection, from our side.
struct State {
    am_interested: bool,
    am_choking: bool,
    peer_interested: bool,
    peer_choking: bool,
    /// Our interest needs rechecking: the peer announced pieces, or we
    /// verified some.
    interest_changed: bool,
    /// The peer's block requests waiting to be read.
    uploads: VecDeque<(u32, u32, u32)>,
    last_sent: Instant,
    last_received: Instant,
    /// When the connection stopped being useful to either side.
    idle_since: Option<Instant>,
    snubbed_until: Option<Instant>,
    /// When a requested block last arrived, or requests last went out to
    /// an idle pipeline.
    last_block: Instant,
    /// Payload bytes received since `rate_at`, and the smoothed rate.
    window_bytes: u64,
    rate_at: Instant,
    rate: f64,
}

impl State {
    fn new(now: Instant) -> Self {
        Self {
            am_interested: false,
            am_choking: true,
            peer_interested: false,
            peer_choking: true,
            interest_changed: false,
            uploads: VecDeque::new(),
            last_sent: now,
            last_received: now,
            idle_since: None,
            snubbed_until: None,
            last_block: now,
            window_bytes: 0,
            rate_at: now,
            rate: 0.0,
        }
    }

    /// Folds the last second's payload into the smoothed download rate.
    fn update_rate(&mut self, now: Instant) {
        let elapsed = now - self.rate_at;
        if elapsed >= Duration::from_secs(1) {
            let sample = self.window_bytes as f64 / elapsed.as_secs_f64();
            self.rate = 0.6 * self.rate + 0.4 * sample;
            self.window_bytes = 0;
            self.rate_at = now;
        }
    }

    /// Blocks to keep requested: enough for [`REQUEST_QUEUE_TIME`] at the
    /// current rate, within the peer's advertised queue (`reqq`).
    fn request_depth(&self, reqq: Option<usize>) -> usize {
        let wanted = (self.rate * REQUEST_QUEUE_TIME.as_secs_f64() / 16_384.0) as usize;
        let ceiling = reqq
            .unwrap_or(MAX_REQUEST_DEPTH)
            .clamp(MIN_REQUEST_DEPTH, MAX_REQUEST_DEPTH);
        wanted.clamp(MIN_REQUEST_DEPTH, ceiling)
    }
}

/// A `reject` for a queued request.
fn reject((piece_index, begin, length): (u32, u32, u32)) -> PeerMessage {
    PeerMessage::RejectRequest {
        piece_index,
        begin,
        length,
    }
}

/// A bitfield with every piece set, for `have_all`.
fn full_bitfield(piece_count: usize) -> Vec<u8> {
    let mut bits = vec![0; piece_count.div_ceil(8)];
    for index in 0..piece_count {
        bits[index / 8] |= 1 << (7 - index % 8);
    }
    bits
}

fn validate_bitfield(bitfield: &[u8], piece_count: usize) -> Result<(), crate::BackendError> {
    let expected_length = piece_count.div_ceil(8);
    if bitfield.len() != expected_length {
        return Err(invalid_peer(
            "peer bitfield length does not match torrent piece count",
        ));
    }
    let unused_bits = expected_length * 8 - piece_count;
    if unused_bits > 0
        && bitfield
            .last()
            .is_some_and(|byte| byte & ((1 << unused_bits) - 1) != 0)
    {
        return Err(invalid_peer(
            "peer bitfield sets bits beyond the torrent piece count",
        ));
    }
    Ok(())
}

/// Block requests kept in flight per peer: enough to cover this long at the
/// peer's current rate, as libtorrent does. One request per round trip
/// would cap a peer at 16 KiB per round trip.
const REQUEST_QUEUE_TIME: Duration = Duration::from_secs(2);
/// At least this many (256 KiB), so new and slow peers can speed up.
const MIN_REQUEST_DEPTH: usize = 16;
/// At most this many, or the peer's own `reqq` if lower.
const MAX_REQUEST_DEPTH: usize = 250;
/// Pieces in flight from one peer at most.
const MAX_PIECES_PER_PEER: usize = 16;
/// Block reads for one peer handed to the driver at once.
const MAX_READS_IN_FLIGHT: usize = 8;
/// Requests queued per peer, matching the `reqq` that clients assume.
const MAX_QUEUED_UPLOADS: usize = 250;
/// BEP 11: at most one peer exchange message a minute.
const PEX_INTERVAL: Duration = Duration::from_secs(60);
/// How soon to check again while there is nobody to tell a peer about.
const PEX_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(1)
};
/// Suggested pieces remembered per peer.
const MAX_SUGGESTIONS: usize = 32;
/// Allowed fast pieces accepted per peer; BEP 6 suggests sets of 10.
const MAX_ALLOWED_FAST: usize = 32;
/// How long a snubbed peer gets no requests.
const SNUB_COOLDOWN: Duration = Duration::from_secs(30);
/// A connection neither side is interested in closes after this, freeing
/// its slot for a peer that can help. Peers learn of new pieces through
/// later connections.
const IDLE_LIMIT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(15)
};
/// Peers send keep-alives about every two minutes.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(90);
const SILENCE_LIMIT: Duration = Duration::from_secs(180);
