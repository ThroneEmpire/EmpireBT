use sha2::{Digest, Sha256};

// BEP 52 Merkle trees:
// - Leaves are SHA-256 hashes of 16 KiB blocks. The final block is hashed as
//   is, without padding.
// - The leaf layer is padded to a power of two with all-zero leaves, so any
//   padding node is the root of an all-zero subtree of its height.
// - A file larger than one piece has a piece layer: one node per piece, each
//   the root of `piece_length / 16 KiB` leaves (the last piece's subtree is
//   padded with zero leaves).
// - A file of at most one piece has no piece layer; its root covers its own
//   leaves, padded to a power of two.

const BLOCK: usize = 16 * 1024;

/// Returns the BEP 52 Merkle root ("pieces root") of a file's contents.
/// Empty files have no root; they are given all zeros.
pub(crate) fn file_root(data: &[u8], piece_length: u32) -> [u8; 32] {
    if data.is_empty() {
        return [0; 32];
    }
    let leaves = leaf_hashes(data);
    let per_piece = blocks_per_piece(piece_length);
    if leaves.len() <= per_piece {
        return padded_root(&leaves, leaves.len().next_power_of_two(), [0; 32]);
    }
    let pieces = piece_roots(data, piece_length);
    let width = pieces.len().next_power_of_two();
    padded_root(&pieces, width, pad_hash(per_piece.trailing_zeros()))
}

/// Returns the piece layer for a file larger than one piece, or the file
/// root alone for a file of at most one piece (which has no piece layer).
pub(crate) fn piece_roots(data: &[u8], piece_length: u32) -> Vec<[u8; 32]> {
    if data.len() <= piece_length as usize {
        return vec![file_root(data, piece_length)];
    }
    data.chunks(piece_length as usize)
        .map(|piece| piece_hash(piece, piece_length))
        .collect()
}

/// One piece-layer node: the root of the piece's leaves, padded with zero
/// leaves to a full piece.
fn piece_hash(piece: &[u8], piece_length: u32) -> [u8; 32] {
    padded_root(&leaf_hashes(piece), blocks_per_piece(piece_length), [0; 32])
}

/// The hash a v2 piece is checked against: its piece-layer node, or for a
/// file of only one piece, the file root.
pub(crate) fn piece_hash_for(data: &[u8], piece_length: u32, single_piece_file: bool) -> [u8; 32] {
    if single_piece_file {
        file_root(data, piece_length)
    } else {
        piece_hash(data, piece_length)
    }
}

fn blocks_per_piece(piece_length: u32) -> usize {
    (piece_length as usize / BLOCK).max(1)
}

/// The root of an all-zero subtree `height` layers above the leaves.
fn pad_hash(height: u32) -> [u8; 32] {
    (0..height).fold([0; 32], |hash, _| combine(hash, hash))
}

/// Root of `nodes` padded with `pad` to `width` (a power of two). Padding
/// pairs combine into the next layer's padding automatically.
fn padded_root(nodes: &[[u8; 32]], width: usize, pad: [u8; 32]) -> [u8; 32] {
    let mut level = nodes.to_vec();
    level.resize(width.max(1), pad);
    while level.len() > 1 {
        level = level
            .chunks_exact(2)
            .map(|pair| combine(pair[0], pair[1]))
            .collect();
    }
    level[0]
}

/// Answers a BEP 52 hash request from a torrent's piece layers, or for the
/// leaf layer from `leaf_layer` (which may decline by returning `None`).
/// Unanswerable ranges get a reject; requests naming unknown files are
/// protocol errors.
pub(crate) fn answer_hash_request(
    info: &crate::TorrentInfo,
    pieces_root: [u8; 32],
    base_layer: u32,
    index: u32,
    length: u32,
    proof_layers: u32,
    leaf_layer: impl FnOnce(
        &crate::V2File,
    ) -> Result<Option<std::sync::Arc<Vec<[u8; 32]>>>, crate::BackendError>,
) -> Result<crate::PeerMessage, crate::BackendError> {
    use crate::PeerMessage;
    let invalid = |message: &str| crate::BackendError::InvalidPeerMessage(message.to_owned());
    let reject = PeerMessage::HashReject {
        pieces_root,
        base_layer,
        index,
        length,
        proof_layers,
    };
    let Some(file) = info
        .v2_files()
        .iter()
        .find(|file| file.pieces_root() == &pieces_root)
    else {
        return Err(invalid("hash request references an unknown v2 file"));
    };
    let piece_layer = info.piece_length().trailing_zeros().saturating_sub(14);
    let hashes = if base_layer == piece_layer {
        let Some(layer) = info
            .v2_piece_layers()
            .iter()
            .find(|layer| layer.root() == &pieces_root)
        else {
            return Ok(reject);
        };
        std::sync::Arc::new(layer.pieces().to_vec())
    } else if base_layer == 0 {
        let Some(leaves) = leaf_layer(file)? else {
            return Ok(reject);
        };
        if leaves.len() != file.length().div_ceil(16 * 1024) as usize {
            return Err(invalid("v2 leaf layer geometry is invalid"));
        }
        leaves
    } else {
        return Ok(reject);
    };
    let proof = if base_layer == piece_layer {
        piece_layer_response(&hashes, index, length, info.piece_length())
    } else {
        merkle_layer_response(&hashes, 0, index, length, [0; 32])
    };
    let Some(proof) = proof else {
        return Ok(reject);
    };
    if (proof_layers as usize) < proof.len() {
        return Ok(reject);
    }
    // Ranges may extend past the last piece; those entries are padding.
    let pad = if base_layer == piece_layer {
        pad_hash(blocks_per_piece(info.piece_length()).trailing_zeros())
    } else {
        [0; 32]
    };
    let mut requested: Vec<[u8; 32]> = hashes
        .iter()
        .skip(index as usize)
        .take(length as usize)
        .copied()
        .collect();
    requested.resize(length as usize, pad);
    Ok(PeerMessage::Hashes {
        pieces_root,
        base_layer,
        index,
        length,
        proof_layers: proof,
        hashes: requested,
    })
}

