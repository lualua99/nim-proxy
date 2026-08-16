//! Key pool: one "lane" per NIM API key, each with an exact sliding-window
//! rate limiter (N requests per rolling 60s). NIM enforces ~40 RPM per key,
//! so a sliding window matches its semantics better than a token bucket
//! (which would allow a double-sized burst inside a single minute).
//!
//! The pool is immutable once built; settings changes build a replacement via
//! [`Pool::rebuild`] and swap it into the shared [`PoolHandle`]. The dispatcher
//! is the only `reserve` caller and holds the handle's read lock across each
//! reserve, so a rebuild (under the write lock) can never interleave with a
//! grant — a kept key's in-window timestamps carry over exactly once.
//!
//! Disabled keys stay in the pool as inactive *state carriers* (never
//! granted, invisible to `len`/capacity/stats — they sit past the `active`
//! boundary). They exist so a disable→enable cycle can't reset the window;
//! a re-enabled key comes back warm instead of double-spending the
//! upstream's window. The same carry-over keeps a rebuilt pool's
//! calibration memory for a key that was only re-configured.
//!
//! ## Calibration
//!
//! NIM's ~40 RPM is a soft, account/load-dependent ceiling, so the
//! configured `rpm` is only a starting guess. Each lane carries a
//! [`Calibration`] that learns the *measured* ceiling from upstream
//! rejections ([`Pool::observe`]) and shrinks admission ahead of the next
//! 429. The factor only ever shrinks quickly and heals slowly (see
//! [`Calibration::maybe_probe`]), so the zero-violation posture — never
//! exceed the window the upstream actually grants — is preserved by
//! construction.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use metrics::gauge;

/// Shared, swappable pool: readers snapshot an `Arc<Pool>`; the settings
/// layer swaps in a rebuilt pool under the write lock.
pub type PoolHandle = Arc<RwLock<Arc<Pool>>>;

/// NIM's rolling window is 60s; the extra second is a delivery-jitter safety
/// margin. We reserve slots at grant time but the upstream clocks arrivals,
/// so a boundary-timed request whose predecessor was delayed more than it can
/// land inside the upstream's window even though it left ours. Load-tested at
/// 100 concurrent clients: with 60s exactly, ~2% of requests tripped a strict
/// upstream window; with the pad, zero. Costs ~1.6% peak throughput.
const WINDOW: Duration = Duration::from_secs(61);

/// How much of the configured `rpm` a lane may use after upstream
/// rejections. Starts at 1.0 (trust the config) and decays on observed
/// 429s/5xxs; `maybe_probe` heals it upward slowly.
const CAL_INITIAL: f64 = 1.0;
/// Floor for the calibration factor: even a badly-locked lane keeps this
/// share of its configured budget, so a key that briefly misbehaves isn't
/// quashed to 0 forever.
const CAL_FLOOR: f64 = 0.2;
/// Multiplicative decay applied per rejection event (hang/short backoff).
const CAL_DECAY: f64 = 0.9;
/// Extra penalty applied after the lockout signature: 2+ rejections with
/// `Retry-After >= 30s` (NIM's exponential-lockout shape). Abrupt and
/// severe, because such lanes are in cooldown anyway.
const CAL_LOCKOUT_FACTOR: f64 = 0.5;
/// Floor for a lockout-flagged lane (severe decay on top of the ordinary one).
const CAL_LOCKOUT_FLOOR: f64 = 0.1;
/// `Retry-After` (s) that counts as "locked out" when seen repeatedly.
const LOCKOUT_RETRY_AFTER: u64 = 30;

/// How long a lane must stay quiet before the calibration factor climbs a
/// step. Deliberately long: recovery must not race the upstream's own
/// account-level view, which can stay throttled for tens of minutes.
static PROBE_INTERVAL: LazyLock<Duration> = LazyLock::new(|| {
    Duration::from_secs(
        std::env::var("NIMPROXY_CALIBRATION_PROBE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600),
    )
});
/// The step size for [`Calibration::maybe_probe`].
const PROBE_STEP: f64 = 0.01;

/// What the lane observed upstream, for calibration. Deliberately typed:
/// a 429 and a 503 are different *reasons* to shrink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Upstream rate-limit: `Retry-After` seconds when the response carried
    /// it (NIM's lockout runway). 5xx and connect errors also carry a
    /// Either value as a conservative miss.
    RateLimited { retry_after: Option<u64> },
    /// A 5xx that was retried. Same shrink, no lockout escalation.
    ServerError,
}

