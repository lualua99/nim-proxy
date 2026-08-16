---
type: Decision
title: Commit to 200 SSE and heartbeat through waits
description: Streaming requests get an immediate 200 + comment-line keepalives while the proxy waits out rate limits and retries.
tags: [streaming, sse, retries]
timestamp: 2026-07-02T00:00:00Z
---

# Commit to 200 SSE and heartbeat through waits

## Context

The founding problem: agent harnesses (OpenCode et al.) abort the whole task
when a provider returns 429/5xx. NIM's 40 RPM means long tasks *will* hit the
limit. The proxy must make waits invisible.

## Options

1. Return 429 with Retry-After and hope the harness retries (they don't).
2. Hold the request open silently until a slot frees (client-side timeouts
   kill idle connections).
3. **Commit to `200 text/event-stream` immediately and emit SSE comment
   lines (`: heartbeat`) during any wait.**

## Choice

Option 3 for streaming requests. SSE comment lines are ignored by every
OpenAI-compatible client per the SSE spec, so the harness sees a healthy,
slow stream. Retries/lane failover happen behind the committed response;
non-retryable upstream errors surface as an in-stream `error` event followed
by `[DONE]`.

The trade-off accepted: once committed, the HTTP status is 200 forever — an
upstream 401 arrives as an in-stream error, not a 401. Harnesses handle
in-stream errors; they don't handle mid-task 429s.

Non-streaming requests can't be heartbeated (no wire format for partial JSON)
— they wait silently up to the `max_wait` limit. Documented limitation; agent
traffic streams.

## Consequences

- Long-running OpenCode tasks survive saturation; verified e2e (429s →
  `: retrying` comments → data → `[DONE]`).
- A stalled upstream would hold the committed stream forever, hence the
  `stream_idle` cutoff in the
  [streaming pipeline](../architecture/streaming-pipeline.md).

### Hang-up reaping (2026-08-14)

While a request parks in the permit or slot wait, a harness that gives up and
re-issues the same call must not leave its abandoned row in the operator queue
next to the fresh retry. The heartbeat keeps an alive client seen, but notice
of a dead one used to wait for the next `: heartbeat` tick. The wait now races
`tx.closed()` directly (the SSE channel closes the moment hyper drops the
response body on hang-up), so the abandoned row, its admissions waiters, and
any dispatch queue position are freed within milliseconds, and the ghost can
never go on to win an rpm slot. Buffered requests were already covered by axum
cancelling the stalled handler when the socket closes; a test locks that in.
Only a *closed* channel counts as a client gone — a full frames buffer is a
slow-but-alive reader and simply skips the heartbeat frame.
