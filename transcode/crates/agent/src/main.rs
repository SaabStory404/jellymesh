//! tcpool-agent: one per transcode device. Probes what the card can do, admits jobs by
//! resolution-weighted capacity, and runs jellyfin-ffmpeg adapted to the card.

mod config;
mod job;
mod metrics;
mod probe;

use config::Config;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tcpool_proto::worker_server::{Worker, WorkerServer};
use tcpool_proto::{client_msg, Caps, ClientMsg, HelloRequest, ServerMsg};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

pub fn log(args: std::fmt::Arguments<'_>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86400;
    println!(
        "{:02}:{:02}:{:02}.{:03} {}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        now.subsec_millis(),
        args
    );
}

/// Set by a preempting `reserve_playback` call; polled by the job task that owns the preempted
/// BATCH reservation (`crates/agent/src/job.rs`). A plain flag, not a `tokio::sync::Notify`: the
/// job task polls it on the same short interval as the existing fence/stall watchdogs, which
/// avoids `Notify`'s lost-wakeup race (a `notify_waiters()` before a waiter has registered is
/// simply dropped) for a signal that must never be missed.
pub struct PreemptFlag(AtomicBool);

impl PreemptFlag {
    fn new() -> Self {
        PreemptFlag(AtomicBool::new(false))
    }

    pub fn is_preempted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn trigger(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// One BATCH reservation's registry entry: what `reserve_playback` needs to preempt it.
struct BatchEntry {
    units: f64,
    preempt: Arc<PreemptFlag>,
    /// Shared with the `Reservation` this entry belongs to: whichever side (this preemption path,
    /// or the reservation's own `Drop`) gets there first performs the one release.
    released: Arc<AtomicBool>,
}

struct UsageState {
    used: f64,
    jobs: u32,
    batch_used: f64,
    batch_jobs: u32,
    next_reservation_id: u64,
    /// Reservations admitted via `reserve_batch`, keyed by a fresh id assigned at admission time.
    /// The entry is inserted atomically with the reservation itself (same lock, same critical
    /// section), before the caller ever sends `Accepted`.
    batch_registry: HashMap<u64, BatchEntry>,
}

/// Units in use; admission is atomic here, so concurrent shims can never overbook a card.
/// PLAYBACK and BATCH share one capacity ceiling; BATCH additionally respects `headroom` units
/// reserved for PLAYBACK bursts, and PLAYBACK may preempt running BATCH reservations to make room
/// (never the reverse -- BATCH never preempts BATCH or PLAYBACK).
pub struct Usage {
    inner: Mutex<UsageState>,
    capacity: f64,
    headroom: f64,
}

pub struct Reservation<'a> {
    usage: &'a Usage,
    units: f64,
    batch_id: Option<u64>,
    released: Arc<AtomicBool>,
    /// `Some` only for a reservation admitted via `reserve_batch`: the job task checks this once
    /// before spawning ffmpeg, then polls it while running.
    pub preempt: Option<Arc<PreemptFlag>>,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        // Lock BEFORE the released check-and-swap, mirroring reserve_playback()'s own preemption
        // path (which removes the registry entry, swaps `released`, and decrements -- all under
        // one held lock). Doing the swap here before acquiring the lock (as this used to) opened
        // a window where a concurrent reserve_playback() could see this reservation's registry
        // entry, find `released` already true (because this swap ran first), and skip its own
        // decrement on the assumption Drop already performed it -- while Drop's decrement was
        // still pending on the lock. A third reservation landing in that window would see `used`
        // inflated by this reservation's units, even though they are, in truth, no longer live.
        // With the swap under the same lock as the decrement, whichever side gets the lock first
        // performs the one decrement atomically with claiming the swap, so no such window exists.
        let mut g = self.usage.inner.lock().unwrap_or_else(|p| p.into_inner());
        if self.released.swap(true, Ordering::SeqCst) {
            return; // a preempting reserve_playback() already released this reservation
        }
        g.used = (g.used - self.units).max(0.0);
        g.jobs = g.jobs.saturating_sub(1);
        if let Some(id) = self.batch_id {
            g.batch_used = (g.batch_used - self.units).max(0.0);
            g.batch_jobs = g.batch_jobs.saturating_sub(1);
            g.batch_registry.remove(&id);
        }
    }
}