/// Per-lane long-term capacity memory: a factor on the configured `rpm`
/// learned from upstream rejections. Not synchronized internally — the
/// caller holds the lane's calibration lock.
#[derive(Clone, Debug)]
pub struct Calibration {
    factor: f64,
    /// Protocol-level lockout seen at least once recently.
    locked: bool,
    /// Wall-clock of the most recent observation (for probe gating).
    last_event: Option<Instant>,
    /// Consecutive `Retry-After`-carrying rate-limit events.
    consecutive_locks: u32,
}

impl Calibration {
    fn new() -> Self {
        Self {
            factor: CAL_INITIAL,
            locked: false,
            last_event: None,
            consecutive_locks: 0,
        }
    }

    /// Restore a persisted factor, clamped to the legal operating range (a
    /// resumed lane never starts sharper than its lockout floor).
    fn from_factor(factor: f64) -> Self {
        Self {
            factor: factor.clamp(CAL_LOCKOUT_FLOOR, 1.0),
            locked: false,
            last_event: None,
            consecutive_locks: 0,
        }
    }

    /// Apply an observed upstream rejection to the calibration S-shaped.
    /// `locked` escalates after the lockout signature (2+ rate-limits with
    /// Retry-After >= 30s): such lanes are in exponential backoff upstream,
    /// so admission is cut far below the floor until clean probes restore.
    fn observe(&mut self, signal: Signal, now: Instant) {
        self.last_event = Some(now);
        match signal {
            Signal::RateLimited { retry_after } => {
                let lockout = retry_after
                    .map(|s| s >= LOCKOUT_RETRY_AFTER)
                    .unwrap_or(false);
                self.consecutive_locks = if lockout {
                    self.consecutive_locks + 1
                } else {
                    0
                };
                if self.consecutive_locks >= 2 {
                    self.locked = true;
                    self.consecutive_locks = 0;
                    // Severe: the upstream is in its exponential-lockout
                    // runway, so cut far below the ordinary floor.
                    self.factor *= CAL_LOCKOUT_FACTOR;
                }
                self.factor *= CAL_DECAY;
            }
            Signal::ServerError => {
                self.consecutive_locks = 0;
                self.factor *= CAL_DECAY;
            }
        }
        self.factor = self.factor.max(if self.locked {
            CAL_LOCKOUT_FLOOR
        } else {
            CAL_FLOOR
        });
    }

    /// Heal toward 1.0, at most one `PROBE_STEP` per PROBE_INTERVAL of
    /// silence. Called on every reserve while the lane has capacity, so an
    /// idle lane simply re-learns its true ceiling after the upstream has
    /// relaxed. Returns true when the factor actually changed (so the gauge
    /// can be republished).
    fn maybe_probe(&mut self, now: Instant) -> bool {
        if self.factor >= 1.0 {
            return false;
        }
        if let Some(last) = self.last_event {
            if now.duration_since(last) >= *PROBE_INTERVAL {
                self.factor = (self.factor + PROBE_STEP).min(1.0);
                // A probe is an "event" too — the next step needs another
                // full interval of quiet, not a fast re-probe.
                self.last_event = Some(now);
                self.locked = false;
                return true;
            }
        }
        false
    }

    /// Configured `rpm` scaled by the factor, never 0 for an enabled lane
    /// (0 would deadlock `reserve`'s window math).
    fn effective_rpm(&self, rpm: usize) -> usize {
        (rpm as f64 * self.factor).floor().max(1.0) as usize
    }
}

/// A lane blueprint. Disabled specs become state carriers: held for their
/// rate state, never granted.
pub struct LaneSpec {
    pub key: String,
    pub rpm: usize,
    pub enabled: bool,
}

struct Lane {
    key: String,
    /// This key's requests-per-minute budget (keys can differ: paid tiers,
    /// self-hosted NIM). Configured value — the *effective* budget for
    /// admission is `cal.effective_rpm(rpm)`.
    rpm: usize,
    /// Timestamps of requests sent within the last WINDOW.
    sent: Mutex<VecDeque<Instant>>,
    /// Lane is benched until this instant (set after an upstream 429/5xx).
    cooldown_until: Mutex<Instant>,
    /// Long-term capacity memory (calibration factor). Locked together
    /// with nothing else; `observe`/`maybe_probe` touch only this.
    calibration: Mutex<Calibration>,
}

