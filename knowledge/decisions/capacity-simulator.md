---
type: Decision
title: Capacity what-if simulator
description: >
  Pure-front-end what-if simulator in the Capacity tab that seeds slider
  defaults from a new `/api/dashboard/capacity-model` endpoint and computes
  bottleneck + approximate queue delay via M/M/1 closed form.
tags: [capacity, simulator, what-if, dashboard, frontend]
timestamp: 2026-08-10T00:00:00Z
---

## Context

The Capacity tab already shows historical utilization, current saturation, and
peak shortfall, but it cannot answer "what if I add 2 keys / remove 3 clients /
widen the model gate?" — there is no predictive or extrapolation capability.

The constraints are:
- No frontend build step (single-file `dashboard.html`).
- No new charting library (charts are self-drawn SVG).
- No new stored fields or request content exposure (privacy invariant).
- Any user can see the Capacity tab, so the endpoint must be session-gated but
  not admin-only.

## Options

1. **Full Monte Carlo simulation on the backend** — statistically accurate but
   expensive, heavy, and contradicts the "no build step" frontend constraint.
2. **Fluid/deterministic model on the backend** — a dedicated endpoint returns
   predictions, but the backend would need to own the model logic, making it
   harder to iterate and verify.
3. **Backend data endpoint + pure-frontend computation** — the backend
   aggregates existing observations into a seed-data payload; the frontend
   runs a ~100-line JS model with slider controls. Lightest, fastest, and
   every tweak is instant.

## Choice

**Option 3**: a new `GET /api/dashboard/capacity-model` endpoint (session-gated,
no admin check) returns per-key budgets/calibration, per-client request shape,
per-model governor state, and histogram quantiles (queue wait, TTFT, upstream
time) from the last hour of history. The Capacity tab renders a "What-if" card
with range sliders for keys, per-key RPM, client request rate, model gate cap,
and service time. The JS model computes:

- Throughput = min(key RPM cap, model gate cap, client request rate)
- Bottleneck identified as the argmin
- Utilization = offered / capacity
- Queue delay via M/M/1 closed form: `W = ρ/(1-ρ) × service time`
- P95 ~ 3× avg (exponential tail approximation)

The model is clearly labeled "M/M/1 approximation · trend estimate, not a
promise" to avoid over-commitment.

## Consequences

- **Positive**: instant slider response, no server round-trip per tweak, no
  new dependencies, no new stored fields.
- **Positive**: seed values are always the last hour's observations, so the
  simulation stays grounded in real behavior (drifting service times, changing
  calibration factors).
- **Negative**: M/M/1 is a strong simplification (Poisson arrivals, exponential
  service, single server). For bursty agent traffic the actual delay may be
  higher. The "approximate" label is honest.
- **Negative**: The model gate cap is a single global slider, not per-model.
  A per-model decomposition is a plausible future extension.