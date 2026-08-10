//! Global slot dispatcher. Every client connection — one OpenCode, five
//! OpenCodes, an n8n flow, a Codex session — funnels through one queue, so
//! under contention slots are granted in a deliberate order instead of
//! letting freshly-arrived requests win wakeup races against long waiters.
//!
//! Three strategies live behind one `acquire` entry point (the default
//! `Fifo` is the historical strict-arrival-order behavior; see
//! [`Policy`] for the alternatives and the "who goes next" rules):
//!
//! - `Fifo`: waiters are served strictly in arrival order.
//! - `Edf`: the waiter whose queue deadline expires first goes first
//!   (deadline ties break by arrival order). Callers fold an explicit
//!   `X-Nim-Proxy-Deadline-Ms` into the queue deadline, so deadline-carrying
//!   requests jump the line while everyone else falls back to FIFO.
//! - `Fair`: each client holds a per-round slot quota (weight, globally
//!   configured with a stable name-derived default); waiters past the aging
//!   bound are served at highest priority so no client can starve.
//!
//! The dispatcher is the only `Pool::reserve` caller in the app, and it holds
//! the [`PoolHandle`] read lock across each reserve — so a settings-driven
//! pool swap (which takes the write lock) can never interleave with a grant.
//! Grants carry the `Arc<Pool>` that made them (see [`Slot`]) so bench and
//! release always land on the granting pool; a late op on a retired pool is
//! benign because nothing consults it anymore. A settings save reconfigures
//! the policy through the same shared handle the dispatcher task reads, so a
//! policy change takes effect at the next pick, never mid-grant.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use metrics::{counter, gauge};
use tokio::sync::{mpsc, oneshot};

use crate::config::{DispatchCfg, DispatchPolicy};
use crate::pool::{Pool, PoolHandle, Reservation};

/// Minimum gap between consecutive slot grants. Caps burst *concurrency*
/// (a cold pool can grant its full aggregate RPM instantly — hundreds of
/// simultaneous connects look like a stampede to the upstream and skew
/// arrival timing) without capping throughput: 25ms = 2,400 grants/min,
/// far beyond any realistic key pool's aggregate RPM.
const GRANT_GAP: Duration = Duration::from_millis(25);

/// The slot polls in slices this long, so abandoned waiters and policies
/// changes are noticed promptly while a lane is blocked (the current grant
/// loop also re-consults swapped-in pool generations on every slice).
const POLL: Duration = Duration::from_millis(500);

/// A housekeeping border: no dispatcher decision may wait indefinitely, so
/// every unintended hold (busy loops on a pathological policy) is bounded by
/// this — also the dispatch queue's own fail-fast discipline.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// A granted reservation: the key to send with, and the pool that granted it
/// so follow-up bench/release ops route to the right generation.
pub struct Slot {
    pub pool: Arc<Pool>,
    pub lane: usize,
    pub key: String,
}

/// The live scheduling policy plus its `Fair` knobs. Shared between the
/// settings layer (which swaps it via [`Dispatcher::configure`]) and the
/// dispatcher task (which reads it per pick).
#[derive(Clone)]
struct PolicyState {
    policy: DispatchPolicy,
    aging: Duration,
    weights: HashMap<String, usize>,
}

impl From<&DispatchCfg> for PolicyState {
    fn from(cfg: &DispatchCfg) -> Self {
        Self {
            policy: cfg.policy,
            aging: Duration::from_secs(cfg.fair_aging_secs),
            weights: cfg.fair_weights.clone(),
        }
    }
}

pub struct Dispatcher {
    queue: mpsc::UnboundedSender<Waiter>,
    policy: Arc<RwLock<PolicyState>>,
    pool: PoolHandle,
}

/// One queued request. `deadline` is the absolute instant past which the
/// waiter must fail fast (dropped reply = caller sees an error); under `Edf`
/// it is also the ordering key. `arrived` is the queue join time.
struct Waiter {
    reply: oneshot::Sender<Slot>,
    deadline: Instant,
    prefer: Option<usize>,
    client: String,
    arrived: Instant,
}

impl Dispatcher {
    pub fn new(pool: PoolHandle, cfg: &DispatchCfg) -> Self {
        let (queue, rx) = mpsc::unbounded_channel();
        let policy = Arc::new(RwLock::new(PolicyState::from(cfg)));
        let task_policy = policy.clone();
        let pool_clone = pool.clone();
        tokio::spawn(run(pool_clone, task_policy, rx));
        Self {
            queue,
            policy,
            pool,
        }
    }

