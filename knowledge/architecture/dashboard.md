---
type: Component
title: Dashboard (src/dashboard.html)
description: Single embedded operator console with one persisted analytical window, typed range/current contracts, and clearly scoped Now values.
tags: [dashboard, dataviz, frontend]
timestamp: 2026-07-03T00:00:00Z
---

# Dashboard — `src/dashboard.html`

One self-contained HTML file compiled into the binary (`include_str!`), with
no frontend build or Grafana and no required external assets. Optional CDN
logos/fonts have offline fallbacks. A dark, NVIDIA-green "operator console":
a 216px sticky sidebar (collapses to an icon-only rail below 860px) with the
nav and follow-state/uptime/version footer, a top bar with range pills + a
custom date-range picker, and seven persona-aligned tabs, each ordered
**at-a-glance → trends → detail**:

- **Overview** (landing, balanced) — KPI cards + threshold ring gauges,
  request/token/savings sparklines, a health strip, a p50/p95 performance
  band, top models & harnesses.
- **Catalog** (provisioner) — the upstream NIM `/v1/models` directory as a
  copy-per-id list with publisher chips, cache-freshness header, and an
  operator Refresh (`?refresh=1`); reads the session-gated
  `/api/models` endpoint that shares the proxy's catalog cache (see
  [dashboard-model-catalog](../decisions/dashboard-model-catalog.md)).
- **Models** (benchmarker) — KPI cards, tokens/min-by-model chart, a
  TTFT/tok-s/TPOT/upstream quantile quad, a "how responses end" breakdown,
  reasoning-vs-output share, a head-to-head scorecard with best-in-column
  highlighting and a tok/s bar race (this section absorbed the former
  Compare tab), leading-model cards, and the full per-model table.
- **Clients** (agent analyst, was **Harnesses**) — per-client tool intensity,
  conversation depth, sampling fingerprint, streaming mix, leaderboard.
  Driven by the per-client request-shape metrics
  ([request-shape-metrics](../decisions/request-shape-metrics.md)).
- **Reliability** (operator, was **Proxy**) — a hero row (availability vs SLO,
  latency composition, live load + error taxonomy), request/outcome/load
  charts, queue-wait quantiles, an hour-of-day heatmap, a non-success-outcome
  breakdown, a reliability & security panel, a request-types panel, per-client
  table.
- **Capacity** (capacity planner, was **Keys**) — a hero row (saturation,
  provisioning, rate-limit pressure), lane utilization meters, 429s/min by
  lane, per-lane table.

The tabs are **identical for every role** — all authenticated users see the
same observability, the deliberate shared-pool-among-friends model. v0.6.0 adds
a **Settings** area (its own sub-nav: Access & keys · Server · Users · Account;
Server/Users hidden for the `user` role) that reads role-filtered data from
`GET /api/config` (hidden sections are absent from the payload, not CSS-hidden —
see [client-auth](client-auth.md) and
[ui-managed-config-store](../decisions/ui-managed-config-store.md)), and a
**Model pressure** card on Reliability (worker-exhaustion rate + per-model
`inflight vs limit` rows) that appears only once the
[governor](governor.md) has engaged.

Every analytical tab shares one selected history window. **Default · Nd**
(30d by default) follows now using the configured default, relative presets
follow now over their duration, and **All retained** follows the server's
current retained boundary. Custom ranges are fixed. Clicking the sidebar
follow control freezes the currently rendered window; clicking again resumes
its preset. Settings hides the range controller because it is
configuration-driven rather than an analytical view.

The former **Compare** tab (head-to-head scorecard + bar race) was folded
into Models as a section — it never carried enough unique content to justify
a sixth tab. See
[dashboard-operator-console-redesign](../decisions/dashboard-operator-console-redesign.md)
for the rationale behind the IA change and the dark-only, fonts-via-CDN, and
delta-chip decisions.

## Rendering primitives

All tabs share one set of primitives (`render()` computes cross-tab
aggregates once, then only the active tab's renderer runs, so hidden charts
size to a real `clientWidth` when their tab is switched to):

- **`lineChart`** — full-bleed SVG plot (no left gutter; y-axis labels are
  right-edge overlays), hairline grid, 2px lines, optional gradient area
  fill, end dots. Hover snaps to the nearest real sample (not a uniform
  index) and draws a crosshair + a dot per series + a tooltip card with a
  timestamp header; the last hovered pointer position is re-applied after
  the 3s live re-render so the tooltip doesn't flicker away.
- **`sortTable`** — replaces every ad-hoc `<table>` builder and the old
  `scorecard()`. Sticky `<thead>`, click-to-sort (numeric or string aware,
  asc/desc toggle), active header turns green with a `↑`/`↓` arrow, header
  alignment matches its column's cell alignment, capped height with an
  internal scroll, optional per-column `best:'min'|'max'` highlighting.
  Sort state lives in a global `Map` keyed by table id, and the table's
  scroll position is saved/restored around the `innerHTML` swap — so neither
  resets on the 3s live poll.
- **`ringGauge`** (replaces `arcGauge`) — a 76px threshold-colored circle
  with a centered percentage, label, and mono sub-line.
- **`kpiCard`** — icon + label, an optional trend delta chip, a big value,
  a mono sub-line, and a bottom-pinned gradient sparkline.
- **`barList`** / **`leaderList`** — one shared row primitive for every
  labeled progress bar and leaderboard row (name, track, chip-colored fill,
  mono value); replaces the old `barRows`/`miniList` near-duplicates.
- **`heatmap`** — same weekday×hour matrix math as before, now a sequential
  green ramp (`#141A0E→#233312→#33501A→#4E7A0F→#76B900→#A7D65A`) instead of
  blue, with per-cell hover tooltips; the table-view toggle was dropped (not
  in the final design).

