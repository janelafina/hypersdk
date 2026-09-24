//! Strict L2 book reconstruction from Dwellir's `StreamL2BookDiff` stream.
//!
//! [`L2BookRecorder`] maintains one coin's aggregated book; [`L2BookSet`]
//! holds one recorder per subscribed coin and routes each
//! [`L2BookDiffUpdate`] frame to them.
//!
//! # Sequencing
//!
//! - A `snapshot: true` entry always replaces the coin's book and adopts its
//!   `seq`, regardless of the previously recorded `seq`. This covers both the
//!   opening snapshots after a (re)connect (`seq == 1`) and a server-initiated
//!   mid-stream reset, so no manual reset is needed after reconnecting.
//! - Every non-snapshot entry must have `prev_seq` equal to the last accepted
//!   `seq` and `seq == prev_seq + 1`. Anything else is reported as a
//!   sequence error and the book is left untouched. The book is then no
//!   longer trustworthy: the caller should force a reconnect (e.g.
//!   [`L2BookDiffConnection::reconnect`](super::L2BookDiffConnection::reconnect))
//!   and let the fresh opening snapshots rebuild it.
//!
//! # Level semantics
//!
//! A diff level with `sz == 0` (compared numerically, so `"0.0"` and `"0"`
//! both qualify) removes that price; any other size inserts or replaces the
//! level. Removing a price that is not in the book is treated as an
//! idempotent no-op: Dwellir does not document it as an error and the
//! `seq`/`prev_seq` chain is the documented loss detector.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
};

use rust_decimal::Decimal;

use super::{
    l2::L2BookDiffRequest,
    types::{L2BookDiffUpdate, L2CoinDiff, L2DiffLevel},
};
use crate::hypercore::types::Side;

/// Result of applying an entry or frame to an L2 recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L2ApplyOutcome {
    /// A `snapshot: true` entry replaced the book.
    Rebuilt,
    /// An incremental diff was applied.
    Applied,
    /// The frame had no entry for this coin (quiet coin); nothing changed.
    Unchanged,
}

/// Strict L2 reconstruction failure. On any sequence error the caller should
/// reconnect and rebuild from fresh snapshots.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum L2ReconstructionError {
    /// An incremental diff arrived before any snapshot for this coin.
    #[error("{coin}: diff seq {seq} before any snapshot")]
    MissingBaseSnapshot { coin: String, seq: u64 },
    /// The entry is for a different coin than the recorder.
    #[error("coin mismatch: recorder={expected}, message={actual}")]
    CoinMismatch { expected: String, actual: String },
    /// `prev_seq` does not equal the last accepted `seq` (`expected`):
    /// frames were lost or duplicated.
    #[error(
        "{coin}: sequence gap: expected prev_seq {expected}, got prev_seq {prev_seq} (seq {seq})"
    )]
    SequenceGap {
        coin: String,
        expected: u64,
        prev_seq: u64,
        seq: u64,
    },
    /// A non-snapshot entry whose `seq` is not `prev_seq + 1`.
    #[error("{coin}: invalid sequence step prev_seq {prev_seq} -> seq {seq}")]
    InvalidSeqStep {
        coin: String,
        prev_seq: u64,
        seq: u64,
    },
    /// A snapshot listed the same price twice on one side.
    #[error("{coin}: duplicate {side} level at px {px} in snapshot")]
    DuplicateLevel {
        coin: String,
        side: Side,
        px: Decimal,
    },
    /// A frame contained an entry for a coin this [`L2BookSet`] does not track.
    #[error("unknown coin {0} in update")]
    UnknownCoin(String),
    /// A frame contained two entries for the same coin.
    #[error("coin {0} appears more than once in one update")]
    DuplicateCoin(String),
}

impl L2ReconstructionError {
    /// `true` for errors that mean data was lost and the stream must be
    /// re-subscribed (sequence gaps, bad steps, diffs before a snapshot).
    #[must_use]
    pub fn requires_resync(&self) -> bool {
        matches!(
            self,
            Self::MissingBaseSnapshot { .. }
                | Self::SequenceGap { .. }
                | Self::InvalidSeqStep { .. }
        )
    }
}

