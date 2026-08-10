mod auth;
mod config;
mod dispatch;
mod governor;
mod history;
mod pool;
mod proxy;
mod ratestate;
mod registry;
mod settings;
mod upstream;

// Fuzzing-only re-exports (the modules themselves stay private). Compiled
// only under cargo-fuzz's `--cfg fuzzing`, so normal builds, coverage, and
// the shipped binary never carry them.
#[cfg(fuzzing)]
#[doc(hidden)]
pub use config::fuzz as fuzz_config;
#[cfg(fuzzing)]
#[doc(hidden)]
pub use proxy::fuzz as fuzz_proxy;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use bytes::Bytes;
use metrics::gauge;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::convert::Infallible;
use tokio::sync::Mutex;

use auth::{Admin, Identity};
use config::Role;
use dispatch::Dispatcher;
use pool::{Pool, PoolHandle};

/// App-level configuration, published as an immutable snapshot: every request
/// takes one `Arc<Config>` via [`AppState::cfg`] and sees a consistent view;
/// the settings layer swaps in a replacement under the write lock.
pub struct Config {
    pub base_url: String,
    /// Ordered upstream endpoint list (the runtime form of the stored list).
    pub upstreams: Vec<String>,
    pub max_wait: Duration,
    pub heartbeat: Duration,
    pub models_ttl: Duration,
    /// Abort a stream when the upstream sends nothing for this long (0 = off).
    pub stream_idle: Duration,
    /// Overall deadline for a non-streaming upstream request (connect + body).
    /// Streaming has no overall cap (generation can be long) — it relies on
    /// `stream_idle` instead. Bounds a stalled buffered read holding a slot.
    pub request_timeout: Duration,
    /// Never modify request bodies (disables stream_options usage injection).
    pub strict_passthrough: bool,
    /// Reference $/1M token prices for the dashboard's "dollars saved" figure.
    pub price_in: f64,
    pub price_out: f64,
    /// token -> client name. None = local mode, no client auth.
    pub clients: Option<HashMap<String, String>>,
    /// Cap on concurrent requests; bounds memory under floods.
    pub max_inflight: usize,
    /// Model-pressure governor settings (worker concurrency, not RPM).
    pub governor: GovernorSettings,
    /// When true, reject requests whose estimated queue wait exceeds the
    /// threshold with 503 + Retry-After instead of letting them queue.
    pub backpressure_enabled: bool,
    /// ETA threshold in seconds: requests that would wait this long or longer
    /// are rejected under backpressure (ignored when backpressure is off).
    pub backpressure_queue_threshold_eta: Duration,
}

pub struct GovernorSettings {
    /// Adaptive governing on worker-exhaustion errors (on by default; the
    /// governor stays dormant until an upstream actually exhausts).
    pub enabled: bool,
    /// Operator-pinned per-model concurrency caps (model id -> max in-flight).
    pub overrides: HashMap<String, usize>,
}

impl Default for GovernorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            overrides: HashMap::new(),
        }
    }
}

pub struct AppState {
    /// Current config snapshot; read via [`AppState::cfg`], swapped whole.
    pub cfg: RwLock<Arc<Config>>,
    /// The persisted store of truth. Its mutex doubles as the save-mutex:
    /// settings writes hold it across build → validate → persist → swap.
    pub store: std::sync::Mutex<config::StoredConfig>,
    /// Where the store lives (DATA_DIR).
    pub data_dir: std::path::PathBuf,
    /// True until a superuser exists: the wizard is open, everything else
    /// is closed (dashboard redirects to /setup, /v1 answers 503).
    pub setup_required: std::sync::atomic::AtomicBool,
    /// Current key pool; the dispatcher reads it per grant, settings swap it.
    pub pool: PoolHandle,
    /// Runtime upstream health/selection state (see `upstream`). Cheap,
    /// non-awaiting operations, so a plain mutex; rebuilt by settings swaps.
    pub upstream_selector: std::sync::Mutex<upstream::UpstreamSelector>,
    pub dispatch: Dispatcher,
    pub http: reqwest::Client,
    pub models_cache: Mutex<Option<(Instant, Bytes)>>,
    /// Models that rejected stream_options injection; never inject for them again.
    pub no_inject: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Distinct sanitized model labels seen (bounds metric cardinality).
    pub model_labels: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Session + throttle machinery for the operator surface.
    pub admin: Admin,
    /// Requests currently in flight; capped to bound memory under floods.
    pub inflight: AtomicUsize,
    /// Per-model worker-concurrency gate (runtime state, settings in Config).
    pub governor: Arc<governor::Governor>,
    pub history: Arc<history::History>,
    /// Recovery slow-start horizon (epoch; 0 = none). Read by the dashboard
    /// and the rate-state saver's ramp gauge.
    pub ramp_until: AtomicU64,
    /// How much persisted rate state the boot actually restored / dropped
    /// (metrics + dashboard visibility).
    pub restored_lanes: AtomicU64,
    pub dropped_lanes: AtomicU64,
    /// Raised whenever rate state changes shape (pool rebuilt by settings):
    /// the saver drains it on its next tick.
    pub ratestate_dirty: std::sync::atomic::AtomicBool,
    /// In-flight request queue: who asked for what, and how to stop it.
    pub registry: Arc<registry::RequestRegistry>,
    /// Shared metrics registry rendered by both `/metrics` and dashboard-now.
    pub prometheus: PrometheusHandle,
    /// Monotonic settings generation for lightweight dashboard refreshes.
    pub config_revision: AtomicU64,
    /// Unix time this process started (dashboard uptime).
    pub started: u64,
    /// Live SSE dashboard-stream connections; bounds memory under many tabs.
    pub sse_connections: AtomicUsize,
}

