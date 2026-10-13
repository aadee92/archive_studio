//! Viewport-driven job scheduling for the thumbnail pipeline (pure logic, no threads).
//!
//! The UI owns priority: on every change of the visible row range it hands
//! [`Sched::schedule`] the full list of wanted tiles, best first (visible top to bottom,
//! then the prefetch band ahead, then a smaller band behind; see [`window_rows`]).
//! That call, under the one pipeline mutex, replaces the not-yet-started lookups and
//! re-ranks or cancels work parked between stages, so a worker only ever takes the
//! front of a queue. Tiles that left the window are never started; a task already
//! inside a stage is dropped at its next hand-off if its key is no longer wanted.
//!
//! Stages: `lookup` (memory / disk cache, CPU workers) → `read` (source bytes,
//! I/O threads) → `decode` (CPU workers). `pending` holds every key somewhere in the
//! pipeline or delivered-but-not-yet-[`finish`](Sched::finish)ed, so nothing is queued twice.

use std::collections::{HashMap, HashSet, VecDeque};

use super::Tier;

pub type Key = (u32, Tier);

/// Display rows to fetch, best first: the visible rows top to bottom, then `ahead` rows in
/// the scroll direction (`down`), then `behind` rows the other way, nearest first.
/// `last` is inclusive; rows at or past `total` are skipped.
///
/// While `moving`, the visible rows are instead taken from the leading edge: when the disk
/// can't keep up with a fast scroll, the rows that just came into view stay there longest,
/// while those about to leave would be wasted work. Once the view rests, top to bottom.
pub fn window_rows(first: usize, last: usize, total: usize, down: bool, moving: bool, ahead: usize, behind: usize) -> Vec<usize> {
    let last = last.min(total.saturating_sub(1));
    if total == 0 || first > last {
        return Vec::new();
    }
    let below = (last + 1..total).take(if down { ahead } else { behind });
    let above = (0..first).rev().take(if down { behind } else { ahead });
    let mut rows: Vec<usize> = (first..=last).collect();
    if moving && down {
        rows.reverse();
    }
    if down {
        rows.extend(below.chain(above));
    } else {
        rows.extend(above.chain(below));
    }
    rows
}

pub struct Sched<T> {
    /// Not yet looked up, in priority order.
    lookup: VecDeque<(Key, T)>,
    /// Cache misses waiting for an I/O thread, sorted by rank.
    read: VecDeque<(u32, Key, T)>,
    /// Source bytes in memory waiting for a CPU worker, sorted by rank.
    decode: VecDeque<(u32, Key, T)>,
    /// Wanted keys -> position in the last [`schedule`](Self::schedule) list.
    rank: HashMap<Key, u32>,
    pending: HashSet<Key>,
    /// Bound on `decode` (memory held by read-ahead); readers wait while it is full.
    decode_cap: usize,
    /// Bound on the wanted list.
    cap: usize,
    pub closed: bool,
    #[cfg(test)]
    pub cancelled: usize,
}

impl<T> Sched<T> {
    pub fn new(cap: usize, decode_cap: usize) -> Self {
        Self {
            lookup: VecDeque::new(),
            read: VecDeque::new(),
            decode: VecDeque::new(),
            rank: HashMap::new(),
            pending: HashSet::new(),
            decode_cap: decode_cap.max(1),
            cap,
            closed: false,
            #[cfg(test)]
            cancelled: 0,
        }
    }

    /// Makes `wanted` (best first, at most `cap` taken) the complete set of wanted keys.
    /// Queued work for keys not in it is cancelled; keys already in flight continue.
    /// `make` builds the task for a key that is not yet in the pipeline (`None`: skip it).
    pub fn schedule(&mut self, wanted: impl IntoIterator<Item = Key>, mut make: impl FnMut(Key) -> Option<T>) {
        let mut queued: HashMap<Key, T> = self.lookup.drain(..).map(|(key, task)| (key, task)).collect();
        for key in queued.keys() {
            self.pending.remove(key);
        }
        self.rank.clear();
        for key in wanted.into_iter().take(self.cap) {
            if self.rank.contains_key(&key) {
                continue;
            }
            self.rank.insert(key, self.rank.len() as u32);
            if self.pending.contains(&key) {
                continue; // past the lookup stage, or delivered and not yet finished
            }
            if let Some(task) = queued.remove(&key).or_else(|| make(key)) {
                self.pending.insert(key);
                self.lookup.push_back((key, task));
            }
        }
        #[cfg(test)]
        {
            self.cancelled += queued.len();
        }
        for queue in [&mut self.read, &mut self.decode] {
            let rank = &self.rank;
            let pending = &mut self.pending;
            #[cfg(test)]
            let before = queue.len();
            queue.retain_mut(|(r, key, _)| match rank.get(key) {
                Some(&new) => {
                    *r = new;
                    true
                }
                None => {
                    pending.remove(key);
                    false
                }
            });
            #[cfg(test)]
            {
                self.cancelled += before - queue.len();
            }
            queue.make_contiguous().sort_by_key(|e| e.0);
        }
    }