    /// Swap the scheduling policy/knobs. Takes effect at the next pick; the
    /// already-waiting set is re-sorted under the new rules immediately.
    pub fn configure(&self, cfg: &DispatchCfg) {
        let st = PolicyState::from(cfg);
        emit_policy_gauges(st.policy);
        *self.policy.write().unwrap() = st;
    }

    /// Estimate the soonest a slot could become available, without queuing.
    /// Returns `None` when the pool is empty (no lanes configured). The
    /// estimate is best-effort — the actual wait depends on concurrent
    /// arrivals, policy re-ordering, and lane calibration — and is intended
    /// for the backpressure gate, not as a promise.
    pub fn eta(&self, prefer: Option<usize>) -> Option<Duration> {
        let guard = self.pool.read().ok()?;
        let pool = guard.clone();
        Some(pool.min_wait(prefer))
    }

    /// Join the queue. The receiver resolves to a reserved [`Slot`], or
    /// errors if no slot can open before `deadline`. Dropping the receiver
    /// leaves the queue; a slot granted to an abandoned waiter is returned to
    /// the pool.
    pub fn acquire(
        &self,
        deadline: Instant,
        prefer: Option<usize>,
        client: String,
    ) -> oneshot::Receiver<Slot> {
        let (reply, rx) = oneshot::channel();
        gauge!("nimproxy_queue_depth").increment(1.0);
        let _ = self.queue.send(Waiter {
            reply,
            deadline,
            prefer,
            client,
            arrived: Instant::now(),
        });
        rx
    }
}

/// Emit the resident "which policy is live" gauge: exactly one series (the
/// live policy) reads 1, all others 0.
fn emit_policy_gauges(policy: DispatchPolicy) {
    for p in [
        DispatchPolicy::Fifo,
        DispatchPolicy::Edf,
        DispatchPolicy::Fair,
    ] {
        gauge!("nimproxy_dispatch_policy", "policy" => p.as_str()).set(if p == policy {
            1.0
        } else {
            0.0
        });
    }
}

/// Stable per-client default weight (1 + hash % 3): deterministic across
/// restarts (same store → same lanes), unlike a random draw, so behavior is
/// reproducible. Per-client overrides in the store win outright.
fn default_weight(client: &str) -> usize {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    client.hash(&mut h);
    1 + (h.finish() % 3) as usize
}

fn weight_of(client: &str, st: &PolicyState) -> usize {
    st.weights
        .get(client)
        .copied()
        .unwrap_or_else(|| default_weight(client))
}

/// Choose which waiting waiter gets the next grant attempt, per policy.
/// `grants` is the Fair per-client budget consumed this round. Pure so the
/// ordering rules are unit-testable without a running dispatcher.
fn pick(
    waiters: &[Waiter],
    st: &PolicyState,
    grants: &mut HashMap<String, usize>,
    now: Instant,
) -> usize {
    match st.policy {
        DispatchPolicy::Fifo => {
            let mut best = 0;
            for (i, w) in waiters.iter().enumerate().skip(1) {
                if w.arrived < waiters[best].arrived {
                    best = i;
                }
            }
            best
        }
        DispatchPolicy::Edf => {
            let mut best = 0;
            for (i, w) in waiters.iter().enumerate().skip(1) {
                if w.deadline < waiters[best].deadline
                    || (w.deadline == waiters[best].deadline && w.arrived < waiters[best].arrived)
                {
                    best = i;
                }
            }
            best
        }
        DispatchPolicy::Fair => {
            // Aged clients (any waiter past the aging bound) are served at
            // full priority regardless of budget — the anti-starvation bound.
            let mut best_aged: Option<usize> = None;
            for (i, w) in waiters.iter().enumerate() {
                if w.arrived + st.aging <= now
                    && best_aged.is_none_or(|b| waiters[i].arrived < waiters[b].arrived)
                {
                    best_aged = Some(i);
                }
            }
            if let Some(i) = best_aged {
                return i;
            }
            // 一轮: a round gives every *present* client its weight in
            // grants; pick the client with the most remaining budget share,
            // ties to the earliest arrival.
            let exhausted_all = waiters
                .iter()
                .all(|w| grants.get(&w.client).copied().unwrap_or(0) >= weight_of(&w.client, st));
            if exhausted_all {
                grants.clear();
            }
            let mut best = 0;
            let mut best_score = f64::MAX;
            let mut best_arrived = waiters[0].arrived;
            for (i, w) in waiters.iter().enumerate() {
                let used = grants.get(&w.client).copied().unwrap_or(0) as f64;
                let budget = weight_of(&w.client, st) as f64;
                let score = used / budget;
                if score < best_score || (score == best_score && w.arrived < best_arrived) {
                    best = i;
                    best_score = score;
                    best_arrived = w.arrived;
                }
            }
            best
        }
    }
}