impl AppState {
    /// One consistent config snapshot; never hold this across a save.
    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
    }

    /// The current pool generation (observability only — reservations go
    /// through the dispatcher, which snapshots under the same lock).
    pub fn pool(&self) -> Arc<Pool> {
        self.pool.read().unwrap().clone()
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// App-level settings moved from env into the UI-managed store (v0.6.0).
/// Ignore — but call out — any that are still set, so a stale .env can't
/// silently mislead an operator.
fn warn_legacy_env() {
    const LEGACY: &[&str] = &[
        "NIM_API_KEYS",
        "NIM_BASE_URL",
        "RPM_PER_KEY",
        "PROXY_API_KEYS",
        "ADMIN_PASSWORD",
        "INSECURE_NO_AUTH",
        "MAX_WAIT_SECS",
        "HEARTBEAT_SECS",
        "MODELS_TTL_SECS",
        "STREAM_IDLE_SECS",
        "REQUEST_TIMEOUT_SECS",
        "STRICT_PASSTHROUGH",
        "REF_PRICE_IN",
        "REF_PRICE_OUT",
        "HISTORY_DAYS",
        "MAX_INFLIGHT",
    ];
    let set: Vec<&str> = LEGACY
        .iter()
        .copied()
        .filter(|v| std::env::var_os(v).is_some())
        .collect();
    if !set.is_empty() {
        tracing::warn!(
            "ignoring legacy env vars ({}) — these settings live in the dashboard now",
            set.join(", ")
        );
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Max concurrent SSE dashboard-stream connections (per-process, not per-user).
const MAX_SSE_CONNECTIONS: usize = 100;

fn capacity_snapshot(pool: &Pool) -> history::CapacitySnapshot {
    history::CapacitySnapshot {
        enabled_lanes: pool.len(),
        rpms: pool.rpms(),
        capacity_rpm: pool.capacity_rpm(),
    }
}

#[derive(serde::Deserialize)]
struct DashboardQuery {
    from: Option<u64>,
    to: Option<u64>,
    points: Option<usize>,
}

async fn api_dashboard(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<DashboardQuery>,
) -> Response {
    let stored = state.store.lock().unwrap();
    let config_revision = state
        .config_revision
        .load(std::sync::atomic::Ordering::SeqCst);
    let now = unix_now();
    let following_now = query.to.is_none();
    let requested_from = query.from.unwrap_or_else(|| {
        now.saturating_sub(stored.dashboard.default_window_days.saturating_mul(86_400))
    });
    let requested_to = query.to.unwrap_or(now);
    if requested_from >= requested_to {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "message": "from must be less than to",
                    "type": "proxy_error",
                    "code": "invalid_time_window",
                }
            })),
        )
            .into_response();
    }

    let rollup = state.history.rollup(
        requested_from,
        requested_to,
        query.points.unwrap_or(288).clamp(2, 1000),
    );
    axum::Json(serde_json::json!({
        "history_revision": rollup.history_revision,
        "config_revision": config_revision,
        "window": {
            "requested_from": requested_from,
            "requested_to": requested_to,
            "following_now": following_now,
            "effective_from": rollup.effective_from,
            "effective_to": rollup.effective_to,
            "available_from": rollup.available_from,
            "available_to": rollup.available_to,
            "default_window_days": stored.dashboard.default_window_days,
            "retention_days": stored.history.days,
        },
        "totals": rollup.totals,
        "latest": rollup.latest,
        "points": rollup.points,
        "diagnostics": rollup.diagnostics,
    }))
    .into_response()
}