/// Strict single-coin L2 book recorder for `StreamL2BookDiff`.
///
/// Bids are kept sorted by descending price and asks by ascending price, so
/// [`bids`](Self::bids)`[0]` / [`asks`](Self::asks)`[0]` are the top of book.
/// Every apply is atomic: on error the recorded state is unchanged.
#[derive(Debug, Clone)]
pub struct L2BookRecorder {
    coin: String,
    /// Last accepted `seq`; `None` until the first snapshot.
    seq: Option<u64>,
    block_number: Option<u64>,
    time_ms: Option<u64>,
    bids: Vec<L2DiffLevel>,
    asks: Vec<L2DiffLevel>,
}

impl L2BookRecorder {
    /// Empty recorder for `coin`; it needs a snapshot entry before diffs.
    #[must_use]
    pub fn new(coin: impl Into<String>) -> Self {
        Self {
            coin: coin.into(),
            seq: None,
            block_number: None,
            time_ms: None,
            bids: Vec::new(),
            asks: Vec::new(),
        }
    }

    /// Coin tracked by this recorder.
    #[must_use]
    pub fn coin(&self) -> &str {
        &self.coin
    }

    /// `true` once a snapshot has been applied.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.seq.is_some()
    }

    /// Last accepted per-coin `seq`.
    #[must_use]
    pub fn seq(&self) -> Option<u64> {
        self.seq
    }

    /// Block number of the frame that last changed this coin. Quiet coins do
    /// not advance, so this may lag the stream's latest block.
    #[must_use]
    pub fn block_number(&self) -> Option<u64> {
        self.block_number
    }

    /// Block time (Unix ms) of the frame that last changed this coin.
    #[must_use]
    pub fn time_ms(&self) -> Option<u64> {
        self.time_ms
    }

    /// Bids, sorted by descending price.
    #[must_use]
    pub fn bids(&self) -> &[L2DiffLevel] {
        &self.bids
    }

    /// Asks, sorted by ascending price.
    #[must_use]
    pub fn asks(&self) -> &[L2DiffLevel] {
        &self.asks
    }

    /// Best (highest) bid.
    #[must_use]
    pub fn best_bid(&self) -> Option<&L2DiffLevel> {
        self.bids.first()
    }

    /// Best (lowest) ask.
    #[must_use]
    pub fn best_ask(&self) -> Option<&L2DiffLevel> {
        self.asks.first()
    }

    /// Clears all state; the next entry must be a snapshot. Not needed after
    /// a reconnect (snapshots always reset), but useful to discard a book
    /// known to be broken.
    pub fn reset(&mut self) {
        self.seq = None;
        self.block_number = None;
        self.time_ms = None;
        self.bids.clear();
        self.asks.clear();
    }

    /// Applies this coin's entry from `update`, or returns
    /// [`L2ApplyOutcome::Unchanged`] if the frame does not mention the coin.
    pub fn apply_update(
        &mut self,
        update: &L2BookDiffUpdate,
    ) -> Result<L2ApplyOutcome, L2ReconstructionError> {
        match update.diff_for(&self.coin) {
            Some(diff) => self.apply_diff(diff, update.block_number, update.time),
            None => Ok(L2ApplyOutcome::Unchanged),
        }
    }

    /// Applies one per-coin entry carried by a frame at `block_number` /
    /// `time_ms`.
    pub fn apply_diff(
        &mut self,
        diff: &L2CoinDiff,
        block_number: u64,
        time_ms: u64,
    ) -> Result<L2ApplyOutcome, L2ReconstructionError> {
        self.check(diff)?;
        Ok(self.commit(diff, block_number, time_ms))
    }

    /// Validates without mutating. After `Ok`, [`Self::commit`] cannot fail.
    fn check(&self, diff: &L2CoinDiff) -> Result<(), L2ReconstructionError> {
        if diff.coin != self.coin {
            return Err(L2ReconstructionError::CoinMismatch {
                expected: self.coin.clone(),
                actual: diff.coin.clone(),
            });
        }
        if diff.snapshot {
            check_unique(&self.coin, Side::Bid, &diff.bids)?;
            check_unique(&self.coin, Side::Ask, &diff.asks)?;
            return Ok(());
        }
        let Some(last) = self.seq else {
            return Err(L2ReconstructionError::MissingBaseSnapshot {
                coin: self.coin.clone(),
                seq: diff.seq,
            });
        };
        if diff.prev_seq != last {
            return Err(L2ReconstructionError::SequenceGap {
                coin: self.coin.clone(),
                expected: last,
                prev_seq: diff.prev_seq,
                seq: diff.seq,
            });
        }
        if diff.prev_seq.checked_add(1) != Some(diff.seq) {
            return Err(L2ReconstructionError::InvalidSeqStep {
                coin: self.coin.clone(),
                prev_seq: diff.prev_seq,
                seq: diff.seq,
            });
        }
        Ok(())
    }

    fn commit(&mut self, diff: &L2CoinDiff, block_number: u64, time_ms: u64) -> L2ApplyOutcome {
        let outcome = if diff.snapshot {
            self.bids = sorted_side(&diff.bids, Side::Bid);
            self.asks = sorted_side(&diff.asks, Side::Ask);
            L2ApplyOutcome::Rebuilt
        } else {
            for level in &diff.bids {
                upsert(&mut self.bids, Side::Bid, level);
            }
            for level in &diff.asks {
                upsert(&mut self.asks, Side::Ask, level);
            }
            L2ApplyOutcome::Applied
        };
        self.seq = Some(diff.seq);
        self.block_number = Some(block_number);
        self.time_ms = Some(time_ms);
        outcome
    }
}

