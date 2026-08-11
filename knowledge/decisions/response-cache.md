---
type: Decision
title: Response cache for idempotent requests
description: Cache non-streaming, idempotent POST requests to save RPM budget
  by serving repeat requests from an in-memory cache.
tags: [cache, rpm, performance, moka, sha256]
timestamp: 2026-08-11T00:00:00Z
---

# Response cache for idempotent requests

## Context

Every request to a NIM key consumes one RPM slot. Many agent harnesses repeat
the same request (an embedding, a fixed-prompt classification, a catalog-adjacent
probe) — the response is byte-for-byte reusable, but the proxy spends a full
RPM slot on each repeat. The pacing queue (see [global-fifo-dispatcher](global-fifo-dispatcher.md))
can backlog under high concurrency even when the work is redundant.

The existing `models_cache` (`GET /v1/models`) uses a different, simpler caching
strategy (a `Mutex<Option<(Instant, Bytes)>>` with single-flight refresh). A
general response cache needs a more scalable design.

## Options

1. **No caching** — status quo, every duplicate request burns an RPM slot.
2. **Full-body semantic hash with moka** — SHA256(model + path + canonical body)
   as key, moka as the eviction/expiry engine.
3. **Parameter-subset key** — extract only `model` + `messages` + `seed` etc.
   as the key, ignoring irrelevant params.
4. **External cache (Redis)** — share cached responses across proxy instances.

## Choice

Option 2: full-body semantic hash with moka.

## Consequences

- **Positive**: Full-body key is simple and never wrong — two requests with the
  same body always produce the same response. No risk of a partial-key collision
  serving a wrong response.
- **Positive**: moka handles TTL-based expiry, LRU eviction, and concurrent access
  with no async overhead.
- **Positive**: The cache lives in `AppState`, is cheap to rebuild on settings
  changes, and survives a drop/recreate cycle.
- **Negative**: Even irrelevent parameter changes (e.g. `frequency_penalty`)
  produce a different key and miss the cache. Acceptable because most harnesses
  send identical bodies.
- **Negative**: Memory-only — a restart loses the cache, and it doesn't share
  across instances. Acceptable for the single-instance deployment model.
- **Negative**: The cache hit path skips rate-limit accounting entirely, which
  changes the behavior of rate-limit tests (the e2e suite defaults to cache
  disabled to avoid this).

## Implementation notes

- Cache is in `src/cache.rs` as `ResponseCache` wrapping `moka::sync::Cache`.
- Key is `SHA256(model + \\0 + path + \\0 + body)`.
- Only `POST /v1/chat/completions` and `POST /v1/embeddings` with `stream: false`
  are cacheable.
- Requests with `X-Nim-Proxy-Deadline-Ms` are never cached (one-shot semantics).
- Cache write happens in `buffered()` after a successful relay (status 2xx).
- Cache TTL and max entries are configurable from the dashboard (Server tab →
  Response caching card).
- Metrics: `nimproxy_cache_hits_total{path}`, `nimproxy_cache_misses_total{path}`,
  `nimproxy_cache_entries` (gauge).