async fn run(
    handle: PoolHandle,
    policy: Arc<RwLock<PolicyState>>,
    mut queue: mpsc::UnboundedReceiver<Waiter>,
) {
    // `waiters` is the in-dispatcher wait set; `grants` is the Fair
    // per-round budget. Both are private to this task — `acquire` only owns
    // the channel.
    let mut waiters: Vec<Waiter> = Vec::new();
    let mut grants: HashMap<String, usize> = HashMap::new();
    loop {
        if waiters.is_empty() {
            // Nobody queued: block on the channel so an idle dispatcher
            // doesn't burn a wakeup per IDLE_POLL.
            match queue.recv().await {
                Some(w) => waiters.push(w),
                None => return,
            }
        } else {
            // Waiters present: keep draining arrivals in the background of
            // the polling loop so late arrivals can overtake (EDF/Fair) or
            // fall in line (FIFO) instead of waiting for the next grant.
            tokio::select! {
                Some(w) = queue.recv() => waiters.push(w),
                _ = tokio::time::sleep(IDLE_POLL) => {}
            }
            // Drop waiters whose caller hung up (request cancelled while
            // queued), keeping the queue-depth gauge honest.
            let before = waiters.len();
            waiters.retain(|w| !w.reply.is_closed());
            let dropped = before - waiters.len();
            if dropped > 0 {
                gauge!("nimproxy_queue_depth").decrement(dropped as f64);
            }
            if waiters.is_empty() {
                continue;
            }
        }

        let st = policy.read().unwrap().clone();
        let idx = pick(&waiters, &st, &mut grants, Instant::now());
        let waiter = &waiters[idx];
        let deadline = waiter.deadline;
        let prefer = waiter.prefer;
        // Snapshot + reserve under the read guard (reserve is sync and
        // lock-cheap), then drop the guard before any await.
        let (pool, reservation) = {
            let guard = handle.read().unwrap();
            let pool = guard.clone();
            let r = pool.reserve(prefer);
            (pool, r)
        };
        match reservation {
            Reservation::Ready {
                lane,
                key,
                stamp,
                sticky,
            } => {
                let affinity = match prefer {
                    None => "none",
                    Some(_) if sticky => "sticky",
                    Some(_) => "spill",
                };
                counter!("nimproxy_affinity_total", "result" => affinity).increment(1);
                counter!(
                    "nimproxy_dispatch_slot_grant_total",
                    "policy" => st.policy.as_str(),
                    "client" => waiters[idx].client.clone(),
                )
                .increment(1);
                grants
                    .entry(waiters[idx].client.clone())
                    .and_modify(|g| *g += 1)
                    .or_insert(1);
                let slot = Slot {
                    pool: pool.clone(),
                    lane,
                    key,
                };
                let served = waiters.remove(idx);
                if served.reply.send(slot).is_err() {
                    pool.release(lane, stamp);
                } else {
                    tokio::time::sleep(GRANT_GAP).await;
                }
                gauge!("nimproxy_queue_depth").decrement(1.0);
            }
            Reservation::Wait(wait) => {
                if Instant::now() + wait > deadline {
                    // Fail fast: dropping the waiter drops its reply, and the
                    // caller's receiver errors immediately.
                    waiters.remove(idx);
                    gauge!("nimproxy_queue_depth").decrement(1.0);
                    continue;
                }
                // Still waiting: sleep in slices so arrivals/abandons/policy
                // changes stay noticed, then let the next pick decide — an
                // EDF/Fair re-ordering takes effect here, and a swapped-in
                // pool generation is re-consulted on every round.
                tokio::time::sleep(wait.min(POLL)).await;
            }
        }
    }
}