fn dashboard_now_payload(state: &AppState, username: &str) -> serde_json::Value {
    let stored = state.store.lock().unwrap();
    let config_revision = state
        .config_revision
        .load(std::sync::atomic::Ordering::SeqCst);
    let pool = state.pool();
    let now = unix_now();
    let current = state.history.current(now, || state.prometheus.render());
    let history_revision = current.tail.base_history_revision;
    let role = stored.user(username).map(|u| match u.role {
        Role::Superuser => "superuser",
        Role::Admin => "admin",
        Role::User => "user",
    });
    let upstream_health: Vec<serde_json::Value> = state
        .upstream_selector
        .lock()
        .unwrap()
        .health()
        .iter()
        .map(|h| {
            serde_json::json!({
                "url": h.url,
                "alive": h.alive,
                "failures": h.failures,
                "last_success": h.last_success.map(|t| now.saturating_sub(t.elapsed().as_secs())),
            })
        })
        .collect();
    let lane_stats = pool.lane_stats();
    let per_lane: Vec<serde_json::Value> = lane_stats
        .iter()
        .map(|l| {
            let effective_rpm = (l.rpm as f64 * l.cal_factor).floor().max(1.0) as usize;
            serde_json::json!({
                "key": l.key,
                "in_window": l.in_window,
                "effective_rpm": effective_rpm,
                "rpm": l.rpm,
            })
        })
        .collect();
    let window_fill: usize = lane_stats.iter().map(|l| l.in_window).sum();
    let window_capacity: usize = lane_stats
        .iter()
        .map(|l| (l.rpm as f64 * l.cal_factor).floor().max(1.0) as usize)
        .sum();
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "sampled_at": now,
        "started": state.started,
        "role": role.unwrap_or_default(),
        "price_in": stored.pricing.ref_price_in,
        "price_out": stored.pricing.ref_price_out,
        "auth": stored.client_auth.mode == config::Mode::Keyed,
        "lanes": pool.len(),
        "rpms": pool.rpms(),
        "capacity_rpm": pool.capacity_rpm(),
        "per_lane": per_lane,
        "window_fill": window_fill,
        "window_capacity": window_capacity,
        "default_window_days": stored.dashboard.default_window_days,
        "retention_days": stored.history.days,
        "slo_target_percent": stored.dashboard.slo_target_percent,
        "history_revision": history_revision,
        "config_revision": config_revision,
        "available_from": current.available_from,
        "available_to": current.available_to,
        "ramp_active": state.ramp_until.load(std::sync::atomic::Ordering::SeqCst) > now,
        "ramp_remaining_secs": state
            .ramp_until
            .load(std::sync::atomic::Ordering::SeqCst)
            .saturating_sub(now),
        "restored_lanes": state.restored_lanes.load(std::sync::atomic::Ordering::SeqCst),
        "dropped_lanes": state.dropped_lanes.load(std::sync::atomic::Ordering::SeqCst),
        "upstream_health": upstream_health,
        "metrics": current.metrics,
        "tail": current.tail,
    })
}

async fn api_dashboard_now(
    State(state): State<Arc<AppState>>,
    Extension(Identity(username)): Extension<Identity>,
) -> axum::Json<serde_json::Value> {
    axum::Json(dashboard_now_payload(&state, &username))
}

async fn api_dashboard_stream(
    State(state): State<Arc<AppState>>,
    Extension(Identity(username)): Extension<Identity>,
) -> Response {
    let current = state
        .sse_connections
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if current >= MAX_SSE_CONNECTIONS {
        state
            .sse_connections
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too_many_streams",
            "too many dashboard SSE connections",
        );
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(4);
    let state = state.clone();
    let username = username.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        interval.tick().await;
        loop {
            interval.tick().await;
            let value = dashboard_now_payload(&state, &username);
            let data = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
            if tx.send(Ok(Event::default().data(data))).await.is_err() {
                break;
            }
        }
        state
            .sse_connections
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    });
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::default()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response()
}

/// Chronicle window for the capacity-model's "recent observation": the last
/// hour of history aggregates the per-key/per-client/per-model signal the
/// what-if simulator seeds from. Short enough to reflect current behavior,
/// long enough that a 5-minute sample interval yields a stable mean.
const CAPACITY_MODEL_WINDOW: u64 = 3600;

fn sum_metric(totals: &[history::MetricValue], metric: &str) -> f64 {
    totals
        .iter()
        .filter(|m| m.metric == metric)
        .map(|m| m.value)
        .sum()
}

/// Quantile (0..1) of a histogram's cumulative `_bucket` deltas over a window,
/// via linear interpolation — the backend twin of the dashboard's `quantile()`.
fn quantile_from_totals(totals: &[history::MetricValue], base: &str, q: f64) -> Option<f64> {
    let mut buckets: Vec<(f64, f64)> = totals
        .iter()
        .filter(|m| m.metric == format!("{base}_bucket"))
        .map(|m| {
            let le = m.labels.get("le").map(String::as_str).unwrap_or("+Inf");
            let le = if le == "+Inf" {
                f64::INFINITY
            } else {
                le.parse().unwrap_or(f64::INFINITY)
            };
            (le, m.value)
        })
        .collect();
    buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let total = buckets.last().map(|b| b.1).unwrap_or(0.0);
    if total <= 0.0 {
        return None;
    }
    let target = total * q;
    let mut prev_le: f64 = 0.0;
    let mut prev_count: f64 = 0.0;
    for (le, count) in buckets {
        if count >= target {
            let hi = if le.is_infinite() {
                prev_le.max(1.0)
            } else {
                le
            };
            return Some(
                prev_le + (hi - prev_le) * (target - prev_count) / (count - prev_count).max(1.0),
            );
        }
        prev_le = le;
        prev_count = count;
    }
    None
}

