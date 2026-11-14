//! Choosing which peers we upload to (unchoking), for every torrent of a
//! runtime at once, as libtorrent's session does (`choker.cpp`,
//! `session_impl::recalculate_unchoke_slots` and
//! `recalculate_optimistic_unchoke_slots`).
//!
//! Every 10 seconds the interested peers that sent us the most get the
//! regular slots; among equals, those of the torrent its swarm most needs
//! (seed rank), then as [`crate::SeedChoking`] says. A torrent takes at
//! most its own number of slots. A fifth of the global slots rotate
//! optimistically every 30 seconds. The swarms keep each peer's state;
//! this module only decides.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::swarm::{PeerKey, Swarm};

pub(super) const RECHOKE_INTERVAL: Duration = Duration::from_secs(10);
pub(super) const OPTIMISTIC_INTERVAL: Duration = Duration::from_secs(30);
/// Rate-based slots: a slot for each peer uploaded to faster than this,
/// rising by the step per slot (libtorrent's
/// `rate_choker_initial_threshold`).
const RATE_THRESHOLD: u64 = 1024;
const RATE_STEP: u64 = 2048;

/// What the choker needs to know of a torrent.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Facts {
    /// Its seed rank: how much its swarm needs us.
    pub(crate) rank: u32,
    pub(crate) piece_length: u64,
    pub(crate) total_size: u64,
}

impl Facts {
    pub(crate) fn of(torrent: &crate::engine::active::ActiveTorrent) -> Self {
        Self {
            rank: torrent.seed_rank(),
            piece_length: u64::from(torrent.info.piece_length()),
            total_size: torrent.info.total_length(),
        }
    }
}

/// A peer in the choker's eyes: its torrent (any number unique among the
/// torrents choked together) and its key in that torrent's swarm.
pub(super) type Slot = (usize, PeerKey);

/// An interested peer the choker may unchoke.
#[derive(Debug, Clone, Copy)]
pub(super) struct Candidate {
    pub(super) slot: Slot,
    /// Bytes it sent us in the last interval.
    pub(super) rate: u64,
    /// Its torrent's seed rank: how much the torrent's swarm needs us.
    pub(super) rank: u32,
    /// Bytes we sent it in the last interval.
    pub(super) uploaded_round: u64,
    pub(super) unchoked: bool,
    /// Round-robin: it has had its turn.
    pub(super) quota_done: bool,
    /// Anti-leech: how far from half done it is.
    pub(super) leech_score: u32,
    pub(super) last_unchoke: Instant,
    /// It connected recently, so it is likelier to get an optimistic slot.
    pub(super) new: bool,
}

/// The choking settings a rechoke follows.
#[derive(Debug, Clone, Copy)]
pub(super) struct Policy {
    /// Upload slots across all torrents; `None` for unlimited.
    pub(super) global: Option<usize>,
    pub(super) per_torrent: usize,
    pub(super) choking: crate::SeedChoking,
    /// The global slots follow the upload rate instead.
    pub(super) rate_based: bool,
}

/// Upload slots as the upload rate fills them, as libtorrent's
/// `rate_based_choker`: one for each peer uploaded to faster than a
/// threshold that rises with each, plus one.
fn rate_based_slots(candidates: &[Candidate], interval: Duration) -> usize {
    let millis = interval.as_millis().max(1) as u64;
    let mut rates: Vec<u64> = candidates
        .iter()
        .map(|candidate| candidate.uploaded_round.saturating_mul(1000) / millis)
        .collect();
    rates.sort_unstable_by(|a, b| b.cmp(a));
    let mut threshold = RATE_THRESHOLD;
    let mut slots = 0;
    for rate in rates {
        if rate < threshold {
            break;
        }
        slots += 1;
        threshold += RATE_STEP;
    }
    slots + 1
}

/// The order peers get regular slots in: those that sent us most, then
/// those of the torrent with the highest seed rank, then as `choking`
/// says, then the one unchoked longest ago (libtorrent's `unchoke_compare_*`).
fn unchoke_order(a: &Candidate, b: &Candidate, choking: crate::SeedChoking) -> std::cmp::Ordering {
    use crate::SeedChoking;
    let by_choking = match choking {
        SeedChoking::FastestUpload => b.uploaded_round.cmp(&a.uploaded_round),
        SeedChoking::RoundRobin => {
            let round = |candidate: &Candidate| {
                if candidate.unchoked {
                    candidate.uploaded_round
                } else {
                    0
                }
            };
            a.quota_done
                .cmp(&b.quota_done)
                .then(round(b).cmp(&round(a)))
        }
        SeedChoking::AntiLeech => b.leech_score.cmp(&a.leech_score),
    };
    b.rate
        .cmp(&a.rate)
        .then(b.rank.cmp(&a.rank))
        .then(by_choking)
        .then(a.last_unchoke.cmp(&b.last_unchoke))
        .then(a.slot.0.cmp(&b.slot.0))
        .then(a.slot.1.0.cmp(&b.slot.1.0))
}

