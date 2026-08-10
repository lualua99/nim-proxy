//! Upstream endpoint selection with health-based failover.
//!
//! The proxy normally talks to a single fixed upstream (`base_url`); when that
//! endpoint is throttled, down, or its account locked, the whole pool dies
//! with it. This module supports an *ordered* list of upstream endpoints: the
//! primary is preferred while it is healthy, and requests automatically fail
//! over to the next healthy endpoint when probes fail, then back when the
//! primary recovers.
//!
//! Health is *passively observed* — there is no dedicated health-check timer
//! and no extra request budget. Every real `/v1/*` exchange and the periodic
//! `/v1/models` catalog refresh already hits one specific endpoint, so we
//! reuse that traffic: a success keeps the endpoint `alive`, [[`Down`]]-two
//! consecutive failures mark it `down` for a cooldown, and a single success
//! brings it back `up`. Order in the list is priority; selection walks it.
//!
//! The selector is cheap mutable runtime state (like the key pool), swapped
//! whole when settings change. `select` clones the winning URL so the caller
//! never holds the lock across the actual upstream exchange — observations
//! are recorded later under a fresh lock.

use std::time::{Duration, Instant};

use metrics::{counter, gauge};

/// Consecutive failures that mark an endpoint `down`.
const DOWN_THRESHOLD: u32 = 2;
/// How long a `down` endpoint stays unselected before it can be probed again.
/// The probe cadence is driven by real traffic and the models TTL, so this
/// cooldown merely guarantees a downed endpoint isn't hammered back-to-back.
const COOLDOWN: Duration = Duration::from_secs(60);

/// Per-endpoint health state.
pub struct UpstreamState {
    pub url: String,
    alive: bool,
    failures: u32,
    last_success: Option<Instant>,
    cooldown_until: Instant,
}

impl UpstreamState {
    fn new(url: String) -> Self {
        Self {
            url,
            alive: true,
            failures: 0,
            last_success: None,
            cooldown_until: Instant::now(),
        }
    }

    fn record_success(&mut self) {
        self.alive = true;
        self.failures = 0;
        self.last_success = Some(Instant::now());
    }

    fn record_failure(&mut self) {
        self.failures += 1;
        if self.failures >= DOWN_THRESHOLD {
            self.alive = false;
            self.cooldown_until = Instant::now() + COOLDOWN;
        }
    }
}

/// Health-based selector over an ordered list of upstream URLs.
///
/// Order is priority: the first entry is the primary, the rest are backups.
pub struct UpstreamSelector {
    endpoints: Vec<UpstreamState>,
}

impl UpstreamSelector {
    pub fn new(urls: Vec<String>) -> Self {
        Self {
            endpoints: urls.into_iter().map(UpstreamState::new).collect(),
        }
    }

    /// Replace the endpoint list (a settings change). Unlisted endpoints are
    /// dropped; listed-again later ones start fresh (their cooldown memory is
    /// not worth persisting across an operator edit).
    pub fn rebuild(&mut self, urls: Vec<String>) {
        self.endpoints = urls.into_iter().map(UpstreamState::new).collect();
    }

    /// Select the best endpoint URL, cloning it so the caller can act without
    /// holding the selector lock. Priority order: the first *alive* endpoint;
    /// else the first *out of cooldown* (a recovery probe); else the primary
    /// — fail-open, so a fully-down upstream yields a 429 rather than a proxy
    /// refusal, while recording the `all_down` event. Returns `None` when the
    /// list is empty (shouldn't happen: config validation requires >= 1).
    pub fn select(&mut self) -> Option<String> {
        let now = Instant::now();
        if let Some(e) = self.endpoints.iter().find(|e| e.alive) {
            return Some(e.url.clone());
        }
        if let Some(e) = self.endpoints.iter().find(|e| now >= e.cooldown_until) {
            return Some(e.url.clone());
        }
        // Every endpoint down or in cooldown: fail open to the primary and
        // count it so the operator sees the outage clearly.
        counter!("nimproxy_upstream_all_down_total").increment(1);
        tracing::warn!("all upstream endpoints are down; failing open to primary");
        self.endpoints.first().map(|e| e.url.clone())
    }