    /// Next CPU job: the better ranked of the next decode and the next lookup.
    pub fn next_cpu(&mut self) -> Option<(Key, T)> {
        let lookup_rank = self.lookup.front().map(|(key, _)| self.rank.get(key).copied().unwrap_or(u32::MAX));
        match (self.decode.front(), lookup_rank) {
            (Some(&(d, ..)), Some(l)) if l < d => self.lookup.pop_front(),
            (Some(_), _) => self.decode.pop_front().map(|(_, key, task)| (key, task)),
            (None, _) => self.lookup.pop_front(),
        }
    }

    /// Next source read, unless enough read-ahead is already waiting for the CPU.
    pub fn next_read(&mut self) -> Option<(Key, T)> {
        if self.decode.len() >= self.decode_cap {
            return None;
        }
        self.read.pop_front().map(|(_, key, task)| (key, task))
    }

    /// A lookup missed every cache: queue the source read, or drop the task if its tile
    /// left the window meanwhile. Returns whether it was queued.
    pub fn miss(&mut self, key: Key, task: T) -> bool {
        Self::insert(&self.rank, &mut self.pending, &mut self.read, key, task)
    }

    /// Source bytes are in memory: queue the decode, or drop if no longer wanted.
    pub fn read_done(&mut self, key: Key, task: T) -> bool {
        Self::insert(&self.rank, &mut self.pending, &mut self.decode, key, task)
    }

    fn insert(rank: &HashMap<Key, u32>, pending: &mut HashSet<Key>, queue: &mut VecDeque<(u32, Key, T)>, key: Key, task: T) -> bool {
        let Some(&r) = rank.get(&key) else {
            pending.remove(&key);
            return false;
        };
        let at = queue.partition_point(|e| e.0 <= r);
        queue.insert(at, (r, key, task));
        true
    }

    /// The result for `key` reached the UI (or the task failed): it may be requested again.
    pub fn finish(&mut self, key: Key) {
        self.pending.remove(&key);
    }