/// One lane's restorable rate state (in-window sends, bench deadline,
/// calibration memory) — the shape the persistence layer writes and
/// [`Pool::restore`] rebuilds from.
pub struct LaneState {
    pub key: String,
    pub rpm: usize,
    /// Sends still inside the rolling window at snapshot time (pruned).
    pub sent: VecDeque<Instant>,
    pub cooldown_until: Instant,
    /// Calibration factor memory (0.0..=1.0 share of `rpm`).
    pub factor: f64,
}

pub struct Pool {
    /// Enabled lanes first (indexes 0..active — the only ones ever granted,
    /// counted, or reported), disabled state carriers after.
    lanes: Vec<Lane>,
    active: usize,
    /// Recovery slow-start: until this instant the admissible budget is
    /// `rpm * ramp_factor` instead of the calibration-learned one. Armed at
    /// boot when fresh rate state was restored; carried across rebuilds so a
    /// settings change mid-ramp can't end it early.
    ramp_until: Instant,
    ramp_factor: f64,
}

/// One lane's live state (see [`Pool::lane_stats`]).
pub struct LaneStat {
    pub key: String,
    pub rpm: usize,
    pub in_window: usize,
    pub cooldown_ms: u64,
    /// The lane's current calibration factor (0.0..=1.0): the share of
    /// `rpm` it actually admits per minute.
    pub cal_factor: f64,
}

pub enum Reservation {
    /// Slot reserved; send the request with this key. `stamp` identifies the
    /// reservation so an unused slot can be returned via [`Pool::release`].
    Ready {
        lane: usize,
        key: String,
        stamp: Instant,
        /// True when the caller's preferred lane won (conversation affinity hit).
        sticky: bool,
    },
    /// All lanes busy; soonest a slot frees up.
    Wait(Duration),
}

impl Pool {
    pub fn new(specs: Vec<LaneSpec>) -> Self {
        Self::assemble(specs, None)
    }

    /// Build a replacement pool from `specs`, carrying over the in-window
    /// timestamps and cooldown of every key kept from `self` (matched by key
    /// string, enabled or carrier). A kept key can never be double-spent
    /// across a swap; a lowered rpm is honored immediately (`try_take`
    /// checks the live count); a disabled key re-enables warm because its
    /// carrier lane kept the window.
    pub fn rebuild(&self, specs: Vec<LaneSpec>) -> Self {
        Self::assemble(specs, Some(self))
    }

    /// Build a pool from persisted rate state ([`Pool::lane_states`] on the
    /// previous run), matched by key — the same carry-over contract as
    /// [`Pool::rebuild`], sourced from disk instead of a live pool. Keys
    /// without persisted state start fresh. `ramp_until`/`ramp_factor` arm
    /// the recovery slow-start; pass `Instant::now()`, `1.0` to leave it off.
    pub fn restore(
        specs: Vec<LaneSpec>,
        states: &[LaneState],
        ramp_until: Instant,
        ramp_factor: f64,
    ) -> Self {
        let mut specs = specs;
        specs.sort_by_key(|s| !s.enabled);
        let active = specs.iter().filter(|s| s.enabled).count();
        let now = Instant::now();
        let lanes = specs
            .into_iter()
            .map(|s| match states.iter().find(|st| st.key == s.key) {
                Some(st) => Lane {
                    sent: Mutex::new(st.sent.clone()),
                    cooldown_until: Mutex::new(st.cooldown_until),
                    calibration: Mutex::new(Calibration::from_factor(st.factor)),
                    key: s.key,
                    rpm: s.rpm,
                },
                None => Lane {
                    key: s.key,
                    rpm: s.rpm,
                    sent: Mutex::new(VecDeque::new()),
                    cooldown_until: Mutex::new(now),
                    calibration: Mutex::new(Calibration::new()),
                },
            })
            .collect();
        Self {
            lanes,
            active,
            ramp_until,
            ramp_factor,
        }
    }

    /// The slow-start budget: `rpm * ramp_factor`, never 0 (a dead lane
    /// would hang the dispatcher's window math).
    fn ramp_budget(&self, lane: &Lane) -> usize {
        (lane.rpm as f64 * self.ramp_factor).floor().max(1.0) as usize
    }

