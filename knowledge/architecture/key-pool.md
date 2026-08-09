---
type: Component
title: Key pool (src/pool.rs)
description: One lane per NIM key; exact sliding-window limiter, least-loaded selection, cooldown benching, releasable reservations.
tags: [pool, rate-limiting]
timestamp: 2026-07-02T00:00:00Z
---

# Key pool — `src/pool.rs`

Each API key is a **lane** holding a `VecDeque<Instant>` of send timestamps
(the sliding window) and a `cooldown_until` instant (benching).

- **Reserve** (`reserve(prefer)`): the preferred lane wins if it has capacity
  ([affinity](../decisions/sticky-affinity-with-spillover.md)); otherwise the
  ready lane with the fewest in-window sends (spreads bursts ~evenly).
  Reservation pushes the timestamp immediately, so concurrent callers can't
  oversubscribe. Returns `Ready { lane, key, stamp, sticky }` or
  `Wait(duration)` until the soonest slot.
- **Window** is 61s for a 60s upstream limit — see
  [window-jitter-margin](../decisions/window-jitter-margin.md).
- **Penalize**: an upstream 429/5xx benches the lane (`Retry-After` honored,
  defaults 10s for 429, 5s for connect errors). Benched lanes are skipped;
  other lanes absorb traffic.
- **Observe** (calibration): the same rejections feed a per-lane
  `Calibration` factor — short-term *cooldown* (benching) and long-term
  *memory* are separate mechanisms. Connect errors are bench-only: they say
  nothing about the lane's own ceiling.
- **Release**: a reservation granted to a client that vanished while queued
  is removed from the window by its stamp, returning the slot.

## Calibration (measured ceiling)

NIM's ~40 RPM is a soft, account/load-dependent ceiling, so the
configured `rpm` is only a starting guess. Each lane holds a
`Calibration` (a factor on the configured budget) learned from upstream
rejections:

- **Shrink**: each observed 429/5xx multiplies the factor by 0.9.
  429s with `Retry-After >= 30s` show NIM's exponential-lockout shape:
  every second such event applies an extra ×0.5 and flags the lane as
  *locked*, whose floor is 0.1 instead of the ordinary 0.2. `effective_rpm`
  is `floor(rpm × factor)`, never 0 (would dead-lock the window math).
- **Heal**: `maybe_probe` runs on every `reserve` visit — after a full
  probe interval of silence (default 3600s, `NIMPROXY_CALIBRATION_PROBE_SECS`)
  the factor steps up by `PROBE_STEP` (0.01). Deliberately slow: the
  upstream's account-level throttle can outlast tens of minutes of quiet.
- **Carry-over**: the factor travels with the lane across
  `Pool::rebuild` (same key-string match as the window/cooldown), so a
  re-configured key doesn't forget what the upstream taught it.
- **Signals**: `Signal::RateLimited { retry_after }` vs `Signal::ServerError`
  — a 429 and a 503 are different reasons to shrink. The proxy feeds
  `Pool::observe` from `upstream_signal()` only for retryable upstream
  responses, never connect errors.

Admission (`try_take`, `reserve`'s window math) uses the effective rate
everywhere, including the dashboard's per-lane meters (key row shows
`in_window / effective` and the × factor when < 1). Gauge
`nimproxy_lane_calibration` per lane is republished on every shrink/probe.

## Per-key rpm and live rebuild (v0.6.0)

Each lane's limit is **per-key** now (`NimKey.rpm`, default 40, range
1–10000 — covers paid tiers / self-hosted NIM), not one global `RPM_PER_KEY`.
Keys, their rpms, and their enabled/disabled state live in the
[config store](../decisions/ui-managed-config-store.md); a Settings save calls
`Pool::rebuild(keys)` under the pool write lock, feeding it **enabled keys
only**. Rebuild **carries over per-lane rate state** (`sent` window,
`cooldown_until`) by key-string match: a kept key keeps its in-window counts
(can't be double-spent across a swap), a lowered rpm is honored immediately
(`try_take` checks `sent.len() < rpm` live), and a disabled key keeps its stored
state so it re-enables **warm**. Grants carry their originating `Arc<Pool>`
(`Slot { pool, lane, key }`) so late bench/release after a swap route to the
pool that granted them — no index-out-of-bounds, late ops on a retired pool are
benign. **Invariant**: the superuser always owns ≥1 enabled key, pinning the
pool floor (removing/disabling the last one is a 400), so the pool can never
empty. Per-model worker-concurrency limits are a separate concern — see the
[governor](governor.md).

Rate state is in-memory only: one proxy instance per key set (documented
limitation), and windows reset on restart (post-restart burst 429s are
absorbed by retry).
