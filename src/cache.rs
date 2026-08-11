//! Response cache for idempotent, non-streaming requests.
//!
//! Every request to a NIM key consumes one RPM slot. When a harness repeats
//! the same deterministic request (an embedding, a fixed-prompt classification,
//! a catalog-adjacent probe), the response is byte-for-byte reusable — so we
//! serve it from an in-memory cache keyed by the request's semantic identity
//! and skip the rate-limit queue and the upstream call entirely. This is memory
//! only: a restart rebuilds it, and it is not shared across instances.

use std::time::Duration;

use axum::http::StatusCode;
use bytes::Bytes;
use sha2::{Digest, Sha256};

/// A cached response: body, status, and the subset of headers worth keeping
/// (content-type). Volatile per-request headers voted out (see D3).
#[derive(Clone)]
pub struct CachedResponse {
    pub status: StatusCode,
    pub content_type: String,
    pub body: Bytes,
}

/// The cacheable endpoint allowlist. Only these paths may be cached — and only
/// when the request is non-streaming (a streamed response is an SSE event flow
/// that cannot be captured whole).
fn is_cacheable_path(path: &str) -> bool {
    matches!(path, "/v1/chat/completions" | "/v1/embeddings")
}

/// Semantic cache key: SHA256 over model + path + the canonical body JSON.
/// The whole body goes in (not a parameter subset) so a cache hit can never
/// be wrong — `seed`, `stop`, `response_format` etc. all participate.
fn cache_key(model: &str, path: &str, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update([0u8]);
    hasher.update(path.as_bytes());
    hasher.update([0u8]);
    hasher.update(body);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The response cache. Wraps a `moka::sync::Cache` so it is cheap, `Send +
/// Sync`, and can be dropped and rebuilt on a settings change (a new TTL or
/// max-entries takes effect by replacing the whole cache — moka's `new` is
/// light).
#[derive(Clone)]
pub struct ResponseCache {
    inner: moka::sync::Cache<String, CachedResponse>,
    enabled: bool,
}

impl ResponseCache {
    /// Build a cache with the given policy. A zero TTL means "disabled":
    /// the cache is empty and never accepts writes, so lookups always miss.
    pub fn new(ttl_secs: u64, max_entries: u64) -> Self {
        let enabled = ttl_secs > 0 && max_entries > 0;
        let inner = if enabled {
            moka::sync::Cache::builder()
                .max_capacity(max_entries)
                .time_to_live(Duration::from_secs(ttl_secs))
                .build()
        } else {
            moka::sync::Cache::new(0)
        };
        Self { inner, enabled }
    }

    /// Whether caching is enabled at all (derived from the config at build).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The cache key for a request, or `None` when the request is not
    /// cacheable (wrong path, or a streamed request).
    pub fn key_for(
        &self,
        model: &str,
        path: &str,
        body: &[u8],
        wants_stream: bool,
    ) -> Option<String> {
        if wants_stream || !is_cacheable_path(path) {
            None
        } else {
            Some(cache_key(model, path, body))
        }
    }

    /// Look a cached response up by key.
    pub fn get(&self, key: &str) -> Option<CachedResponse> {
        self.inner.get(key)
    }

    /// Store a response under a key. Only 2xx responses reach here; the
    /// caller decides that. A disabled cache is a no-op.
    pub fn set(&self, key: &str, status: StatusCode, content_type: String, body: Bytes) {
        if self.enabled {
            self.inner.insert(
                key.to_owned(),
                CachedResponse {
                    status,
                    content_type,
                    body,
                },
            );
        }
    }

    /// Current number of live entries (for the gauge).
    pub fn len(&self) -> u64 {
        self.inner.entry_count()
    }

    /// Drop the current cache and rebuild under a new policy. Settings swaps
    /// call this so a TTL/max-entries change applies immediately.
    pub fn reconfigure(&mut self, ttl_secs: u64, max_entries: u64) {
        *self = Self::new(ttl_secs, max_entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_deterministic() {
        let body = br#"{"model":"m","messages":[]}"#;
        assert_eq!(
            cache_key("m", "/v1/chat/completions", body),
            cache_key("m", "/v1/chat/completions", body)
        );
    }

    #[test]
    fn cache_key_varies_with_input() {
        let a = cache_key("m", "/v1/chat/completions", br#"{"temperature":0.7}"#);
        let b = cache_key("m", "/v1/chat/completions", br#"{"temperature":1.0}"#);
        let c = cache_key("m2", "/v1/chat/completions", br#"{"temperature":0.7}"#);
        let d = cache_key("m", "/v1/embeddings", br#"{"temperature":0.7}"#);
        assert_ne!(a, b, "differs by body");
        assert_ne!(a, c, "differs by model");
        assert_ne!(a, d, "differs by path");
    }

    #[test]
    fn cacheable_path_allowlist() {
        assert!(is_cacheable_path("/v1/chat/completions"));
        assert!(is_cacheable_path("/v1/embeddings"));
        assert!(!is_cacheable_path("/v1/completions"));
        assert!(!is_cacheable_path("/v1/models"));
        assert!(!is_cacheable_path("/v1/anything"));
    }

    #[test]
    fn key_for_excludes_streams_and_non_cacheable_paths() {
        let c = ResponseCache::new(60, 1024);
        assert!(c
            .key_for("m", "/v1/chat/completions", b"{}", false)
            .is_some());
        assert!(
            c.key_for("m", "/v1/chat/completions", b"{}", true)
                .is_none(),
            "streamed requests are never cached"
        );
        assert!(
            c.key_for("m", "/v1/completions", b"{}", false).is_none(),
            "legacy path is not cached"
        );
    }

    #[test]
    fn disabled_cache_never_stores_or_serves() {
        let mut c = ResponseCache::new(0, 1024);
        assert!(!c.enabled());
        let key = c
            .key_for("m", "/v1/chat/completions", b"{}", false)
            .unwrap();
        c.set(
            &key,
            StatusCode::OK,
            "application/json".into(),
            Bytes::from_static(b"{}"),
        );
        assert!(c.get(&key).is_none());
        assert_eq!(c.len(), 0);
        c.reconfigure(60, 1024);
        assert!(c.enabled());
    }

    #[test]
    fn cache_stores_and_serves_with_ttl() {
        let c = ResponseCache::new(60, 1024);
        let key = c.key_for("m", "/v1/embeddings", b"{}", false).unwrap();
        assert!(c.get(&key).is_none());
        c.set(
            &key,
            StatusCode::OK,
            "application/json".into(),
            Bytes::from_static(b"[1,2]"),
        );
        let hit = c.get(&key).expect("stored response");
        assert_eq!(hit.status, StatusCode::OK);
        assert_eq!(hit.content_type, "application/json");
        assert_eq!(hit.body, Bytes::from_static(b"[1,2]"));
    }

    #[test]
    fn reconfigure_rebuilds_under_new_policy() {
        let mut c = ResponseCache::new(60, 1024);
        let key = c.key_for("m", "/v1/embeddings", b"{}", false).unwrap();
        c.set(
            &key,
            StatusCode::OK,
            "application/json".into(),
            Bytes::from_static(b"{}"),
        );
        assert!(c.get(&key).is_some());
        c.reconfigure(0, 1024);
        assert!(!c.enabled());
        assert!(c.get(&key).is_none(), "reconfigure drops the old entries");
    }
}