/// Seed data for the Capacity tab's what-if simulator: purely aggregate
/// observations from the existing registry/metrics/history — no request
/// content, no new stored fields. Session-gated like every dashboard read
/// (any user may view Capacity, matching the tab's existing permission line).
async fn api_capacity_model(
    State(state): State<Arc<AppState>>,
    Extension(Identity(_username)): Extension<Identity>,
) -> Response {
    let now = unix_now();
    let from = now.saturating_sub(CAPACITY_MODEL_WINDOW);
    let rollup = state.history.rollup(from, now, 60);
    let totals = &rollup.totals;

    let pool = state.pool();
    let rpms = pool.rpms();
    let calibration = pool.calibration_factors();
    let effective_rpm: Vec<usize> = rpms
        .iter()
        .zip(calibration.iter())
        .map(|(r, c)| ((*r).max(1) as f64 * c).round().max(1.0) as usize)
        .collect();
    let capacity_rpm: usize = effective_rpm.iter().sum();

    let observed_rpm = req_count_to_rpm(
        sum_metric(totals, "nimproxy_requests_total"),
        CAPACITY_MODEL_WINDOW,
    );

    // Per-client: request totals + request-shape averages over the window.
    let mut client_requests: BTreeMap<String, f64> = BTreeMap::new();
    for mv in totals {
        if mv.metric == "nimproxy_requests_total" {
            if let Some(client) = mv.labels.get("client") {
                *client_requests.entry(client.clone()).or_default() += mv.value;
            }
        }
    }
    let msg_s = sum_metric(totals, "nimproxy_request_messages_sum");
    let msg_n = sum_metric(totals, "nimproxy_request_messages_count");
    let tool_s = sum_metric(totals, "nimproxy_request_tools_sum");
    let tool_n = sum_metric(totals, "nimproxy_request_tools_count");
    let total_requests: f64 = client_requests.values().sum();

    // Per-model: governor gate state plus operator-pinned overrides.
    let mut model_caps: Vec<serde_json::Value> = state
        .governor
        .view()
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "model": m.model,
                "limit": m.limit,
                "inflight": m.inflight,
                "blocked": m.blocked,
            })
        })
        .collect();
    model_caps.sort_by(|a, b| a["model"].as_str().cmp(&b["model"].as_str()));
    let overrides: BTreeMap<String, usize> = state
        .cfg()
        .governor
        .overrides
        .iter()
        .map(|(m, c)| (m.clone(), *c))
        .collect();

    let upstream_s = sum_metric(totals, "nimproxy_upstream_seconds_sum");
    let upstream_n = sum_metric(totals, "nimproxy_upstream_seconds_count");
    let upstream_mean = if upstream_n > 0.0 {
        upstream_s / upstream_n
    } else {
        f64::NAN
    };
    let tps_s = sum_metric(totals, "nimproxy_tokens_per_second_sum");
    let tps_n = sum_metric(totals, "nimproxy_tokens_per_second_count");
    let tps_mean = if tps_n > 0.0 { tps_s / tps_n } else { f64::NAN };

    ok_json(serde_json::json!({
        "sampled_at": now,
        "window_seconds": CAPACITY_MODEL_WINDOW,
        "per_key": {
            "rpm": rpms,
            "calibration": calibration,
            "effective_rpm": effective_rpm,
            "capacity_rpm": capacity_rpm,
        },
        "per_client": {
            "count": client_requests.len(),
            "total_requests": total_requests,
            "observed_rpm": req_count_to_rpm(total_requests, CAPACITY_MODEL_WINDOW),
            "avg_messages": if msg_n > 0.0 { msg_s / msg_n } else { f64::NAN },
            "avg_tools": if tool_n > 0.0 { tool_s / tool_n } else { f64::NAN },
        },
        "per_model": {
            "caps": model_caps,
            "overrides": overrides,
        },
        "history_sample": {
            "queue_wait_p50": quantile_from_totals(totals, "nimproxy_queue_wait_seconds", 0.5),
            "queue_wait_p95": quantile_from_totals(totals, "nimproxy_queue_wait_seconds", 0.95),
            "upstream_seconds_mean": upstream_mean,
            "tokens_per_second_mean": tps_mean,
            "ttft_p50": quantile_from_totals(totals, "nimproxy_ttft_seconds", 0.5),
            "requests_per_min": observed_rpm,
        },
    }))
}

fn req_count_to_rpm(count: f64, window_seconds: u64) -> f64 {
    if window_seconds == 0 {
        0.0
    } else {
        count / window_seconds as f64 * 60.0
    }
}

/// Gate the operator-queue surface. Permissions re-checked from the live
/// store: a session whose user was deleted mid-flight gets a stale-session
/// response, and any non-admin role is denied outright.
fn operator_check(state: &AppState, username: &str) -> Result<(), Box<Response>> {
    let role = state.store.lock().unwrap().user(username).map(|u| u.role);
    match role {
        Some(r) if r.is_admin() => Ok(()),
        Some(_) => Err(Box::new(json_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "request queue requires an admin session",
        ))),
        None => Err(Box::new(json_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "your user no longer exists",
        ))),
    }
}

