---
type: Decision
title: Dispatcher policy modes (FIFO / EDF / Fair)
description: The single global queue now schedules waiters by policy — arrival order, deadline order, or per-client weighted rounds with aging — swapped live from the dashboard.
tags: [dispatcher, fairness, deadlines, multi-client]
timestamp: 2026-08-09T00:00:00Z
---

# Dispatcher policy modes (FIFO / EDF / Fair)

## Context

[Global FIFO](global-fifo-dispatcher.md) removed the polling wakeup race, but
arrival order is one fairness doctrine among several, and the simplest
arrival-ordered grant ignores everything a waiter is made of. Two pressures
made the single fixed policy wrong:

- Deadline-carrying requests
  ([explicit header deadlines](explicit-request-deadline.md)) queue for as
  long as `max_wait` permits even when their own much shorter bound has
  lapsed — a grant then arrives only to be cancelled by the deadline wrapper.
- Long-running agents starve fast interactive harnesses under pure FIFO: one
  OpenCode session can hold multiple slots for minutes while manual requests
  wait behind it.

## Options

1. Keep strict FIFO, let clients police priority themselves. Rejected:
   nothing about arrival order expresses urgency, and there is no per-client
   view of resource use.
2. Encode priority in the deadline header only. Partly right for bounded
   workloads, meaningless for harnesses that send no header.
3. **Make scheduling a configurable policy** behind one `acquire` entry
   point, selectable per deployment (and from the dashboard):

   - `Fifo` — strict arrival order (historical behavior, still the default).
   - `Edf` — earliest deadline first; ties break by arrival order. The queue
     deadline presented by callers is the blend
     `min(wait_deadline, explicit header deadline)`, so header-carrying
     requests jump the line under EDF.
   - `Fair` — per-client round-robin quota using a per-round ticket weight
     (global, with a stable name-derived default per client); waiters past
     the aging bound are served at highest priority so no client can starve.

## Choice

Option 3. The policy lives in a `PolicyState` under an `Arc<RwLock<>>` shared
between the settings layer and the dispatcher task, so a dashboard/settings
save swaps the policy at the next pick — never mid-grant. `emit_policy_gauges`
publishes exactly one live `nimproxy_dispatch_policy` gauge series. Timing
knobs: `GRANT_GAP` 25 ms caps burst concurrency without capping throughput
(2,400 grants/min) so a cold pool does not stampede the upstream; polls run in
`POLL` 500 ms slices so abandoned waiters and policy swaps are noticed while a
lane is blocked; `IDLE_POLL` 100 ms bounds every unintended dispatcher hold.

Deadline-carrying waiters fail fast from the queue when their own folded
bound lapses instead of parking until `max_turns` — ordering key under EDF,
beneficial under every policy.

## Consequences

- A single dispatcher still owns all grants; affinity, pacing, and
  abandoned-waiter slot return are unchanged.
- The default deployment behaves exactly as the historical FIFO.
- EDF makes the deadline header's latency value visible under contention;
  deadline-less clients under EDF fall back to arrival order.
- Fair mode adds a per-client starvation bound at the cost of two
  configurable knobs.
- The policy is runtime state (settings JSON key `dispatch.policy`), not an
  env var — see [UI-managed config](ui-managed-config-store.md).