impl Usage {
    pub fn new(capacity: f64, headroom: f64) -> Usage {
        Usage {
            inner: Mutex::new(UsageState {
                used: 0.0,
                jobs: 0,
                batch_used: 0.0,
                batch_jobs: 0,
                next_reservation_id: 0,
                batch_registry: HashMap::new(),
            }),
            capacity,
            headroom,
        }
    }

    /// PLAYBACK admission: the unchanged ceiling (`used + units <= capacity`), never blocked by
    /// `headroom` -- that ceiling exists only to keep BATCH out, not to cap PLAYBACK. When it
    /// doesn't fit outright, preempts the fewest running BATCH reservations (largest-units-first,
    /// which minimizes the count preempted) needed to cover the gap, releasing their units
    /// synchronously (this call never waits on a victim's ffmpeg actually exiting) before
    /// admitting.
    pub fn reserve_playback(&self, units: f64) -> Option<Reservation<'_>> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.used + units <= self.capacity + 1e-9 {
            g.used += units;
            g.jobs += 1;
            return Some(Reservation {
                usage: self,
                units,
                batch_id: None,
                released: Arc::new(AtomicBool::new(false)),
                preempt: None,
            });
        }
        let deficit = units - (self.capacity - g.used);
        let mut victims: Vec<(u64, f64)> = g
            .batch_registry
            .iter()
            .map(|(id, e)| (*id, e.units))
            .collect();
        victims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut freed = 0.0;
        let mut chosen = Vec::new();
        for (id, u) in victims {
            if freed >= deficit - 1e-9 {
                break;
            }
            freed += u;
            chosen.push(id);
        }
        if freed < deficit - 1e-9 {
            return None; // preempting every batch reservation still would not make room
        }
        let mut triggered = Vec::new();
        for id in chosen {
            if let Some(entry) = g.batch_registry.remove(&id) {
                if !entry.released.swap(true, Ordering::SeqCst) {
                    g.used = (g.used - entry.units).max(0.0);
                    g.batch_used = (g.batch_used - entry.units).max(0.0);
                    g.jobs = g.jobs.saturating_sub(1);
                    g.batch_jobs = g.batch_jobs.saturating_sub(1);
                }
                triggered.push(entry.preempt);
            }
        }
        g.used += units;
        g.jobs += 1;
        drop(g);
        // Signal victims after releasing the lock: their accounting is already released above,
        // so this admission never waits on their ffmpeg processes actually exiting.
        for p in triggered {
            p.trigger();
        }
        Some(Reservation {
            usage: self,
            units,
            batch_id: None,
            released: Arc::new(AtomicBool::new(false)),
            preempt: None,
        })
    }

    /// BATCH admission: only within `capacity - headroom`, so a playback burst always has
    /// `headroom` units to land in without preempting anything. Registers the reservation's
    /// preempt handle in the registry atomically with the reservation itself.
    pub fn reserve_batch(&self, units: f64) -> Option<Reservation<'_>> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.used + units > self.capacity - self.headroom + 1e-9 {
            return None;
        }
        let id = g.next_reservation_id;
        g.next_reservation_id += 1;
        let preempt = Arc::new(PreemptFlag::new());
        let released = Arc::new(AtomicBool::new(false));
        g.batch_registry.insert(
            id,
            BatchEntry {
                units,
                preempt: preempt.clone(),
                released: released.clone(),
            },
        );
        g.used += units;
        g.jobs += 1;
        g.batch_used += units;
        g.batch_jobs += 1;
        Some(Reservation {
            usage: self,
            units,
            batch_id: Some(id),
            released,
            preempt: Some(preempt),
        })
    }

    pub fn snapshot(&self) -> (f64, f64) {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        (g.used, self.capacity)
    }

    /// `(batch units in use, active batch jobs)`, for the `tcpool_batch_*` gauges.
    pub fn batch_snapshot(&self) -> (f64, u32) {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        (g.batch_used, g.batch_jobs)
    }

    pub fn headroom(&self) -> f64 {
        self.headroom
    }

    fn jobs(&self) -> u32 {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).jobs
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use proptest::prelude::*;
    use std::thread;

    fn make_usage(capacity: f64) -> Usage {
        Usage::new(capacity, 0.0)
    }

    #[test]
    fn reserve_admits_up_to_capacity_and_refuses_over() {
        let u = make_usage(2.0);
        let a = u.reserve_playback(1.5).expect("fits");
        assert!(
            u.reserve_playback(1.0).is_none(),
            "0.5 unit of headroom, 1.0 requested"
        );
        let b = u.reserve_playback(0.5).expect("fits exactly");
        assert_eq!(u.snapshot(), (2.0, 2.0));
        drop(a);
        drop(b);
        assert_eq!(u.snapshot(), (0.0, 2.0));
        assert_eq!(u.jobs(), 0);
    }

    #[test]
    fn reserve_batch_refuses_into_headroom_even_with_capacity_free() {
        let u = Usage::new(4.0, 1.0); // headroom=1: batch may use at most 3.0
        let a = u
            .reserve_batch(3.0)
            .expect("fits exactly under capacity - headroom");
        assert!(
            u.reserve_batch(0.5).is_none(),
            "would eat into the 1.0-unit headroom, even though 1.0 of raw capacity is free"
        );
        assert_eq!(u.batch_snapshot(), (3.0, 1));
        drop(a);
    }

    #[test]
    fn reserve_playback_is_never_blocked_by_headroom() {
        let u = Usage::new(4.0, 3.0); // headroom leaves batch only 1.0 unit
        let a = u
            .reserve_playback(4.0)
            .expect("headroom restricts batch only, never playback");
        drop(a);
    }

    #[test]
    fn reserve_playback_preempts_the_minimum_batch_reservations_largest_first() {
        let u = Usage::new(4.0, 0.0);
        let small = u.reserve_batch(1.0).expect("fits");
        let big = u.reserve_batch(2.0).expect("fits");
        // used=3.0, 1.0 free. A 2.0-unit playback job needs 1.0 more: preempting `big` alone
        // (2.0 units) covers the deficit, so the fewest-jobs choice must spare `small`.
        let p = u.reserve_playback(2.0).expect("preempts to fit");
        assert!(big.preempt.as_ref().unwrap().is_preempted());
        assert!(!small.preempt.as_ref().unwrap().is_preempted());
        assert_eq!(u.batch_snapshot(), (1.0, 1)); // only `small` still counted
        assert_eq!(u.snapshot(), (3.0, 4.0)); // small (1.0) + playback (2.0)
        drop(p);
        drop(small);
        // `big`'s own task later drops its guard too: must not double-release.
        drop(big);
        assert_eq!(u.snapshot(), (0.0, 4.0));
        assert_eq!(u.jobs(), 0);
    }

    #[test]
    fn reserve_playback_does_not_preempt_when_headroom_already_covers_it() {
        let u = Usage::new(4.0, 2.0); // headroom reserves 2.0 for playback
        let batch = u
            .reserve_batch(2.0)
            .expect("fits exactly under capacity - headroom");
        let p = u
            .reserve_playback(2.0)
            .expect("fits in the reserved headroom without preempting anything");
        assert!(!batch.preempt.as_ref().unwrap().is_preempted());
        assert_eq!(u.batch_snapshot(), (2.0, 1));
        drop(p);
        drop(batch);
    }

    #[test]
    fn reserve_playback_refuses_when_even_full_preemption_is_not_enough() {
        let u = Usage::new(4.0, 0.0);
        let _batch = u.reserve_batch(2.0).expect("fits");
        assert!(
            u.reserve_playback(5.0).is_none(),
            "no amount of preemption covers a 5.0-unit request on a 4.0-unit card"
        );
    }

    #[test]
    fn preempted_reservation_release_is_idempotent() {
        let u = Usage::new(2.0, 0.0);
        let batch = u.reserve_batch(2.0).expect("fits");
        let handle = batch.preempt.clone().unwrap();
        let p = u
            .reserve_playback(2.0)
            .expect("preempts the batch reservation to fit");
        assert!(handle.is_preempted());
        assert_eq!(u.snapshot(), (2.0, 2.0)); // batch released, playback admitted
                                              // The preempted job's own task later drops its guard too (job.rs, after its ffmpeg --
                                              // which it may never have gotten to spawn -- ends): must not double-release.
        drop(batch);
        assert_eq!(u.snapshot(), (2.0, 2.0));
        drop(p);
        assert_eq!(u.snapshot(), (0.0, 2.0));
        assert_eq!(u.jobs(), 0);
    }

    #[test]
    fn concurrent_batch_drop_and_playback_preemption_never_inflates_used() {
        // Regression for the Reservation::drop()/reserve_playback() preemption race: Drop used to
        // flip `released` BEFORE acquiring the lock, opening a window where a concurrent
        // reserve_playback() could see this reservation still in the registry, find `released`
        // already true (because Drop's swap ran first), and skip its own decrement on the
        // assumption Drop's decrement -- still pending on the lock -- would cover it. A snapshot
        // taken in that window (or reserve_playback()'s own admission math) read `used` inflated
        // by exactly the victim's units, which could push `used` past `capacity` outright (the
        // repro in review: used=5.0 on a 4.0-unit card). With the swap moved under the same lock
        // as the decrement, that window no longer exists; this drives the race hard (many threads,
        // tiny units, `yield_now()` at the seam) and asserts `used` never exceeds capacity and
        // always drains back to exactly 0.
        let capacity = 4.0;
        let usage = Arc::new(Usage::new(capacity, 0.0));
        let stop = Arc::new(AtomicBool::new(false));
        let overbooked = Arc::new(AtomicBool::new(false));

        let batch_handles: Vec<_> = (0..4)
            .map(|_| {
                let usage = Arc::clone(&usage);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        if let Some(r) = usage.reserve_batch(0.5) {
                            thread::yield_now();
                            drop(r);
                        }
                    }
                })
            })
            .collect();

        let playback_handles: Vec<_> = (0..4)
            .map(|_| {
                let usage = Arc::clone(&usage);
                let stop = Arc::clone(&stop);
                let overbooked = Arc::clone(&overbooked);
                thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        if let Some(r) = usage.reserve_playback(1.0) {
                            let (used, cap) = usage.snapshot();
                            if used > cap + 1e-6 {
                                overbooked.store(true, Ordering::SeqCst);
                            }
                            thread::yield_now();
                            drop(r);
                        }
                    }
                })
            })
            .collect();

        thread::sleep(std::time::Duration::from_millis(300));
        stop.store(true, Ordering::SeqCst);
        for h in batch_handles.into_iter().chain(playback_handles) {
            h.join().unwrap();
        }

        assert!(
            !overbooked.load(Ordering::SeqCst),
            "used exceeded capacity during concurrent batch-drop/playback-preemption racing"
        );
        let (used, _) = usage.snapshot();
        assert!(used.abs() < 1e-6, "used did not drain to exactly 0: {used}");
        assert_eq!(usage.jobs(), 0);
    }

    #[test]
    fn batch_reservation_preempt_flag_starts_clear() {
        let u = Usage::new(2.0, 0.0);
        let batch = u.reserve_batch(1.0).expect("fits");
        assert!(
            !batch.preempt.as_ref().unwrap().is_preempted(),
            "a job task's 'preempted before start' check must read false until actually preempted"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        // Property (3): weighted admission never overbooks and always drains back to 0, under
        // concurrent reserve/drop from multiple threads with random unit sizes.
        #[test]
        fn concurrent_reserve_drop_never_overbooks(
            capacity in 1.0f64..20.0,
            units in prop::collection::vec(0.05f64..6.0, 30..150),
        ) {
            let usage = Arc::new(make_usage(capacity));
            let n_threads = 6usize;
            let chunk_size = (units.len() / n_threads).max(1);
            let chunks: Vec<Vec<f64>> = units.chunks(chunk_size).map(|c| c.to_vec()).collect();
            let overbooked = Arc::new(AtomicBool::new(false));

            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    let usage = Arc::clone(&usage);
                    let overbooked = Arc::clone(&overbooked);
                    thread::spawn(move || {
                        for u in chunk {
                            if let Some(r) = usage.reserve_playback(u) {
                                let (used, cap) = usage.snapshot();
                                if used > cap + 1e-6 {
                                    overbooked.store(true, Ordering::SeqCst);
                                }
                                // Interleave a little before releasing.
                                thread::yield_now();
                                drop(r);
                            }
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }

            prop_assert!(
                !overbooked.load(Ordering::SeqCst),
                "units_used exceeded capacity during concurrent admission"
            );
            let (used, _cap) = usage.snapshot();
            prop_assert!(used.abs() < 1e-6, "units_used did not return to 0: {used}");
            prop_assert_eq!(usage.jobs(), 0);
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Playback(f64),
        Batch(f64),
        /// Drop the `i % live.len()`-th still-live reservation (a no-op if none are live).
        Drop(usize),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => (0.05f64..4.0).prop_map(Op::Playback),
            3 => (0.05f64..4.0).prop_map(Op::Batch),
            2 => (0usize..64).prop_map(Op::Drop),
        ]
    }

    /// Mark every live reservation whose preempt flag has newly flipped to `true` (i.e. was
    /// triggered by the `reserve_playback` call that was just made) as no longer counted toward
    /// `expected_used` -- mirrors, from the outside, the synchronous release `reserve_playback`
    /// performs on its victims.
    fn sync_preemptions(live: &mut [(Reservation<'_>, f64, bool)], expected_used: &mut f64) {
        for (r, units, counted) in live.iter_mut() {
            if *counted {
                if let Some(p) = &r.preempt {
                    if p.is_preempted() {
                        *counted = false;
                        *expected_used -= *units;
                    }
                }
            }
        }
    }

    /// `(sum of counted batch units, count of counted batch reservations, count of counted
    /// reservations overall)` -- what `batch_snapshot()`/`jobs()` should read given the model.
    fn counted_batch_and_job_totals(live: &[(Reservation<'_>, f64, bool)]) -> (f64, u32, u32) {
        let mut batch_units = 0.0;
        let mut batch_jobs = 0u32;
        let mut jobs = 0u32;
        for (r, units, counted) in live {
            if *counted {
                jobs += 1;
                if r.preempt.is_some() {
                    batch_units += units;
                    batch_jobs += 1;
                }
            }
        }
        (batch_units, batch_jobs, jobs)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        // Property (4): under a random interleaving of reserve_playback / reserve_batch / drop
        // (which exercises release, and -- via reserve_playback's preemption path -- the
        // preempted-before-the-caller-drops-it double-release guard too), `used` always equals
        // the sum of units of reservations that are both still held AND not yet preempted, is
        // never negative, never exceeds capacity, and drains back to exactly 0 once every
        // reservation (preempted or not) has been dropped.
        #[test]
        fn random_interleaving_keeps_used_equal_to_live_unpreempted_reservations(
            capacity in 1.0f64..12.0,
            headroom_frac in 0.0f64..1.0,
            ops in prop::collection::vec(op_strategy(), 20..200),
        ) {
            let usage = Usage::new(capacity, capacity * headroom_frac);
            let mut live: Vec<(Reservation<'_>, f64, bool)> = Vec::new();
            let mut expected_used = 0.0f64;

            for op in ops {
                match op {
                    Op::Playback(u) => {
                        if let Some(r) = usage.reserve_playback(u) {
                            // Any victims this call preempted must be caught before this
                            // reservation's own units are added, so a same-iteration
                            // preempt-and-admit doesn't cancel itself out in the model.
                            sync_preemptions(&mut live, &mut expected_used);
                            live.push((r, u, true));
                            expected_used += u;
                        }
                    }
                    Op::Batch(u) => {
                        if let Some(r) = usage.reserve_batch(u) {
                            live.push((r, u, true));
                            expected_used += u;
                        }
                    }
                    Op::Drop(i) => {
                        if !live.is_empty() {
                            let idx = i % live.len();
                            let (r, units, counted) = live.remove(idx);
                            if counted {
                                expected_used -= units;
                            }
                            drop(r); // exercises Drop -- a no-op if `reserve_playback` already
                                     // released this one via preemption (idempotent release).
                        }
                    }
                }
                sync_preemptions(&mut live, &mut expected_used);

                let (used, cap) = usage.snapshot();
                prop_assert!(used >= -1e-6, "used went negative: {used}");
                prop_assert!(used <= cap + 1e-6, "usage overbooked: used={used} cap={cap}");
                prop_assert!(
                    (used - expected_used).abs() < 1e-6,
                    "used ({used}) diverged from the sum of live, unpreempted reservations ({expected_used})"
                );

                let (expected_batch_units, expected_batch_jobs, expected_jobs) =
                    counted_batch_and_job_totals(&live);
                prop_assert_eq!(usage.jobs(), expected_jobs, "jobs() diverged from the model");
                let (batch_used, batch_jobs) = usage.batch_snapshot();
                prop_assert!(
                    (batch_used - expected_batch_units).abs() < 1e-6,
                    "batch_snapshot().0 ({batch_used}) diverged from the model ({expected_batch_units})"
                );
                prop_assert_eq!(
                    batch_jobs, expected_batch_jobs,
                    "batch_snapshot().1 diverged from the model"
                );
                prop_assert!(
                    batch_used <= capacity - usage.headroom() + 1e-6,
                    "batch_used ({batch_used}) ate into headroom ({})",
                    usage.headroom()
                );
            }

            drop(live); // release everything still held
            let (used, _) = usage.snapshot();
            prop_assert!(used.abs() < 1e-6, "usage did not drain to exactly 0: {used}");
            prop_assert_eq!(usage.jobs(), 0);
            let (batch_used, batch_jobs) = usage.batch_snapshot();
            prop_assert!(batch_used.abs() < 1e-6, "batch usage did not drain to exactly 0: {batch_used}");
            prop_assert_eq!(batch_jobs, 0);
        }
    }
}

pub struct State {
    pub cfg: Config,
    pub probed: probe::Probed,
    pub usage: Usage,
    pub probed_unix: i64,
    /// Set on SIGTERM: stop admitting; running jobs end after their current segment.
    pub drain: tokio::sync::watch::Receiver<bool>,
    pub metrics: metrics::Metrics,
}

#[derive(Clone)]
struct Svc(Arc<State>);

type RunStream = Pin<Box<dyn Stream<Item = Result<ServerMsg, Status>> + Send>>;

#[tonic::async_trait]
impl Worker for Svc {
    async fn hello(&self, _: Request<HelloRequest>) -> Result<Response<Caps>, Status> {
        let s = &self.0;
        let (used, cap) = s.usage.snapshot();
        let (batch_used, _) = s.usage.batch_snapshot();
        Ok(Response::new(Caps {
            name: s.cfg.name.clone(),
            kind: s.cfg.backend.as_str().into(),
            outputs: s.probed.outputs.clone(),
            gpu_tonemap: s.probed.gpu_tonemap,
            capacity: cap,
            units_used: used,
            active_jobs: s.usage.jobs(),
            ffmpeg_version: s.probed.ffmpeg_version.clone(),
            agent_version: env!("CARGO_PKG_VERSION").into(),
            probed_unix: s.probed_unix,
            node: s.cfg.node.clone(),
            batch_units_used: batch_used,
        }))
    }

    type RunStream = RunStream;

    async fn run(&self, req: Request<Streaming<ClientMsg>>) -> Result<Response<RunStream>, Status> {
        let mut inbound = req.into_inner();
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no job"))??;
        let job = match first.and_then(|m| m.msg) {
            Some(client_msg::Msg::Job(j)) => j,
            _ => return Err(Status::invalid_argument("first message must be a job")),
        };
        // The security boundary: only Jellyfin's HLS transcode or trickplay shapes may run here.
        // `shape` (not `job.priority`, which is client-asserted) decides BATCH admission below,
        // in job.rs -- it's derived from the argv itself and already validated here.
        let mapped: Vec<String> = job
            .args
            .iter()
            .map(|a| tcpool_ir::map_path(a, &self.0.cfg.pathmap))
            .collect();
        let shape = tcpool_ir::classify(&mapped);
        if matches!(shape, tcpool_ir::Shape::Other) {
            let e = "unrecognized command shape".to_string();
            log(format_args!("REFUSED a job: {e}"));
            self.0.metrics.inc(metrics::Outcome::RefusedPolicy);
            return Err(Status::invalid_argument(e));
        }
        if let Err(e) = tcpool_ir::validate::validate(&mapped, &self.0.cfg.policy, shape) {
            log(format_args!("REFUSED a job: {e}"));
            self.0.metrics.inc(metrics::Outcome::RefusedPolicy);
            return Err(Status::permission_denied(e));
        }
        let (tx, rx) = mpsc::channel(128);
        tokio::spawn(job::run_job(self.0.clone(), job, shape, inbound, tx));
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

#[tokio::main]
async fn main() {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tcpool-agent: {e}");
            std::process::exit(2);
        }
    };
    // CUDA JIT cache: the worker's HOME does not exist in the pod, so without this every ffmpeg
    // recompiled its CUDA kernels (~12 s on the P4, MEASURED). Warmed by the startup probe.
    if std::env::var_os("CUDA_CACHE_PATH").is_none() {
        std::env::set_var("CUDA_CACHE_PATH", "/tmp/cuda-cache");
    }
    // Probe before serving: not ready until the card is characterised.
    let probed = probe::probe(&cfg).await;
    let probed_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    log(format_args!(
        "[{}/{}] listening on {addr} ffmpeg={} {} outputs={:?} gpu_tonemap={} capacity={} (4K={} 1440={} copy={})",
        cfg.name, cfg.backend, cfg.ffmpeg, probed.ffmpeg_version, probed.outputs, probed.gpu_tonemap,
        cfg.capacity, cfg.weight_4k, cfg.weight_1440, cfg.weight_copy
    ));
    let capacity = cfg.capacity;
    let batch_headroom = cfg.batch_headroom;
    let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);
    let state = Arc::new(State {
        cfg,
        probed,
        usage: Usage::new(capacity, batch_headroom),
        probed_unix,
        drain: drain_rx,
        metrics: metrics::Metrics::default(),
    });

    if let Some(port) = state.cfg.metrics_port {
        let m = state.clone();
        tokio::spawn(async move { metrics::serve(m, port).await });
    }

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter.set_serving::<WorkerServer<Svc>>().await;
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    let tls = match tcpool_proto::tls::TlsFiles::from_env() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tcpool-agent: {e}");
            std::process::exit(2);
        }
    };
    // kubelet's gRPC probes cannot speak TLS: health (only) on a separate plaintext port.
    // Health is always served on the main port. With TLS, kubelet's gRPC probe can't reach it
    // there, so it is ALSO served in plaintext on TC_HEALTH_PORT (default 9902 when TLS is on).
    // Without TLS no second port is opened unless asked for: several local agents on one host
    // would otherwise collide on a fixed default (MEASURED in the protocol suite).
    let health_port = std::env::var("TC_HEALTH_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .or(if tls.is_some() { Some(9902) } else { None });
    if let Some(health_port) = health_port {
        let hs = health_service.clone();
        tokio::spawn(async move {
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], health_port));
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(hs)
                .serve(addr)
                .await
            {
                eprintln!("tcpool-agent: health server on :{health_port}: {e}");
                std::process::exit(1);
            }
        });
    }
    // cert-manager rotates the certificate in place: drain (like SIGTERM) and let the container
    // restart with the new one, so rotation never cuts a session.
    let (rotate_tx, mut rotate_rx) = tokio::sync::watch::channel(false);
    if let Some(files) = tls.clone() {
        let first = files.modified();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if files.modified() != first {
                    log(format_args!(
                        "TLS certificate changed on disk; draining to restart with it"
                    ));
                    let _ = rotate_tx.send(true);
                    return;
                }
            }
        });
    }

    // Graceful drain: stop admitting and report not-ready at once, let each running job finish
    // its current segment (job.rs ends it there, Jellyfin restarts it elsewhere), then exit.
    // Bounded well inside terminationGracePeriodSeconds.
    let drain_state = state.clone();
    let shutdown = async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm");
        tokio::select! {
            _ = term.recv() => {}
            _ = rotate_rx.changed() => {}
        }
        log(format_args!(
            "SIGTERM: draining {} job(s)",
            drain_state.usage.jobs()
        ));
        let _ = drain_tx.send(true);
        health_reporter
            .set_service_status("", tonic_health::ServingStatus::NotServing)
            .await;
        health_reporter.set_not_serving::<WorkerServer<Svc>>().await;
        let started = std::time::Instant::now();
        while drain_state.usage.jobs() > 0 && started.elapsed() < std::time::Duration::from_secs(12)
        {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        // let the final exit messages reach the shims
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        log(format_args!(
            "drained in {:.1}s; stopping",
            started.elapsed().as_secs_f64()
        ));
        // Every job has ended; don't let HTTP/2 graceful shutdown of idle client connections
        // stretch past terminationGracePeriodSeconds (MEASURED +8 s without this).
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            std::process::exit(0);
        });
    };
    let mut builder = tonic::transport::Server::builder();
    if let Some(files) = &tls {
        let cfg = match files.server_config() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("tcpool-agent: tls: {e}");
                std::process::exit(2);
            }
        };
        builder = match builder.tls_config(cfg) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("tcpool-agent: tls: {e}");
                std::process::exit(2);
            }
        };
        log(format_args!(
            "mTLS on: clients must present a certificate from the pool CA"
        ));
    } else {
        log(format_args!(
            "WARNING plaintext gRPC (no TC_TLS_*): lab/dev only"
        ));
    }
    if let Err(e) = builder
        .http2_keepalive_interval(Some(std::time::Duration::from_secs(2)))
        .http2_keepalive_timeout(Some(std::time::Duration::from_secs(4)))
        .add_service(health_service)
        .add_service(WorkerServer::new(Svc(state)))
        .serve_with_shutdown(addr, shutdown)
        .await
    {
        eprintln!("tcpool-agent: server: {e}");
        std::process::exit(1);
    }
}
