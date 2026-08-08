---
type: Decision
title: Dashboard operator-console redesign
description: 6-tab IA collapsed to 5 (Compare merged in, three renames), dark-only palette, webfonts via Google Fonts CDN under an extended CSP, and window-halves delta-chip semantics.
tags: [dashboard, dataviz, frontend, csp]
timestamp: 2026-07-03T00:00:00Z
---

# Dashboard operator-console redesign

## Context

The dashboard's data layer and charts already worked; the presentation had
grown organically across three prior passes (three tabs → six persona-aligned
tabs → incremental polish) and needed a deliberate visual pass. A prototype
handoff specified a dark, NVIDIA-green
"operator console" aesthetic with a revised information architecture, richer
KPI cards, and two new interactions (chart hover tooltips, click-to-sort
tables) — all layered on the **unchanged** `parseProm`/`groups`/`buckets`/
`wgroups` data machinery. Four decisions needed to be locked in before
implementation; this decision page is their durable record.

## Options & Choice

### (a) Information architecture: keep six tabs or merge

- **Keep six** (`Overview · Models · Compare · Harnesses · Proxy · Keys`).
- **Merge Compare into Models, rename three tabs** (chosen): `Overview ·
  Models · Clients · Reliability · Capacity`. Compare's two visuals (the
  best-in-column scorecard, the tok/s bar race) become the last section of
  Models — a comparative view is still one scroll away from every other model
  number, and Compare never carried enough unique content to earn a whole
  tab. `Harnesses → Clients`, `Proxy → Reliability`, `Keys → Capacity` rename
  each tab to what it actually answers ("what are my agents doing", "is the
  proxy healthy and where does time go", "am I near my limits") rather than
  the noun of the thing it lists — closer persona alignment for an operator
  scanning the sidebar under pressure.

### (b) Dark-only vs. keeping light mode

- **Keep both palettes** with `prefers-color-scheme` — status quo.
- **Dark only** (chosen): the design is a committed dark aesthetic (NVIDIA
  green on near-black), matching the "operator console" convention this
  redesign is aiming for — think Grafana/Datadog dark dashboards, not a
  general-purpose light-first app. Deleting the light palette and the
  `prefers-color-scheme` machinery halves the token surface (one set of CSS
  variables instead of two) and removes an entire untested combinatorial
  dimension (light × hover, light × sort-highlight, …) from every future
  change.

### (c) Webfonts: Google Fonts CDN vs. embed vs. system fonts only

- **System fonts only** — zero risk, zero network dependency, but gives up
  the typographic identity (Space Grotesk headings, Spline Sans Mono numerics)
  that the design spec treats as load-bearing.
- **Embed the font files** in the binary — no CDN dependency, but Space
  Grotesk + Spline Sans Mono at the weights used (400/500/600/700 and
  400/500/600) add an estimated ~150–250KB, working against the project's
  "tiny binary" identity (the whole image is ~3.5–4.6MB `FROM scratch`) for a
  cosmetic upgrade.
- **Google Fonts CDN with a system-font fallback** (chosen): matches the
  precedent already set by the LobeHub model-logo CDN — an optional external
  resource with graceful degradation when unreachable, not a hard dependency.
  Zero binary growth. Requires two CSP additions: `style-src` gains
  `https://fonts.googleapis.com` (the stylesheet `<link>`), and a new
  `font-src` allows `https://fonts.gstatic.com` (the actual font files). Both
  are additive and don't touch `connect-src` (still `'self'`) or
  `frame-ancestors` (still `'none'`), so the exfiltration- and
  clickjacking-blocking properties documented in
  [input-sanitizing-and-xss](input-sanitizing-and-xss.md) are unaffected.

### (d) Delta-chip semantics (the `+8.2%`-style trend pill on KPI cards)

- **Fetch a second history window** to compute a proper period-over-period
  comparison — most accurate, but doubles `/api/history` calls for every
  range view and adds a round trip live mode doesn't need.
- **Second half vs. first half of the currently visible window** (chosen):
  computed entirely from the sample buffer already in memory (live ring or
  loaded range) in about ten lines — no extra fetch, live or range. It's an
  honest label ("second half of this window vs. the first", shown as a
  tooltip on the chip) rather than a precise week-over-week number, which is
  the right tradeoff for a glanceable trend indicator. Hidden below 4 samples
  where the split would be too noisy to mean anything.

## Consequences

- One render path per tab, fewer primitives than before serving more UI:
  `sortTable` replaces every ad-hoc table builder and the old `scorecard()`;
  `barList`/`leaderList` replace `barRows`/`miniList`; `ringGauge` replaces
  `arcGauge`. The slot-based categorical color allocator is gone, replaced by
  publisher/client identity maps with a hash-to-hue fallback.
  See [dashboard](../architecture/dashboard.md).
- `src/main.rs`'s CSP changed by exactly two additive clauses; a new e2e
  assertion pins the `font-src` value alongside the existing CSP checks.
- No backend, metric, or history-format change — every number in the new UI
  maps to a value `render()` already computed before the redesign.
- The security invariant is unchanged: every dynamic string still passes
  through `esc()` before reaching `innerHTML`; the redesign added interaction
  state (sort index, hover index) but no new unescaped sink. The existing
  label-injection e2e test continues to cover this.
- Anyone linking to the old tab anchors/names (`#harnesses`, `#proxy`,
  `#keys`, a "Compare" mention) needs the new names: `#clients`,
  `#reliability`, `#capacity`, and "the scorecard section of Models".

## Amendment (2026-07-28 — reset-aware history)

The visual, information-architecture, font/CSP, interaction, and security
choices above remain current. The statements that the data layer was
unchanged, that delta comparisons used a browser live ring, and that an
alternative would double `/api/history` calls describe the 2026-07-03
implementation and are superseded by
[reset-aware-dashboard-history](reset-aware-dashboard-history.md).

Today every analytical tab consumes the typed `/api/dashboard` range contract
plus `/api/dashboard/now`, shares one following/fixed selection, and computes
delta chips from that selected server-backed sample window. The old
`/api/history` transport and browser-owned lifetime mode no longer exist.

## Amendment (2026-08-09 — dual theme and Inter, superseding (b) and (c))

The dark-only and fonts-via-CDN choices above are superseded by a dual-theme,
font-neutral redesign of `src/dashboard.html`:

- **(b) is reversed: light default + dark.** The `:root` tokens are a light
  palette by default (page `#f8f9fb`, cards `#ffffff`, indigo accent
  `#6366F1`); `:root[data-theme="dark"]` carries the dark set. A dark theme
  follows `prefers-color-scheme` unless the operator pins one via the topbar
  `#theme-toggle`, persisted in `localStorage` under `np-theme` and applied by
  an inline `<head>` script (no flash-of-unstyled-content on reload). The
  toggle shows a sun in dark mode and a moon in light mode.
- **(c) is replaced: Inter only.** The Space Grotesk / Spline Sans Mono pair
  is dropped for a single Inter family (UI + numeric via `--mono`), still
  loaded from Google Fonts under the unchanged CSP with a system-font
  fallback.
- **Color roles renamed, JS stays theme-free.** The semantic constants the
  renderers emit are `--brand` (indigo), `--green`/`--amber`/`--red`
  (success/rate/error), and `--med`/`--p95` (chart quantiles). `MED`/`P95`
  and the `gGreen`/`gMuted` SVG gradients now read the theme variables via
  `css()` / `var(--…)` instead of hardcoded literals, so the JS needs no color
  literals and switching themes recolors every chart live. NVIDIA brand
  `#76B900` remains only as a publisher/logo identity constant, not a theme
  token.
- **Security invariant unchanged**: still fail-closed auth, `esc()` on every
  `innerHTML` sink, strict CSP. The theme toggle adds no new sink (attribute
  set on `<html>`, `textContent`-free).