/// The peers the choker unchokes.
#[derive(Debug, Default)]
pub(super) struct Chosen {
    pub(super) regular: HashSet<Slot>,
    pub(super) optimistic: Vec<Slot>,
}

impl Chosen {
    pub(super) fn contains(&self, slot: &Slot) -> bool {
        self.regular.contains(slot) || self.optimistic.contains(slot)
    }
}

/// Picks the peers to upload to, as libtorrent's session choker does
/// (`recalculate_unchoke_slots`, `recalculate_optimistic_unchoke_slots`).
///
/// Regular slots go in [`unchoke_order`]. A torrent takes at most
/// `per_torrent - 1` of them, keeping one for an optimistic unchoke. With a
/// `global` limit (or rate-based slots, counted over the last `interval`),
/// a fifth of it (at least one) is optimistic, the rest regular; without,
/// each torrent has one optimistic slot. Optimistic slots stay with their
/// peers until `rotate`, then go to new random picks, newcomers three times
/// as likely.
pub(super) fn choose(
    mut candidates: Vec<Candidate>,
    policy: &Policy,
    interval: Duration,
    previous: &[Slot],
    rotate: bool,
    random: &mut u64,
) -> Chosen {
    let per_torrent = policy.per_torrent.max(1);
    let global = if policy.rate_based {
        Some(rate_based_slots(&candidates, interval))
    } else {
        policy.global
    };
    let optimistic_total = match global {
        None => usize::MAX,
        Some(global) if global > 1 => (global / 5).max(1),
        Some(_) => 0,
    };
    // One slot of each torrent's is for an optimistic unchoke, unless it
    // has only one.
    let (regular_per_torrent, optimistic_per_torrent) = if per_torrent > 1 {
        (per_torrent - 1, 1)
    } else {
        (1, 0)
    };
    let mut regular_left = global.map_or(usize::MAX, |global| global - optimistic_total);
    candidates.sort_by(|a, b| unchoke_order(a, b, policy.choking));
    let mut chosen = Chosen::default();
    let mut regular_count: HashMap<usize, usize> = HashMap::new();
    for candidate in &candidates {
        if regular_left == 0 {
            break;
        }
        let count = regular_count.entry(candidate.slot.0).or_default();
        if *count < regular_per_torrent {
            *count += 1;
            regular_left -= 1;
            chosen.regular.insert(candidate.slot);
        }
    }
    let mut optimistic_count: HashMap<usize, usize> = HashMap::new();
    let eligible = |slot: &Slot, optimistic_count: &HashMap<usize, usize>| {
        !chosen.regular.contains(slot)
            && optimistic_count.get(&slot.0).copied().unwrap_or(0) < optimistic_per_torrent
    };
    if !rotate {
        for slot in previous {
            if chosen.optimistic.len() < optimistic_total
                && candidates.iter().any(|candidate| candidate.slot == *slot)
                && eligible(slot, &optimistic_count)
            {
                *optimistic_count.entry(slot.0).or_default() += 1;
                chosen.optimistic.push(*slot);
            }
        }
    }
    while chosen.optimistic.len() < optimistic_total {
        let weighted: Vec<(Slot, u64)> = candidates
            .iter()
            .filter(|candidate| eligible(&candidate.slot, &optimistic_count))
            .map(|candidate| (candidate.slot, if candidate.new { 3 } else { 1 }))
            .collect();
        let total: u64 = weighted.iter().map(|(_, weight)| weight).sum();
        if total == 0 {
            break;
        }
        let mut pick = next_random(random) % total;
        let Some(slot) = weighted.into_iter().find_map(|(slot, weight)| {
            if pick < weight {
                Some(slot)
            } else {
                pick -= weight;
                None
            }
        }) else {
            break;
        };
        *optimistic_count.entry(slot.0).or_default() += 1;
        chosen.optimistic.push(slot);
    }
    chosen
}

fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// The event loop's choking state across rechokes.
#[derive(Debug)]
pub(crate) struct Choker {
    last_rechoke: Option<Instant>,
    last_optimistic: Option<Instant>,
    optimistic: Vec<Slot>,
    random: u64,
}

impl Default for Choker {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| elapsed.as_nanos() as u64)
            | 1;
        Self {
            last_rechoke: None,
            last_optimistic: None,
            optimistic: Vec::new(),
            random: seed,
        }
    }
}