    fn assemble(mut specs: Vec<LaneSpec>, old: Option<&Pool>) -> Self {
        // Enabled lanes first (stable — preserves relative order), carriers
        // after, so index-based semantics only ever see enabled lanes.
        specs.sort_by_key(|s| !s.enabled);
        let active = specs.iter().filter(|s| s.enabled).count();
        let now = Instant::now();
        let lanes = specs
            .into_iter()
            .map(
                |s| match old.and_then(|o| o.lanes.iter().find(|l| l.key == s.key)) {
                    Some(prev) => Lane {
                        sent: Mutex::new(prev.sent.lock().unwrap().clone()),
                        cooldown_until: Mutex::new(*prev.cooldown_until.lock().unwrap()),
                        calibration: Mutex::new(prev.calibration.lock().unwrap().clone()),
                        key: s.key,
                        rpm: s.rpm,
                    },
                    None => Lane {
                        key: s.key,
                        rpm: s.rpm,
                        sent: Mutex::new(VecDeque::new()),
                        cooldown_until: Mutex::new(now),
                        calibration: Mutex::new(Calibration::new()),
                    },
                },
            )
            .collect();
        // The ramp horizon belongs to the pool generation: armed by a
        // restore, inherited by every rebuild of that generation.
        let (ramp_until, ramp_factor) = match old {
            Some(o) => (o.ramp_until, o.ramp_factor),
            None => (now, 1.0),
        };
        Self {
            lanes,
            active,
            ramp_until,
            ramp_factor,
        }
    }

    /// Enabled lanes only — carriers are invisible everywhere.
    pub fn len(&self) -> usize {
        self.active
    }

    /// Aggregate requests-per-minute across enabled lanes.
    pub fn capacity_rpm(&self) -> usize {
        self.lanes[..self.active].iter().map(|l| l.rpm).sum()
    }

    /// Per-lane rpm budgets, in lane order (feeds the dashboard config).
    pub fn rpms(&self) -> Vec<usize> {
        self.lanes[..self.active].iter().map(|l| l.rpm).collect()
    }

    /// Point-in-time per-lane view for the Settings key rows.
    pub fn lane_stats(&self) -> Vec<LaneStat> {
        let now = Instant::now();
        self.lanes[..self.active]
            .iter()
            .map(|l| {
                let in_window = {
                    let sent = l.sent.lock().unwrap();
                    sent.iter().filter(|t| now - **t < WINDOW).count()
                };
                let cooldown_ms = l
                    .cooldown_until
                    .lock()
                    .unwrap()
                    .saturating_duration_since(now)
                    .as_millis() as u64;
                LaneStat {
                    key: l.key.clone(),
                    rpm: l.rpm,
                    in_window,
                    cooldown_ms,
                    cal_factor: l.calibration.lock().unwrap().factor,
                }
            })
            .collect()
    }

    /// Take a slot on lane `i` if it has capacity right now. Reserving
    /// records the send timestamp immediately, so concurrent callers can't
    /// oversubscribe a lane.
    fn try_take(&self, i: usize, now: Instant, sticky: bool) -> Option<Reservation> {
        let lane = &self.lanes[i];
        if *lane.cooldown_until.lock().unwrap() > now {
            return None;
        }
        let effective = if now < self.ramp_until {
            self.ramp_budget(lane)
        } else {
            lane.calibration.lock().unwrap().effective_rpm(lane.rpm)
        };
        let mut sent = lane.sent.lock().unwrap();
        while sent.front().is_some_and(|t| now - *t >= WINDOW) {
            sent.pop_front();
        }
        if sent.len() < effective {
            sent.push_back(now);
            Some(Reservation::Ready {
                lane: i,
                key: lane.key.clone(),
                stamp: now,
                sticky,
            })
        } else {
            None
        }
    }