fn check_unique(
    coin: &str,
    side: Side,
    levels: &[L2DiffLevel],
) -> Result<(), L2ReconstructionError> {
    let mut seen = HashSet::with_capacity(levels.len());
    for level in levels {
        // Normalize so "100" and "100.0" collide.
        if !seen.insert(level.px.normalize()) {
            return Err(L2ReconstructionError::DuplicateLevel {
                coin: coin.to_owned(),
                side,
                px: level.px,
            });
        }
    }
    Ok(())
}

/// Position of `px` in a side sorted best-first.
fn search(levels: &[L2DiffLevel], side: Side, px: Decimal) -> Result<usize, usize> {
    match side {
        Side::Bid => levels.binary_search_by(|level| px.cmp(&level.px)),
        Side::Ask => levels.binary_search_by(|level| level.px.cmp(&px)),
    }
}

fn sorted_side(levels: &[L2DiffLevel], side: Side) -> Vec<L2DiffLevel> {
    let mut out: Vec<L2DiffLevel> = levels
        .iter()
        .filter(|level| !level.sz.is_zero())
        .copied()
        .collect();
    match side {
        Side::Bid => out.sort_by_key(|level| Reverse(level.px)),
        Side::Ask => out.sort_by_key(|level| level.px),
    }
    out
}

fn upsert(levels: &mut Vec<L2DiffLevel>, side: Side, level: &L2DiffLevel) {
    match (search(levels, side, level.px), level.sz.is_zero()) {
        (Ok(idx), true) => {
            levels.remove(idx);
        }
        (Ok(idx), false) => levels[idx] = *level,
        (Err(_), true) => {}
        (Err(idx), false) => levels.insert(idx, *level),
    }
}

/// Multi-coin L2 book container: one [`L2BookRecorder`] per subscribed coin.
///
/// One `StreamL2BookDiff` RPC carries up to 20 coins; [`Self::apply`] routes
/// each entry of a frame to its coin's recorder. A frame is applied
/// atomically: every entry is validated first and nothing is mutated if any
/// entry fails.
///
/// An entry for a coin that is not tracked is rejected with
/// [`L2ReconstructionError::UnknownCoin`] rather than ignored: the server only
/// sends subscribed coins, so an unexpected coin means the set and the
/// subscription disagree.
#[derive(Debug, Clone, Default)]
pub struct L2BookSet {
    books: HashMap<String, L2BookRecorder>,
    /// Subscription order, for deterministic iteration.
    order: Vec<String>,
    block_number: Option<u64>,
    time_ms: Option<u64>,
}

