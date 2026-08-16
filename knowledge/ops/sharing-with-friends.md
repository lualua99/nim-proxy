---
type: Runbook
title: Sharing the proxy with friends (retired)
description: The multi-user sharing flow, removed in the v0.7.0 single-operator build.
tags: [multi-user, security, tos]
timestamp: 2026-07-02T00:00:00Z
---

# Sharing the proxy with friends (retired)

> **Retired.** v0.7.0 is a **single-operator local build** — users, roles,
> per-key ownership, and logins are gone (see
> [single-user-local-build](../decisions/single-user-local-build.md)). One
> machine, one operator, one key pool. If you need several people to share a
> pool with per-person attribution, run a pre-v0.7.0 build. This page is the
> historical multi-user runbook.

The intended shape was: several people each register their own NIM account
(unique email + phone per NVIDIA's signup), contribute their key(s) to the
pool, and share the aggregate throughput. Five keys = 200 RPM for everyone.

- The [FIFO dispatcher](../decisions/global-fifo-dispatcher.md) guarantees no
  friend can starve another; under saturation everyone slows down equally.
- ToS position is unchanged and version-independent: the proxy respects
  NVIDIA's per-key limits, never evades them; whether pooling keys across
  people complies with NVIDIA's terms is between the key owners and NVIDIA.
  All traffic originates from one IP, visible to NVIDIA and not disguised.