---
type: Decision
title: Persist rate windows across restart, then slow-start ramp
description: Rate windows and governor caps survive a restart via a versioned JSONL snapshot, and a fresh restore arms a slow-start ramp so a recovered key doesn't burst into NIM.
tags: [rate-limiting, pool, recovery, persistence]
timestamp: 2026-08-10T00:00:00Z
---

# Persist rate windows across restart, then slow-start ramp

## Context

NIM's per-key limit is a rolling window ([research](../research/nim-free-tier-40rpm-no-credits.md)),
and the whole point of the proxy is never tripping it. Rate state was
in-memory only, so a restart right after heavy traffic reset every window to
empty: the next boot drew a burst of 429s from NIM. The retry machinery
absorbed them invisibly, but it was a real gap — the proxy should resume
*pacing*, not re-learn the limit the hard way.

## Options

1. **Do nothing** — keep windows in-memory; absorb the post-restart 429 storm
   invisibly (the previous behavior).
2. **Persist windows/caps, restore exactly** — snapshot state to disk and
   resume as it stood, no behavioral change on the higher side.
3. **Persist + slow-start ramp** — restore the windows, but for the first
   `ramp_secs` after a *fresh* restart admit only `rpm × ramp_factor`,
   mirroring the upstream's own post-restart relaxation.

## Choice

Option 3: persist (**ratestate.jsonl**, versioned JSONL, atomic
tmp+fsync+rename, corrupt lines dropped lane-by-lane — never a hard boot
error) and pair it with an optionally-armed slow-start ramp.

Two conservative-by-construction details:

- **Epoch truncation errs under-counting.** Timestamps are persisted as
  integer epoch seconds; a sub-second truncation only makes a window look
  *older* (more slots free), which leans safe — never over-committed.
- **The ramp is a fresh-restore affordance, not a blanket slow-down.**
  It arms only when `ramp_secs > 0` (default 60) **and** the persisted file is
  younger than `RAMP_STALE_CUTOFF` (5 min). After a long downtime the upstream
  window is empty anyway, so ramping would only needlessly throttle recovery.
  The ramp laps the calibration handoff: while armed the pool admits at
  `rpm × factor`; once the horizon passes, normal self-calibrating admission
  ([key-pool](../architecture/key-pool.md)) resumes.

## Consequences

- Restart no longer resets pacing: a still-full key stays throttled across the
  boot, and a fresh restart ramps slowly instead of bursting.
- A **clean-shutdown save** (SIGTERM/docker stop) flushes the exact final
  windows; the 30s tick and settings-change save cover crash/power-loss cases,
  accepting at most the tick interval of staleness.
- Windows are bounded and never leak key material: the file stores epoch
  timestamps and caps, not request content.
- The ramp is tunable per install (`ramp_secs` / `ramp_factor` in Settings →
  Restart recovery; `NIMPROXY_RAMP_SECS` / `NIMPROXY_RAMP_FACTOR` override at
  boot), is surfaced on the dashboard ("recovering" chip) and in metrics
  (`nimproxy_ramp_active`, `nimproxy_restore_count{outcome}`).
- A lane that was mid-cooldown (benched) at shutdown resumes its cooldown
  rather than reopening hot; far-future expiries (> `MAX_BENCH_AHEAD_SECS`)
  are treated as corruption and dropped.