fn json_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": { "message": message.into(), "type": "proxy_error", "code": code }
        })),
    )
        .into_response()
}

fn ok_json(v: serde_json::Value) -> Response {
    axum::Json(v).into_response()
}

/// The request queue: every in-flight `/v1` request (client · model · path ·
/// phase · age), admin-only. Metadata only — never request content.
async fn api_queue(
    State(state): State<Arc<AppState>>,
    Extension(Identity(username)): Extension<Identity>,
) -> Response {
    if let Err(resp) = operator_check(&state, &username) {
        return *resp;
    }
    let now = unix_now();
    let dispatch_policy = state
        .store
        .lock()
        .unwrap()
        .dispatch
        .policy
        .as_str()
        .to_owned();
    let requests: Vec<serde_json::Value> = state
        .registry
        .snapshot()
        .into_iter()
        .map(|e| {
            let age = e.started.elapsed().as_secs();
            serde_json::json!({
                "id": e.id,
                "client": e.client,
                "model": e.model,
                "path": e.path,
                "started_at": now.saturating_sub(age),
                "age_s": age,
                "phase": e.phase,
            })
        })
        .collect();
    ok_json(serde_json::json!({
        "requests": requests,
        "dispatch_policy": dispatch_policy,
    }))
}

#[derive(serde::Deserialize)]
struct TerminateReq {
    id: u64,
}

/// Terminate one in-flight request; its client receives error code `-91`.
/// The admin may kill any request, including their own.
async fn api_queue_terminate(
    State(state): State<Arc<AppState>>,
    Extension(Identity(username)): Extension<Identity>,
    axum::Json(req): axum::Json<TerminateReq>,
) -> Response {
    if let Err(resp) = operator_check(&state, &username) {
        return *resp;
    }
    if state.registry.terminate(req.id) {
        metrics::counter!("nimproxy_terminated_total", "by" => username.clone()).increment(1);
        tracing::info!(
            request = req.id,
            operator = %username,
            "operator terminated an in-flight request (code -91)"
        );
        ok_json(serde_json::json!({ "ok": true }))
    } else {
        json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no such in-flight request {}", req.id),
        )
    }
}

async fn metrics_text(State(state): State<Arc<AppState>>) -> String {
    state.prometheus.render()
}

/// Add hardening headers to every response. The CSP allows the dashboard's
/// own inline script/style, unpkg logos, and Google Fonts (system-font
/// fallback offline), but pins `connect-src` to 'self' so an injected
/// element can't exfiltrate to another origin — a second line of defense
/// behind server-side sanitizing and the dashboard's `esc()`.
async fn security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::HeaderValue;
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; img-src 'self' https://unpkg.com data:; \
             style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
             font-src https://fonts.gstatic.com; \
             script-src 'self' 'unsafe-inline'; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    resp
}

