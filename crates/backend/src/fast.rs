//! BEP 6, the Fast Extension: the allowed fast set.
//!
//! A peer that has just joined has nothing to trade, so tit-for-tat would
//! never unchoke it. Each side therefore lets the other download a few
//! pieces even while choked. Which pieces is decided by hashing the peer's
//! address with the info hash, so every client picks the same set for the
//! same peer, and a peer cannot get more by reconnecting.

use sha1::{Digest, Sha1};
use std::net::IpAddr;

/// The set size BEP 6 recommends.
pub(crate) const ALLOWED_FAST_COUNT: usize = 10;

/// The first `count` pieces of the allowed fast set for a peer at `ip`.
/// BEP 6 covers IPv4 only; IPv6 peers get no set.
pub(crate) fn allowed_fast_set(
    ip: IpAddr,
    info_hash: &[u8; 20],
    piece_count: usize,
    count: usize,
) -> Vec<u32> {
    let IpAddr::V4(ip) = ip else {
        return Vec::new();
    };
    let count = count.min(piece_count);
    let mut set = Vec::with_capacity(count);
    // The last octet is masked, so a whole /24 shares one set.
    let mut x = (u32::from(ip) & 0xffff_ff00).to_be_bytes().to_vec();
    x.extend_from_slice(info_hash);
    while set.len() < count {
        x = Sha1::digest(&x).to_vec();
        for chunk in x.as_chunks::<4>().0 {
            if set.len() == count {
                break;
            }
            let y = u32::from_be_bytes(*chunk);
            let index = (u64::from(y) % piece_count as u64) as u32;
            if !set.contains(&index) {
                set.push(index);
            }
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_bep_6_test_vectors() {
        let ip = IpAddr::from([80, 4, 4, 200]);
        let info_hash = [0xaa; 20];
        assert_eq!(
            allowed_fast_set(ip, &info_hash, 1313, 7),
            [1059, 431, 808, 1217, 287, 376, 1188]
        );
        assert_eq!(
            allowed_fast_set(ip, &info_hash, 1313, 9),
            [1059, 431, 808, 1217, 287, 376, 1188, 353, 508]
        );
    }

    #[test]
    fn shares_a_set_across_a_subnet_and_fits_small_torrents() {
        let info_hash = [7; 20];
        assert_eq!(
            allowed_fast_set(IpAddr::from([10, 1, 2, 3]), &info_hash, 500, 10),
            allowed_fast_set(IpAddr::from([10, 1, 2, 250]), &info_hash, 500, 10)
        );
        let small = allowed_fast_set(IpAddr::from([10, 1, 2, 3]), &info_hash, 3, 10);
        assert_eq!(small.len(), 3);
        assert!(allowed_fast_set("::1".parse().unwrap(), &info_hash, 500, 10).is_empty());
    }
}