impl Choker {
    /// Whether a rechoke is due.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.last_rechoke
            .is_none_or(|last| now.saturating_duration_since(last) >= RECHOKE_INTERVAL)
    }

    /// Decides who every torrent uploads to, for `torrents` (each swarm
    /// with its torrent's seed rank), under the shared `slots`. Every swarm
    /// is handed to the event loop's choking ([`Swarm::manage`]) first.
    pub(crate) fn rechoke_all(
        &mut self,
        torrents: &[(&Swarm, Facts)],
        slots: &std::sync::Arc<crate::engine::limits::UploadSlots>,
        now: Instant,
    ) {
        let interval = self
            .last_rechoke
            .map_or(RECHOKE_INTERVAL, |last| now.saturating_duration_since(last));
        self.last_rechoke = Some(now);
        let mut candidates = Vec::new();
        for (swarm, facts) in torrents {
            swarm.manage(slots);
            candidates.extend(swarm.choking_candidates(Self::id(swarm), *facts, now));
        }
        let rotate = self
            .last_optimistic
            .is_none_or(|last| now.saturating_duration_since(last) >= OPTIMISTIC_INTERVAL);
        if rotate {
            self.last_optimistic = Some(now);
        }
        let policy = Policy {
            global: slots.global(),
            per_torrent: slots.per_torrent(),
            choking: slots.choking(),
            rate_based: slots.mode() == crate::UploadSlotsMode::RateBased,
        };
        let chosen = choose(
            candidates,
            &policy,
            interval,
            &self.optimistic,
            rotate,
            &mut self.random,
        );
        self.optimistic.clone_from(&chosen.optimistic);
        // A recount, so a count that drifted (a swarm dropped with peers
        // unchoked) is put right.
        let mut unchoked = 0;
        for (swarm, _) in torrents {
            unchoked += swarm.apply_choice(Self::id(swarm), &chosen, now);
        }
        slots.unchoked.store(unchoked, Ordering::Relaxed);
    }

    /// A torrent's id across rechokes: its swarm's address, which is
    /// stable while the swarm lives.
    fn id(swarm: &Swarm) -> usize {
        std::ptr::from_ref(swarm) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(torrent: usize, key: u64, rate: u64, rank: u32) -> Candidate {
        Candidate {
            slot: (torrent, PeerKey(key)),
            rate,
            rank,
            uploaded_round: 0,
            unchoked: false,
            quota_done: false,
            leech_score: 0,
            last_unchoke: Instant::now(),
            new: false,
        }
    }

    fn fixed(global: usize, per_torrent: usize) -> Policy {
        Policy {
            global: Some(global),
            per_torrent,
            choking: crate::SeedChoking::FastestUpload,
            rate_based: false,
        }
    }

    /// libtorrent's session choker: 20 global slots, 16 regular and a
    /// fifth (4) optimistic, at most 4 per torrent.
    #[test]
    fn shares_global_upload_slots_among_torrents() {
        let candidates: Vec<Candidate> = (0..10)
            .flat_map(|torrent| (0..10).map(move |key| candidate(torrent, key, 0, 1000)))
            .collect();
        let mut random = 7;
        let chosen = choose(
            candidates,
            &fixed(20, 4),
            RECHOKE_INTERVAL,
            &[],
            true,
            &mut random,
        );
        assert_eq!(chosen.regular.len(), 16);
        assert_eq!(chosen.optimistic.len(), 4);
        for torrent in 0..10 {
            let regular = chosen
                .regular
                .iter()
                .filter(|slot| slot.0 == torrent)
                .count();
            let optimistic = chosen
                .optimistic
                .iter()
                .filter(|slot| slot.0 == torrent)
                .count();
            assert!(regular <= 3 && optimistic <= 1 && regular + optimistic <= 4);
        }
    }

    /// Peers that send us data come first; among equals, those of the
    /// torrent its swarm needs most (highest seed rank).
    #[test]
    fn ranks_peers_by_what_they_send_then_by_seed_rank() {
        let candidates = vec![
            candidate(0, 1, 0, 50),
            candidate(1, 2, 0, 9000),
            candidate(2, 3, 5000, 0),
            candidate(3, 4, 0, 400),
        ];
        let mut random = 7;
        // Two regular slots, no optimistic one.
        let chosen = choose(
            candidates,
            &fixed(1, 2),
            RECHOKE_INTERVAL,
            &[],
            true,
            &mut random,
        );
        assert_eq!(
            chosen.regular,
            HashSet::from([(2, PeerKey(3))]),
            "the sender"
        );
        let candidates = vec![
            candidate(0, 1, 0, 50),
            candidate(1, 2, 0, 9000),
            candidate(3, 4, 0, 400),
        ];
        let chosen = choose(
            candidates,
            &fixed(3, 1),
            RECHOKE_INTERVAL,
            &[],
            true,
            &mut random,
        );
        assert_eq!(
            chosen.regular,
            HashSet::from([(1, PeerKey(2)), (3, PeerKey(4))]),
            "the rarest-seeded torrents"
        );
    }

    /// Which seeding peer gets the one regular slot, by each algorithm.
    #[test]
    fn picks_seeding_peers_by_the_chosen_algorithm() {
        let now = Instant::now();
        let earlier = now - Duration::from_secs(120);
        // 1: uploaded to fastest, unchoked long ago, has had its quota.
        // 2: slower, unchoked a while ago, still within its turn.
        // 3: choked, never had a turn since connecting earlier still.
        let mut fast = candidate(0, 1, 0, 0);
        (fast.uploaded_round, fast.unchoked, fast.quota_done) = (900_000, true, true);
        fast.last_unchoke = earlier;
        fast.leech_score = 0; // half done
        let mut slow = candidate(0, 2, 0, 0);
        (slow.uploaded_round, slow.unchoked) = (10_000, true);
        slow.last_unchoke = now - Duration::from_secs(30);
        slow.leech_score = 900; // nearly done
        let mut waiting = candidate(0, 3, 0, 0);
        waiting.last_unchoke = earlier - Duration::from_secs(60);
        waiting.leech_score = 500;
        let pick = |choking| {
            let policy = Policy {
                global: Some(1),
                per_torrent: 2,
                choking,
                rate_based: false,
            };
            let mut random = 7;
            let chosen = choose(
                vec![fast, slow, waiting],
                &policy,
                RECHOKE_INTERVAL,
                &[],
                true,
                &mut random,
            );
            chosen.regular.into_iter().next().unwrap().1
        };
        assert_eq!(pick(crate::SeedChoking::FastestUpload), PeerKey(1));
        // The fast one had its turn; of the rest, the unchoked one keeps
        // going until its quota.
        assert_eq!(pick(crate::SeedChoking::RoundRobin), PeerKey(2));
        assert_eq!(pick(crate::SeedChoking::AntiLeech), PeerKey(2));
    }

    /// libtorrent's rate-based choker: a slot per peer above 1 KiB/s,
    /// then 3, 5... KiB/s, plus one.
    #[test]
    fn rate_based_slots_follow_the_upload() {
        let at = |bytes_per_second: u64| {
            let mut peer = candidate(0, bytes_per_second, 0, 0);
            peer.uploaded_round = bytes_per_second * 10;
            peer
        };
        let ten = Duration::from_secs(10);
        assert_eq!(rate_based_slots(&[], ten), 1);
        assert_eq!(rate_based_slots(&[at(500)], ten), 1);
        assert_eq!(rate_based_slots(&[at(2000), at(500)], ten), 2);
        assert_eq!(rate_based_slots(&[at(9000), at(4000), at(3000)], ten), 3);
        let policy = Policy {
            global: Some(100),
            per_torrent: 4,
            choking: crate::SeedChoking::FastestUpload,
            rate_based: true,
        };
        let candidates: Vec<Candidate> = (0..4)
            .map(|torrent| {
                let mut peer = at(1_000_000);
                peer.slot = (torrent, PeerKey(1));
                peer
            })
            .chain((0..4).map(|torrent| candidate(torrent, 2, 0, 0)))
            .collect();
        let mut random = 7;
        let chosen = choose(candidates, &policy, ten, &[], true, &mut random);
        // 4 fast peers: thresholds 1, 3, 5, 7 KiB/s passed, so 5 slots, not
        // the fixed 100: 4 regular and 1 optimistic.
        assert_eq!(chosen.regular.len() + chosen.optimistic.len(), 5);
    }

    #[test]
    fn keeps_optimistic_slots_until_they_rotate() {
        // Three peers in each of six torrents: one regular slot each, and
        // two optimistic ones between them.
        let candidates: Vec<Candidate> = (0..6)
            .flat_map(|torrent| (0..3).map(move |key| candidate(torrent, key, 0, 1000)))
            .collect();
        let mut random = 7;
        let first = choose(
            candidates.clone(),
            &fixed(10, 2),
            RECHOKE_INTERVAL,
            &[],
            true,
            &mut random,
        );
        assert_eq!(first.optimistic.len(), 2);
        let kept = choose(
            candidates.clone(),
            &fixed(10, 2),
            RECHOKE_INTERVAL,
            &first.optimistic,
            false,
            &mut random,
        );
        assert_eq!(kept.optimistic, first.optimistic);
        // A peer that left gives its slot to another.
        let fewer: Vec<Candidate> = candidates
            .iter()
            .filter(|candidate| candidate.slot != first.optimistic[0])
            .copied()
            .collect();
        let refilled = choose(
            fewer,
            &fixed(10, 2),
            RECHOKE_INTERVAL,
            &first.optimistic,
            false,
            &mut random,
        );
        assert_eq!(refilled.optimistic.len(), first.optimistic.len());
        assert!(!refilled.optimistic.contains(&first.optimistic[0]));
    }
}
