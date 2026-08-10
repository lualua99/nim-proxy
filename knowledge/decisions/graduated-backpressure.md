---
type: Decision
title: Graduated backpressure — queue ETA + Retry-After rejections
description: Replace the binary queue-or-503 with a gradient: estimate the queue wait, accept under the threshold (with ETA header), reject at or above it (503 + Retry-After). Deadline-carrying requests are exempt.
tags: [backpressure, queue, rate-limit, reliability]
timestamp: 2026-08-10T00:00:00Z
---

# Graduated backpressure — queue ETA + Retry-After rejections

## Context

The proxy had two hard edges for admission:

1. **`max_inflight` overload shedding** (`proxy.rs:543-546`): a cold 503 with
   no `Retry-After` — purely a memory flood guard, not a queue-depth signal.
2. **Rate-limit slot queue** (`reserve_slot`): once admitted past the inflight
   cap, a request joins the dispatcher FIFO and waits there until `max_wait`
   (default 900 s) or a slot opens. The client has no idea how long it will
   wait — only heartbeats tell it the connection is alive.

Neither path gives an agent harness enough information to make a good
decision: should it wait? Should it retry elsewhere? Should it back off?

## Options

1. **Status quo**: binary choice — queue silently or be cold-rejected. The
   harness must guess; it typically waits until the timeout fires, burning
   time and connection slots.

2. **ETA-only**: always let the request queue, but tell the client the
   estimated wait via `X-Nim-Proxy-Eta`. The harness still has to stay
   connected; it cannot free the slot.

3. **Reject-only**: reject every request that would have to queue (always
   `Retry-After`). Wastes the "patient" design — the proxy is built to wait
   through short bursts.

4. **Graduated backpressure (chosen)**: an operator-configured threshold
   distinguishes short waits (queue with ETA header) from long waits (reject
   with 503 + `Retry-After`). Agents with a short wait keep the heartbeat
   path; agents facing a long wait can back off immediately.

## Choice

Graduated backpressure, with the threshold defaulting to 20 s and the feature
defaulting to **off** (opt-in until validated in production).

## Consequences

- **Positive**: agents stop waiting 900 s through a saturated queue. The
  `Retry-After` header gives them a concrete backoff target.
- **Positive**: the `X-Nim-Proxy-Eta` header on sub-threshold streaming
  responses gives clients visibility without changing the protocol.
- **Positive**: deadline-carrying requests are exempt, so time-sensitive
  workloads are not silently dropped.
- **Neutral**: ETA is an estimate, not a promise. Concurrent arrivals,
  policy re-ordering (EDF/Fair), and lane calibration all change the real
  wait. The header is documented as best-effort.
- **Negative**: adds one configuration knob most operators will not touch
  until the defaults prove wrong.