pub fn verify_hashes_response(
    pieces_root: [u8; 32],
    _base_layer: u32,
    index: u32,
    hashes: &[[u8; 32]],
    proof_layers: &[[u8; 32]],
) -> bool {
    if hashes.len() < 2 || !hashes.len().is_power_of_two() || index % hashes.len() as u32 != 0 {
        return false;
    }

    let mut level = hashes.to_vec();
    let mut node_index = index / hashes.len() as u32;
    while level.len() > 1 {
        level = level
            .chunks_exact(2)
            .map(|pair| combine(pair[0], pair[1]))
            .collect();
    }

    for proof in proof_layers {
        level[0] = if node_index % 2 == 0 {
            combine(level[0], *proof)
        } else {
            combine(*proof, level[0])
        };
        node_index /= 2;
    }

    level[0] == pieces_root
}

/// Proof for a range of a file's piece layer. Padding beyond the last
/// piece is the root of an all-zero piece, not zero bytes.
pub(crate) fn piece_layer_response(
    pieces: &[[u8; 32]],
    index: u32,
    length: u32,
    piece_length: u32,
) -> Option<Vec<[u8; 32]>> {
    let pad = pad_hash(blocks_per_piece(piece_length).trailing_zeros());
    merkle_layer_response(pieces, 0, index, length, pad)
}

/// Proof for a range of `leaves`, a tree layer whose padding nodes are
/// `pad`.
pub(crate) fn merkle_layer_response(
    leaves: &[[u8; 32]],
    base_layer: u32,
    index: u32,
    length: u32,
    pad: [u8; 32],
) -> Option<Vec<[u8; 32]>> {
    // Ranges are power-of-two aligned, so one may extend past the last
    // real node into padding; it must stay within the padded layer.
    if leaves.is_empty()
        || !(2..=512).contains(&length)
        || !length.is_power_of_two()
        || index % length != 0
        || index as usize >= leaves.len()
        || index as usize + length as usize > leaves.len().next_power_of_two().max(2)
    {
        return None;
    }

    let tree_size = leaves.len().next_power_of_two();
    let mut level = leaves.to_vec();
    level.resize(tree_size, pad);
    let mut current_layer = 0;
    while current_layer < base_layer {
        if level.len() == 1 {
            return None;
        }
        level = level
            .chunks_exact(2)
            .map(|pair| combine(pair[0], pair[1]))
            .collect();
        current_layer += 1;
    }
    if index as usize + length as usize > level.len() {
        return None;
    }
    // Climb to the layer where a single node covers the requested range,
    // then collect that node's sibling at every layer up to the root.
    for _ in 0..length.trailing_zeros() {
        level = level
            .chunks_exact(2)
            .map(|pair| combine(pair[0], pair[1]))
            .collect();
    }
    let mut node = index as usize / length as usize;
    let mut proof = Vec::new();
    while level.len() > 1 {
        proof.push(level[node ^ 1]);
        level = level
            .chunks_exact(2)
            .map(|pair| combine(pair[0], pair[1]))
            .collect();
        node /= 2;
    }
    Some(proof)
}

/// SHA-256 of each 16 KiB block; the final block is not padded.
pub(crate) fn leaf_hashes(data: &[u8]) -> Vec<[u8; 32]> {
    data.chunks(BLOCK)
        .map(|block| Sha256::digest(block).into())
        .collect()
}

