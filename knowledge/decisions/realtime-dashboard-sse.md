---
type: Decision
title: Realtime dashboard push over SSE
description: >
  Replace the dashboard's 3-second polling of `/api/dashboard/now` with a
  server-sent-events stream, keeping the poll endpoint as a fallback. v1 sends
  full snapshots; incremental delta push is deferred.
tags: [dashboard, sse, realtime, event-source, frontend, push]
timestamp: 2026-08-10T00:00:00Z
---

## Context

The dashboard has no need to poll: it only ever renders server state, it never
sends realtime data up. Yet `pollNow()` fires `GET /api/dashboard/now` every 3
seconds, and every response rebuilds the full JSON blob (all lanes, metrics
snapshot, history tail, upstream health). That is:

- **Latency**: dashboard updates trail the server by 0-3 seconds.
- **Waste**: up to several KB serialized per client per tick, multiplied across
  tabs and operators, each time re-rendering the same mostly-unchanged JSON.
- **Battery/bandwidth**: on mobile/weak networks an HTTP request every 3s is
  expensive even when nothing changed.

The constraints are:
- No new build step (single-file `dashboard.html`), no new JS library.
- The endpoint must sit behind the existing `require_session` gate.
- The existing CSP `connect-src 'self'` must keep working unchanged.
- axum 0.8 ships SSE natively (`axum::response::sse`) and WebSocket
  (`axum::extract::ws`) — no new dependency either way.

## Options

1. **WebSocket** — bidirectional, but the dashboard never sends realtime data
   up. Requires an upgrade handshake, is harder to proxy/load-balance, and
   needs a JS client library. Wrong tool for a one-way push.
2. **Incremental delta push (JSON Patch style diff)** — only changed fields are
   sent after an initial snapshot. Most bandwidth-efficient, but requires the
   server to track per-client last state and run a diff per tick. Real benefit
   only at very high update rates; adds real complexity.
3. **Server-sent events, full snapshots (v1)** — a new `GET /api/dashboard/stream`
   endpoint pushes the same `api_dashboard_now` JSON every 3 seconds. Browser
   `EventSource` auto-reconnects and needs no library. Minimal change, minimal
   risk.

## Choice

**Option 3**: SSE with full snapshots, keeping `/api/dashboard/now` as a
fallback.

- `src/lib.rs` gains `GET /api/dashboard/stream` on the protected router
  (under `require_session`). Handler `api_dashboard_stream` extracts the payload
  into `dashboard_now_payload(state, username)` (shared with `api_dashboard_now`)
  and pushes it every 3 seconds over a per-connection `mpsc` channel to an
  `Sse` stream with a 15-second text keepalive.
- A process-wide `AtomicUsize` (`AppState::sse_connections`) bounds concurrent
  streams at `MAX_SSE_CONNECTIONS = 100`; exceeding it returns 503
  `too_many_streams`. The counter is decremented when a stream's send loop
  observes the client disconnect (`tx.send` returns `Err`).
- `src/dashboard.html` reuses the poll processing logic via a new
  `handleNowPayload(next)` function. `startSSE()` opens an `EventSource` and
  calls `handleNowPayload` per message; on error it closes the stream and falls
  back to the existing `setInterval(pollNow, POLL_MS)`. A `visibilitychange`
  listener closes the stream when the tab is hidden and reopens it on return.
  The initial `pollNow()` still runs once at boot so the first paint is
  immediate, then SSE takes over.

## Consequences

- **Positive**: dashboard updates land within ~3s with no per-tick HTTP request
  churn; `EventSource` auto-reconnects across server restarts, so the
  "stop server → fall back to poll → restart → resume SSE" flow is seamless.
- **Positive**: no new dependencies, no CSP change (`connect-src 'self'` already
  covers SSE), no build step.
- **Positive**: the poll path is untouched and still works, so a broken SSE
  connection degrades gracefully instead of breaking the dashboard.
- **Negative**: full snapshots are still pushed every 3s, so the redundant-JSON
  cost is not eliminated — only the transport overhead and artificial latency.
- **Negative**: incremental delta push (Option 2) remains the natural v2 if the
  snapshot cost ever matters at scale; it needs a diff algorithm and per-client
  state tracking the current design deliberately avoids.

## Related

- [dashboard](architecture/dashboard.md) — the embedded operator console this
  endpoint feeds.
- [auth-posture-and-dashboard-password](auth-posture-and-dashboard-password.md) —
  the `require_session` gate the stream sits behind.
- [input-sanitizing-and-xss](input-sanitizing-and-xss.md) — the CSP that already
  permits `connect-src 'self'`.