Colors follow the entity, not the chart: models take their publisher's brand
color from the `PUBLISHERS` map (extended with StepFun and a Moonshot teal);
known harnesses (`claude-code`, `aider`, `opencode`, `cline`, `continue`,
`cursor`, `roo-code`, `zed`, `codex`, `n8n`) take a fixed client-color map;
anything else — and lane colors, which use six fixed slot colors — falls
back to a stable hash-to-hue (`hueFor`). The old first-six-slots categorical
allocator (`modelSlots`/`slotFor`) is gone; there's no "ran out of colors"
case left to handle.

**Dual theme.** The app is light by default with a dark theme under
`prefers-color-scheme`; the topbar `#theme-toggle` pins either mode, persisted
in `localStorage` (`np-theme`) and applied by an inline `<head>` script so a
reload never flashes the wrong theme. `:root` carries the light tokens (page
`#f8f9fb`, cards `#ffffff`, indigo accent `#6366F1`); `:root[data-theme="dark"]`
replaces them (page `#111625`, cards `#1A2135`, indigo `#818CF8`). The renderers
emit semantic roles — `--brand`, `--green`/`--amber`/`--red`, `--med`/`--p95` —
and both `MED`/`P95` and the `gGreen`/`gMuted` SVG gradients resolve them via
`css()`/`var(--…)`, so the JS holds no color literals and every chart recolors
live on toggle. This reverses the earlier dark-only choice; see the amendment in
[dashboard-operator-console-redesign](../decisions/dashboard-operator-console-redesign.md).

**Fonts**: Inter (UI and, via `--mono`, all numeric values/axis labels/table
cells) loads from Google Fonts via `<link>`, allowed by an extended CSP
(`style-src` gains `https://fonts.googleapis.com`, a new `font-src` allows
`https://fonts.gstatic.com`). Offline or CDN-blocked, the CSS falls back to
`system-ui`/`monospace` — same graceful-degradation pattern as the LobeHub
logo CDN. The NIM Proxy logo mark itself is inlined as a base64 data URI
(68×68 PNG, ~10KB) in the sidebar, so it never depends on the network.

## Data flow

Two authenticated typed endpoints replace browser parsing of raw exposition:

- `GET /api/dashboard?from&to&points` returns exact totals, chart buckets,
  latest gauges, effective/available bounds, sample-time capacity, diagnostics,
  and both history/config revisions. Omitting `to` uses now and marks the
  request as following; omitting `from` also selects
  `now - default_window_days`.
- `GET /api/dashboard/now` is lightweight current state: live metrics,
  current capacity/config/SLO, retained bounds, revisions, and the
  post-persistence counter tail.

The retired `/api/history` and `/dash/config.json` routes are absent. Raw
`/metrics` remains available to authenticated Prometheus scrapers, not as a
dashboard transport.

`rangeSamples()` adapts the normalized range contract back into the
`samples: [{t, rows}]` cumulative structure the rendering primitives consume.
Every selection begins with a synthetic zero-counter baseline, then applies
server deltas; gauges are replaced rather than accumulated. The exact range
totals overwrite the final historical point. A live tail is accepted only
when its `base_history_revision` matches the selected range, and is replaced
after persistence advances that revision.

The browser polls `/api/dashboard/now` every three seconds. Only a
following, unpaused window refetches history when its revision changes.
Custom and paused totals remain fixed, while **Now** widgets—active requests,
queue depth, current RPM/capacity, enabled lane slots, uptime, and header
metadata—continue refreshing. A config-revision change updates pricing,
capacity, auth state, default window, retention, and SLO without reloading
the page; if the active preset is the default, its following bounds are
recomputed.

**Notable derivations, worth recording so they aren't rediscovered:**

- **Delta chips** (the `+8.2%`-style pill on every KPI card) compare the
  second half of the visible window's average against the first half — an
  honest trend computable from the selected sample buffer, with no extra
  history fetch. Hidden below 4 samples.
- **"Where time goes"** (Reliability hero) splits average end-to-end time
  into queue wait, first token, and generation, where **generation = avg
  `upstream_seconds` − avg `nimproxy_ttft_seconds`** — verified against
  `proxy.rs`: `upstream_seconds` spans send→stream-end, `ttft` spans
  send→first-byte, so the difference is genuinely token-generation time, not
  double-counted latency.
- **Availability** (Reliability hero) uses
  `dashboard.slo_target_percent` from the current server config (99.9% by
  default). HTTP 4xx and disconnect outcomes stay visible in outcome/error
  views but do not consume the service-availability error budget. Capacity
  history uses the contemporaneous value stored with each v2 sample; legacy
  intervals explicitly show capacity unavailable. Active load, lane count,
  current RPM, and utilization are labeled **Now**; selected-window lane
  request and bench counts remain historical.

Following history survives refresh and process restart because it is rebuilt
from the server index; only the adjacent-poll rate shown in **Now** widgets
needs two current polls. Model cards derive identity from the id namespace
([schema research](../research/nim-models-endpoint-schema.md)): LobeHub CDN
logo with brand-colored monogram fallback, ranked by completion tokens.

## Security invariant

Every dynamic string that reaches `innerHTML` — model/client names, tooltip
and legend labels, table cells — passes through the `esc()` HTML-escaper.
The typed history adapter preserves that rule for labels received from both
dashboard endpoints, and scope/status text uses `textContent`. No range,
current, Settings, hover, or sort path introduces an unescaped sink. See
[input-sanitizing-and-xss](../decisions/input-sanitizing-and-xss.md).