    /// Try to reserve a request slot. `prefer` pins a conversation to one
    /// lane while it has capacity (keeping any upstream prefix cache warm on
    /// a single key); otherwise the least-loaded ready lane wins, spreading
    /// concurrent in-flight requests evenly across keys. An out-of-range
    /// `prefer` (computed against a pool that has since shrunk) is ignored.
    ///
    /// Each visit is also a chance to heal the lane's calibration factor
    /// ([`Calibration::maybe_probe`]): a lane that has been quiet for a full
    /// probe interval and has capacity climbs toward its configured ceiling.
    pub fn reserve(&self, prefer: Option<usize>) -> Reservation {
        let now = Instant::now();
        if let Some(p) = prefer.filter(|&p| p < self.active) {
            if self.lanes[p].calibration.lock().unwrap().maybe_probe(now) {
                gauge!("nimproxy_lane_calibration", "lane" => p.to_string())
                    .set(self.lanes[p].calibration.lock().unwrap().factor);
            }
            if let Some(r) = self.try_take(p, now, true) {
                return r;
            }
        }

        let mut ready: Vec<(usize, usize)> = Vec::new(); // (in-window load, lane)
        let mut best_wait = WINDOW;
        for (i, lane) in self.lanes[..self.active].iter().enumerate() {
            let cooldown = *lane.cooldown_until.lock().unwrap();
            let effective = {
                let mut cal = lane.calibration.lock().unwrap();
                let probed = cal.maybe_probe(now);
                if probed {
                    gauge!("nimproxy_lane_calibration", "lane" => i.to_string()).set(cal.factor);
                }
                if now < self.ramp_until {
                    self.ramp_budget(lane)
                } else {
                    cal.effective_rpm(lane.rpm)
                }
            };
            let mut sent = lane.sent.lock().unwrap();
            while sent.front().is_some_and(|t| now - *t >= WINDOW) {
                sent.pop_front();
            }
            let window_ready = if sent.len() < effective {
                now
            } else if effective == 0 {
                // never happens (effective_rpm floors at 1), but a
                // panic here would kill the dispatcher task and hang
                // every request — never index in.
                now + WINDOW
            } else {
                sent[sent.len() - effective] + WINDOW
            };
            let ready_at = window_ready.max(cooldown);
            if ready_at <= now {
                ready.push((sent.len(), i));
            } else {
                best_wait = best_wait.min(ready_at - now);
            }
        }
        ready.sort_unstable();
        for (_, i) in ready {
            if let Some(r) = self.try_take(i, now, false) {
                return r;
            }
        }
        Reservation::Wait(best_wait)
    }

    /// Return a reserved slot that was never spent on an upstream request
    /// (e.g. the client hung up while queued).
    pub fn release(&self, lane: usize, stamp: Instant) {
        let mut sent = self.lanes[lane].sent.lock().unwrap();
        if let Some(pos) = sent.iter().rposition(|t| *t == stamp) {
            sent.remove(pos);
        }
    }

    /// Bench a lane after the upstream told us to back off.
    pub fn penalize(&self, lane: usize, backoff: Duration) {
        let until = Instant::now() + backoff;
        let mut cd = self.lanes[lane].cooldown_until.lock().unwrap();
        if *cd < until {
            *cd = until;
        }
    }

    /// Feed an upstream rejection into the lane's long-term calibration.
    /// Short-term backoff (cooldown) is `penalize`'s job; this is the
    /// long-term memory that reshapes admission ahead of the next 429.
    pub fn observe(&self, lane: usize, signal: Signal) {
        let now = Instant::now();
        let factor = {
            let mut cal = self.lanes[lane].calibration.lock().unwrap();
            cal.observe(signal, now);
            cal.factor
        };
        gauge!("nimproxy_lane_calibration", "lane" => lane.to_string()).set(factor);
    }

    /// The per-lane calibration factors (enabled lanes, in lane order) —
    /// feeds the dashboard's measured-capacity meter.
    pub fn calibration_factors(&self) -> Vec<f64> {
        self.lanes[..self.active]
            .iter()
            .map(|l| l.calibration.lock().unwrap().factor)
            .collect()
    }