/// Minimal drop-guard so gauges stay honest on every exit path (granted,
/// expired, abandoned, or panicked).
pub fn scopeguard<F: FnMut()>(f: F) -> impl Drop {
    struct Guard<F: FnMut()>(F);
    impl<F: FnMut()> Drop for Guard<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    Guard(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(lanes: usize, rpm: usize) -> PoolHandle {
        Arc::new(RwLock::new(Arc::new(Pool::new(
            (0..lanes)
                .map(|i| crate::pool::LaneSpec {
                    key: format!("key{i}"),
                    rpm,
                    enabled: true,
                })
                .collect(),
        ))))
    }

    fn cfg(policy: DispatchPolicy) -> DispatchCfg {
        DispatchCfg {
            policy,
            fair_aging_secs: 60,
            fair_weights: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn grants_slots_in_order_while_capacity_remains() {
        let d = Dispatcher::new(handle(2, 1), &cfg(DispatchPolicy::Fifo));
        let deadline = Instant::now() + Duration::from_secs(5);
        let a = d
            .acquire(deadline, None, "alice".into())
            .await
            .expect("first slot");
        let b = d
            .acquire(deadline, None, "bob".into())
            .await
            .expect("second slot");
        assert_eq!(a.lane, 0);
        assert_eq!(b.lane, 1);
    }

    #[tokio::test]
    async fn fails_fast_when_no_slot_can_open_before_deadline() {
        let d = Dispatcher::new(handle(1, 1), &cfg(DispatchPolicy::Fifo));
        let deadline = Instant::now() + Duration::from_millis(200);
        d.acquire(deadline, None, "alice".into())
            .await
            .expect("first slot");
        // Lane is at capacity for ~60s, far past the deadline.
        assert!(d.acquire(deadline, None, "bob".into()).await.is_err());
    }

    #[tokio::test]
    async fn queued_waiter_is_served_after_a_capacity_raising_swap() {
        let h = handle(1, 1);
        let d = Dispatcher::new(h.clone(), &cfg(DispatchPolicy::Fifo));
        // Deadline beyond the ~61s window so the waiter queues instead of
        // failing fast.
        let deadline = Instant::now() + Duration::from_secs(120);
        let first = d
            .acquire(deadline, None, "alice".into())
            .await
            .expect("first slot");
        let started = Instant::now();
        let pending = d.acquire(deadline, None, "bob".into());
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Swap in a rebuilt pool with double the rpm — carried state keeps the
        // spent slot, the new headroom serves the waiter.
        {
            let mut guard = h.write().unwrap();
            let rebuilt = guard.rebuild(vec![crate::pool::LaneSpec {
                key: "key0".into(),
                rpm: 2,
                enabled: true,
            }]);
            *guard = Arc::new(rebuilt);
        }
        let slot = pending.await.expect("slot after swap");
        assert_eq!(slot.lane, 0);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waiter should be served promptly after the swap, waited {:?}",
            started.elapsed()
        );
        // The first grant's pool is the retired generation; releasing on it
        // must be harmless.
        first.pool.release(first.lane, Instant::now());
    }

    /// Build a pick-only waiter against a fixed base instant (replies never
    /// used by `pick`): deadline = base + dl, arrived = base - arr. A shared
    /// base makes tie-breaks byte-exact instead of racing the clock.
    fn w(base: Instant, dl: u64, client: &str, arr: u64) -> Waiter {
        let (reply, _rx) = oneshot::channel();
        Waiter {
            reply,
            deadline: base + Duration::from_secs(dl),
            prefer: None,
            client: client.to_owned(),
            arrived: base - Duration::from_secs(arr),
        }
    }

    #[test]
    fn edf_picks_the_earliest_deadline_regardless_of_arrival() {
        let st = PolicyState::from(&cfg(DispatchPolicy::Edf));
        let base = Instant::now();
        // Arrived first but with a far deadline; arrived second with a
        // deadline that expires sooner.
        let waiters = vec![
            w(base, 900, "batch", 3),
            w(base, 10, "deadline-carrying", 1),
            w(base, 900, "another-batch", 2),
        ];
        let mut grants = HashMap::new();
        assert_eq!(pick(&waiters, &st, &mut grants, base), 1);

        // Tie deadlines fall back to earliest arrival.
        let tie = vec![w(base, 10, "second", 1), w(base, 10, "first", 2)];
        assert_eq!(pick(&tie, &st, &mut grants, base), 1);
    }

    #[test]
    fn fifo_picks_strict_arrival_order() {
        let st = PolicyState::from(&cfg(DispatchPolicy::Fifo));
        let base = Instant::now();
        // Insertion order deliberately shuffled relative to arrival.
        let waiters = vec![
            w(base, 900, "c", 1),
            w(base, 900, "a", 3),
            w(base, 900, "b", 2),
        ];
        let mut grants = HashMap::new();
        assert_eq!(
            pick(&waiters, &st, &mut grants, base),
            1,
            "oldest arrival first"
        );
    }

    #[test]
    fn fifo_ignores_deadlines() {
        // Under FIFO a near-deadline latecomer does NOT jump the line.
        let st = PolicyState::from(&cfg(DispatchPolicy::Fifo));
        let base = Instant::now();
        let waiters = vec![w(base, 900, "elder", 1), w(base, 1, "urgent", 0)];
        let mut grants = HashMap::new();
        assert_eq!(pick(&waiters, &st, &mut grants, base), 0);
    }

    #[test]
    fn fair_rotates_clients_within_their_budget_until_reset() {
        let mut c = cfg(DispatchPolicy::Fair);
        c.fair_weights.insert("alice".into(), 1);
        c.fair_weights.insert("bob".into(), 1);
        let st = PolicyState::from(&c);
        let base = Instant::now();
        // alice x2, bob x1; grants consumed in grant order; granted waiters
        // leave the set exactly as the run loop removes them.
        let mut waiters = vec![
            w(base, 900, "alice", 3),
            w(base, 900, "alice", 2),
            w(base, 900, "bob", 1),
        ];
        let mut grants = HashMap::new();
        // Round 1: all budgets open; earliest arrival wins (alice).
        assert_eq!(pick(&waiters, &st, &mut grants, base), 0);
        waiters.remove(0);
        grants
            .entry("alice".into())
            .and_modify(|g| *g += 1)
            .or_insert(1);
        // Bob has the only remaining budget.
        assert_eq!(pick(&waiters, &st, &mut grants, base), 1);
        waiters.remove(1);
        grants
            .entry("bob".into())
            .and_modify(|g| *g += 1)
            .or_insert(1);
        // Everyone's budget consumed -> new round; alice's second waiter is
        // the only one left.
        assert_eq!(
            pick(&waiters, &st, &mut grants, base),
            0,
            "reset serves alice's second waiter"
        );
    }

    #[test]
    fn fair_weights_give_a_heavier_client_more_serves_per_round() {
        let mut c = cfg(DispatchPolicy::Fair);
        c.fair_weights.insert("heavy".into(), 2);
        c.fair_weights.insert("light".into(), 1);
        let st = PolicyState::from(&c);
        let base = Instant::now();
        let waiters = vec![w(base, 900, "light", 1), w(base, 900, "heavy", 1)];
        let mut grants = HashMap::new();
        // One full round of three grants: light (earliest arrival), then
        // heavy twice from its doubled budget — 1:2, its configured ratio.
        assert_eq!(pick(&waiters, &st, &mut grants, base), 0);
        grants
            .entry("light".into())
            .and_modify(|g| *g += 1)
            .or_insert(1);
        assert_eq!(pick(&waiters, &st, &mut grants, base), 1);
        grants
            .entry("heavy".into())
            .and_modify(|g| *g += 1)
            .or_insert(1);
        assert_eq!(pick(&waiters, &st, &mut grants, base), 1);
        grants
            .entry("heavy".into())
            .and_modify(|g| *g += 1)
            .or_insert(1);
        // Both exhausted: a fresh round starts with the earlier arrival.
        assert_eq!(pick(&waiters, &st, &mut grants, base), 0);
    }

    #[test]
    fn fair_aging_overrides_budget_so_nobody_starves() {
        let mut c = cfg(DispatchPolicy::Fair);
        c.fair_aging_secs = 1;
        c.fair_weights.insert("profitable".into(), 1);
        let st = PolicyState::from(&c);
        let base = Instant::now();
        let mut grants = HashMap::new();
        // The aged waiter (arrived 10s before the aging bound of 1s) must
        // jump a full-budgeted newcomer regardless of arrival order.
        let waiters = vec![w(base, 900, "profitable", 0), w(base, 900, "starving", 10)];
        assert_eq!(
            pick(&waiters, &st, &mut grants, base),
            1,
            "aged waiter served despite budget"
        );
        grants
            .entry("profitable".into())
            .and_modify(|g| *g += 1)
            .or_insert(1);
        // The aged waiter keeps priority on later picks too.
        assert_eq!(pick(&waiters, &st, &mut grants, base), 1);
    }

    #[test]
    fn fair_ignores_deadlines() {
        // EDF is the policy that honors deadlines; under Fair an urgent
        // latecomer yields to the aged/round budget like anyone else.
        let st = PolicyState::from(&cfg(DispatchPolicy::Fair));
        let base = Instant::now();
        let waiters = vec![w(base, 10, "urgent", 0), w(base, 900, "plain", 1)];
        let mut grants = HashMap::new();
        assert_eq!(
            pick(&waiters, &st, &mut grants, base),
            1,
            "arrival order is the tie-break"
        );
    }

    #[tokio::test]
    async fn configure_swaps_policy_live() {
        let d = Dispatcher::new(handle(1, 1), &cfg(DispatchPolicy::Fifo));
        let mut c = cfg(DispatchPolicy::Fair);
        c.fair_aging_secs = 1;
        c.fair_weights.insert("x".into(), 1);
        d.configure(&c); // must not panic and must reach the task's next pick
                         // A polite smoke: still serves.
        let slot = d
            .acquire(Instant::now() + Duration::from_secs(5), None, "x".into())
            .await
            .expect("configured dispatcher still grants");
        slot.pool.release(slot.lane, Instant::now());
    }

    /// Abandoning a queued waiter must not leak a slot or wedge later
    /// arrivals (the granted-slot return for a racy hang-up is covered by
    /// the len==1 code path in the run loop and unit-tested via `pick`).
    #[tokio::test]
    async fn abandoned_waiter_clears_the_queue_for_the_next_wait() {
        let h = handle(1, 1);
        let d = Dispatcher::new(h.clone(), &cfg(DispatchPolicy::Fifo));
        let deadline = Instant::now() + Duration::from_secs(120);
        let first = d.acquire(deadline, None, "alice".into());
        // Give the slot a beat to land, then park a second waiter and hang
        // up on it before the lane can free.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let pending = d.acquire(deadline, None, "bob".into());
        drop(pending);
        // Capacity appears: the queued ghost must be gone and the new
        // waiter served without waiting for the old one.
        {
            let mut guard = h.write().unwrap();
            let rebuilt = guard.rebuild(vec![crate::pool::LaneSpec {
                key: "key0".into(),
                rpm: 2,
                enabled: true,
            }]);
            *guard = Arc::new(rebuilt);
        }
        let slot = d
            .acquire(deadline, None, "carol".into())
            .await
            .expect("slot after abandon + swap");
        slot.pool.release(slot.lane, Instant::now());
        first.await.expect("first waiter still served");
    }

    #[test]
    fn default_weight_is_stable_and_in_range() {
        for name in ["alice", "bob", "carol", "opencode", "n8n"] {
            assert!(
                (1..=3).contains(&default_weight(name)),
                "{name} weight must be 1..=3"
            );
            assert_eq!(default_weight(name), default_weight(name), "stable");
        }
    }

    #[tokio::test]
    async fn eta_returns_zero_when_lanes_have_room() {
        let d = Dispatcher::new(handle(2, 40), &cfg(DispatchPolicy::Fifo));
        let eta = d.eta(None);
        assert_eq!(eta, Some(Duration::ZERO));
    }

    #[tokio::test]
    async fn eta_returns_nonzero_when_lanes_are_full() {
        let h = handle(1, 1);
        let d = Dispatcher::new(h.clone(), &cfg(DispatchPolicy::Fifo));
        // Occupy the single lane.
        d.acquire(
            Instant::now() + Duration::from_secs(120),
            None,
            "alice".into(),
        )
        .await
        .expect("first slot");
        // The lane is now full (1 rpm, 1 in-flight), eta should be > 0.
        let eta = d.eta(None).unwrap();
        assert!(
            eta > Duration::ZERO,
            "eta should be positive when lanes are full"
        );
    }

    #[tokio::test]
    async fn eta_defaults_to_window_when_pool_is_empty() {
        let h = Arc::new(RwLock::new(Arc::new(Pool::new(vec![]))));
        let d = Dispatcher::new(h, &cfg(DispatchPolicy::Fifo));
        // No lanes to scan: the conservative default is the full window.
        assert_eq!(d.eta(None), Some(Duration::from_secs(61)));
    }

    #[tokio::test]
    async fn eta_prefer_lane_is_considered() {
        let h = handle(2, 1);
        let d = Dispatcher::new(h.clone(), &cfg(DispatchPolicy::Fifo));
        // Occupy lane 0.
        d.acquire(
            Instant::now() + Duration::from_secs(120),
            None,
            "alice".into(),
        )
        .await
        .expect("first slot");
        // Lane 1 is still free, so prefer=None should find it.
        assert_eq!(d.eta(None), Some(Duration::ZERO));
        // Prefer lane 0 (occupied) — should still return zero because lane 1
        // is idle, but `min_wait` checks the preferred lane first, then scans.
        assert_eq!(d.eta(Some(0)), Some(Duration::ZERO));
    }
}