    #[cfg(test)]
    fn lens(&self) -> (usize, usize, usize) {
        (self.lookup.len(), self.read.len(), self.decode.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(ids: impl IntoIterator<Item = u32>) -> Vec<Key> {
        ids.into_iter().map(|id| (id, Tier::Small)).collect()
    }

    fn drain_cpu(s: &mut Sched<u32>) -> Vec<u32> {
        std::iter::from_fn(|| s.next_cpu().map(|(_, task)| task)).collect()
    }

    #[test]
    fn window_orders_visible_then_ahead_then_behind() {
        // rows 10..=12 visible of 100, resting after scrolling down: 3 ahead, 1 behind
        assert_eq!(window_rows(10, 12, 100, true, false, 3, 1), vec![10, 11, 12, 13, 14, 15, 9]);
        // scrolling up: ahead is above, nearest first
        assert_eq!(window_rows(10, 12, 100, false, false, 3, 1), vec![10, 11, 12, 9, 8, 7, 13]);
        // while moving, visible rows from the leading edge
        assert_eq!(window_rows(10, 12, 100, true, true, 3, 1), vec![12, 11, 10, 13, 14, 15, 9]);
        assert_eq!(window_rows(10, 12, 100, false, true, 1, 1), vec![10, 11, 12, 9, 13]);
        // clipped at both ends of the list
        assert_eq!(window_rows(0, 2, 4, true, false, 5, 5), vec![0, 1, 2, 3]);
        assert_eq!(window_rows(0, 9, 3, false, false, 2, 2), vec![0, 1, 2]);
        assert!(window_rows(0, 3, 0, true, false, 2, 2).is_empty());
    }

    #[test]
    fn jobs_run_in_schedule_order() {
        let mut s = Sched::new(512, 4);
        // visible 0..4 (row 0), ahead 8..12 (row 2), behind 4..8 is ranked last
        let rows = window_rows(1, 1, 3, true, false, 1, 1);
        assert_eq!(rows, vec![1, 2, 0]);
        let wanted: Vec<Key> = rows.iter().flat_map(|&r| keys(r as u32 * 4..r as u32 * 4 + 4)).collect();
        s.schedule(wanted, |k| Some(k.0));
        assert_eq!(drain_cpu(&mut s), vec![4, 5, 6, 7, 8, 9, 10, 11, 0, 1, 2, 3]);
        assert!(s.next_cpu().is_none());
    }

    #[test]
    fn rescheduling_cancels_tiles_that_left_the_window() {
        let mut s = Sched::new(512, 4);
        s.schedule(keys(0..10), |k| Some(k.0));
        let (key, task) = s.next_cpu().unwrap(); // 0 is being looked up
        assert_eq!(key.0, 0);
        let (key1, task1) = s.next_cpu().unwrap();
        assert!(s.miss(key1, task1)); // 1 waits for a read

        // scrolled: now 5..15 wanted, best first
        let mut made = Vec::new();
        s.schedule(keys(5..15), |k| {
            made.push(k.0);
            Some(k.0)
        });
        assert_eq!(made, (10..15).collect::<Vec<_>>(), "5..10 reuse their queued tasks");
        assert_eq!(s.cancelled, 3 + 1, "lookups of 2..5 and the read of 1 dropped");
        assert_eq!(s.lens(), (10, 0, 0));
        // the in-flight lookup of 0 misses: no longer wanted, so dropped at the hand-off
        assert!(!s.miss(key, task));
        assert!(!s.pending.contains(&key), "and can be requested again later");
        assert_eq!(drain_cpu(&mut s), (5..15).collect::<Vec<_>>());
    }

    #[test]
    fn in_flight_keys_are_not_queued_twice() {
        let mut s = Sched::new(512, 4);
        s.schedule(keys(0..3), |k| Some(k.0));
        let (k0, _) = s.next_cpu().unwrap();
        s.schedule(keys(0..3), |k| Some(k.0 + 100));
        assert_eq!(drain_cpu(&mut s), vec![1, 2], "0 is in flight; 1 and 2 keep their tasks");
        s.finish(k0);
        s.schedule(keys(0..1), |k| Some(k.0));
        assert_eq!(drain_cpu(&mut s), vec![0]);
    }

    #[test]
    fn stages_follow_rank_and_bound_read_ahead() {
        let mut s = Sched::new(512, 2);
        s.schedule(keys(0..6), |k| Some(k.0));
        for _ in 0..6 {
            let (k, t) = s.next_cpu().unwrap();
            assert!(s.miss(k, t));
        }
        // reads come out in rank order even though misses were inserted in any order
        let reads: Vec<_> = (0..2).map(|_| s.next_read().unwrap()).collect();
        assert_eq!(reads.iter().map(|r| r.0 .0).collect::<Vec<_>>(), vec![0, 1]);
        for (k, t) in reads.into_iter().rev() {
            assert!(s.read_done(k, t));
        }
        assert!(s.next_read().is_none(), "decode queue full: readers wait");
        assert_eq!(s.next_cpu().unwrap().0 .0, 0);
        assert_eq!(s.next_read().unwrap().0 .0, 2);

        // re-rank: 5 becomes the most wanted; 1 (read, awaiting decode) is dropped
        s.schedule(keys([5, 4, 3, 0]), |k| Some(k.0));
        assert_eq!(s.next_read().unwrap().0 .0, 5);
        assert_eq!(s.lens(), (0, 2, 0));
    }

    #[test]
    fn queue_is_bounded_and_decodes_beat_worse_lookups() {
        let mut s = Sched::new(8, 4);
        s.schedule(keys(0..100), |k| Some(k.0));
        assert_eq!(s.lens().0, 8);
        assert_eq!(s.rank.len(), 8);

        let mut s = Sched::new(512, 4);
        s.schedule(keys(0..4), |k| Some(k.0));
        let (k, t) = s.next_cpu().unwrap();
        s.miss(k, t);
        let (k, t) = s.next_read().unwrap();
        s.read_done(k, t);
        assert_eq!(s.next_cpu().unwrap().0 .0, 0, "decode of rank 0 before lookup of rank 1");
        assert_eq!(s.next_cpu().unwrap().0 .0, 1);
    }
}
