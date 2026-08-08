---
type: Decision
title: Request queue with operator termination
description: An in-memory registry of in-flight requests powers a Queue sidebar tab (admin-only) listing who requested what, and a terminate action that kills any request — queued or streaming — with error code -91.
tags: [dashboard, queue, termination, registry, operations]
timestamp: 2026-08-09T00:00:00Z
---

# Request queue with operator termination

## Context

Operators share one proxy among many users (see
[sharing-with-friends](../ops/sharing-with-friends.md)) and have no way to see
what is actually in flight — which user is asking for which model, and how
long each request has sat. When a harness misbehaves (stalls, floods, loops),
the only remedy today is restarting the proxy or editing keys, both of which
kill *everything*. The requested feature: a dashboard entry showing all
in-flight requests and an admin action to terminate any one of them
(including the admin's own), with the killed request receiving error code
`-91` "Your request has been terminated by the system".

The proxy already had everything but the entity:

- Request identity (`client`, `model`, `path`, `started`) is parsed early in
  `proxy::handle()` — exactly the fields a queue view needs; `model`/`client`
  are already sanitized labels (see
  [input-sanitizing-and-xss](input-sanitizing-and-xss.md)).
- Cancellation infrastructure existed in pieces: the streaming loop already
  `select!`s on client disconnect; `reserve_slot` / `acquire_model_permit`
  accept `on_wait` heartbeat callbacks that can bail out; reqwest futures are
  safely cancellable.
- Admin gating, session cookies, and the tab mechanism were all proven by the
  Catalog work ([dashboard-model-catalog](dashboard-model-catalog.md)).

## Options

1. **Scrape the dispatcher internals for queue data.** Rejected: the
   dispatcher knows slots, not requests; requests spend their life in the
   pool, the model governor, and the streaming task. No single existing
   structure holds "one row per live request".
2. **Registry keyed by nothing but metrics labels.** Rejected: metrics are
   aggregated counters; a *mutable* per-request entity was needed, with a
   Drop-guaranteed cleanup so the queue can never leak entries.
3. **Dedicated `RequestRegistry` in `src/registry.rs`** — an in-memory
   `HashMap<u64, Entry>` guarded by `RwLock`, ids monotonic from an
   `AtomicU64`, each entry carrying a `tokio::sync::watch` channel pair that
   is the terminate signal. Registration happens in `handle()` right after
   the context is parsed; a Drop guard unregisters on every exit path (the
   same scopeguard pattern as `dispatch.rs`).

## Choice

Option 3, wired as:

- **Entry**: `{ id, client, model, path, started, phase, kill }` where
  `phase` is one of `queued` (waiting for a slot or model permit) /
  `upstream` (request sent) / `retry` (riding out a wait). No content —
  prompts, tools, usage are never recorded (privacy posture of
  [request-shape-metrics](request-shape-metrics.md) holds: counts and
  metadata only).
- **Kill switch**: terminate = `kill.send(true)`. The watch's
  `changed()`-then-`borrow()` pattern is edge-triggered but self-correcting
  (a pre-check covers kills that landed before a checkpoint; any later kill
  resolves immediately). Checkpoints: (a) the streaming select gets a third
  branch; (b) the `on_wait` heartbeat closures check `*kill.borrow()` and
  bail silently (a queued request dying costs nothing — no slot was
  reserved); (c) `buffered()` wraps its upstream `.send()` in a select with
  the kill branch. The residual race — killed after a slot is taken but
  before the request goes out — spends one window count on the victim's key
  (µs window, documented, accepted).
- **Wire contract**: streaming requests already committed to 200 get the SSE
  error event `{"error":{"message":"Your request has been terminated by the
  system","type":"proxy_error","code":"-91"}}` followed by `data: [DONE]`;
  buffered requests answer HTTP 400 with the same JSON envelope (termination
  is a human decision — not retryable). The `-91` code is a string in the
  OpenAI-compatible error envelope, matching what agent harnesses already
  surface to users.
- **API**: `GET /api/queue` → `{requests: [...]}` and
  `POST /api/queue/terminate` `{id}` → `{"ok": true}` or 404, both under
  `require_session` with a server-side admin check (403 otherwise). Terminate
  applies to any request in the registry — no self-exemption, as required.
  Both endpoints touch only the registry: zero rate budget, zero upstream.
- **Dashboard**: a **Queue** sidebar tab (`data-tab="queue"`) visible only
  to admins — `/api/dashboard/now` now reports `role` so the button is
  hidden for plain users (server still enforces 403; no privilege widening).
  KPI strip (in-flight total, queued count), a table of age · client
  (monogram chip) · model (pretty name + publisher chip) · path · phase ·
  terminate button, plus empty/error states. Polls `/api/queue` at 2s only
  while the tab is visible.

## Consequences

- Operators can now kill runaway work without touching config or keys; the
  killed harness receives a proper OpenAI-style error instead of a dropped
  socket.
- Kills land on a stream only when the client reads: the response is pushed
  through the same bounded SSE channel as data, so a totally stalled client
  sees the `-91` the moment it drains. The registry entry lives as long as
  the request task does — that is the honest "still working" signal.
- A queued request killed before getting a slot consumes nothing — global
  FIFO discipline ([global-fifo-dispatcher](global-fifo-dispatcher.md)) is
  preserved. An in-flight kill wastes its key's window slot already spent,
  which is unavoidable and rare.
- `nimproxy_terminated_total{by}` records admin actions (labels bounded by
  user count, safe cardinality); request logging marks the record
  "terminated" for audit.
- Fresh instances start with an empty registry (no persistence); after a
  restart the queue view simply shows live activity only.

Related: [dashboard](../architecture/dashboard.md),
[auth-posture-and-dashboard-password](auth-posture-and-dashboard-password.md),
[input-sanitizing-and-xss](input-sanitizing-and-xss.md).