const BANNER: &str = r#"
     _  _ ___ __  __   ___ ___  _____  ____   __
    | \| |_ _|  \/  | | _ \ _ \/ _ \ \/ /\ \ / /
    | .` || || |\/| | |  _/   / (_) >  <  \ V /
    |_|\_|___|_|  |_| |_| |_|_\\___/_/\_\  |_|
"#;

/// `nim-proxy --health`: probe our own /health endpoint and exit 0/1.
/// Exists because the scratch image has no shell or curl for HEALTHCHECK.
fn health_probe() -> ! {
    use std::io::{Read, Write};
    let port = env_or("PORT", "8000");
    let ok = (|| -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port.parse().unwrap_or(8000)))?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut buf = [0u8; 32];
        let n = s.read(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf[..n]).contains("200"))
    })()
    .unwrap_or(false);
    std::process::exit(if ok { 0 } else { 1 });
}

/// Full proxy entry point — everything `main()` used to be. Lives in the
/// library crate so the fuzz targets (fuzz/) can link the internals;
/// src/main.rs is a shim that calls this.
#[tokio::main]
pub async fn run() {
    if std::env::args().any(|a| a == "--health") {
        health_probe();
    }
    dotenvy::dotenv().ok();
    println!("{BANNER}    v{}\n", env!("CARGO_PKG_VERSION"));
    tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nim_proxy=info".into()),
        )
        .init();

    let trust_proxy = env_or("TRUST_PROXY", "false") == "true";
    warn_legacy_env();

    // The config store is the app's source of truth and holds credentials,
    // so its home must exist and be writable before anything else happens.
    let data_dir = std::path::PathBuf::from(env_or("DATA_DIR", "data"));
    if data_dir.as_os_str().is_empty() {
        eprintln!("DATA_DIR must point at a writable directory (the config store lives there)");
        std::process::exit(1);
    }
    let writable = std::fs::create_dir_all(&data_dir).and_then(|()| {
        let probe = data_dir.join(".write-probe");
        std::fs::write(&probe, b"ok")?;
        std::fs::remove_file(&probe)
    });
    if let Err(e) = writable {
        eprintln!(
            "\nnim-proxy cannot start: DATA_DIR {} is not writable ({e}).\n\
             The config store (settings, users, keys) persists there.\n",
            data_dir.display()
        );
        std::process::exit(1);
    }
    let stored = match config::load(&data_dir) {
        Ok(Some(sc)) => sc,
        Ok(None) => config::StoredConfig::default(),
        Err(e) => {
            eprintln!("\nnim-proxy cannot start: {e}\n");
            std::process::exit(1);
        }
    };
    let setup_required = stored.superuser().is_none();
    let cfg = stored.runtime();
    let port: u16 = env_or("PORT", "8000").parse().expect("PORT");

    if setup_required {
        tracing::warn!(
            "SETUP REQUIRED — no superuser exists yet. The FIRST VISITOR to the dashboard \
             claims this proxy; finish setup immediately. /v1 stays closed until then."
        );
    }
    tracing::info!(
        "config store      {}",
        config::store_path(&data_dir).display()
    );
    tracing::info!("upstream          {}", cfg.base_url);
    if cfg.upstreams.len() > 1 {
        tracing::info!(
            "failover          {} backup endpoint(s) configured",
            cfg.upstreams.len() - 1
        );
    }
    let pool_specs = stored.pool_specs();
    tracing::info!(
        "lanes             {} enabled key(s), {} rpm aggregate",
        pool_specs.iter().filter(|s| s.enabled).count(),
        pool_specs
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.rpm)
            .sum::<usize>()
    );
    tracing::info!(
        "API auth          {}",
        match &cfg.clients {
            Some(c) => format!("keyed ({} client key(s))", c.len()),
            None => "open (no client keys required — keep this on a trusted network)".to_owned(),
        }
    );
    tracing::info!(
        "dashboard auth    {}",
        if setup_required {
            "setup wizard (no users yet)".to_owned()
        } else {
            format!("session ({} user(s))", stored.users.len())
        }
    );
    tracing::info!(
        "patience          waits up to {}s per request, heartbeat every {}s",
        cfg.max_wait.as_secs(),
        cfg.heartbeat.as_secs()
    );
    tracing::info!(
        "dispatch          {} scheduling (aging {}s in fair mode)",
        stored.dispatch.policy,
        stored.dispatch.fair_aging_secs
    );

    // Histogram bucket bounds, one row per metric.
    #[rustfmt::skip]
    let buckets: &[(&str, &[f64])] = &[
        ("nimproxy_ttft_seconds",       &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]),
        ("nimproxy_tokens_per_second",  &[1.0, 2.0, 5.0, 10.0, 20.0, 40.0, 80.0, 160.0, 320.0]),
        ("nimproxy_queue_wait_seconds", &[0.001, 0.05, 0.25, 1.0, 5.0, 15.0, 60.0, 180.0, 600.0]),
        ("nimproxy_upstream_seconds",   &[0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0]),
        ("nimproxy_tpot_seconds",       &[0.005, 0.01, 0.02, 0.04, 0.08, 0.16, 0.32]),
        ("nimproxy_request_messages",   &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0]),
        ("nimproxy_request_tools",      &[0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0]),
        ("nimproxy_request_max_tokens", &[128.0, 256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0, 16384.0, 32768.0, 65536.0, 131072.0]),
        ("nimproxy_request_temperature", &[0.0, 0.2, 0.4, 0.6, 0.8, 1.0, 1.2, 1.5, 2.0]),
    ];
    let mut builder = PrometheusBuilder::new();
    for (name, bounds) in buckets {
        builder = builder
            .set_buckets_for_metric(Matcher::Full((*name).into()), bounds)
            .unwrap();
    }
    let prometheus = builder.install_recorder().expect("prometheus recorder");

    // Rate-state restore: resume the upstream pacing window and governor caps
    // from the last run, arming the slow-start ramp while the persisted
    // state is still fresh. ratestate::load never hard-errors — a missing or
    // corrupt file simply means starting with a fresh window.
    let saved = ratestate::load(&data_dir);
    let ramp_secs: u64 = env_or("NIMPROXY_RAMP_SECS", &stored.recovery.ramp_secs.to_string())
        .parse()
        .expect("NIMPROXY_RAMP_SECS");
    let ramp_factor: f64 = env_or(
        "NIMPROXY_RAMP_FACTOR",
        &stored.recovery.ramp_factor.to_string(),
    )
    .parse()
    .expect("NIMPROXY_RAMP_FACTOR");
    let saved_governor = saved
        .as_ref()
        .map(|rs| rs.governor.clone())
        .unwrap_or_default();
    let (restored_lanes, dropped_lanes) = match &saved {
        Some(rs) => (rs.lanes.len() as u64, rs.dropped),
        None => (0, 0),
    };
    let ramp_until_epoch = match &saved {
        Some(rs)
            if ramp_secs > 0
                && rs
                    .file_age
                    .is_some_and(|a| a < ratestate::RAMP_STALE_CUTOFF) =>
        {
            let until = unix_now() + ramp_secs;
            tracing::info!(
                ramp_secs,
                ramp_factor,
                "recovered rate state is fresh; slow-start ramp armed"
            );
            until
        }
        _ => 0,
    };
    gauge!("nimproxy_ramp_active").set(if ramp_until_epoch > 0 { 1.0 } else { 0.0 });
    let pool_value = match saved {
        Some(rs) => Pool::restore(
            pool_specs,
            &rs.lanes,
            if ramp_until_epoch > 0 {
                Instant::now() + Duration::from_secs(ramp_secs)
            } else {
                Instant::now()
            },
            ramp_factor,
        ),
        None => Pool::new(pool_specs),
    };
    let pool = Arc::new(RwLock::new(Arc::new(pool_value)));

    // Metrics history: finish indexing before the listener can report ready,
    // then sample the registry with contemporaneous pool capacity.
    let hist = Arc::new(history::History::load(
        Some(data_dir.clone()),
        stored.history.days,
        capacity_snapshot(&pool.read().unwrap()),
    ));
    {
        let hist = hist.clone();
        let prom = prometheus.clone();
        let pool = pool.clone();
        // Undocumented test knob; the 5-minute default is the contract.
        let sample_secs: u64 = env_or("HISTORY_SAMPLE_SECS", &history::SAMPLE_SECS.to_string())
            .parse()
            .expect("HISTORY_SAMPLE_SECS");
        tokio::spawn(async move {
            loop {
                hist.append(
                    unix_now(),
                    &prom.render(),
                    capacity_snapshot(&pool.read().unwrap()),
                );
                tokio::time::sleep(Duration::from_secs(sample_secs.max(1))).await;
            }
        });
    }

    let upstream_selector =
        std::sync::Mutex::new(upstream::UpstreamSelector::new(cfg.upstreams.clone()));

    let state = Arc::new(AppState {
        dispatch: Dispatcher::new(pool.clone(), &stored.dispatch),
        pool,
        upstream_selector,
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // No overall timeout: generations stream for a long time.
            .build()
            .expect("http client"),
        models_cache: Mutex::new(None),
        no_inject: std::sync::Mutex::new(std::collections::HashSet::new()),
        model_labels: std::sync::Mutex::new(std::collections::HashSet::new()),
        admin: Admin::new(trust_proxy),
        inflight: AtomicUsize::new(0),
        governor: Arc::new(governor::Governor::restored(&saved_governor)),
        history: hist,
        ramp_until: AtomicU64::new(ramp_until_epoch),
        restored_lanes: AtomicU64::new(restored_lanes),
        dropped_lanes: AtomicU64::new(dropped_lanes),
        ratestate_dirty: std::sync::atomic::AtomicBool::new(false),
        prometheus,
        config_revision: AtomicU64::new(1),
        started: unix_now(),
        sse_connections: AtomicUsize::new(0),
        store: std::sync::Mutex::new(stored),
        data_dir,
        setup_required: std::sync::atomic::AtomicBool::new(setup_required),
        cfg: RwLock::new(Arc::new(cfg)),
        registry: Arc::new(registry::RequestRegistry::new()),
    });

    // Rate-state saver: persists the pool's windows + governor caps every
    // interval, and sooner whenever something changed them (settings swap
    // raises the dirty flag). Snapshots the pool under a momentary read
    // lock; the atomic write keeps the file whole on any crash.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let dir = state.data_dir.clone();
            let mut last_save = Instant::now();
            loop {
                let due = state
                    .ratestate_dirty
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                    || last_save.elapsed() >= ratestate::SAVE_INTERVAL;
                if due {
                    last_save = Instant::now();
                    let lanes = state.pool().lane_states();
                    let limits = state.governor.limits();
                    if let Err(e) = ratestate::save(&dir, &lanes, &limits) {
                        tracing::warn!("rate-state save failed: {e}");
                    }
                    gauge!("nimproxy_ramp_active").set(
                        if unix_now() < state.ramp_until.load(std::sync::atomic::Ordering::SeqCst) {
                            1.0
                        } else {
                            0.0
                        },
                    );
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    let dash = || async {
        (
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            include_str!("dashboard.html"),
        )
    };
    // Session-gated surface: dashboard, config, history, metrics. The guard
    // middleware requires an authenticated user (session cookie, or
    // user:password header credentials for scrapers); pre-setup it routes
    // everything to the wizard.
    let protected = Router::new()
        .route("/", get(dash))
        .route("/dash", get(dash))
        .route("/api/dashboard", get(api_dashboard))
        .route("/api/dashboard/now", get(api_dashboard_now))
        .route("/api/dashboard/stream", get(api_dashboard_stream))
        .route("/api/dashboard/capacity-model", get(api_capacity_model))
        .route("/api/models", get(proxy::api_models))
        .route("/api/queue", get(api_queue))
        .route("/api/queue/terminate", post(api_queue_terminate))
        .route("/api/config", get(settings::api_config))
        .route("/api/settings/nim-keys", post(settings::nim_keys))
        .route("/api/settings/clients", post(settings::clients))
        .route("/api/settings/upstream", post(settings::upstream))
        .route("/api/settings/limits", post(settings::limits))
        .route("/api/settings/pricing", post(settings::pricing))
        .route("/api/settings/history", post(settings::history))
        .route("/api/settings/governor", post(settings::governor_cfg))
        .route("/api/settings/recovery", post(settings::recovery))
        .route("/api/settings/dispatch", post(settings::dispatch_cfg))
        .route("/api/settings/users", post(settings::users))
        .route("/api/settings/account", post(settings::account))
        .route("/api/settings/validate-key", post(settings::validate_key))
        .route("/metrics", get(metrics_text))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    // Public surface: health probe, login flow, the first-run wizard (404
    // once setup completes), and the API (its own key gate + setup gate).
    let app = Router::new()
        .merge(protected)
        .route("/health", get(|| async { "ok" }))
        .route("/login", get(auth::login_page).post(auth::login_submit))
        .route("/logout", post(auth::logout))
        .route(
            "/setup",
            get(settings::setup_page).post(settings::setup_submit),
        )
        .route("/setup/validate-key", post(settings::setup_validate_key))
        .route("/v1/{*path}", any(proxy::handle))
        .layer(axum::middleware::from_fn(security_headers))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state.clone());

    let host = env_or("HOST", "0.0.0.0");
    let addr = format!("{host}:{port}");
    tracing::info!("dashboard         http://localhost:{port}/  (metrics at /metrics)");
    tracing::info!("listening on      {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Docker sends SIGTERM on stop; terminals send SIGINT.
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                let _ = ctrl_c.await;
            }
            tracing::info!("shutting down");
        })
        .await
        .expect("server");

    // Clean stop (docker stop, restart helper): flush the last rate-limit
    // snapshot so the next boot resumes the pacing windows and governor caps
    // exactly as they stood when traffic drained.
    let lanes = state.pool().lane_states();
    let limits = state.governor.limits();
    if let Err(e) = ratestate::save(&state.data_dir, &lanes, &limits) {
        tracing::warn!("final rate-state save failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(metric: &str, labels: &[(&str, &str)], value: f64) -> history::MetricValue {
        history::MetricValue {
            metric: metric.to_owned(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            value,
        }
    }

    #[test]
    fn sum_metric_sums_matching_series_only() {
        let totals = vec![
            total("nimproxy_queue_wait_seconds_sum", &[], 12.0),
            total("nimproxy_queue_wait_seconds_count", &[], 4.0),
            total("nimproxy_requests_total", &[("client", "a")], 10.0),
            total("nimproxy_requests_total", &[("client", "b")], 5.0),
        ];
        assert_eq!(sum_metric(&totals, "nimproxy_requests_total"), 15.0);
        assert_eq!(
            sum_metric(&totals, "nimproxy_queue_wait_seconds_count"),
            4.0
        );
        assert_eq!(sum_metric(&totals, "nimproxy_missing"), 0.0);
    }

    #[test]
    fn quantile_from_totals_interpolates_across_cumulative_buckets() {
        // 4 observations: 0.5, 0.5, 1.5, 1.5 seconds.
        let totals = vec![
            total("nimproxy_queue_wait_seconds_bucket", &[("le", "0.5")], 2.0),
            total("nimproxy_queue_wait_seconds_bucket", &[("le", "1.0")], 2.0),
            total("nimproxy_queue_wait_seconds_bucket", &[("le", "2.0")], 4.0),
            total("nimproxy_queue_wait_seconds_bucket", &[("le", "+Inf")], 4.0),
        ];
        // p50 target = 2 observations -> lands atop the 0.5..1.0 bucket.
        let p50 = quantile_from_totals(&totals, "nimproxy_queue_wait_seconds", 0.5).unwrap();
        assert!((p50 - 0.5).abs() < 1e-9, "p50 = {p50}");
        // p100 = all observations -> tops out at the last finite bucket.
        let p100 = quantile_from_totals(&totals, "nimproxy_queue_wait_seconds", 1.0).unwrap();
        assert!((p100 - 2.0).abs() < 1e-9, "p100 = {p100}");
    }

    #[test]
    fn quantile_from_totals_empty_returns_none() {
        assert_eq!(
            quantile_from_totals(&[], "nimproxy_queue_wait_seconds", 0.5),
            None
        );
    }

    #[test]
    fn req_count_to_rpm_scales_by_window() {
        assert_eq!(req_count_to_rpm(60.0, 60), 60.0);
        assert_eq!(req_count_to_rpm(30.0, 3600), 0.5);
        assert_eq!(req_count_to_rpm(100.0, 0), 0.0);
    }
}
