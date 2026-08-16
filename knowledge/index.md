---
type: Index
title: nim-proxy knowledge base
description: Catalog of every page in this Open Knowledge Format bundle.
timestamp: 2026-07-02T00:00:00Z
---

# nim-proxy knowledge base

The project's compiled memory: design decisions with their reasoning,
validated research about NVIDIA NIM, per-component architecture, and
operational runbooks. Maintenance rules live in [AGENTS.md](../AGENTS.md);
the chronology in [log.md](log.md).

## Decisions — why the design is what it is

| Page | One-liner |
|---|---|
| [sliding-window-not-token-bucket](decisions/sliding-window-not-token-bucket.md) | Exact 40-per-rolling-60s window; GCRA-style buckets allow a double burst |
| [window-jitter-margin](decisions/window-jitter-margin.md) | 61s window: load test proved delivery jitter trips a strict upstream at 60s |
| [global-fifo-dispatcher](decisions/global-fifo-dispatcher.md) | One queue for all clients; polling races starve long waiters |
| [dispatcher-policy-modes](decisions/dispatcher-policy-modes.md) | The queue schedules by policy: FIFO (default), EDF (deadline order), or Fair (weighted rounds + aging) — swapped live from settings |
| [sticky-affinity-with-spillover](decisions/sticky-affinity-with-spillover.md) | Conversations pin to one key for prefix cache; throughput beats locality when full |
| [sse-heartbeats-for-rate-waits](decisions/sse-heartbeats-for-rate-waits.md) | Commit to 200 SSE + comment heartbeats so harnesses never see a 429 |
| [history-retention-days-not-size](decisions/history-retention-days-not-size.md) | Time-based retention matches report intent; real operation disproved the fixed snapshot-size estimate |
| [reset-aware-dashboard-history](decisions/reset-aware-dashboard-history.md) | Generic startup index, explicit boot epochs, exact typed rollups, and one analytical window |
| [distroless-scratch-image](decisions/distroless-scratch-image.md) | Static musl binary with baked-in TLS roots; FROM scratch, non-root, --health probe |
| [usage-injection-auto-fallback](decisions/usage-injection-auto-fallback.md) | Inject stream_options for exact tokens; 400 → retry untouched and remember |
| [auth-posture-and-dashboard-password](decisions/auth-posture-and-dashboard-password.md) | Fail closed without auth; API keys + a shared-password dashboard session — v0.7.0 single-operator build has no auth plane |
| [input-sanitizing-and-xss](decisions/input-sanitizing-and-xss.md) | Sanitize client `model`/`path` labels; escape + CSP the dashboard (XSS/cardinality/log-injection) |
| [request-shape-metrics](decisions/request-shape-metrics.md) | Capture agent-behavior & quality signal as bounded metrics — counts, never content — for benchmarking |
| [dashboard-operator-console-redesign](decisions/dashboard-operator-console-redesign.md) | 6→5 tabs (Compare merged in), dual light/dark theme (dark-only superseded), Inter via Google Fonts CDN under CSP, window-halves delta chips |
| [ui-managed-config-store](decisions/ui-managed-config-store.md) | App config moves from env into a JSON store edited from the dashboard; wizard/multi-user/ownership retired in v0.7.0 |
| [explicit-request-deadline](decisions/explicit-request-deadline.md) | Opt-in wall-clock bound cancels queue/retry/generation work without weakening patient defaults |
| [dependency-update-cooldown](decisions/dependency-update-cooldown.md) | Routine dependency updates wait seven days; security updates remain immediate |
| [single-user-local-build](decisions/single-user-local-build.md) | v0.7.0 drops the multi-user/auth plane for a single-operator local build; /v1 and the dashboard are open, trust = bind address |
| [dashboard-model-catalog](decisions/dashboard-model-catalog.md) | A Catalog sidebar tab surfaces the upstream model list via a session-gated /api/models sharing the proxy's TTL cache |
| [request-queue-and-termination](decisions/request-queue-and-termination.md) | In-memory registry of live requests; admin Queue tab terminates any request with error code -91 |
| [persist-rate-windows-across-restart](decisions/persist-rate-windows-across-restart.md) | Rate windows + governor caps survive restart via versioned JSONL; fresh restore arms a slow-start ramp |
| [graduated-backpressure](decisions/graduated-backpressure.md) | Estimate queue wait; accept under threshold (ETA header), reject at or above it (503 + Retry-After). Deadline requests exempt |
| [multi-upstream-failover](decisions/multi-upstream-failover.md) | Ordered upstream list with passive health detection; fail over when the primary goes down, back when it recovers, zero extra RPM |
| [realtime-dashboard-sse](decisions/realtime-dashboard-sse.md) | Dashboard swaps 3s polling for a server-sent-events stream (full snapshots, v1); poll kept as fallback, bounded connections |
| [response-cache](decisions/response-cache.md) | Cache non-streaming idempotent requests (SHA256 key, moka) to skip the rate-limit queue and upstream call, saving RPM |

## Research — validated external facts

| Page | One-liner |
|---|---|
| [nim-free-tier-40rpm-no-credits](research/nim-free-tier-40rpm-no-credits.md) | NVIDIA staff: trial usage is not credit-based, ~40 RPM per key governs |
| [nim-kv-cache-reuse](research/nim-kv-cache-reuse.md) | NIM supports prefix caching (~2x TTFT); hosted scope undocumented, likely per-account |
| [nim-models-endpoint-schema](research/nim-models-endpoint-schema.md) | /v1/models returns only id/created/object/owned_by — card visuals need local enrichment |

## Architecture — how each component works

| Page | One-liner |
|---|---|
| [key-pool](architecture/key-pool.md) | Per-key sliding-window lanes; least-loaded selection; cooldown benching |
| [dispatcher](architecture/dispatcher.md) | Policy slot queue (fifo/edf/fair); abandoned-waiter slot return; affinity accounting |
| [governor](architecture/governor.md) | Per-model concurrency gate; classifies worker exhaustion apart from 429s and backs off the model, adaptively |
| [streaming-pipeline](architecture/streaming-pipeline.md) | Heartbeats, retry/failover, absolute deadlines, idle timeout, SSE usage scanning |
| [metrics-history](architecture/metrics-history.md) | Prometheus registry + versioned JSONL, reset-aware startup index, exact rollups, and atomic retention |
| [dashboard](architecture/dashboard.md) | Embedded operator console; one persisted window across 5 tabs plus clearly scoped Now values. v0.7.0: no session gate, auth tabs removed |
| [client-auth](architecture/client-auth.md) | `/v1` client keys (open/keyed) + store-backed multi-user dashboard sessions; fail-closed posture — **retired in v0.7.0** |

## Operations — runbooks

| Page | One-liner |
|---|---|
| [deploy-docker](ops/deploy-docker.md) | Compose, volume, healthcheck, hardening flags |
| [configure-env](ops/configure-env.md) | Compose publishing, the container env vars, Settings, and the open-to-the-network posture |
| [sharing-with-friends](ops/sharing-with-friends.md) | Create-a-user multi-user setup, key etiquette, ToS positioning — **retired in v0.7.0** |
| [capacity-math](ops/capacity-math.md) | What N clients on K keys actually does (the 50-clients/3-lanes analysis) |

## Testing

| Page | One-liner |
|---|---|
| [test-strategy](testing/test-strategy.md) | Unit / e2e / load layers, what each catches, how to run them |