fn combine(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same deterministic contents libtorrent hashed to produce the
    /// reference values below.
    fn sample(length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| ((index * 7 + 3) % 251) as u8)
            .collect()
    }

    fn hex(text: &str) -> [u8; 32] {
        std::array::from_fn(|index| {
            u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap()
        })
    }

    const PIECE: u32 = 64 * 1024;

    // Reference values from libtorrent 2.1, the BEP 52 reference
    // implementation, for v2 torrents with 64 KiB pieces.
    const MULTI_ROOT: &str = "43e526334f9d5dd6703f9933b0fa2115b985cdd7d3c5296761328cd65d6de86e";
    const MULTI_LAYER: [&str; 4] = [
        "88866eeb00fd9d7f8d4b79a31bf452581a9788c0190bf03a1e122a4a19111f80",
        "412aa8a525033889165edf8be55a68bb4da12c6c9630a1e3dd6b0699e5cbb7f6",
        "3a0f71bc627f836e3a989d22b7dbc54367875102bc1fe697a8326756643c87d1",
        "dcdc5df33d09cc5fd921ea38731ce734014428d39f9dfa65fdcfa1e740ee8684",
    ];

    #[test]
    fn matches_libtorrent_for_a_multi_piece_file() {
        // Four pieces, the last partial and ending in a short block.
        let data = sample(200_000);
        let layer: Vec<[u8; 32]> = MULTI_LAYER.iter().map(|text| hex(text)).collect();
        assert_eq!(piece_roots(&data, PIECE), layer);
        assert_eq!(file_root(&data, PIECE), hex(MULTI_ROOT));
    }

    #[test]
    fn matches_libtorrent_for_files_within_one_piece() {
        assert_eq!(
            file_root(&sample(20_000), PIECE),
            hex("670134999f01a79ce913e3dbc6c38165dd6754d1abae39a87588ecda9e486139")
        );
        assert_eq!(
            file_root(&sample(100), PIECE),
            hex("8ccfe0e9682941c3451db3606c96dfdde905896fd071c0bb2d58d01b8c6daf7d")
        );
    }

    #[test]
    fn verifies_pieces_against_the_reference_layer() {
        let data = sample(200_000);
        let layer: Vec<[u8; 32]> = MULTI_LAYER.iter().map(|text| hex(text)).collect();
        for (index, expected) in layer.iter().enumerate() {
            let start = index * PIECE as usize;
            let end = (start + PIECE as usize).min(data.len());
            assert_eq!(piece_hash_for(&data[start..end], PIECE, false), *expected);
        }
        let last = &data[3 * PIECE as usize..];
        assert_ne!(piece_hash_for(&vec![0; last.len()], PIECE, false), layer[3]);

        // A single-piece file is checked against its root.
        let small = sample(20_000);
        assert_eq!(
            piece_hash_for(&small, PIECE, true),
            file_root(&small, PIECE)
        );
    }

    #[test]
    fn piece_layer_proofs_reach_the_reference_root() {
        // Three of four slots used: the proof for the first pair must
        // include the padding piece's subtree, not zero bytes.
        let data = sample(3 * PIECE as usize - 1_000);
        let pieces = piece_roots(&data, PIECE);
        assert_eq!(pieces.len(), 3);
        let root = file_root(&data, PIECE);
        // The second range covers the last piece and one padding slot.
        for index in [0, 2] {
            let end = (index as usize + 2).min(pieces.len());
            let mut hashes = pieces[index as usize..end].to_vec();
            hashes.resize(2, pad_hash(blocks_per_piece(PIECE).trailing_zeros()));
            let proof = piece_layer_response(&pieces, index, 2, PIECE).unwrap();
            assert!(verify_hashes_response(root, 0, index, &hashes, &proof));
        }
        // A range starting past the last piece is not a real request.
        assert!(piece_layer_response(&pieces, 4, 2, PIECE).is_none());
    }

    #[test]
    fn leaf_layer_proofs_reach_the_root() {
        let data = sample(20_000 + 3 * BLOCK);
        let leaves = leaf_hashes(&data);
        let root = file_root(&data, PIECE);
        let proof = merkle_layer_response(&leaves, 0, 2, 2, [0; 32]).unwrap();
        assert!(verify_hashes_response(root, 0, 2, &leaves[2..4], &proof));
    }

    #[test]
    fn verifies_v2_pieces_read_back_from_storage() {
        let root = std::env::temp_dir().join(format!("empirebt-v2-resume-{}", std::process::id()));
        let storage = crate::Storage::new(&root).unwrap();
        let data = sample(200_000);
        let layer: Vec<[u8; 32]> = MULTI_LAYER.iter().map(|text| hex(text)).collect();
        let file = crate::V2File::from_parts("data", data.len() as u64, hex(MULTI_ROOT));
        storage.write("data", &data).unwrap();
        for (index, expected) in layer.iter().enumerate() {
            let piece = storage.read_v2_piece(&file, PIECE, index).unwrap();
            assert_eq!(piece_hash_for(&piece, PIECE, false), *expected);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_files_have_no_pieces() {
        assert_eq!(
            crate::V2File::from_parts("empty", 0, [0; 32]).piece_count(PIECE),
            0
        );
    }
}
