---
type: Decision
title: Multi-upstream failover
description: Support an ordered upstream endpoint list, with passive health detection and automatic failover, so the proxy survives a single upstream going down without needing dedicated health-check traffic.
tags: [upstream, failover, health, reliability]
timestamp: 2026-08-10T00:00:00Z
---

# Multi-upstream failover

## Context

Before this change the proxy talks to a single fixed upstream (`cfg.base_url`).
When NIM's free tier is throttled site-wide, when the endpoint goes down, or
when an account is locked, every key is dead together — the whole pool
collapses and every client sees 429/5xx.

The `/v1/models` catalog is already fetched periodically (TTL, default 600s),
and every real `/v1/*` request hits the same base URL. These are the only
probes the proxy makes — there is no dedicated health-check timer and no extra
RPM budget.

## Options

1. **Active health-checking** (periodic HEAD/GET to a probe endpoint). Rejected:
   burns RPM budget on traffic that produces no value for clients. NIM's free
   tier is 40 RPM/key; every probe request is a request that could have served
   a client.

2. **Passive observation only** (reuse the existing `/v1/models` refresh and
   every real request). Zero extra RPM cost. The only question is how fast a
   downed endpoint is detected — bounded by the models TTL (default 600s) plus
   any natural traffic.

3. **Concurrent dual-upstream** (fan out every request to two upstreams, take
   the first response). Rejected: doubles RPM burn per request, and the
   failover contract is correctness (the client gets a response), not latency.

## Choice

Option 2 — passive observation. The new `src/upstream.rs` module implements:

- **Per-endpoint health state machine** (`UpstreamState`): `alive: bool`,
  `failures: u32`, `last_success: Option<Instant>`, `cooldown_until: Instant`.
  Two consecutive failures mark the endpoint `down` for a 60-second cooldown;
  a single success marks it `up` immediately.

- **Selection algorithm** (`UpstreamSelector::select`): walk the list in order
  (priority). First alive endpoint wins; if none alive, first out of cooldown
  gets a probe; if all down, fail open to the primary and record `all_down`.

- **Observation points**: every upstream exchange in `buffered()` and
  `streaming()` reports success/failure to the selector via
  `observe_upstream()`; the `catalog()` models refresh path also reports.

- **Metrics**: `nimproxy_upstream_endpoint_health{endpoint}` gauge (1.0 alive,
  0.0 down), `nimproxy_upstream_all_down_total` counter.

- **Configuration**: `StoredConfig.upstream.upstreams` — an ordered `Vec<String>`
  that defaults to `[base_url]` for backward compatibility. The settings UI
  (dashboard) and `POST /api/settings/upstream` accept the list. The `base_url`
  field is kept as the primary; the list overrides when non-empty.

## Consequences

- Zero added RPM budget: health is entirely passive.
- A downed endpoint is detected within at most `models_ttl` (default 600s) of
  the next real request, plus the `COOLDOWN` (60s) before it can be probed
  again.
- The fail-open posture (serve the primary when all are down) means the proxy
  never refuses to proxy a request due to upstream health alone — the upstream
  will still return 429 if it's genuinely throttled.
- The dashboard's `/api/dashboard/now` response includes `upstream_health`
  (URL, alive, failures, last_success) for operator visibility.
- The existing key-level failover (lane-level bench + retry) is unchanged and
  composes: the upstream selector picks the endpoint, then the pool picks the
  key within that endpoint.

Related: [streaming-pipeline](../architecture/streaming-pipeline.md),
[key-pool](../architecture/key-pool.md).