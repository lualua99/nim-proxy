//! Settings handlers — the config store's only writers. Every write runs the
//! same pipeline under the store mutex: build candidate → validate → persist
//! → swap the runtime snapshot → side effects. A failed disk write applies
//! nothing; concurrent saves serialize on the mutex.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{self, DispatchPolicy, NimKey, StoredConfig};
use crate::AppState;

/// SHA-256 hex of a string — the stable identifier for stored NIM keys (the
/// value itself is never sent back to a browser).
fn sha256_hex(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(2 * d.len());
    for b in d {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// First 8 hex chars of SHA-256(key): the stable public identifier for a
/// stored NIM key.
fn fingerprint(key: &str) -> String {
    sha256_hex(key)[..8].to_owned()
}

fn last4(s: &str) -> String {
    s.chars()
        .skip(s.chars().count().saturating_sub(4))
        .collect()
}

/// Commit a candidate store: validate, persist, swap the runtime config and
/// pool (with rate-state carryover), retune history retention, and only then
/// publish the candidate as the store's truth. Callers hold the store lock
/// (`guard`), which is the save-mutex.
pub fn commit(
    state: &AppState,
    guard: &mut StoredConfig,
    candidate: StoredConfig,
) -> Result<(), String> {
    config::validate(&candidate)?;
    config::save(&state.data_dir, &candidate)
        .map_err(|e| format!("cannot write the config store: {e}"))?;
    *state.cfg.write().unwrap() = Arc::new(candidate.runtime());
    {
        let mut pool = state.pool.write().unwrap();
        *pool = Arc::new(pool.rebuild(candidate.pool_specs()));
    }
    // Rebuild the upstream selector from the new endpoint list so health
    // state tracks the current upstream set.
    state
        .upstream_selector
        .lock()
        .unwrap()
        .rebuild(candidate.upstream.endpoints());
    // Rate state moved (windows/calibration/benches): ask the rate-state
    // saver to persist the new snapshot promptly, not on its next 30s tick.
    state
        .ratestate_dirty
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // Scheduling policy is runtime state the dispatcher owns; apply the
    // candidate alongside the cfg swap so the next queued pick obeys it.
    state.dispatch.configure(&candidate.dispatch);
    if candidate.history.days != guard.history.days {
        state
            .history
            .clone()
            .reconfigure_retention(candidate.history.days, crate::unix_now());
    }
    *guard = candidate;
    state.config_revision.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn json_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let body = serde_json::json!({
        "error": { "message": message.into(), "type": "proxy_error", "code": code }
    });
    (status, axum::Json(body)).into_response()
}

fn bad_request(msg: impl Into<String>) -> Response {
    json_error(StatusCode::BAD_REQUEST, "invalid_config", msg)
}

fn ok_json(v: serde_json::Value) -> Response {
    axum::Json(v).into_response()
}

/// `GET /api/config` — the Settings page's data source: the full server
/// configuration plus live lane state for the stored NIM keys.
pub async fn api_config(State(state): State<Arc<AppState>>) -> Response {
    let sc = state.store.lock().unwrap().clone();

    // Live lane state, keyed by the key string (enabled keys only).
    let pool = state.pool();
    let stats: std::collections::HashMap<String, (usize, usize, u64, f64)> = pool
        .lane_stats()
        .into_iter()
        .enumerate()
        .map(|(lane, s)| {
            (
                s.key.clone(),
                (lane, s.in_window, s.cooldown_ms, s.cal_factor),
            )
        })
        .collect();

    let nim_keys: Vec<serde_json::Value> = sc
        .upstream
        .nim_keys
        .iter()
        .map(|k| {
            let lane = stats.get(&k.key);
            serde_json::json!({
                "fingerprint": fingerprint(&k.key),
                "last4": last4(&k.key),
                "enabled": k.enabled,
                "rpm": k.rpm,
                "lane": lane.map(|(i, _, _, _)| i),
                "in_window": lane.map(|(_, w, _, _)| w),
                "cooldown_ms": lane.map(|(_, _, c, _)| c),
                "cal_factor": lane.map(|(_, _, _, f)| f),
            })
        })
        .collect();

    let history = state.history.status();
    let body = serde_json::json!({
        "mode": "open",
        "pool": {
            "enabled": pool.len(),
            "capacity_rpm": pool.capacity_rpm(),
        },
        "nim_keys": nim_keys,
        "server": {
            "base_url": sc.upstream.base_url,
            "upstreams": sc.upstream.upstreams,
            "limits": sc.limits,
            "pricing": sc.pricing,
            "history": {
                "days": sc.history.days,
                "available_from": history.available_from,
                "file_bytes": history.file_bytes,
                "compaction_pending": history.compaction_pending,
            },
            "dashboard": sc.dashboard,
            "governor": sc.governor,
            "recovery": sc.recovery,
            "dispatch": sc.dispatch,
        },
    });
    ok_json(body)
}

#[derive(Deserialize)]
pub struct NimKeysReq {
    add: Option<AddNimKey>,
    remove: Option<String>, // fingerprint
    set: Option<SetNimKey>,
}

#[derive(Deserialize)]
pub struct AddNimKey {
    key: String,
    rpm: Option<usize>,
}

#[derive(Deserialize)]
pub struct SetNimKey {
    fingerprint: String,
    enabled: Option<bool>,
    rpm: Option<usize>,
}

/// `POST /api/settings/nim-keys` — add/remove/configure the NIM keys that
/// make up the shared pool. The local operator manages the whole set.
pub async fn nim_keys(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<NimKeysReq>,
) -> Response {
    let mut guard = state.store.lock().unwrap();
    let mut cand = guard.clone();
    match (req.add, req.remove, req.set) {
        (Some(add), None, None) => {
            cand.upstream.nim_keys.push(NimKey {
                key: add.key.trim().to_owned(),
                enabled: true,
                rpm: add.rpm.unwrap_or(40),
            });
        }
        (None, Some(fp), None) => {
            let Some(pos) = cand
                .upstream
                .nim_keys
                .iter()
                .position(|k| fingerprint(&k.key) == fp)
            else {
                return bad_request("no such key");
            };
            cand.upstream.nim_keys.remove(pos);
        }
        (None, None, Some(set)) => {
            let Some(k) = cand
                .upstream
                .nim_keys
                .iter_mut()
                .find(|k| fingerprint(&k.key) == set.fingerprint)
            else {
                return bad_request("no such key");
            };
            if let Some(e) = set.enabled {
                k.enabled = e;
            }
            if let Some(rpm) = set.rpm {
                k.rpm = rpm;
            }
        }
        _ => return bad_request("send exactly one of add / remove / set"),
    }
    match commit(&state, &mut guard, cand) {
        Ok(()) => ok_json(serde_json::json!({"ok": true})),
        Err(e) => bad_request(e),
    }
}

/// Settings sections that own one flat config struct share one skeleton:
/// mutate the candidate store and commit (validate → persist → swap).
macro_rules! admin_section {
    ($fn_name:ident, $req:ty, $apply:expr) => {
        pub async fn $fn_name(
            State(state): State<Arc<AppState>>,
            axum::Json(req): axum::Json<$req>,
        ) -> Response {
            let mut guard = state.store.lock().unwrap();
            let mut cand = guard.clone();
            #[allow(clippy::redundant_closure_call)]
            ($apply)(&mut cand, req);
            match commit(&state, &mut guard, cand) {
                Ok(()) => ok_json(serde_json::json!({"ok": true})),
                Err(e) => bad_request(e),
            }
        }
    };
}

#[derive(Deserialize)]
pub struct UpstreamReq {
    base_url: String,
    #[serde(default)]
    upstreams: Vec<String>,
}

/// `POST /api/settings/upstream` — set the base URL and the ordered upstream
/// endpoint list. An empty list means single-endpoint mode. Also flushes the
/// model-catalog cache and the per-model no-inject memory, which are
/// upstream-specific.
pub async fn upstream(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<UpstreamReq>,
) -> Response {
    let result = {
        let mut guard = state.store.lock().unwrap();
        let mut cand = guard.clone();
        cand.upstream.base_url = req.base_url.trim().trim_end_matches('/').to_owned();
        cand.upstream.upstreams = req
            .upstreams
            .into_iter()
            .map(|u| u.trim().trim_end_matches('/').to_owned())
            .collect();
        commit(&state, &mut guard, cand)
    };
    match result {
        Ok(()) => {
            *state.models_cache.lock().await = None;
            state.no_inject.lock().unwrap().clear();
            ok_json(serde_json::json!({"ok": true}))
        }
        Err(e) => bad_request(e),
    }
}

/// Mirror of `config::Limits` WITHOUT serde defaults: a partial body is a
/// 422, never a silent reset of the omitted fields.
#[derive(Deserialize)]
pub struct LimitsReq {
    max_wait_secs: u64,
    heartbeat_secs: u64,
    models_ttl_secs: u64,
    stream_idle_secs: u64,
    request_timeout_secs: u64,
    max_inflight: usize,
    strict_passthrough: bool,
}

admin_section!(
    limits,
    LimitsReq,
    |cand: &mut StoredConfig, req: LimitsReq| {
        cand.limits = crate::config::Limits {
            max_wait_secs: req.max_wait_secs,
            heartbeat_secs: req.heartbeat_secs,
            models_ttl_secs: req.models_ttl_secs,
            stream_idle_secs: req.stream_idle_secs,
            request_timeout_secs: req.request_timeout_secs,
            max_inflight: req.max_inflight,
            strict_passthrough: req.strict_passthrough,
        };
    }
);

#[derive(Deserialize)]
pub struct PricingReq {
    ref_price_in: f64,
    ref_price_out: f64,
}

admin_section!(
    pricing,
    PricingReq,
    |cand: &mut StoredConfig, req: PricingReq| {
        cand.pricing.ref_price_in = req.ref_price_in;
        cand.pricing.ref_price_out = req.ref_price_out;
    }
);

#[derive(Deserialize)]
pub struct HistoryReq {
    days: u64,
    default_window_days: u64,
    slo_target_percent: f64,
}

admin_section!(
    history,
    HistoryReq,
    |cand: &mut StoredConfig, req: HistoryReq| {
        cand.history.days = req.days;
        cand.dashboard.default_window_days = req.default_window_days;
        cand.dashboard.slo_target_percent = req.slo_target_percent;
    }
);

#[derive(Deserialize)]
pub struct RecoveryReq {
    ramp_secs: u64,
    ramp_factor: f64,
}

admin_section!(
    recovery,
    RecoveryReq,
    |cand: &mut StoredConfig, req: RecoveryReq| {
        cand.recovery.ramp_secs = req.ramp_secs;
        cand.recovery.ramp_factor = req.ramp_factor;
    }
);

#[derive(Deserialize)]
pub struct GovernorReq {
    enabled: Option<bool>,
    set_override: Option<GovernorOverride>,
    remove_override: Option<String>,
}

#[derive(Deserialize)]
pub struct GovernorOverride {
    model: String,
    cap: usize,
}

admin_section!(
    governor_cfg,
    GovernorReq,
    |cand: &mut StoredConfig, req: GovernorReq| {
        if let Some(e) = req.enabled {
            cand.governor.enabled = e;
        }
        if let Some(o) = req.set_override {
            cand.governor.overrides.insert(o.model, o.cap);
        }
        if let Some(m) = req.remove_override {
            cand.governor.overrides.remove(&m);
        }
    }
);

/// Slot-scheduling settings: which policy (fifo/edf).
#[derive(Deserialize)]
pub struct DispatchReq {
    policy: Option<String>,
}

/// `POST /api/settings/dispatch` — swap slot scheduling live.
pub async fn dispatch_cfg(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<DispatchReq>,
) -> Response {
    let mut guard = state.store.lock().unwrap();
    let mut cand = guard.clone();
    if let Some(p) = req.policy {
        cand.dispatch.policy = match p.as_str() {
            "fifo" => DispatchPolicy::Fifo,
            "edf" => DispatchPolicy::Edf,
            _ => return bad_request("policy must be \"fifo\" or \"edf\""),
        };
    }
    match commit(&state, &mut guard, cand) {
        Ok(()) => ok_json(serde_json::json!({"ok": true})),
        Err(e) => bad_request(e),
    }
}

#[derive(Deserialize)]
pub struct ValidateKeyReq {
    key: String,
}

/// `POST /api/settings/validate-key` — probe a NIM key against the configured
/// upstream: does `/v1/models` answer for it? Probes the configured
/// `base_url`, never a caller-supplied one (a request-supplied target would
/// turn the proxy into an SSRF probe of internal hosts). Bypasses the pool
/// and the models cache — this is an explicit-key check.
pub async fn validate_key(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<ValidateKeyReq>,
) -> Response {
    let base = state.store.lock().unwrap().upstream.base_url.clone();
    let base = base.trim().trim_end_matches('/').to_owned();
    axum::Json(match probe_key(&state.http, &base, req.key.trim()).await {
        Ok(models) => serde_json::json!({"ok": true, "models": models}),
        Err(e) => serde_json::json!({"ok": false, "error": e}),
    })
    .into_response()
}

/// Probe a NIM key against an upstream: does `/v1/models` answer for it?
/// Bypasses the pool and the models cache — this is an explicit-key check.
pub async fn probe_key(http: &reqwest::Client, base_url: &str, key: &str) -> Result<usize, String> {
    match crate::proxy::fetch_models(http, base_url, key).await {
        Ok(resp) if resp.status().is_success() => {
            let body = resp
                .bytes()
                .await
                .map_err(|e| format!("upstream sent an unreadable model list: {e}"))?;
            let v: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|e| format!("upstream sent an unreadable model list: {e}"))?;
            Ok(v["data"].as_array().map(|a| a.len()).unwrap_or(0))
        }
        Ok(resp) => Err(format!("upstream rejected the key ({})", resp.status())),
        Err(e) => Err(format!("cannot reach upstream: {e}")),
    }
}
