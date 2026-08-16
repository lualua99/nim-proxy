---
type: Decision
title: Single-operator local build (auth removed)
description: Drop the multi-user dashboard, client keys, roles, and first-run wizard for a single local operator; keep the core rate-limit/failover/governor machinery and an open /v1.
tags: [auth, deployment, refactor]
timestamp: 2026-08-14T00:00:00Z
---

# Single-operator local build (auth removed)

## Context

The proxy's operator surface (multi-user sessions, roles, per-key ownership,
client API keys, first-run wizard) grew around sharing a key pool across
people. An operator who runs it for one harness on a single machine pays that
whole surface — login, session cookies, PBKDF2 hashing, ownership validation
and its pool-floor invariant, secret minting, RBAC checks on every settings
call — without using any of it.

## Options

1. Keep everything; add an "advanced" single-user shortcut. (Rejected — the
   attack surface and code weight stay, and two auth models multiply the bugs.)
2. **Remove the auth/user plane entirely.** Dashboard, `/metrics`, and `/v1`
   become open to the local operator; every request is labeled `local`. The
   bound address (`HOST`, default `0.0.0.0` but loopback in the Compose
   default) is the whole trust boundary the same way `open` mode already
   documented.
3. Keep sessions but hard-code one operator identity. (Rejected — the signing
   keys, cookies, login route, and hashing all survive for no benefit.)

## Choice

Option 2. The following are **deleted**: `src/auth.rs`, `src/setup.html`, the
`User`/`Role`, `ClientAuth`/`ClientKey`/`Mode` types, `NimKey.owner`, the
`Config.clients` plane, `setup_required`/`Admin` state, the `/login`,
`/logout`, `/setup*`, `/api/settings/{clients,users,account}` routes, and the
session/role middleware. `proxy.rs` now labels every request `local`, so the
whole metrics/log/queue/dashboard per-client surface collapses to one row.
Dependencies `hmac`/`subtle` drop; `sha2` (NIM-key fingerprints) and
`getrandom` (history boot ID) stay.

## Consequences

- **Simpler**: `cargo build`/`test`/`clippy` green with ~1/3 fewer e2e tests;
  no setup step, no credentials anywhere, no lockout recovery runbook.
- **Trust boundary moves to the network**: anyone who can reach the dashboard
  can edit settings, and anyone who can reach `/v1` can spend the pool. Keep
  the listener on loopback (default Compose publish is `127.0.0.1`) or behind
  a firewall/VPN. This is the old `open` ("local") mode, now the *only* mode.
- Rate windows, calibration, governor, dispatch, failover, SSE heartbeats,
  history, and the dashboard's monitoring tabs are untouched — the removal is
  strictly the auth/user plane.
- Supersedes the contested parts of
  [auth-posture-and-dashboard-password](auth-posture-and-dashboard-password.md),
  [client-auth](../architecture/client-auth.md), and
  [sharing-with-friends](../ops/sharing-with-friends.md): they now apply only
  to the pre-v0.7.0 multi-user builds.