---
type: Decision
title: Dashboard model catalog view
description: A new Catalog sidebar tab surfaces the upstream NIM /v1/models list to operators, served by a session-gated /api/models endpoint that shares the proxy's TTL cache and refresh path.
tags: [dashboard, models, catalog, cache]
timestamp: 2026-08-09T00:00:00Z
---

# Dashboard model catalog view

## Context

Operators pairing new harnesses with this proxy need the exact `id` strings
their harness configs must reference. Before this change nothing surfaced the
upstream NIM `/v1/models` catalog anywhere in the UI:

- The Models tab is *telemetry* (per-model metrics aggregated from request
  labels), not the upstream directory — a fresh instance with zero traffic
  shows "No traffic yet" and nothing about what NIM actually serves.
- The catalog itself was already fetched, cached (TTL, default 600s) and
  served to harnesses at `/v1/models`, but only the bytes existed in a
  mutex-guarded cache — there was no authenticated endpoint a browser could
  read, and the NIM schema
  ([nim-models-endpoint-schema](../research/nim-models-endpoint-schema.md))
  guarantees exactly four fields: `id`, `created`, `object`, `owned_by`.

## Options

1. **Client-facing `/v1/models` passthrough in the dashboard.** Point the UI
   at the existing keyed endpoint. Rejected: it requires a client API key and
   exposes /v1 semantics to the dashboard; the session gate is the right
   boundary for UI traffic.
2. **New dashboard endpoint, separate cache/refresh path.** Duplicates the
   fetch/retry/bench logic and risks conflicting cache states between the two
   surfaces.
3. **Shared catalog accessor + session-gated `/api/models`.** One code path
   (`catalog()` in `src/proxy.rs`) handles "serve fresh cache, else take one
   rate-limited slot and fetch upstream once"; `/v1/models` re-emits the raw
   bytes it always did, `/api/models` re-parses the same cached body into
   JSON plus cache metadata (`cached_at`, `ttl_secs`).

## Choice

Option 3. `src/proxy.rs` gains:

- `catalog(state, cfg, force)` → `Catalog { body, cached_at, ttl_secs }`
  or `CatalogError::{Upstream, Unavailable}` — the cache-check/refresh core,
  unchanged for `/v1/models` (same status codes, same rate-budget discipline:
  the lock is held across the refresh so concurrent misses make one upstream
  call).
- `GET /api/models` under the session gate (`require_session` in
  `src/lib.rs`), with an opt-in `?refresh=1` that bypasses the TTL once (the
  operator's Refresh button). Any authenticated user may read it — the
  catalog is not sensitive.
- Entries are filtered at ingest by the same 256-char id cap as metric model
  labels (`MODEL_LABEL_CAP`); malformed bodies degrade to an empty list.

The dashboard adds a **Catalog** sidebar tab (data-tab `catalog`, after
Overview): a freshness header (cached age vs upstream, TTL, one-line schema
note), a copy-per-id list rendered with the existing `publisher()` /
`chipHtml()` identity machinery, and empty/error states. It loads on tab
entry, re-fetches quietly every 60s while open, and never rides the 3s
metrics poll (`renderCatalog` lives in `RENDERERS` but only fetches its own
data).

## Consequences

- The dashboard refresh also warms the harness-facing cache: a forced
  refresh writes the shared cache, so `/v1/models` answers from it — one
  coherent catalog across both surfaces.
- `?refresh=1` spends rate-limit budget (it takes a pool slot); the UI
  therefore only refreshes on demand and every 60s, never aggressively.
- Forced refresh semantics differ from `?refresh=1` on `/v1/models` — that
  path never forces; the dashboard is the only force entry point.
- XSS posture unchanged: everything dynamic passes `esc()`; id length cap
  and the required-`id` filter bound what the upstream can inject (see
  [input-sanitizing-and-xss](input-sanitizing-and-xss.md)).
- No description/capability display: the schema has none
  ([nim-models-endpoint-schema](../research/nim-models-endpoint-schema.md)),
  so the page shows exactly the four fields plus locally-derived identity.

Related: [dashboard](../architecture/dashboard.md),
[nim-models-endpoint-schema](../research/nim-models-endpoint-schema.md).