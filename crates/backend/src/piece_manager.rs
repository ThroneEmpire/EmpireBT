/// Verification state for a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceState {
    Missing,
    Downloading,
    Complete,
}

/// Tracks piece availability and verification state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceManager {
    states: Vec<PieceState>,
    /// How many connected peers advertise each piece.
    availability: Vec<u32>,
}

impl PieceManager {
    pub fn new(piece_count: usize) -> Self {
        Self {
            states: vec![PieceState::Missing; piece_count],
            availability: vec![0; piece_count],
        }
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn state(&self, index: usize) -> Option<PieceState> {
        self.states.get(index).copied()
    }

    pub fn mark_downloading(&mut self, index: usize) -> bool {
        self.mark(index, PieceState::Downloading)
    }

    pub fn mark_complete(&mut self, index: usize) -> bool {
        self.mark(index, PieceState::Complete)
    }

    pub fn complete_count(&self) -> usize {
        self.states
            .iter()
            .filter(|state| **state == PieceState::Complete)
            .count()
    }

    pub fn is_complete(&self) -> bool {
        !self.states.is_empty()
            && self
                .states
                .iter()
                .all(|state| *state == PieceState::Complete)
    }

    /// Counts a connected peer's advertised pieces toward availability.
    pub fn add_peer_pieces(&mut self, peer_bitfield: &[u8]) {
        for index in advertised(peer_bitfield, self.states.len()) {
            self.availability[index] = self.availability[index].saturating_add(1);
        }
    }

    /// Removes a disconnected peer's pieces from availability.
    pub fn remove_peer_pieces(&mut self, peer_bitfield: &[u8]) {
        for index in advertised(peer_bitfield, self.states.len()) {
            self.availability[index] = self.availability[index].saturating_sub(1);
        }
    }

    /// Counts one newly announced piece from a connected peer.
    pub fn add_peer_piece(&mut self, index: usize) {
        if let Some(count) = self.availability.get_mut(index) {
            *count = count.saturating_add(1);
        }
    }

    /// Claims the missing piece that the fewest connected peers advertise,
    /// among those this peer has. Downloading rare pieces first keeps them
    /// in the swarm when peers leave. `tie_breaker` spreads peers with equal
    /// choices across different pieces.
    pub fn claim_rarest(&mut self, peer_bitfield: &[u8], tie_breaker: usize) -> Option<usize> {
        let count = self.states.len();
        let index = advertised(peer_bitfield, count)
            .filter(|index| self.states[*index] == PieceState::Missing)
            .min_by_key(|index| {
                (
                    self.availability[*index],
                    (index + count - tie_breaker % count.max(1)) % count.max(1),
                )
            })?;
        self.mark_downloading(index);
        Some(index)
    }

    /// In end-game, when nothing is left to claim, returns a piece another
    /// peer is still downloading that this peer can also supply, so one
    /// slow peer cannot stall the final pieces.
    pub fn end_game_candidate(&self, peer_bitfield: &[u8], tie_breaker: usize) -> Option<usize> {
        let in_flight: Vec<usize> = advertised(peer_bitfield, self.states.len())
            .filter(|index| self.states[*index] == PieceState::Downloading)
            .collect();
        (!in_flight.is_empty()).then(|| in_flight[tie_breaker % in_flight.len()])
    }

    /// Whether a peer advertises any piece we have not verified yet, which
    /// is when we are interested in it.
    pub fn wants_any(&self, peer_bitfield: &[u8]) -> bool {
        advertised(peer_bitfield, self.states.len())
            .any(|index| self.states[index] != PieceState::Complete)
    }

    /// Verified pieces as a bitfield (most significant bit first), as sent
    /// to peers.
    pub fn complete_bitfield(&self) -> Vec<u8> {
        let mut bits = vec![0; self.states.len().div_ceil(8)];
        for (index, state) in self.states.iter().enumerate() {
            if *state == PieceState::Complete {
                bits[index / 8] |= 1 << (7 - index % 8);
            }
        }
        bits
    }

    /// Claims one specific missing piece when a shared scheduler selected it.
    pub fn claim(&mut self, index: usize) -> bool {
        self.state(index) == Some(PieceState::Missing) && self.mark_downloading(index)
    }

    /// Releases one in-flight piece after a peer failure.
    pub fn reset(&mut self, index: usize) {
        if self.state(index) == Some(PieceState::Downloading) {
            self.states[index] = PieceState::Missing;
        }
    }

    fn mark(&mut self, index: usize, state: PieceState) -> bool {
        if let Some(piece) = self.states.get_mut(index) {
            *piece = state;
            true
        } else {
            false
        }
    }
}

/// Piece indexes set in a most-significant-bit-first bitfield.
fn advertised(bitfield: &[u8], piece_count: usize) -> impl Iterator<Item = usize> + '_ {
    (0..piece_count).filter(move |index| {
        bitfield
            .get(index / 8)
            .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_the_rarest_piece_first() {
        let mut manager = PieceManager::new(4);
        manager.add_peer_pieces(&[0b1111_0000]);
        manager.add_peer_pieces(&[0b1101_0000]);
        manager.add_peer_pieces(&[0b1000_0000]);
        // Availability: piece 0 → 3, 1 → 2, 2 → 1, 3 → 2.
        assert_eq!(manager.claim_rarest(&[0b1111_0000], 0), Some(2));
        assert_eq!(manager.claim_rarest(&[0b1111_0000], 0), Some(1));
        assert_eq!(manager.claim_rarest(&[0b1111_0000], 0), Some(3));
        assert_eq!(manager.claim_rarest(&[0b1111_0000], 0), Some(0));
        assert_eq!(manager.claim_rarest(&[0b1111_0000], 0), None);

        // Everything is in flight, so end-game offers those pieces again.
        assert!(manager.end_game_candidate(&[0b1111_0000], 1).is_some());
        manager.remove_peer_pieces(&[0b1111_0000]);
        assert_eq!(manager.availability, vec![2, 1, 0, 1]);
    }

    #[test]
    fn spreads_equal_choices_across_peers() {
        let mut manager = PieceManager::new(4);
        assert_eq!(manager.claim_rarest(&[0b1111_0000], 0), Some(0));
        assert_eq!(manager.claim_rarest(&[0b1111_0000], 2), Some(2));
    }
}