impl L2BookSet {
    /// Creates empty recorders for `coins` (duplicates ignored).
    pub fn new<I, S>(coins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut set = Self::default();
        for coin in coins {
            let coin = coin.into();
            if !set.books.contains_key(&coin) {
                set.order.push(coin.clone());
                set.books.insert(coin.clone(), L2BookRecorder::new(coin));
            }
        }
        set
    }

    /// Creates recorders for every coin in `request`.
    #[must_use]
    pub fn from_request(request: &L2BookDiffRequest) -> Self {
        Self::new(request.coins.iter().cloned())
    }

    /// Recorder for `coin`.
    #[must_use]
    pub fn get(&self, coin: &str) -> Option<&L2BookRecorder> {
        self.books.get(coin)
    }

    /// Recorders in subscription order.
    pub fn iter(&self) -> impl Iterator<Item = &L2BookRecorder> {
        self.order.iter().filter_map(|coin| self.books.get(coin))
    }

    /// Number of tracked coins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.books.len()
    }

    /// `true` when no coins are tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }

    /// `true` once every tracked coin has received a snapshot.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.books.values().all(L2BookRecorder::is_ready)
    }

    /// Block number of the last applied frame (any coin).
    #[must_use]
    pub fn block_number(&self) -> Option<u64> {
        self.block_number
    }

    /// Block time (Unix ms) of the last applied frame (any coin).
    #[must_use]
    pub fn time_ms(&self) -> Option<u64> {
        self.time_ms
    }

    /// Resets every recorder (see [`L2BookRecorder::reset`]).
    pub fn reset(&mut self) {
        for book in self.books.values_mut() {
            book.reset();
        }
        self.block_number = None;
        self.time_ms = None;
    }

    /// Applies a frame atomically across coins. Returns the coins that were
    /// changed together with the per-coin outcome.
    pub fn apply(
        &mut self,
        update: &L2BookDiffUpdate,
    ) -> Result<Vec<(String, L2ApplyOutcome)>, L2ReconstructionError> {
        let mut seen = HashSet::with_capacity(update.diffs.len());
        for diff in &update.diffs {
            if !seen.insert(diff.coin.as_str()) {
                return Err(L2ReconstructionError::DuplicateCoin(diff.coin.clone()));
            }
            let book = self
                .books
                .get(&diff.coin)
                .ok_or_else(|| L2ReconstructionError::UnknownCoin(diff.coin.clone()))?;
            book.check(diff)?;
        }
        let mut outcomes = Vec::with_capacity(update.diffs.len());
        for diff in &update.diffs {
            let book = self
                .books
                .get_mut(&diff.coin)
                .expect("validated above: coin is tracked");
            let outcome = book.commit(diff, update.block_number, update.time);
            outcomes.push((diff.coin.clone(), outcome));
        }
        self.block_number = Some(update.block_number);
        self.time_ms = Some(update.time);
        Ok(outcomes)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::dec;

    use super::*;
    use crate::hypercore::{dwellir::l2::wire, types::BookLevel};

    fn lvl(px: &str, sz: &str, n: usize) -> BookLevel {
        BookLevel {
            px: px.parse().unwrap(),
            sz: sz.parse().unwrap(),
            n,
        }
    }

    fn diff(
        coin: &str,
        seq: u64,
        prev_seq: u64,
        snapshot: bool,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    ) -> L2CoinDiff {
        L2CoinDiff {
            coin: coin.into(),
            seq,
            prev_seq,
            bids,
            asks,
            snapshot,
        }
    }

    fn update(block: u64, diffs: Vec<L2CoinDiff>) -> L2BookDiffUpdate {
        L2BookDiffUpdate {
            time: block * 10,
            block_number: block,
            diffs,
        }
    }

    fn pxs(levels: &[BookLevel]) -> Vec<Decimal> {
        levels.iter().map(|l| l.px).collect()
    }

    fn snapshot_btc() -> L2CoinDiff {
        diff(
            "BTC",
            1,
            0,
            true,
            vec![lvl("99", "1", 1), lvl("100", "2", 2), lvl("98", "3", 3)],
            vec![lvl("102", "1", 1), lvl("101", "2", 1)],
        )
    }

    #[test]
    fn converts_wire_levels_strictly() {
        let raw = wire::L2BookDiffUpdate {
            time: 5,
            block_number: 6,
            diffs: vec![wire::L2CoinDiff {
                coin: "BTC".into(),
                seq: 2,
                prev_seq: 1,
                bids: vec![wire::L2Level {
                    px: "100.5".into(),
                    sz: "0.0".into(),
                    n: 0,
                }],
                asks: vec![wire::L2Level {
                    px: "101".into(),
                    sz: "1.25".into(),
                    n: 4,
                }],
                snapshot: false,
            }],
        };
        let typed = L2BookDiffUpdate::try_from(raw.clone()).unwrap();
        assert_eq!(typed.time, 5);
        assert_eq!(typed.block_number, 6);
        let entry = &typed.diffs[0];
        assert_eq!(entry.bids[0].px, dec!(100.5));
        assert!(entry.bids[0].sz.is_zero());
        assert_eq!(entry.asks[0].sz, dec!(1.25));
        assert_eq!(entry.asks[0].n, 4);

        use crate::hypercore::dwellir::L2DiffConversionError as E;
        let mut bad = raw.clone();
        bad.diffs[0].asks[0].px = "abc".into();
        assert!(matches!(
            L2BookDiffUpdate::try_from(bad),
            Err(E::InvalidDecimal {
                field: "px",
                side: Side::Ask,
                ..
            })
        ));
        let mut bad = raw.clone();
        bad.diffs[0].bids[0].sz = String::new();
        assert!(matches!(
            L2BookDiffUpdate::try_from(bad),
            Err(E::InvalidDecimal {
                field: "sz",
                side: Side::Bid,
                ..
            })
        ));
        let mut bad = raw.clone();
        bad.diffs[0].bids[0].sz = "-1".into();
        assert!(matches!(
            L2BookDiffUpdate::try_from(bad),
            Err(E::OutOfRange { field: "sz", .. })
        ));
        let mut bad = raw.clone();
        bad.diffs[0].bids[0].px = "0".into();
        assert!(matches!(
            L2BookDiffUpdate::try_from(bad),
            Err(E::OutOfRange { field: "px", .. })
        ));
        let mut bad = raw.clone();
        bad.block_number = -1;
        assert!(matches!(
            L2BookDiffUpdate::try_from(bad),
            Err(E::NegativeField {
                field: "block_number",
                ..
            })
        ));
        let mut bad = raw;
        bad.diffs[0].coin = String::new();
        assert_eq!(L2BookDiffUpdate::try_from(bad), Err(E::EmptyCoin));
    }

    #[test]
    fn snapshot_then_diffs_keep_sorted_book() {
        let mut book = L2BookRecorder::new("BTC");
        assert!(!book.is_ready());
        assert_eq!(
            book.apply_update(&update(10, vec![snapshot_btc()])),
            Ok(L2ApplyOutcome::Rebuilt)
        );
        assert_eq!(pxs(book.bids()), vec![dec!(100), dec!(99), dec!(98)]);
        assert_eq!(pxs(book.asks()), vec![dec!(101), dec!(102)]);
        assert_eq!(book.seq(), Some(1));
        assert_eq!(book.block_number(), Some(10));
        assert_eq!(book.time_ms(), Some(100));

        // Update 99, remove 100 via "0.0", insert 99.5 and 97; asks: insert 100.5, remove 102.
        let d = diff(
            "BTC",
            2,
            1,
            false,
            vec![
                lvl("99", "5", 4),
                lvl("100", "0.0", 0),
                lvl("99.5", "1", 1),
                lvl("97", "1", 1),
            ],
            vec![lvl("100.5", "1", 1), lvl("102", "0", 0)],
        );
        assert_eq!(
            book.apply_update(&update(11, vec![d])),
            Ok(L2ApplyOutcome::Applied)
        );
        assert_eq!(
            pxs(book.bids()),
            vec![dec!(99.5), dec!(99), dec!(98), dec!(97)]
        );
        assert_eq!(book.bids()[1].sz, dec!(5));
        assert_eq!(book.bids()[1].n, 4);
        assert_eq!(pxs(book.asks()), vec![dec!(100.5), dec!(101)]);
        assert_eq!(book.best_bid().unwrap().px, dec!(99.5));
        assert_eq!(book.best_ask().unwrap().px, dec!(100.5));
        assert_eq!(book.seq(), Some(2));
        assert_eq!(book.block_number(), Some(11));

        // Removing an absent level is a no-op; a quiet frame changes nothing.
        let d = diff("BTC", 3, 2, false, vec![lvl("50", "0", 0)], vec![]);
        book.apply_update(&update(12, vec![d])).unwrap();
        assert_eq!(book.bids().len(), 4);
        assert_eq!(
            book.apply_update(&update(13, vec![])),
            Ok(L2ApplyOutcome::Unchanged)
        );
        assert_eq!(book.block_number(), Some(12));
    }

    #[test]
    fn detects_sequence_errors_without_mutating() {
        let mut book = L2BookRecorder::new("BTC");
        let err = book
            .apply_diff(&diff("BTC", 2, 1, false, vec![], vec![]), 1, 1)
            .unwrap_err();
        assert!(matches!(
            err,
            L2ReconstructionError::MissingBaseSnapshot { .. }
        ));
        assert!(err.requires_resync());

        book.apply_diff(&snapshot_btc(), 1, 1).unwrap();
        book.apply_diff(&diff("BTC", 2, 1, false, vec![], vec![]), 2, 2)
            .unwrap();

        // Lost seq 3: prev_seq 3 does not match last accepted 2.
        let before = book.bids().to_vec();
        let gap = diff("BTC", 4, 3, false, vec![lvl("1", "1", 1)], vec![]);
        assert_eq!(
            book.apply_diff(&gap, 3, 3),
            Err(L2ReconstructionError::SequenceGap {
                coin: "BTC".into(),
                expected: 2,
                prev_seq: 3,
                seq: 4,
            })
        );
        // Duplicate/replayed entry also breaks the chain.
        assert!(matches!(
            book.apply_diff(&diff("BTC", 2, 1, false, vec![], vec![]), 3, 3),
            Err(L2ReconstructionError::SequenceGap { expected: 2, .. })
        ));
        // Chained prev_seq but seq skips.
        let bad_step = diff("BTC", 5, 2, false, vec![lvl("1", "1", 1)], vec![]);
        assert_eq!(
            book.apply_diff(&bad_step, 3, 3),
            Err(L2ReconstructionError::InvalidSeqStep {
                coin: "BTC".into(),
                prev_seq: 2,
                seq: 5,
            })
        );
        assert_eq!(book.bids(), before.as_slice());
        assert_eq!(book.seq(), Some(2));
        assert_eq!(book.block_number(), Some(2));

        assert!(matches!(
            book.apply_diff(&diff("ETH", 3, 2, false, vec![], vec![]), 3, 3),
            Err(L2ReconstructionError::CoinMismatch { .. })
        ));
    }

    #[test]
    fn snapshot_always_rebuilds() {
        let mut book = L2BookRecorder::new("BTC");
        book.apply_diff(&snapshot_btc(), 1, 1).unwrap();
        for seq in 2..=5 {
            book.apply_diff(&diff("BTC", seq, seq - 1, false, vec![], vec![]), seq, seq)
                .unwrap();
        }
        // Mid-stream reset with an arbitrary seq.
        let reset = diff("BTC", 42, 7, true, vec![lvl("10", "1", 1)], vec![]);
        assert_eq!(book.apply_diff(&reset, 6, 6), Ok(L2ApplyOutcome::Rebuilt));
        assert_eq!(pxs(book.bids()), vec![dec!(10)]);
        assert!(book.asks().is_empty());
        assert_eq!(book.seq(), Some(42));
        book.apply_diff(&diff("BTC", 43, 42, false, vec![], vec![]), 7, 7)
            .unwrap();

        // Reconnect: fresh opening snapshot with seq 1 is accepted despite seq 43.
        assert_eq!(
            book.apply_diff(&snapshot_btc(), 8, 8),
            Ok(L2ApplyOutcome::Rebuilt)
        );
        assert_eq!(book.seq(), Some(1));
        assert_eq!(pxs(book.bids()), vec![dec!(100), dec!(99), dec!(98)]);

        // Duplicate price in a snapshot is rejected ("100" == "100.0").
        let dup = diff(
            "BTC",
            1,
            0,
            true,
            vec![lvl("100", "1", 1), lvl("100.0", "2", 1)],
            vec![],
        );
        assert!(matches!(
            book.apply_diff(&dup, 9, 9),
            Err(L2ReconstructionError::DuplicateLevel {
                side: Side::Bid,
                ..
            })
        ));
        assert_eq!(book.bids().len(), 3);

        book.reset();
        assert!(!book.is_ready());
        assert!(book.bids().is_empty());
    }

    #[test]
    fn book_set_routes_and_is_atomic() {
        let req = L2BookDiffRequest::new(["BTC", "ETH"]);
        let mut set = L2BookSet::from_request(&req);
        assert_eq!(set.len(), 2);
        assert!(!set.is_ready());

        let eth_snapshot = diff("ETH", 1, 0, true, vec![lvl("3000", "1", 1)], vec![]);
        let out = set
            .apply(&update(1, vec![snapshot_btc(), eth_snapshot]))
            .unwrap();
        assert_eq!(
            out,
            vec![
                ("BTC".to_string(), L2ApplyOutcome::Rebuilt),
                ("ETH".to_string(), L2ApplyOutcome::Rebuilt)
            ]
        );
        assert!(set.is_ready());

        // Only ETH changes; BTC stays at block 1.
        set.apply(&update(
            2,
            vec![diff("ETH", 2, 1, false, vec![lvl("3001", "1", 1)], vec![])],
        ))
        .unwrap();
        assert_eq!(set.get("ETH").unwrap().bids()[0].px, dec!(3001));
        assert_eq!(set.get("ETH").unwrap().block_number(), Some(2));
        assert_eq!(set.get("BTC").unwrap().block_number(), Some(1));
        assert_eq!(set.block_number(), Some(2));

        // A good BTC entry plus a gapped ETH entry: nothing is applied.
        let frame = update(
            3,
            vec![
                diff("BTC", 2, 1, false, vec![lvl("100", "0", 0)], vec![]),
                diff("ETH", 4, 3, false, vec![], vec![]),
            ],
        );
        assert!(matches!(
            set.apply(&frame),
            Err(L2ReconstructionError::SequenceGap { .. })
        ));
        assert_eq!(set.get("BTC").unwrap().seq(), Some(1));
        assert_eq!(set.get("BTC").unwrap().bids().len(), 3);
        assert_eq!(set.block_number(), Some(2));

        assert_eq!(
            set.apply(&update(3, vec![diff("SOL", 1, 0, true, vec![], vec![])])),
            Err(L2ReconstructionError::UnknownCoin("SOL".into()))
        );
        assert_eq!(
            set.apply(&update(
                3,
                vec![
                    diff("BTC", 2, 1, false, vec![], vec![]),
                    diff("BTC", 3, 2, false, vec![], vec![]),
                ]
            )),
            Err(L2ReconstructionError::DuplicateCoin("BTC".into()))
        );
        assert_eq!(
            set.iter().map(L2BookRecorder::coin).collect::<Vec<_>>(),
            vec!["BTC", "ETH"]
        );
    }
}