    /// All lanes (enabled + carriers), windows pruned to the rolling
    /// horizon — the shape the persistence layer writes.
    pub fn lane_states(&self) -> Vec<LaneState> {
        let now = Instant::now();
        self.lanes
            .iter()
            .map(|l| {
                let mut sent = l.sent.lock().unwrap();
                while sent.front().is_some_and(|t| now - *t >= WINDOW) {
                    sent.pop_front();
                }
                LaneState {
                    key: l.key.clone(),
                    rpm: l.rpm,
                    sent: sent.clone(),
                    cooldown_until: *l.cooldown_until.lock().unwrap(),
                    factor: l.calibration.lock().unwrap().factor,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(key: &str, rpm: usize, enabled: bool) -> LaneSpec {
        LaneSpec {
            key: key.into(),
            rpm,
            enabled,
        }
    }

    fn keys(n: usize, rpm: usize) -> Vec<LaneSpec> {
        (0..n)
            .map(|i| spec(&format!("key{i}"), rpm, true))
            .collect()
    }

    fn take(pool: &Pool, prefer: Option<usize>) -> usize {
        match pool.reserve(prefer) {
            Reservation::Ready { lane, .. } => lane,
            Reservation::Wait(_) => panic!("expected Ready"),
        }
    }

    #[test]
    fn spreads_load_across_lanes_then_waits() {
        let pool = Pool::new(keys(2, 1));
        assert_eq!(take(&pool, None), 0);
        assert_eq!(take(&pool, None), 1);
        // Both lanes at their 1-per-minute cap: caller must wait ~60s.
        match pool.reserve(None) {
            Reservation::Wait(w) => assert!(w > Duration::from_secs(55) && w <= WINDOW),
            _ => panic!("expected Wait"),
        }
    }

    #[test]
    fn burst_lands_on_least_loaded_lane() {
        let pool = Pool::new(keys(3, 10));
        let mut per_lane = [0usize; 3];
        for _ in 0..9 {
            per_lane[take(&pool, None)] += 1;
        }
        assert_eq!(per_lane, [3, 3, 3]);
    }

    #[test]
    fn per_lane_rpm_budgets_are_honored() {
        // Lane 0 allows 1/min, lane 1 allows 3/min: four grants total.
        let pool = Pool::new(vec![spec("small", 1, true), spec("big", 3, true)]);
        let mut per_lane = [0usize; 2];
        for _ in 0..4 {
            per_lane[take(&pool, None)] += 1;
        }
        assert_eq!(per_lane, [1, 3]);
        assert!(matches!(pool.reserve(None), Reservation::Wait(_)));
        assert_eq!(pool.capacity_rpm(), 4);
        assert_eq!(pool.rpms(), vec![1, 3]);
    }

    #[test]
    fn sticky_lane_wins_until_full_then_spills_over() {
        let pool = Pool::new(keys(2, 2));
        assert_eq!(take(&pool, Some(1)), 1);
        assert_eq!(take(&pool, Some(1)), 1);
        // Preferred lane is at capacity: spill to the other lane.
        assert_eq!(take(&pool, Some(1)), 0);
    }

    #[test]
    fn sticky_flag_reports_affinity_outcome() {
        let pool = Pool::new(keys(2, 1));
        match pool.reserve(Some(0)) {
            Reservation::Ready {
                lane: 0,
                sticky: true,
                ..
            } => {}
            _ => panic!("expected sticky hit on lane 0"),
        }
        match pool.reserve(Some(0)) {
            Reservation::Ready {
                lane: 1,
                sticky: false,
                ..
            } => {}
            _ => panic!("expected spill to lane 1"),
        }
    }

    #[test]
    fn out_of_range_prefer_is_ignored() {
        // An affinity index computed against a bigger, since-replaced pool
        // must not panic — it just falls through to least-loaded.
        let pool = Pool::new(keys(1, 2));
        assert_eq!(take(&pool, Some(7)), 0);
    }

    #[test]
    fn released_slot_becomes_available_again() {
        let pool = Pool::new(keys(1, 1));
        let Reservation::Ready { lane, stamp, .. } = pool.reserve(None) else {
            panic!("expected Ready");
        };
        assert!(matches!(pool.reserve(None), Reservation::Wait(_)));
        pool.release(lane, stamp);
        assert!(matches!(pool.reserve(None), Reservation::Ready { .. }));
    }

    #[test]
    fn penalized_lane_is_skipped() {
        let pool = Pool::new(keys(2, 10));
        pool.penalize(0, Duration::from_secs(30));
        match pool.reserve(None) {
            Reservation::Ready { lane, key, .. } => {
                assert_eq!(lane, 1);
                assert_eq!(key, "key1");
            }
            _ => panic!("expected Ready on lane 1"),
        }
    }

    #[test]
    fn all_lanes_penalized_reports_soonest_recovery() {
        let pool = Pool::new(keys(2, 10));
        pool.penalize(0, Duration::from_secs(30));
        pool.penalize(1, Duration::from_secs(5));
        match pool.reserve(None) {
            Reservation::Wait(w) => assert!(w <= Duration::from_secs(5)),
            _ => panic!("expected Wait"),
        }
    }

    #[test]
    fn rebuild_carries_window_state_for_kept_keys() {
        // A slot spent before the swap counts against the key after it: the
        // same key can never be double-spent across a rebuild.
        let pool = Pool::new(keys(1, 1));
        take(&pool, None);
        let rebuilt = pool.rebuild(keys(1, 1));
        assert!(matches!(rebuilt.reserve(None), Reservation::Wait(_)));
    }

    #[test]
    fn rebuild_carries_cooldown_for_kept_keys() {
        let pool = Pool::new(keys(2, 10));
        pool.penalize(0, Duration::from_secs(30));
        let rebuilt = pool.rebuild(keys(2, 10));
        assert_eq!(take(&rebuilt, Some(0)), 1, "benched lane stays benched");
    }

    #[test]
    fn rebuild_new_key_starts_fresh_and_removed_key_is_gone() {
        let pool = Pool::new(vec![spec("old", 1, true)]);
        take(&pool, None);
        let rebuilt = pool.rebuild(vec![spec("new", 1, true)]);
        assert_eq!(rebuilt.len(), 1);
        match rebuilt.reserve(None) {
            Reservation::Ready { key, .. } => assert_eq!(key, "new"),
            _ => panic!("fresh key should be ready"),
        }
    }

    #[test]
    fn disabled_lanes_are_carriers_never_granted_never_counted() {
        let pool = Pool::new(vec![spec("on", 2, true), spec("off", 40, false)]);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.capacity_rpm(), 2);
        assert_eq!(pool.rpms(), vec![2]);
        assert_eq!(pool.lane_stats().len(), 1);
        for _ in 0..2 {
            match pool.reserve(None) {
                Reservation::Ready { key, .. } => assert_eq!(key, "on"),
                _ => panic!("enabled lane should grant"),
            }
        }
        // Capacity spent: the carrier must not pick up the slack.
        assert!(matches!(pool.reserve(None), Reservation::Wait(_)));
    }

    #[test]
    fn disable_enable_cycle_cannot_double_spend_the_window() {
        // The exact sequence that produced real upstream 429s in the load
        // test: spend the key, disable it (rebuild 1), re-enable it
        // (rebuild 2). The carrier lane keeps the window across both.
        let pool = Pool::new(vec![spec("k", 1, true)]);
        take(&pool, None);
        let disabled = pool.rebuild(vec![spec("k", 1, false)]);
        assert!(matches!(disabled.reserve(None), Reservation::Wait(_)));
        let re_enabled = disabled.rebuild(vec![spec("k", 1, true)]);
        assert!(
            matches!(re_enabled.reserve(None), Reservation::Wait(_)),
            "the pre-disable send must still count against the window"
        );
    }

    #[test]
    fn rebuild_honors_lowered_and_raised_rpm() {
        // Two spent on rpm=2; lowering to 1 means no capacity until the
        // window drains — the live count check does this for free.
        let pool = Pool::new(keys(1, 2));
        take(&pool, None);
        take(&pool, None);
        let lowered = pool.rebuild(keys(1, 1));
        assert!(matches!(lowered.reserve(None), Reservation::Wait(_)));
        // Raising grants the extra headroom immediately.
        let raised = pool.rebuild(keys(1, 3));
        assert!(matches!(raised.reserve(None), Reservation::Ready { .. }));
    }

    #[test]
    fn observe_rate_limit_shrinks_admission() {
        // rpm=10, one 429 with Retry-After=5: the lane admits ~9/min now.
        let pool = Pool::new(keys(1, 10));
        pool.observe(
            0,
            Signal::RateLimited {
                retry_after: Some(5),
            },
        );
        let mut granted = 0;
        while let Reservation::Ready { .. } = pool.reserve(None) {
            granted += 1;
        }
        assert_eq!(granted, 9);
    }

    #[test]
    fn lockout_signature_cuts_far_below_floor() {
        // Repeated 429s with Retry-After >= 30s = exponential-lockout
        // shape: each pair triggers the severe factor, so after a few
        // lockouts the lane lands below the ordinary floor — but keeps a
        // minimal budget (never dead-locked).
        let pool = Pool::new(keys(1, 10));
        for _ in 0..4 {
            pool.observe(
                0,
                Signal::RateLimited {
                    retry_after: Some(30),
                },
            );
        }
        let eff = pool.lane_stats()[0].cal_factor;
        assert!(
            eff < CAL_FLOOR,
            "locked lane should sit under the floor: {eff}"
        );
        assert!(eff >= CAL_LOCKOUT_FLOOR);
        // Effective must keep a minimal budget, not zero: one grant stays.
        assert!(matches!(pool.reserve(None), Reservation::Ready { .. }));
        assert!(matches!(pool.reserve(None), Reservation::Wait(_)));
    }

    #[test]
    fn server_error_shrinks_without_lockout() {
        let pool = Pool::new(keys(1, 10));
        pool.observe(0, Signal::ServerError);
        let eff = pool.lane_stats()[0].cal_factor;
        assert!((0.2..1.0).contains(&eff));
        assert_eq!((10.0 * eff).floor().max(1.0) as usize, 9);
        // No lockout flag, so on the 1-min scale a single 503 costs ~10%.
        assert_eq!((10.0 * eff).floor() as usize, 9);
    }

    #[test]
    fn rebuild_carries_calibration_for_kept_keys() {
        // A key that misbehaved keeps its shrunken factor across a rebuild
        // (yes — the same one that carries the window and cooldown).
        let pool = Pool::new(keys(1, 10));
        pool.observe(
            0,
            Signal::RateLimited {
                retry_after: Some(5),
            },
        );
        let before = pool.lane_stats()[0].cal_factor;
        let rebuilt = pool.rebuild(keys(1, 10));
        assert!((rebuilt.lane_stats()[0].cal_factor - before).abs() < 1e-9);
    }

    #[test]
    fn restore_carries_window_state_and_factor() {
        let pool = Pool::new(keys(1, 1));
        take(&pool, None);
        pool.observe(
            0,
            Signal::RateLimited {
                retry_after: Some(5),
            },
        );
        let states = pool.lane_states();
        assert_eq!(states[0].sent.len(), 1, "spent slot survives the snapshot");
        assert!(states[0].factor < 1.0);
        let restored = Pool::restore(keys(1, 1), &states, Instant::now(), 1.0);
        assert!(
            matches!(restored.reserve(None), Reservation::Wait(_)),
            "a restored spend still counts against the window"
        );
        let stats = restored.lane_stats();
        assert!((stats[0].cal_factor - pool.lane_stats()[0].cal_factor).abs() < 1e-9);
    }

    #[test]
    fn restore_keeps_a_persisted_bench() {
        let states = vec![LaneState {
            key: "k".into(),
            rpm: 10,
            sent: VecDeque::new(),
            cooldown_until: Instant::now() + Duration::from_secs(45),
            factor: 1.0,
        }];
        let pool = Pool::restore(
            vec![spec("k", 10, true), spec("other", 10, true)],
            &states,
            Instant::now(),
            1.0,
        );
        match pool.reserve(Some(0)) {
            Reservation::Ready { lane, .. } => assert_eq!(lane, 1),
            _ => panic!("expected the spill lane to take the grant"),
        }
    }

    #[test]
    fn restore_unknown_keys_start_fresh() {
        let now = Instant::now();
        let states = vec![LaneState {
            key: "old".into(),
            rpm: 1,
            sent: VecDeque::from([now]),
            cooldown_until: now + Duration::from_secs(999),
            factor: 0.5,
        }];
        let restored = Pool::restore(vec![spec("new", 1, true)], &states, Instant::now(), 1.0);
        assert!(matches!(restored.reserve(None), Reservation::Ready { .. }));
        assert_eq!(restored.lane_stats()[0].cal_factor, 1.0);
    }

    #[test]
    fn ramp_slows_admission_only_while_armed() {
        let ramp = Instant::now() + Duration::from_secs(300);
        let pool = Pool::restore(keys(1, 10), &[], ramp, 0.5);
        let mut granted = 0;
        while let Reservation::Ready { .. } = pool.reserve(None) {
            granted += 1;
        }
        assert_eq!(granted, 5, "slow-start admits rpm * ramp_factor");
        // Unarmed (no ramp horizon) pools run at full budget.
        let mut granted = 0;
        let unarmed = Pool::restore(keys(1, 10), &[], Instant::now(), 0.5);
        while let Reservation::Ready { .. } = unarmed.reserve(None) {
            granted += 1;
        }
        assert_eq!(granted, 10);
    }

    #[test]
    fn rebuild_carries_the_ramp_window() {
        let pool = Pool::restore(
            keys(1, 10),
            &[],
            Instant::now() + Duration::from_secs(120),
            0.3,
        );
        let rebuilt = pool.rebuild(keys(1, 10));
        let mut granted = 0;
        while let Reservation::Ready { .. } = rebuilt.reserve(None) {
            granted += 1;
        }
        assert_eq!(granted, 3, "a settings swap must not sever the slow-start");
    }
}
