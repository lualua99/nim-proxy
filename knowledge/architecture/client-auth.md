---
type: Component
title: Auth (retired in v0.7.0)
description: The multi-user / client-key auth plane that was removed in the single-operator build; kept as the historical record for v0.6.x.
tags: [auth, multi-user, security]
timestamp: 2026-07-04T00:00:00Z
---

# Auth (retired in v0.7.0)

> **Retired.** v0.7.0 removed the whole auth/user plane for the single-operator
> local build — see [single-user-local-build](../decisions/single-user-local-build.md).
> `/v1`, the dashboard, and `/metrics` are open; every request is labeled
> `local`. This page documents the v0.6.x designs for reference only.

The v0.6.x build had two independent gates, both driven by the
[config store](../decisions/ui-managed-config-store.md):

- **Setup phase**: `setup_required` (no superuser in the store) → `/v1` 503
  `setup_required`, browsers → `/setup` wizard, one atomic claim POST created
  the superuser + NIM keys + optional first client key.
- **API gate `/v1`**: `client_auth.mode` `keyed` (SHA-256 + constant-time
  compare of the bearer against stored digests, fail-closed) vs `open`
  ("local", unauthenticated). Secrets were `npk_…` 128-bit, shown once, stored
  only as digests.
- **Dashboard gate**: `require_session` (HMAC-signed cookie carrying
  `expiry || username || first8(sha256(password_hash))`), roles looked up live,
  PBKDF2-HMAC-SHA256 600k passwords, scraper memoization, login throttle.
- **Roles & ownership**: superuser/admin/user; per-key/per-client-key `owner`
  with the pool-floor invariant; `GET /api/config` filtered server-side.
- **Recovery**: admin password resets, or the volume edit that emptied `users`
  to re-run the wizard.

Default `HOST` is `0.0.0.0` but Compose publishes `127.0.0.1` by default, and
TLS was never built in — terminate at a reverse proxy and set `TRUST_PROXY`.
In the v0.7.0 single-operator build the loopback bind is the entire safeguard
[by design](../decisions/single-user-local-build.md).