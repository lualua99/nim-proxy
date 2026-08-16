---
type: Runbook
title: Configuration reference
description: Compose publishing, the container-level env vars, and Settings.
tags: [configuration]
timestamp: 2026-07-04T00:00:00Z
---

# Configuration

**App-level configuration lives in the dashboard**, not env vars. A fresh
install runs with defaults (no keys — the pool starts empty); Settings edits
everything and persists it to `DATA_DIR/config.json` (atomic, 0600 — see
[ui-managed-config-store](../decisions/ui-managed-config-store.md)). Env covers
**deployment-level concerns only**. There is no wizard and no auth: /v1, the
dashboard, and /metrics are open to whoever can reach the port
([single-user-local-build](../decisions/single-user-local-build.md)).

## Compose-only publish setting

`PUBLISH_HOST` controls the host interface where Docker Compose publishes
container port 8000. It defaults to `127.0.0.1`, keeping a bare deployment
loopback-only. Since there is **no authentication**, keep this loopback, or
point it at `0.0.0.0` only inside a VPN/firewall. Compose consumes this value
while interpolating `docker-compose.yml`; nim-proxy itself does not read it.

## Environment variables

| Variable | Default | Change it when… |
|---|---|---|
| `HOST` | `0.0.0.0` | Bind loopback-only (`127.0.0.1`) for bare-metal local — this is now the *only* safeguard |
| `PORT` | `8000` | Port conflicts |
| `DATA_DIR` | `data` (image sets `/data`) | Non-Docker layouts. Must be writable — the config store *and* history live here; an unwritable dir is a **hard boot error** |
| `RUST_LOG` | `nim_proxy=info` | Debugging (`nim_proxy=debug`) |

(`HISTORY_SAMPLE_SECS` also exists as an undocumented test knob; 5 minutes is
the contract.)

## Everything else → Settings

NIM keys (per-key rpm, enable/disable), the upstream base URL, limits
(max_wait, heartbeat, stream_idle, request_timeout, models_ttl, max_inflight,
strict_passthrough), pricing, the default dashboard window, history retention
days, the availability SLO, the model-pressure governor, and the slot
dispatch policy all live in the store and are edited from the dashboard. A
Settings save validates the complete candidate, writes `config.json`
atomically, and swaps the live configuration; no restart is needed.

The file is otherwise **boot-read**, not watched. An out-of-band edit to
`DATA_DIR/config.json`—by an operator, deployment tool, or mounted secret
writer—does not update the running process and requires a restart. Use the
Settings API for live changes.

The default dashboard window and retention are separate settings and both
default to 30 days. The default window must be at least one day. Retention `0`
is unlimited; finite retention must be at least the default window. The SLO
must be a finite percentage greater than 0 and at most 100. A combined save is
all-or-nothing: any invalid field leaves the persisted and live configuration
unchanged. Reducing retention trims the visible index immediately and
schedules atomic background compaction of `history.jsonl`.

**Legacy env vars are ignored.** `NIM_API_KEYS`, `NIM_BASE_URL`, `RPM_PER_KEY`,
`MAX_WAIT_SECS`, `HEARTBEAT_SECS`, `MODELS_TTL_SECS`, `STREAM_IDLE_SECS`,
`REQUEST_TIMEOUT_SECS`, `STRICT_PASSTHROUGH`, `REF_PRICE_IN`/`REF_PRICE_OUT`,
`HISTORY_DAYS`, and `MAX_INFLIGHT` no longer do anything; a set-but-ignored one
gets a single boot warning (`ignoring legacy env vars (…) — these settings live
in the dashboard now`). There is no seed-from-env and no migration (there were
no deployments to migrate).

## Gotchas

- **Security of the trust-boundary move**: with no auth, anyone who can reach
  the port can edit settings and spend the pool. Bind loopback (default in
  Compose) or stay behind a VPN/firewall. See
  [single-user-local-build](../decisions/single-user-local-build.md).
- A **corrupt or unreadable store is a hard boot error**, never a silent
  fall-through to defaults (that would discard configured keys). Restore from
  backup or deliberately delete the file.
- Rate state is in-memory: **one instance per key set**, never two replicas
  sharing keys.
- Per-key rpm is per *rolling* minute with a built-in safety margin
  ([why](../decisions/window-jitter-margin.md)) — don't add your own headroom.
- `TRUST_PROXY` was removed with the session cookies (v0.7.0).