const BLOCK_SIZE: u32 = 16 * 1024;

/// A single peer request for part of a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRequest {
    pub piece_index: u32,
    pub begin: u32,
    pub length: u32,
}

/// Schedules piece blocks and assembles responses from peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockManager {
    piece_index: Option<u32>,
    piece_length: u32,
    blocks: Vec<Option<Vec<u8>>>,
}

impl BlockManager {
    /// Starts assembling one piece.
    pub fn start(piece_index: u32, piece_length: u32) -> Result<Self, crate::BackendError> {
        if piece_length == 0 {
            return Err(invalid("piece length must be greater than zero"));
        }

        let block_count = piece_length.div_ceil(BLOCK_SIZE) as usize;
        Ok(Self {
            piece_index: Some(piece_index),
            piece_length,
            blocks: vec![None; block_count],
        })
    }

    /// Returns a request for every block not yet received, in piece order,
    /// so callers can keep several requests in flight to one peer.
    pub fn missing_requests(&self) -> Vec<BlockRequest> {
        let Some(piece_index) = self.piece_index else {
            return Vec::new();
        };
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.is_none())
            .map(|(index, _)| {
                let begin = index as u32 * BLOCK_SIZE;
                BlockRequest {
                    piece_index,
                    begin,
                    length: (self.piece_length - begin).min(BLOCK_SIZE),
                }
            })
            .collect()
    }

    /// Stores a peer response and returns the complete piece when assembled.
    pub fn receive(
        &mut self,
        piece_index: u32,
        begin: u32,
        data: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, crate::BackendError> {
        if self.piece_index != Some(piece_index) {
            return Err(invalid("piece response does not match active piece"));
        }
        if data.is_empty() || begin >= self.piece_length {
            return Err(invalid("piece block is outside the active piece"));
        }
        let block_index = (begin / BLOCK_SIZE) as usize;
        let expected_begin = block_index as u32 * BLOCK_SIZE;
        let expected_length = (self.piece_length - expected_begin).min(BLOCK_SIZE);
        if begin != expected_begin || data.len() as u32 != expected_length {
            return Err(invalid("piece block has an invalid offset or length"));
        }
        let slot = self
            .blocks
            .get_mut(block_index)
            .ok_or_else(|| invalid("piece block index is out of range"))?;
        if slot.is_some() {
            return Err(invalid("piece block was already received"));
        }
        *slot = Some(data);

        if self.blocks.iter().all(Option::is_some) {
            let mut piece = Vec::with_capacity(self.piece_length as usize);
            for block in &mut self.blocks {
                piece.extend(block.take().expect("all blocks are present"));
            }
            self.piece_index = None;
            return Ok(Some(piece));
        }
        Ok(None)
    }
}

fn invalid(message: impl Into<String>) -> crate::BackendError {
    crate::BackendError::InvalidPeerMessage(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_and_assembles_piece_blocks() {
        let mut manager = BlockManager::start(2, 16_385).unwrap();
        let requests = manager.missing_requests();
        assert_eq!(
            requests[0],
            BlockRequest {
                piece_index: 2,
                begin: 0,
                length: 16_384
            }
        );
        assert!(manager.receive(2, 0, vec![1; 16_384]).unwrap().is_none());

        let second = manager.missing_requests()[0];
        assert_eq!(second.begin, 16_384);
        assert_eq!(second.length, 1);
        let piece = manager.receive(2, second.begin, vec![2]).unwrap().unwrap();
        assert_eq!(piece.len(), 16_385);
        assert_eq!(piece[0], 1);
        assert_eq!(piece[16_384], 2);
        assert!(manager.missing_requests().is_empty());
    }

    #[test]
    fn rejects_duplicate_blocks() {
        let mut manager = BlockManager::start(0, 1).unwrap();
        manager.receive(0, 0, vec![1]).unwrap();

        assert!(manager.receive(0, 0, vec![1]).is_err());
    }
}