    /// Live health view for the dashboard / metrics (endpoint, alive, last
    /// success, failures).
    pub fn health(&self) -> Vec<EndpointHealth> {
        self.endpoints
            .iter()
            .map(|e| EndpointHealth {
                url: e.url.clone(),
                alive: e.alive,
                failures: e.failures,
                last_success: e.last_success,
            })
            .collect()
    }

    /// Record a successful exchange against `url`: the endpoint is up.
    pub fn observe_success(&mut self, url: &str) {
        if let Some(e) = self.endpoints.iter_mut().find(|e| e.url == url) {
            e.record_success();
        }
        publish_gauge(url, 1.0);
    }

    /// Record a failed exchange against `url`. Two consecutive failures mark
    /// it down; a later success brings it back.
    pub fn observe_failure(&mut self, url: &str) {
        if let Some(e) = self.endpoints.iter_mut().find(|e| e.url == url) {
            e.record_failure();
        }
        publish_gauge(url, 0.0);
    }

    /// Publish the health gauge for every endpoint (full refresh).
    pub fn publish(&self) {
        for e in &self.endpoints {
            publish_gauge(&e.url, if e.alive { 1.0 } else { 0.0 });
        }
    }
}

/// A live per-endpoint health view for the dashboard.
pub struct EndpointHealth {
    pub url: String,
    pub alive: bool,
    pub failures: u32,
    pub last_success: Option<Instant>,
}

/// A URL as a Prometheus-safe label value: operator-configured, but keep it
/// inert anyway (no quotes/braces/newlines) so an odd value can't break the
/// exposition format.
fn hazard(url: &str) -> String {
    url.chars()
        .filter(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '[' | ']')
        })
        .take(128)
        .collect()
}

fn publish_gauge(url: &str, v: f64) {
    gauge!("nimproxy_upstream_endpoint_health", "endpoint" => hazard(url)).set(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(urls: &[&str]) -> UpstreamSelector {
        UpstreamSelector::new(urls.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn primary_is_selected_while_alive() {
        let mut s = sel(&["https://a", "https://b"]);
        assert_eq!(s.select(), Some("https://a".into()));
    }

    #[test]
    fn fails_over_to_backup_after_two_failures() {
        let mut s = sel(&["https://a", "https://b"]);
        s.observe_failure("https://a");
        // One failure: still alive, primary still preferred.
        assert_eq!(s.select(), Some("https://a".into()));
        s.observe_failure("https://a");
        // Two failures: primary is down, backup takes over.
        assert_eq!(s.select(), Some("https://b".into()));
    }

    #[test]
    fn success_recovers_a_down_endpoint() {
        let mut s = sel(&["https://a", "https://b"]);
        for _ in 0..2 {
            s.observe_failure("https://a");
        }
        assert_eq!(s.select(), Some("https://b".into()));
        s.observe_success("https://a");
        assert_eq!(s.select(), Some("https://a".into()));
    }

    #[test]
    fn all_down_fails_open_to_primary() {
        let mut s = sel(&["https://a", "https://b"]);
        for _ in 0..2 {
            s.observe_failure("https://a");
            s.observe_failure("https://b");
        }
        // Both down and in cooldown: pick the primary rather than refuse.
        assert_eq!(s.select(), Some("https://a".into()));
    }

    #[test]
    fn empty_list_selects_none() {
        let mut s = sel(&[]);
        assert_eq!(s.select(), None);
    }

    #[test]
    fn rebuild_replaces_the_list() {
        let mut s = sel(&["https://a", "https://b"]);
        s.observe_failure("https://a");
        s.observe_failure("https://a");
        s.rebuild(vec!["https://c".into()]);
        assert_eq!(s.select(), Some("https://c".into()));
        assert_eq!(s.health().len(), 1);
    }

    #[test]
    fn health_reports_alive_and_failures() {
        let mut s = sel(&["https://a", "https://b"]);
        s.observe_failure("https://b");
        let h = s.health();
        assert!(h[0].alive && h[0].failures == 0);
        assert!(h[1].alive && h[1].failures == 1, "one failure, still alive");
    }
}
