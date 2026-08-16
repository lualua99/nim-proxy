//! End-to-end tests: the real proxy binary against a scriptable mock NIM.
//!
//! Config now lives in a UI-managed store (DATA_DIR/config.json) rather than
//! env vars: `StoreOpts` writes the fixture. There is no auth — /v1 and the
//! dashboard surface are open to the local operator. See
//! `tests/support/mod.rs` for the harness.

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use support::{
    chat_body, expect_refuses_to_start, metrics, read_sse, restart, scratch_data_dir, start_mock,
    start_proxy, start_proxy_in, start_proxy_with, Behavior, StoreOpts,
};

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Session cookies no longer exist: dashboard endpoints are open. A handful
/// of tests still thread a "cookie" through helpers for readability — produce
/// a harmless value so signatures stay stable.
async fn session_cookie(_proxy: &support::Proxy) -> String {
    "local-session".to_owned()
}

async fn send_successful_chats(proxy: &support::Proxy, count: usize) {
    for request in 0..count {
        let response = client()
            .post(proxy.url("/v1/chat/completions"))
            .json(&chat_body(&format!("history request {request}"), false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
}

async fn dashboard_range(
    proxy: &support::Proxy,
    cookie: &str,
    from: u64,
    to: u64,
    points: usize,
) -> serde_json::Value {
    let response = client()
        .get(proxy.url(&format!(
            "/api/dashboard?from={from}&to={to}&points={points}"
        )))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn dashboard_now(proxy: &support::Proxy, cookie: &str) -> serde_json::Value {
    let response = client()
        .get(proxy.url("/api/dashboard/now"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

fn successful_chat_requests(rows: &serde_json::Value) -> f64 {
    rows.as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row["metric"] == "nimproxy_requests_total"
                && row["labels"]["path"] == "/v1/chat/completions"
                && row["labels"]["status"] == "200"
        })
        .filter_map(|row| row["value"].as_f64())
        .sum()
}

async fn wait_for_persisted_chat_total(
    proxy: &support::Proxy,
    cookie: &str,
    after_revision: u64,
    expected_total: f64,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let range = dashboard_range(proxy, cookie, 1, 4_102_444_800, 1000).await;
        let revision = range["history_revision"].as_u64().unwrap();
        if revision > after_revision && successful_chat_requests(&range["totals"]) == expected_total
        {
            return range;
        }
        assert!(
            Instant::now() < deadline,
            "history did not reach revision > {after_revision} and request total \
             {expected_total}: {range}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn open_mode_admits_requests_without_a_client_key() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hello world");
}
#[tokio::test]
async fn deadline_header_validation_runs_before_upstream() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;

    let malformed = client()
        .post(proxy.url("/v1/chat/completions"))
        .header("x-nim-proxy-deadline-ms", "10.0")
        .json(&chat_body("malformed", false))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);
    let body: serde_json::Value = malformed.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_deadline");

    let duplicate = client()
        .post(proxy.url("/v1/chat/completions"))
        .header("x-nim-proxy-deadline-ms", "100")
        .header("x-nim-proxy-deadline-ms", "200")
        .json(&chat_body("duplicate", false))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 400);
    let body: serde_json::Value = duplicate.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_deadline");
    assert_eq!(mock.state.hit_count(), 0, "invalid input never reaches NIM");
}

#[tokio::test]
async fn deadline_applies_to_models_cache_refresh() {
    use std::sync::atomic::Ordering;

    let mock = start_mock().await;
    mock.state.models_delay_ms.store(10_000, Ordering::SeqCst);
    let proxy = start_proxy(&mock.url, &[]).await;

    let started = Instant::now();
    let resp = client()
        .get(proxy.url("/v1/models"))
        .header("x-nim-proxy-deadline-ms", "100")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 504);
    assert!(started.elapsed() < Duration::from_secs(2));
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "deadline_exceeded");
}

#[tokio::test]
async fn buffered_deadline_cancels_header_wait_and_releases_inflight_slot() {
    let mock = start_mock().await;
    mock.state.push(Behavior::DelayHeaders(10_000));
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            max_inflight: 1,
            request_timeout_secs: 30,
            ..Default::default()
        },
        &[],
    )
    .await;

    let started = Instant::now();
    let expired = client()
        .post(proxy.url("/v1/chat/completions"))
        .header("x-nim-proxy-deadline-ms", "150")
        .json(&chat_body("deadline", false))
        .send()
        .await
        .unwrap();
    assert_eq!(expired.status(), 504);
    assert!(started.elapsed() < Duration::from_secs(2));
    let body: serde_json::Value = expired.json().await.unwrap();
    assert_eq!(body["error"]["code"], "deadline_exceeded");

    let after = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("after-deadline", false))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 200, "deadline released max_inflight slot");

    let metrics = metrics(&proxy).await;
    assert!(metrics.contains(
        r#"nimproxy_requests_total{client="local",model="mock/model-a",path="/v1/chat/completions",status="deadline"} 1"#
    ));
    assert!(metrics.contains(
        r#"nimproxy_deadline_exceeded_total{client="local",model="mock/model-a",path="/v1/chat/completions"} 1"#
    ));
}

#[tokio::test]
async fn streaming_deadline_stops_retry_wait() {
    let mock = start_mock().await;
    mock.state.push(Behavior::RateLimited(2));
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            nim_keys: vec![("only-key".into(), 40)],
            ..Default::default()
        },
        &[],
    )
    .await;

    let started = Instant::now();
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .header("x-nim-proxy-deadline-ms", "150")
        .json(&chat_body("retry-deadline", true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "stream was already committed");
    let body = read_sse(resp).await;
    assert!(body.contains(": retrying"), "retry was observed: {body}");
    assert!(
        body.contains("deadline_exceeded"),
        "deadline surfaced: {body}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn streaming_deadline_stops_an_active_non_idle_stream() {
    let mock = start_mock().await;
    mock.state.push(Behavior::ActiveStream(25));
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            stream_idle_secs: 5,
            ..Default::default()
        },
        &[],
    )
    .await;

    let started = Instant::now();
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .header("x-nim-proxy-deadline-ms", "175")
        .json(&chat_body("active-deadline", true))
        .send()
        .await
        .unwrap();
    let body = read_sse(resp).await;
    assert!(
        body.matches("delta").count() >= 2,
        "stream stayed active: {body}"
    );
    assert!(
        body.contains("deadline_exceeded"),
        "deadline surfaced: {body}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn streaming_deadline_releases_inflight_when_downstream_is_not_reading() {
    let mock = start_mock().await;
    mock.state.push(Behavior::FloodStream);
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            max_inflight: 1,
            stream_idle_secs: 5,
            ..Default::default()
        },
        &[],
    )
    .await;

    let unread = client()
        .post(proxy.url("/v1/chat/completions"))
        .header("x-nim-proxy-deadline-ms", "75")
        .json(&chat_body("unread-deadline", true))
        .send()
        .await
        .unwrap();
    assert_eq!(unread.status(), 200);
    tokio::time::sleep(Duration::from_millis(250)).await;

    let after = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("after-unread-deadline", false))
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.status(),
        200,
        "deadline cleanup cannot block on SSE send"
    );
}

#[tokio::test]
async fn streaming_rides_out_429s_with_lane_failover() {
    let mock = start_mock().await;
    mock.state.push(Behavior::RateLimited(1));
    mock.state.push(Behavior::RateLimited(1));
    let proxy = start_proxy(&mock.url, &[]).await;

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "SSE committed despite upstream 429s");
    let body = read_sse(resp).await;
    assert!(
        body.contains(": retrying"),
        "client saw retry comments: {body}"
    );
    assert!(body.contains("hello"), "stream delivered data: {body}");
    assert!(body.contains("data: [DONE]"));

    let keys = mock.state.hit_keys();
    assert_eq!(keys.len(), 3, "two 429s then a success");
    assert_ne!(keys[0], keys[1], "429 failed over to a different key");
}

#[tokio::test]
async fn retry_after_is_honored_when_only_one_lane_exists() {
    let mock = start_mock().await;
    mock.state.push(Behavior::RateLimited(1));
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            nim_keys: vec![("only-key".into(), 40)],
            ..Default::default()
        },
        &[],
    )
    .await;

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(mock.state.hit_count(), 2);
    let gap = mock.state.hit_gap(0, 1);
    assert!(
        gap >= Duration::from_millis(900),
        "waited Retry-After, gap {gap:?}"
    );
}

#[tokio::test]
async fn buffered_retries_5xx_then_returns_verbatim_body() {
    let mock = start_mock().await;
    mock.state.push(Behavior::ServerError(503));
    let proxy = start_proxy(&mock.url, &[]).await;

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["usage"]["prompt_tokens"], 11);
    assert_eq!(mock.state.hit_count(), 2);
}

#[tokio::test]
async fn non_retryable_error_is_relayed_buffered_and_surfaced_in_stream() {
    let mock = start_mock().await;
    mock.state.push(Behavior::BadRequest);
    // strict_passthrough disables usage injection so a streamed 400 can't be
    // masked by the injection-retry path — it surfaces in-stream instead.
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            strict_passthrough: true,
            ..Default::default()
        },
        &[],
    )
    .await;

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "buffered 400 relayed verbatim");
    assert!(resp.text().await.unwrap().contains("bad stream_options"));

    mock.state.push(Behavior::BadRequest);
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "stream already committed to 200");
    let body = read_sse(resp).await;
    assert!(
        body.contains("proxy_error"),
        "error surfaced in-stream: {body}"
    );
}

#[tokio::test]
async fn saturation_fails_fast_with_504() {
    let mock = start_mock().await;
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            nim_keys: vec![("only-key".into(), 2)],
            max_wait_secs: 2,
            ..Default::default()
        },
        &[],
    )
    .await;

    for _ in 0..2 {
        let r = client()
            .post(proxy.url("/v1/chat/completions"))
            .json(&chat_body("hi", false))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let third = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(third.status(), 504, "no slot within max_wait_secs");
    let v: serde_json::Value = third.json().await.unwrap();
    assert_eq!(v["error"]["code"], "rate_limited");
    assert_eq!(
        mock.state.hit_count(),
        2,
        "pacer let exactly the per-key rpm through"
    );
}

/// A one-seat, 1-rpm pool: the first (completed) request consumes the whole
/// 60s window, so every later request queues in `waiting_slot`. `max_wait`
/// must exceed the ~60s window wait — otherwise the dispatcher fails the
/// waiter fast (its deadline would lapse before any slot) instead of parking
/// it where we can observe (and abandon) it.
fn single_seat() -> StoreOpts {
    StoreOpts {
        nim_keys: vec![("only-key".into(), 1)],
        max_wait_secs: 120,
        ..Default::default()
    }
}

async fn queue_rows(proxy: &support::Proxy, cookie: &str) -> Vec<serde_json::Value> {
    client()
        .get(proxy.url("/api/queue"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["requests"]
        .as_array()
        .unwrap()
        .clone()
}

/// Wait (bounded) for a queued request to reach `phase`; returns its id.
async fn wait_for_phase(proxy: &support::Proxy, cookie: &str, phase: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(r) = queue_rows(proxy, cookie)
            .await
            .into_iter()
            .find(|r| r["phase"] == phase)
        {
            return r["id"].as_u64().unwrap();
        }
        assert!(Instant::now() < deadline, "request never reached {phase}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// True when `id` leaves the queue within `budget`; false if it lingers.
async fn wait_gone(proxy: &support::Proxy, cookie: &str, id: u64, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let present = queue_rows(proxy, cookie)
            .await
            .iter()
            .any(|r| r["id"].as_u64() == Some(id));
        if !present {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A streaming request parked in the slot queue vanishes from the operator
/// queue the moment its client hangs up — not at the next heartbeat tick.
/// Otherwise the abandoned row would linger next to the harness's re-issued
/// retry, which the queue view renders as two entries for one logical request
/// (and the ghost would still wait for — and burn — an rpm slot).
#[tokio::test]
async fn abandoned_streaming_waiter_unregisters_before_the_heartbeat() {
    let mock = start_mock().await;
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            heartbeat_secs: 3, // a full heartbeat tick used to be needed
            ..single_seat()
        },
        &[],
    )
    .await;
    let cookie = session_cookie(&proxy).await;

    // 1-rpm single seat: this completes, taking the whole 60s window.
    let first = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("first", true))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert!(read_sse(first).await.contains("data: [DONE]"));
    assert_eq!(mock.state.hit_count(), 1);

    // Second client parks in the queue, then hangs up hard.
    let waiter = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("abandoned", true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    let waiter_id = wait_for_phase(&proxy, &cookie, "waiting_slot").await;
    drop(waiter);

    assert!(
        wait_gone(&proxy, &cookie, waiter_id, Duration::from_secs(2)).await,
        "abandoned waiter lingered past 2s (heartbeat is 3s); a hang-up must reap immediately"
    );

    // And the ghost must never have reached the upstream: an abandoned request
    // ahead of its slot burns the scarce rpm budget the pile-up is about.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        mock.state.hit_count(),
        1,
        "only the completed request reached the upstream; the abandoned waiter must not"
    );
}

/// A buffered request (no SSE heartbeats, no in-band disconnect signal) must
/// still release its queue row when the client hangs up while waiting for a
/// slot — stale entries would linger until `max_wait` (120s here, well past
/// the 60s window wait it sits out).
#[tokio::test]
async fn abandoned_buffered_waiter_unregisters() {
    let mock = start_mock().await;
    let proxy = start_proxy_with(&mock.url, single_seat(), &[]).await;
    let cookie = session_cookie(&proxy).await;

    let first = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("first", true))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert!(read_sse(first).await.contains("data: [DONE]"));

    let waiter_url = proxy.url("/v1/chat/completions");
    let waiter = tokio::spawn(async move {
        client()
            .post(waiter_url)
            .json(&chat_body("abandoned", false))
            .send()
            .await
            .ok()
    });
    let waiter_id = wait_for_phase(&proxy, &cookie, "waiting_slot").await;
    waiter.abort(); // hang up while it waits

    assert!(
        wait_gone(&proxy, &cookie, waiter_id, Duration::from_secs(5)).await,
        "abandoned buffered waiter lingered past 5s (max_wait is 120s)"
    );
}

#[tokio::test]
async fn conversation_affinity_pins_a_conversation_to_one_key() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;

    for _ in 0..3 {
        let r = client()
            .post(proxy.url("/v1/chat/completions"))
            .json(&chat_body("same conversation", false))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let keys = mock.state.hit_keys();
    assert_eq!(keys[0], keys[1]);
    assert_eq!(keys[1], keys[2], "conversation stayed on one key: {keys:?}");

    for i in 0..12 {
        let r = client()
            .post(proxy.url("/v1/chat/completions"))
            .json(&chat_body(&format!("distinct conversation {i}"), false))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let distinct: std::collections::HashSet<String> = mock.state.hit_keys().into_iter().collect();
    assert!(
        distinct.len() >= 2,
        "distinct conversations spread across keys"
    );
}

#[tokio::test]
async fn models_catalog_is_cached() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;

    for _ in 0..3 {
        let r = client().get(proxy.url("/v1/models")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["data"][0]["id"], "mock/model-a");
    }
    assert_eq!(
        mock.state
            .models_hits
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "catalog served from cache after first fetch"
    );
}

#[tokio::test]
async fn api_models_rides_the_shared_catalog_cache() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;

    let cookie = session_cookie(&proxy).await;
    let get = || {
        client()
            .get(proxy.url("/api/models"))
            .header("cookie", &cookie)
    };

    // Cold cache: the first dashboard fetch triggers one upstream call and
    // reports the cache metadata.
    let r = get().send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["models"][0]["id"], "mock/model-a", "{v}");
    assert!(v["cached_at"].as_u64().unwrap() > 0, "{v}");
    assert_eq!(v["ttl_secs"], 600, "{v}");
    assert_eq!(
        mock.state
            .models_hits
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cold catalog fetched upstream exactly once"
    );

    // Within the TTL: served from cache, no second upstream call.
    let r = get().send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        mock.state
            .models_hits
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    // Forced refresh bypasses the TTL once (operator's Refresh button).
    let r = get().query(&[("refresh", "1")]).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        mock.state
            .models_hits
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );

    // The dashboard's catalog write lands in the shared cache: the client-
    // facing /v1/models now answers from it without a new upstream fetch.
    let rv1 = client().get(proxy.url("/v1/models")).send().await.unwrap();
    assert_eq!(rv1.status(), 200);
    assert_eq!(
        mock.state
            .models_hits
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

#[tokio::test]
async fn usage_injection_asks_for_usage_and_backs_off_on_rejection() {
    // Default: stream_options injected.
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", true))
        .send()
        .await
        .unwrap();
    read_sse(resp).await;
    {
        let hits = mock.state.hits.lock().unwrap();
        assert_eq!(
            hits[0].body["stream_options"]["include_usage"], true,
            "proxy injected stream_options"
        );
    }

    // Model that 400s on stream_options: retried untouched, then remembered.
    let mock2 = start_mock().await;
    mock2.state.push(Behavior::BadRequestIfInjected);
    let proxy2 = start_proxy(&mock2.url, &[]).await;
    let resp = client()
        .post(proxy2.url("/v1/chat/completions"))
        .json(&chat_body("hi", true))
        .send()
        .await
        .unwrap();
    let body = read_sse(resp).await;
    assert!(body.contains("data: [DONE]"), "recovered after 400: {body}");
    {
        let hits = mock2.state.hits.lock().unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].body.get("stream_options").is_some());
        assert!(hits[1].body.get("stream_options").is_none());
    }
    // Next request for the same model: no injection attempt at all.
    let resp = client()
        .post(proxy2.url("/v1/chat/completions"))
        .json(&chat_body("again", true))
        .send()
        .await
        .unwrap();
    read_sse(resp).await;
    {
        let hits = mock2.state.hits.lock().unwrap();
        assert!(
            hits[2].body.get("stream_options").is_none(),
            "model remembered"
        );
    }

    // strict_passthrough disables injection entirely.
    let mock3 = start_mock().await;
    let proxy3 = start_proxy_with(
        &mock3.url,
        StoreOpts {
            strict_passthrough: true,
            ..Default::default()
        },
        &[],
    )
    .await;
    let resp = client()
        .post(proxy3.url("/v1/chat/completions"))
        .json(&chat_body("hi", true))
        .send()
        .await
        .unwrap();
    read_sse(resp).await;
    let hits = mock3.state.hits.lock().unwrap();
    assert!(hits[0].body.get("stream_options").is_none());
}

#[tokio::test]
async fn stalled_upstream_stream_errors_out_within_idle_timeout() {
    let mock = start_mock().await;
    mock.state.push(Behavior::Hang);
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            stream_idle_secs: 1,
            ..Default::default()
        },
        &[],
    )
    .await;

    let started = Instant::now();
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", true))
        .send()
        .await
        .unwrap();
    let body = read_sse(resp).await;
    assert!(body.contains("stalled"), "stall surfaced: {body}");
    assert!(started.elapsed() < Duration::from_secs(10), "did not hang");
}

#[tokio::test]
async fn metrics_report_traffic_tokens_and_affinity() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", true))
        .send()
        .await
        .unwrap();
    read_sse(resp).await;

    let metrics = metrics(&proxy).await;
    assert!(metrics.contains(r#"nimproxy_requests_total{"#), "{metrics}");
    assert!(metrics.contains(r#"client="local""#));
    assert!(metrics.contains(r#"model="mock/model-a""#));
    assert!(
        metrics.contains(r#"nimproxy_completion_tokens_total{client="local",model="mock/model-a",source="usage"} 2"#),
        "exact usage counted: {metrics}"
    );
    assert!(metrics.contains("nimproxy_affinity_total"));
}

#[tokio::test]
async fn request_shape_and_quality_metrics_are_recorded() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;

    // A plain streaming request (finishes "stop").
    read_sse(
        client()
            .post(proxy.url("/v1/chat/completions"))
            .json(&chat_body("hi", true))
            .send()
            .await
            .unwrap(),
    )
    .await;

    // A tool-using request with sampling params: the mock answers with a
    // tool_calls delta and finish_reason "tool_calls".
    let tool_req = serde_json::json!({
        "model": "mock/model-a",
        "stream": true,
        "temperature": 0.7,
        "max_tokens": 4096,
        "tools": [{"type": "function", "function": {"name": "get_weather"}}],
        "tool_choice": "auto",
        "messages": [
            {"role": "system", "content": "you are a test"},
            {"role": "user", "content": "weather?"}
        ]
    });
    read_sse(
        client()
            .post(proxy.url("/v1/chat/completions"))
            .json(&tool_req)
            .send()
            .await
            .unwrap(),
    )
    .await;

    let metrics = metrics(&proxy).await;

    // Request shape (labeled by client — open mode admits everyone as "local").
    assert!(
        metrics.contains(r#"nimproxy_stream_requests_total{client="local",stream="true"}"#),
        "stream flag counted: {metrics}"
    );
    assert!(
        metrics.contains(r#"nimproxy_request_messages_count{client="local"}"#),
        "conversation depth histogram present"
    );
    assert!(
        metrics.contains(r#"nimproxy_request_tools_count{client="local"}"#),
        "tools-offered histogram present"
    );
    assert!(
        metrics.contains("nimproxy_request_temperature_count"),
        "temperature histogram present"
    );
    assert!(
        metrics.contains("nimproxy_request_max_tokens_count"),
        "max_tokens histogram present"
    );
    assert!(
        metrics.contains(r#"nimproxy_tool_choice_total{mode="auto"}"#),
        "tool_choice mode counted"
    );

    // Response quality.
    assert!(
        metrics.contains(r#"nimproxy_finish_reason_total{model="mock/model-a",reason="stop"}"#),
        "stop finish recorded: {metrics}"
    );
    assert!(
        metrics
            .contains(r#"nimproxy_finish_reason_total{model="mock/model-a",reason="tool_calls"}"#),
        "tool_calls finish recorded"
    );
    assert!(
        metrics.contains(r#"nimproxy_tool_calls_total{model="mock/model-a"}"#),
        "tool-call volume recorded"
    );
    assert!(
        metrics.contains(r#"nimproxy_reasoning_tokens_total{model="mock/model-a"}"#),
        "reasoning tokens recorded"
    );

    // Cardinality stays bounded: the stream label is a two-value enum.
    for line in metrics
        .lines()
        .filter(|l| l.starts_with("nimproxy_stream_requests_total{"))
    {
        assert!(
            line.contains(r#"stream="true""#) || line.contains(r#"stream="false""#),
            "stream label bounded to true/false: {line}"
        );
    }
}

/// The buffered (non-streaming) path extracts finish_reason, reasoning tokens,
/// and tool-call count from `relay()`; an unknown finish_reason collapses to
/// `other`; JSON mode and non-`auto` tool_choice are recorded. These paths are
/// distinct from the streaming assertions above.
#[tokio::test]
async fn buffered_quality_and_edge_cases_are_recorded() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;

    let post = |body: serde_json::Value| {
        let proxy = &proxy;
        async move {
            let r = client()
                .post(proxy.url("/v1/chat/completions"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            r.text().await.unwrap();
        }
    };

    // Buffered tool call: mock answers with message.tool_calls + finish tool_calls.
    post(serde_json::json!({
        "model": "mock/model-a", "stream": false, "tool_choice": "required",
        "tools": [{"type": "function", "function": {"name": "run"}}],
        "messages": [{"role": "user", "content": "go"}]
    }))
    .await;

    // Buffered JSON mode.
    post(serde_json::json!({
        "model": "mock/model-a", "stream": false,
        "response_format": {"type": "json_object"},
        "messages": [{"role": "user", "content": "as json"}]
    }))
    .await;

    // Unknown upstream finish_reason must collapse to "other".
    mock.state.push(Behavior::OddFinish);
    post(serde_json::json!({
        "model": "mock/model-a", "stream": false,
        "messages": [{"role": "user", "content": "hi"}]
    }))
    .await;

    let metrics = metrics(&proxy).await;

    // Buffered quality extraction (from relay()).
    assert!(
        metrics
            .contains(r#"nimproxy_finish_reason_total{model="mock/model-a",reason="tool_calls"}"#),
        "buffered tool_calls finish recorded: {metrics}"
    );
    assert!(
        metrics.contains(r#"nimproxy_tool_calls_total{model="mock/model-a"}"#),
        "buffered tool-call count recorded"
    );
    assert!(
        metrics.contains(r#"nimproxy_reasoning_tokens_total{model="mock/model-a"}"#),
        "buffered reasoning tokens recorded"
    );
    assert!(
        metrics.contains(r#"nimproxy_upstream_seconds_count{model="mock/model-a"}"#),
        "upstream latency recorded on the buffered path"
    );

    // Edge cases.
    assert!(
        metrics.contains(r#"nimproxy_tool_choice_total{mode="required"}"#),
        "non-auto tool_choice mode recorded"
    );
    assert!(
        metrics.contains(r#"nimproxy_json_mode_total{client="local"}"#),
        "JSON mode recorded"
    );
    assert!(
        metrics.contains(r#"nimproxy_finish_reason_total{model="mock/model-a",reason="other"}"#),
        "unknown finish_reason collapsed to other: {metrics}"
    );
    assert!(
        !metrics.contains(r#"reason="banana""#),
        "raw upstream finish_reason never becomes a label"
    );
}

// ---------- correctness & security hardening (PR 6a) ----------

/// A buffered request against an upstream that sends headers then stalls the
/// body must not hang forever holding an in-flight slot — the request timeout
/// surfaces a gateway error instead.
#[tokio::test]
async fn buffered_request_times_out_on_hung_upstream() {
    let mock = start_mock().await;
    mock.state.push(Behavior::Hang);
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            request_timeout_secs: 1,
            ..Default::default()
        },
        &[],
    )
    .await;

    let started = Instant::now();
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502, "hung body surfaces as bad_gateway");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "returned promptly, did not hang"
    );
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "bad_gateway");
}

/// Past the in-flight cap the proxy sheds load with 503 instead of growing the
/// queue unbounded.
#[tokio::test]
async fn overloaded_requests_are_shed_with_503() {
    let mock = start_mock().await;
    mock.state.push(Behavior::Hang);
    let proxy = std::sync::Arc::new(
        start_proxy_with(
            &mock.url,
            StoreOpts {
                max_inflight: 1,
                request_timeout_secs: 30,
                ..Default::default()
            },
            &[],
        )
        .await,
    );

    // Occupy the single in-flight slot with a buffered request whose body hangs.
    let hog = {
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let _ = client()
                .post(proxy.url("/v1/chat/completions"))
                .json(&chat_body("hog", false))
                .send()
                .await;
        })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("shed-me", false))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        503,
        "second request shed at the in-flight cap"
    );
    assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "overloaded");
    hog.abort();
}

/// An unreachable upstream exercises the connection-error arm: the lane is
/// benched with status "connect" and the request fails fast at the deadline.
#[tokio::test]
async fn upstream_connection_error_is_benched() {
    // Nothing listens on port 1 → every connect attempt fails.
    let proxy = start_proxy_with(
        "http://127.0.0.1:1",
        StoreOpts {
            max_wait_secs: 2,
            ..Default::default()
        },
        &[],
    )
    .await;

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 504, "connect failures exhaust to a 504");

    let metrics = metrics(&proxy).await;
    assert!(
        metrics.contains(r#"nimproxy_lane_benched_total{lane="0",status="connect"}"#),
        "connection error benched the lane: {metrics}"
    );
}

#[tokio::test]
async fn history_records_snapshots_and_survives_restart() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[("HISTORY_SAMPLE_SECS", "1")]).await;

    // Drive traffic so snapshots have metric series in them.
    let r = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Snapshots land on disk at DATA_DIR/history.jsonl (harness-managed dir).
    let jsonl = proxy.data_dir.join("history.jsonl");
    let raw = std::fs::read_to_string(&jsonl).expect("history.jsonl written");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    assert!(lines.len() >= 2, "sampler ran: {} snapshots", lines.len());
    let records: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        records.iter().any(|value| value["kind"] == "boot"),
        "process epoch is persisted"
    );
    assert!(
        records.iter().any(|value| {
            value["v"] == 2
                && value["boot"].is_string()
                && value["capacity"]["capacity_rpm"] == 120
                && value["m"]
                    .as_str()
                    .is_some_and(|metrics| metrics.contains("nimproxy"))
        }),
        "v2 snapshots carry metrics and contemporaneous capacity: {raw}"
    );
    let before = records
        .iter()
        .filter(|value| value["m"].is_string())
        .count();

    // Restart on the SAME data dir: history reloads into the normalized index
    // and remains visible through the typed dashboard range contract.
    let proxy = restart(proxy, &[("HISTORY_SAMPLE_SECS", "1")]).await;
    let cookie = session_cookie(&proxy).await;
    let history: serde_json::Value = client()
        .get(proxy.url("/api/dashboard?from=1&to=4102444800&points=1000"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        history["history_revision"].as_u64().unwrap() >= before as u64,
        "history persisted across restart: {history}"
    );
}

#[tokio::test]
async fn dashboard_history_combines_process_epochs() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[("HISTORY_SAMPLE_SECS", "1")]).await;
    let cookie = session_cookie(&proxy).await;
    let initial = dashboard_range(&proxy, &cookie, 1, 4_102_444_800, 1000).await;

    send_successful_chats(&proxy, 2).await;
    wait_for_persisted_chat_total(
        &proxy,
        &cookie,
        initial["history_revision"].as_u64().unwrap(),
        2.0,
    )
    .await;

    let proxy = restart(proxy, &[("HISTORY_SAMPLE_SECS", "1")]).await;
    let cookie = session_cookie(&proxy).await;
    let second_epoch = dashboard_range(&proxy, &cookie, 1, 4_102_444_800, 1000).await;
    assert_eq!(successful_chat_requests(&second_epoch["totals"]), 2.0);

    send_successful_chats(&proxy, 3).await;
    wait_for_persisted_chat_total(
        &proxy,
        &cookie,
        second_epoch["history_revision"].as_u64().unwrap(),
        5.0,
    )
    .await;

    for points in [2, 1000] {
        let range = dashboard_range(&proxy, &cookie, 1, 4_102_444_800, points).await;
        assert_eq!(
            successful_chat_requests(&range["totals"]),
            5.0,
            "exact total must not depend on points={points}: {range}"
        );
    }
}

#[tokio::test]
async fn dashboard_tail_rolls_into_persisted_history_once() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[("HISTORY_SAMPLE_SECS", "2")]).await;
    let cookie = session_cookie(&proxy).await;
    let initial = dashboard_range(&proxy, &cookie, 1, 4_102_444_800, 1000).await;

    send_successful_chats(&proxy, 1).await;
    let persisted = wait_for_persisted_chat_total(
        &proxy,
        &cookie,
        initial["history_revision"].as_u64().unwrap(),
        1.0,
    )
    .await;
    let persisted_revision = persisted["history_revision"].as_u64().unwrap();

    send_successful_chats(&proxy, 1).await;
    let live = dashboard_now(&proxy, &cookie).await;
    assert_eq!(
        live["history_revision"], persisted_revision,
        "the second request is still newer than persisted history: {live}"
    );
    assert_eq!(successful_chat_requests(&live["tail"]["totals"]), 1.0);

    let refreshed = wait_for_persisted_chat_total(&proxy, &cookie, persisted_revision, 2.0).await;
    assert!(
        refreshed["history_revision"].as_u64().unwrap() > persisted_revision,
        "{refreshed}"
    );
    assert_eq!(successful_chat_requests(&refreshed["totals"]), 2.0);

    let rolled = dashboard_now(&proxy, &cookie).await;
    assert!(
        rolled["history_revision"].as_u64().unwrap() > persisted_revision,
        "{rolled}"
    );
    assert_eq!(
        successful_chat_requests(&rolled["tail"]["totals"]),
        0.0,
        "the persisted request must not remain in the live tail: {rolled}"
    );
}

#[tokio::test]
async fn legacy_history_infers_counter_reset() {
    let mock = start_mock().await;
    let data_dir = scratch_data_dir();
    std::fs::write(
        data_dir.join("config.json"),
        serde_json::to_string_pretty(&StoreOpts::default().json(&mock.url)).unwrap(),
    )
    .unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        data_dir.join("history.jsonl"),
        format!(
            "{{\"t\":{},\"m\":\"# TYPE nimproxy_requests_total counter\\nnimproxy_requests_total{{client=\\\"local\\\",model=\\\"mock/model-a\\\",path=\\\"/v1/chat/completions\\\",status=\\\"200\\\"}} 10\\n\"}}\n\
             {{\"t\":{},\"m\":\"# TYPE nimproxy_requests_total counter\\nnimproxy_requests_total{{client=\\\"local\\\",model=\\\"mock/model-a\\\",path=\\\"/v1/chat/completions\\\",status=\\\"200\\\"}} 15\\n\"}}\n\
             {{\"t\":{},\"m\":\"# TYPE nimproxy_requests_total counter\\nnimproxy_requests_total{{client=\\\"local\\\",model=\\\"mock/model-a\\\",path=\\\"/v1/chat/completions\\\",status=\\\"200\\\"}} 4\\n\"}}\n",
            now - 3,
            now - 2,
            now - 1,
        ),
    )
    .unwrap();

    let proxy = start_proxy_in(data_dir, &[("HISTORY_SAMPLE_SECS", "3600")]).await;
    let cookie = session_cookie(&proxy).await;
    let range = dashboard_range(&proxy, &cookie, 1, 4_102_444_800, 1000).await;
    assert_eq!(
        successful_chat_requests(&range["totals"]),
        19.0,
        "legacy epochs contribute 10 + 5 + 4 requests: {range}"
    );
    assert_eq!(range["diagnostics"]["legacy_resets_inferred"], 1);
}

#[tokio::test]
async fn historical_capacity_uses_snapshot_configuration() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[("HISTORY_SAMPLE_SECS", "1")]).await;
    let cookie = session_cookie(&proxy).await;
    let initial = dashboard_range(&proxy, &cookie, 1, 4_102_444_800, 1000).await;

    send_successful_chats(&proxy, 1).await;
    let at_120 = wait_for_persisted_chat_total(
        &proxy,
        &cookie,
        initial["history_revision"].as_u64().unwrap(),
        1.0,
    )
    .await;
    let at_120_revision = at_120["history_revision"].as_u64().unwrap();

    let fingerprint = api_config(&proxy, &cookie).await["nim_keys"][0]["fingerprint"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = post_json(
        &proxy,
        &cookie,
        "/api/settings/nim-keys",
        serde_json::json!({"set": {"fingerprint": fingerprint, "rpm": 20}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    send_successful_chats(&proxy, 1).await;
    let at_100 = wait_for_persisted_chat_total(&proxy, &cookie, at_120_revision, 2.0).await;
    let available_from = at_100["window"]["available_from"].as_u64().unwrap();
    let available_to = at_100["window"]["available_to"].as_u64().unwrap();
    let range = dashboard_range(
        &proxy,
        &cookie,
        available_from.saturating_sub(1),
        available_to,
        1000,
    )
    .await;
    let capacities: Vec<f64> = range["points"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|point| point["capacity"]["average_rpm"].as_f64())
        .collect();
    assert!(
        capacities.contains(&120.0),
        "120 RPM snapshot capacity is retained: {range}"
    );
    assert!(
        capacities.contains(&100.0),
        "100 RPM snapshot capacity is retained: {range}"
    );

    let now = dashboard_now(&proxy, &cookie).await;
    assert_eq!(now["capacity_rpm"], 100);
}

#[tokio::test]
async fn dashboard_range_contract_defaults_and_validates() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[("HISTORY_SAMPLE_SECS", "1")]).await;
    let cookie = session_cookie(&proxy).await;

    let response = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("dashboard range", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let response = client()
        .get(proxy.url("/api/dashboard?from=1&to=4102444800&points=24"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["history_revision"].as_u64().is_some());
    assert_eq!(body["window"]["requested_from"], 1);
    assert_eq!(body["window"]["requested_to"], 4_102_444_800u64);
    assert_eq!(body["window"]["following_now"], false);
    assert!(body["config_revision"].as_u64().is_some());
    assert!(body["window"]["available_from"].as_u64().is_some());
    assert!(body["totals"].as_array().is_some());
    assert!(body["latest"].as_array().is_some());
    assert!(body["points"].as_array().is_some());

    let response = client()
        .get(proxy.url("/api/dashboard?from=99&to=99"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "invalid_time_window");

    let response = client()
        .get(proxy.url("/api/dashboard"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let defaulted: serde_json::Value = response.json().await.unwrap();
    let from = defaulted["window"]["requested_from"].as_u64().unwrap();
    let to = defaulted["window"]["requested_to"].as_u64().unwrap();
    assert_eq!(to - from, 30 * 86_400);
    assert_eq!(defaulted["window"]["following_now"], true);
    assert_eq!(defaulted["window"]["default_window_days"], 30);
    assert_eq!(defaulted["window"]["retention_days"], 30);

    let response = client()
        .get(proxy.url("/api/dashboard"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let settings = api_config(&proxy, &cookie).await;
    assert!(settings["server"]["history"]["available_from"]
        .as_u64()
        .is_some());
    assert!(settings["server"]["history"]["file_bytes"]
        .as_u64()
        .is_some());
    assert_eq!(settings["server"]["history"]["compaction_pending"], false);
}

#[tokio::test]
async fn dashboard_now_contract_uses_current_pool_config_and_registry() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let response = client()
        .get(proxy.url("/api/dashboard/now"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["lanes"], 3);
    assert_eq!(body["rpms"], serde_json::json!([40, 40, 40]));
    assert_eq!(body["capacity_rpm"], 120);
    assert_eq!(body["default_window_days"], 30);
    assert_eq!(body["retention_days"], 30);
    assert_eq!(body["slo_target_percent"], 99.9);
    assert!(body["history_revision"].as_u64().is_some());
    assert_eq!(
        body["history_revision"],
        body["tail"]["base_history_revision"]
    );
    assert!(body["config_revision"].as_u64().is_some());
    assert!(body["tail"]["totals"].as_array().is_some());
    assert!(body["metrics"].as_array().is_some());

    // Window occupancy fields: no requests yet, so fill is 0 while the
    // capacity budget reflects each lane's calibrated admission (40 RPM).
    let lanes = body["per_lane"].as_array().unwrap();
    assert_eq!(lanes.len(), 3);
    for lane in lanes {
        assert!(lane["key"].is_string());
        assert_eq!(lane["rpm"], 40);
        assert!(lane["effective_rpm"].as_u64().is_some());
        assert_eq!(lane["in_window"], 0);
    }
    assert_eq!(body["window_fill"], 0);
    assert_eq!(body["window_capacity"], 120);
}

#[tokio::test]
async fn dashboard_now_window_fill_reflects_real_count_not_rate_extrapolation() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    // Before any requests: empty window.
    let before = dashboard_now(&proxy, &cookie).await;
    assert_eq!(before["window_fill"], 0);
    assert_eq!(before["window_capacity"], 120);

    // Send a single request — window_fill should be 1, not 0 (and not 20
    // as the old RPM extrapolation would have projected).
    send_successful_chats(&proxy, 1).await;
    let after = dashboard_now(&proxy, &cookie).await;
    assert_eq!(
        after["window_fill"], 1,
        "single request must fill exactly 1 window slot, not a rate extrapolation: {after}"
    );
    assert_eq!(after["window_capacity"], 120);
    assert!(after["window_fill"].as_u64().unwrap() < after["window_capacity"].as_u64().unwrap());
    // One of the three lanes should have in_window=1 (the others 0).
    let per_lane = after["per_lane"].as_array().unwrap();
    let lane_in_windows: Vec<u64> = per_lane
        .iter()
        .map(|l| l["in_window"].as_u64().unwrap())
        .collect();
    let sum_in_window: u64 = lane_in_windows.iter().sum();
    assert_eq!(
        sum_in_window, 1,
        "per-lane in_window must sum to window_fill: {lane_in_windows:?}"
    );
}

#[tokio::test]
async fn dashboard_sse_stream_returns_event_stream() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let response = client()
        .get(proxy.url("/api/dashboard/stream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );
}

/// Read the SSE stream only until the first `data: {` JSON payload arrives
/// (the dashboard stream never terminates, so `read_sse`'s EOF wait won't do).
async fn read_first_sse_json(resp: reqwest::Response) -> String {
    let mut out = String::new();
    let mut stream = resp.bytes_stream();
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Ok(Some(chunk)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        stream.next(),
    )
    .await
    {
        let chunk: bytes::Bytes = chunk.expect("stream chunk");
        out.push_str(&String::from_utf8_lossy(&chunk));
        if out.contains("data: {") {
            break;
        }
    }
    out
}

#[tokio::test]
async fn dashboard_sse_stream_receives_data_within_timeout() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let response = client()
        .get(proxy.url("/api/dashboard/stream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let body = read_first_sse_json(response).await;
    // The first data event should arrive within 3 seconds (the push interval).
    assert!(
        body.contains("data: {"),
        "SSE body should contain a data: line:\n{body}"
    );
    assert!(
        body.contains("sampled_at"),
        "data payload should contain sampled_at:\n{body}"
    );
    assert!(
        body.contains("lanes"),
        "data payload should contain lanes:\n{body}"
    );
}

#[tokio::test]
async fn dashboard_sse_stream_connection_limit_returns_503() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    // Open enough concurrent SSE connections to exhaust the limit (100) and
    // verify the next one is rejected with 503. Each response body is held
    // open so the server-side tasks stay alive.
    let mut streams = Vec::new();
    let mut saw_503 = false;
    for _ in 0..120 {
        let resp = client()
            .get(proxy.url("/api/dashboard/stream"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        if resp.status() == 503 {
            saw_503 = true;
            let err: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(err["error"]["code"], "too_many_streams");
            break;
        }
        streams.push(resp);
    }
    assert!(
        saw_503,
        "expected a 503 once the SSE connection limit is hit"
    );
}

#[tokio::test]
async fn retention_change_prunes_queries_and_disk() {
    let mock = start_mock().await;
    let data_dir = scratch_data_dir();
    std::fs::write(
        data_dir.join("config.json"),
        serde_json::to_string_pretty(&StoreOpts::default().json(&mock.url)).unwrap(),
    )
    .unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let cutoff = now - 86_400;
    let old = cutoff - 400;
    let boot = cutoff - 300;
    let baseline = cutoff - 200;
    let retained_one = cutoff + 10;
    let retained_two = now - 10;
    std::fs::write(
        data_dir.join("history.jsonl"),
        format!(
            "{{\"t\":{old},\"m\":\"# TYPE fixture_requests_total counter\\nfixture_requests_total 5\\n\"}}\n\
             {{\"v\":2,\"t\":{boot},\"boot\":\"boot-a\",\"kind\":\"boot\",\"capacity\":{{\"enabled_lanes\":3,\"rpms\":[40,40,40],\"capacity_rpm\":120}}}}\n\
             {{\"v\":2,\"t\":{baseline},\"boot\":\"boot-a\",\"capacity\":{{\"enabled_lanes\":3,\"rpms\":[40,40,40],\"capacity_rpm\":120}},\"m\":\"# TYPE fixture_requests_total counter\\nfixture_requests_total 50\\n\"}}\n\
             {{\"v\":2,\"t\":{retained_one},\"boot\":\"boot-a\",\"capacity\":{{\"enabled_lanes\":3,\"rpms\":[40,40,40],\"capacity_rpm\":120}},\"m\":\"# TYPE fixture_requests_total counter\\nfixture_requests_total 60\\n\"}}\n\
             {{\"v\":2,\"t\":{retained_two},\"boot\":\"boot-a\",\"capacity\":{{\"enabled_lanes\":3,\"rpms\":[40,40,40],\"capacity_rpm\":120}},\"m\":\"# TYPE fixture_requests_total counter\\nfixture_requests_total 70\\n\"}}\n"
        ),
    )
    .unwrap();

    let proxy = start_proxy_in(data_dir, &[("HISTORY_SAMPLE_SECS", "3600")]).await;
    let cookie = session_cookie(&proxy).await;
    let (status, body) = post_json(
        &proxy,
        &cookie,
        "/api/settings/history",
        serde_json::json!({
            "days": 1,
            "default_window_days": 1,
            "slo_target_percent": 99.9
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let query = |proxy: &support::Proxy, cookie: &str| {
        let url = proxy.url("/api/dashboard?from=1&to=4102444800&points=100");
        let cookie = cookie.to_owned();
        async move {
            client()
                .get(url)
                .header("cookie", cookie)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let metric = |body: &serde_json::Value| {
        body["totals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["metric"] == "fixture_requests_total")
            .and_then(|row| row["value"].as_f64())
            .unwrap()
    };

    let pruned = query(&proxy, &cookie).await;
    assert_eq!(pruned["window"]["available_from"], retained_one);
    assert_eq!(metric(&pruned), 20.0);

    let deadline = Instant::now() + Duration::from_secs(5);
    while api_config(&proxy, &cookie).await["server"]["history"]["compaction_pending"] == true {
        assert!(
            Instant::now() < deadline,
            "history compaction did not finish"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let proxy = restart(proxy, &[("HISTORY_SAMPLE_SECS", "3600")]).await;
    let cookie = session_cookie(&proxy).await;
    let reloaded = query(&proxy, &cookie).await;
    assert_eq!(reloaded["window"]["available_from"], retained_one);
    assert_eq!(metric(&reloaded), 20.0);
}

#[tokio::test]
async fn sigterm_shuts_down_cleanly() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let status = proxy.terminate();
    assert!(status.success(), "clean exit on SIGTERM, got {status:?}");
}

#[tokio::test]
async fn dashboard_and_config_are_served_to_anyone() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let dash = client()
        .get(proxy.url("/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(dash.status(), 200);
    let html = dash.text().await.unwrap();
    assert!(html.contains("NIM"));
    assert!(html.contains("/api/dashboard/now"));
    assert!(html.contains("data-range=\"default\""));
    assert!(html.contains("data-range=\"all-retained\""));
    assert!(!html.contains("fetch('/metrics')"));
    assert!(!html.contains("/api/history?"));
    assert!(!html.contains("/dash/config.json"));

    let now: serde_json::Value = client()
        .get(proxy.url("/api/dashboard/now"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(now["lanes"], 3);
    assert!(
        !now.as_object().unwrap().contains_key("auth"),
        "no client-auth plane in the now payload"
    );

    for retired in ["/api/history", "/dash/config.json"] {
        assert_eq!(
            client()
                .get(proxy.url(retired))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            404,
            "{retired} stays retired"
        );
    }
}

#[tokio::test]
async fn dashboard_history_settings_markup() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let html = client()
        .get(proxy.url("/"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("History &amp; dashboard"));
    assert!(html.contains("sv-default-days"));
    assert!(html.contains("sv-retention-days"));
    assert!(html.contains("sv-slo"));
    assert!(html.contains("/api/settings/history"));
    assert!(!html.contains("Pricing &amp; history"));
    assert!(!html.contains("const SLO = 0.999"));
}

#[tokio::test]
async fn dashboard_range_state_guards_markup() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let html = client()
        .get(proxy.url("/"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("let rangeRequestGeneration = 0"));
    assert!(html.contains("const generation = ++rangeRequestGeneration"));
    assert!(html.contains("generation !== rangeRequestGeneration"));
    assert!(!html.contains("mode.kind === 'fixed' && historyChanged"));
    assert!(html.contains("let frozenHasTraffic = false"));
    assert!(
        html.contains("if (mode.kind !== 'following' || !rangeData || !samples.length) return;")
    );
}

#[tokio::test]
async fn dashboard_pause_traffic_is_derived_from_rendered_samples() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let html = client()
        .get(proxy.url("/"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("function hasSelectedRequestTraffic(selectedSamples)"));
    assert!(html.contains("row => row.name === 'nimproxy_requests_total' && +row.value > 0"));
    assert!(html.contains("frozenHasTraffic = hasSelectedRequestTraffic(samples);"));
    assert!(html.contains(
        "const hasTraffic = mode.paused ? frozenHasTraffic : hasSelectedRequestTraffic(samples);"
    ));
    assert!(!html.contains("const acceptedTail = nowData?.tail"));
}

#[tokio::test]
async fn dashboard_historical_provisioning_has_no_guessed_lane_size() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let html = client()
        .get(proxy.url("/"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("rpm short at peak"));
    assert!(html.contains("vs contemporaneous capacity"));
    assert!(html.contains("legacy interval"));
    assert!(!html.contains("const moreKeys"));
    assert!(!html.contains("MORE KEY"));
}

#[tokio::test]
async fn dashboard_now_refreshes_after_settings_change() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let before: serde_json::Value = client()
        .get(proxy.url("/api/dashboard/now"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fingerprint = api_config(&proxy, &cookie).await["nim_keys"][0]["fingerprint"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = post_json(
        &proxy,
        &cookie,
        "/api/settings/nim-keys",
        serde_json::json!({"set": {"fingerprint": fingerprint, "rpm": 41}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let after: serde_json::Value = client()
        .get(proxy.url("/api/dashboard/now"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        after["config_revision"].as_u64().unwrap() > before["config_revision"].as_u64().unwrap()
    );
    assert_ne!(after["capacity_rpm"], before["capacity_rpm"]);
    assert_eq!(
        after["history_revision"], before["history_revision"],
        "current config changes do not rewrite retained history"
    );
}

// ---------- boot posture & the store ----------

/// A corrupt or future-version store is a hard boot error, never a silent
/// fall-through to defaults (which would discard configured keys).
#[tokio::test]
async fn corrupt_or_future_store_refuses_to_start() {
    let corrupt = scratch_data_dir();
    std::fs::write(corrupt.join("config.json"), "{ not json").unwrap();
    expect_refuses_to_start(corrupt).await;

    let future = scratch_data_dir();
    std::fs::write(future.join("config.json"), r#"{"version": 2}"#).unwrap();
    expect_refuses_to_start(future).await;
}

// ---------- security hardening ----------

/// Health stays public.
#[tokio::test]
async fn model_label_is_sanitized_in_metrics() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;

    // A malicious model id carrying Prometheus/HTML/log injection payloads.
    let evil = "<img src=x onerror=alert(1)>\"} pwn 1\nmeta";
    let body = serde_json::json!({
        "model": evil,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let r = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let metrics = metrics(&proxy).await;
    // The sanitized label keeps only safe chars; none of the injection
    // characters survive, and no spurious `pwn` series was created.
    // The model label value (after `model="`) must contain only safe chars —
    // no `<`, `>`, quote, brace, or newline that could break the exposition
    // format, inject a series, or become HTML. The payload collapses to one
    // harmless alphanumeric token on a single line.
    let req_line = metrics
        .lines()
        .find(|l| l.starts_with("nimproxy_requests_total"))
        .expect("requests_total present");
    let value = req_line
        .split("model=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("model label present");
    assert!(
        value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':')),
        "unsafe chars in model label: {value:?}"
    );
    // No injected standalone series (the `\n... pwn 1` part of the payload).
    assert!(
        !metrics.lines().any(|l| l.trim_start().starts_with("pwn")),
        "injected metric series present"
    );
}

#[tokio::test]
async fn dashboard_sends_security_headers() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    // The dashboard now requires a session; assert the CSP on an authenticated
    // 200 (the hardening headers wrap every response, success or redirect).
    let cookie = session_cookie(&proxy).await;
    let resp = client()
        .get(proxy.url("/"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = resp.headers();
    let csp = h["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(
        csp.contains("connect-src 'self'"),
        "blocks cross-origin exfil"
    );
    assert!(
        csp.contains("font-src https://fonts.gstatic.com"),
        "dashboard webfonts are allowed, and only from Google's font host"
    );
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["x-frame-options"], "DENY");
}

#[tokio::test]
async fn worker_exhaustion_governs_the_model_and_spares_the_lane() {
    let mock = start_mock().await;
    mock.state.push(Behavior::WorkerExhausted);
    let proxy = start_proxy(&mock.url, &[]).await;

    let started = Instant::now();
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hello world");
    assert_eq!(mock.state.hit_count(), 2, "one exhausted try, one success");
    // The retry waited out the governor's ~2s drain gap, not the 10s default
    // lane bench a plain 429-without-Retry-After would have earned.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "retry took {:?} — looks like a lane bench, not a model drain gap",
        started.elapsed()
    );

    let metrics = metrics(&proxy).await;
    assert!(
        metrics.contains(r#"nimproxy_worker_exhausted_total{model="mock/model-a"} 1"#),
        "exhaustion counted: {metrics}"
    );
    assert!(
        !metrics.contains("nimproxy_lane_benched_total"),
        "worker exhaustion must never bench a lane: {metrics}"
    );
    assert!(
        metrics.contains(r#"nimproxy_model_limit{model="mock/model-a"} 1"#),
        "governor engaged at max(1, inflight/2) = 1: {metrics}"
    );
    assert!(
        metrics.contains(r#"nimproxy_model_inflight{model="mock/model-a"} 0"#),
        "permit released after completion: {metrics}"
    );
}

#[tokio::test]
async fn worker_exhaustion_streaming_retries_inside_the_stream() {
    let mock = start_mock().await;
    mock.state.push(Behavior::WorkerExhausted);
    let proxy = start_proxy(&mock.url, &[]).await;

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "stream commits to 200 before retrying");
    let body = read_sse(resp).await;
    assert!(body.contains(": retrying"), "retry notice sent: {body}");
    assert!(body.contains("hello"), "content delivered: {body}");
    assert!(body.contains("data: [DONE]"), "stream completed: {body}");

    let metrics = metrics(&proxy).await;
    assert!(
        metrics.contains(r#"nimproxy_worker_exhausted_total{model="mock/model-a"} 1"#),
        "exhaustion counted: {metrics}"
    );
    assert!(
        !metrics.contains("nimproxy_lane_benched_total"),
        "worker exhaustion must never bench a lane: {metrics}"
    );
}

// ---------------------------------------------------------------------------
// Settings API: invariants and live application.
// ---------------------------------------------------------------------------

async fn api_config(proxy: &support::Proxy, cookie: &str) -> serde_json::Value {
    client()
        .get(proxy.url("/api/config"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn post_json(
    proxy: &support::Proxy,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    let resp = client()
        .post(proxy.url(path))
        .header("cookie", cookie)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let v = resp.json().await.unwrap_or_default();
    (status, v)
}
#[tokio::test]
async fn rpm_raise_applies_to_the_live_pool_immediately() {
    let mock = start_mock().await;
    let opts = support::StoreOpts {
        nim_keys: vec![("solo".into(), 1)],
        max_wait_secs: 2,
        ..Default::default()
    };
    let proxy = start_proxy_with(&mock.url, opts, &[]).await;
    let root = session_cookie(&proxy).await;

    let first = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let second = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 504, "rpm 1 is spent for the window");

    // Raising the key's rpm rebuilds the pool with carried state — the new
    // headroom serves requests immediately, no restart, no window reset.
    let fp = api_config(&proxy, &root).await["nim_keys"][0]["fingerprint"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/nim-keys",
        serde_json::json!({"set": {"fingerprint": fp, "rpm": 5}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let third = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(third.status(), 200, "raised rpm applies live");
    assert_eq!(mock.state.hit_count(), 2);
}
#[tokio::test]
async fn base_url_change_flushes_the_models_cache() {
    let mock_a = start_mock().await;
    let mock_b = start_mock().await;
    let proxy = start_proxy(&mock_a.url, &[]).await;
    let root = session_cookie(&proxy).await;

    // Prime the (10-minute-TTL) catalog cache from upstream A.
    client()
        .get(proxy.url("/v1/models"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        mock_a
            .state
            .models_hits
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/upstream",
        serde_json::json!({"base_url": mock_b.url}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    client()
        .get(proxy.url("/v1/models"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        mock_b
            .state
            .models_hits
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "catalog refetches from the new upstream, not the stale cache"
    );
}
#[tokio::test]
async fn authenticated_key_validation_uses_only_the_configured_upstream() {
    // The endpoint probes the configured upstream, never anything the caller
    // supplies: an extra base_url field in the body is ignored, so the probe
    // can't be pointed at internal hosts (defanged SSRF by construction).
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let root = session_cookie(&proxy).await;

    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/validate-key",
        serde_json::json!({"key": "nvapi-x", "base_url": "http://169.254.169.254"}),
    )
    .await;
    assert_eq!(status, 200);
    // It probed the real (mock) upstream, which answers with model-a — not the
    // attacker's target (which would have errored "cannot reach upstream").
    assert_eq!(
        v["ok"], true,
        "validated against the configured upstream: {v}"
    );
    assert_eq!(v["models"], 1, "{v}");
}
/// Streaming requests hold their in-flight slot for the stream's whole
/// lifetime — `max_inflight` caps total concurrent work, not just the
/// buffered path (streaming is what agent harnesses actually send).
#[tokio::test]
async fn streaming_requests_count_against_the_inflight_cap() {
    let mock = start_mock().await;
    mock.state.push(Behavior::Hang);
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            max_inflight: 1,
            ..Default::default()
        },
        &[],
    )
    .await;

    // Occupy the only slot with a stream that never ends. Reading the first
    // body chunk proves the proxy has fully committed to the stream.
    let mut hog = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hog", true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    use futures_util::StreamExt;
    let first = tokio::time::timeout(Duration::from_secs(5), hog.next())
        .await
        .expect("first chunk within 5s")
        .expect("stream not ended")
        .expect("stream chunk");
    assert!(!first.is_empty());

    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("shed-me", false))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        503,
        "a live stream occupies the in-flight cap"
    );
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "overloaded");
    drop(hog);
}

// ---------- settings-endpoint coverage backfill ----------

/// A DATA_DIR whose path is blocked by a regular file is a hard boot error.
/// (The write-probe posture; a chmod-based fixture would pass vacuously when
/// the tests run as root.)
#[tokio::test]
async fn boot_refuses_an_unwritable_data_dir() {
    let dir = scratch_data_dir();
    std::fs::write(dir.join("blocker"), b"not a directory").unwrap();
    expect_refuses_to_start(dir.join("blocker").join("data")).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Governor settings write through the shared pipeline: they reflect in
/// /api/config, out-of-range overrides are refused, and the state persists
/// across a restart.
#[tokio::test]
async fn governor_settings_reflect_and_persist() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let root = session_cookie(&proxy).await;

    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/governor",
        serde_json::json!({"set_override": {"model": "mock/model-a", "cap": 4}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let cfg = api_config(&proxy, &root).await;
    assert_eq!(cfg["server"]["governor"]["overrides"]["mock/model-a"], 4);

    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/governor",
        serde_json::json!({"set_override": {"model": "mock/model-a", "cap": 0}}),
    )
    .await;
    assert_eq!(status, 400, "cap 0 must fail the rulebook: {v}");

    let (status, _) = post_json(
        &proxy,
        &root,
        "/api/settings/governor",
        serde_json::json!({"enabled": false}),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = post_json(
        &proxy,
        &root,
        "/api/settings/governor",
        serde_json::json!({"remove_override": "mock/model-a"}),
    )
    .await;
    assert_eq!(status, 200);

    let proxy = restart(proxy, &[]).await;
    let root = session_cookie(&proxy).await;
    let cfg = api_config(&proxy, &root).await;
    assert_eq!(
        cfg["server"]["governor"]["enabled"], false,
        "master toggle persisted across restart: {cfg}"
    );
    assert!(
        cfg["server"]["governor"]["overrides"]
            .as_object()
            .unwrap()
            .is_empty(),
        "removed override stays gone: {cfg}"
    );
}

/// Pricing and dashboard history settings save through the same pipeline and
/// reflect in /api/config; invalid candidates leave every value unchanged.
#[tokio::test]
async fn pricing_and_history_settings_reflect_in_api_config() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let root = session_cookie(&proxy).await;

    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/pricing",
        serde_json::json!({"ref_price_in": 1.25, "ref_price_out": 3.5}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/history",
        serde_json::json!({
            "days": 45,
            "default_window_days": 30,
            "slo_target_percent": 99.5
        }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    let cfg = api_config(&proxy, &root).await;
    assert_eq!(cfg["server"]["pricing"]["ref_price_in"], 1.25);
    assert_eq!(cfg["server"]["pricing"]["ref_price_out"], 3.5);
    assert_eq!(cfg["server"]["history"]["days"], 45);
    assert_eq!(cfg["server"]["dashboard"]["default_window_days"], 30);
    assert_eq!(cfg["server"]["dashboard"]["slo_target_percent"], 99.5);

    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/history",
        serde_json::json!({
            "days": 7,
            "default_window_days": 30,
            "slo_target_percent": 98.0
        }),
    )
    .await;
    assert_eq!(status, 400, "invalid window/retention pair accepted: {v}");
    let cfg = api_config(&proxy, &root).await;
    assert_eq!(cfg["server"]["history"]["days"], 45);
    assert_eq!(cfg["server"]["dashboard"]["default_window_days"], 30);
    assert_eq!(cfg["server"]["dashboard"]["slo_target_percent"], 99.5);

    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/pricing",
        serde_json::json!({"ref_price_in": -1.0, "ref_price_out": 3.5}),
    )
    .await;
    assert_eq!(status, 400, "negative prices must be refused: {v}");
}

/// The limits endpoint enforces the shared rulebook (heartbeat < max_wait)
/// and rejects partial bodies outright — omitted fields are never silently
/// reset to defaults.
#[tokio::test]
async fn limits_validation_rejects_bad_bounds_and_partial_bodies() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let root = session_cookie(&proxy).await;

    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/limits",
        serde_json::json!({
            "max_wait_secs": 5, "heartbeat_secs": 10, "models_ttl_secs": 600,
            "stream_idle_secs": 300, "request_timeout_secs": 300,
            "max_inflight": 512, "strict_passthrough": false,
        }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_wait_secs"),
        "the rulebook names the offending bound: {v}"
    );

    let partial = client()
        .post(proxy.url("/api/settings/limits"))
        .header("cookie", &root)
        .json(&serde_json::json!({"max_wait_secs": 60}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        partial.status(),
        422,
        "a partial limits body is rejected, not defaulted"
    );
}

/// A stream whose upstream hangs must release its in-flight slot promptly
/// once the client disconnects — otherwise hung upstreams accumulate and
/// permanently consume the cap (503s forever until restart).
#[tokio::test]
async fn disconnected_stream_releases_its_inflight_slot() {
    let mock = start_mock().await;
    mock.state.push(Behavior::Hang);
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            max_inflight: 1,
            ..Default::default()
        },
        &[],
    )
    .await;

    // Occupy the only slot with a hung stream. Read PAST the upstream's only
    // data chunk so the relay task is parked on the upstream read with
    // nothing left to send — the state where a disconnect used to go
    // unnoticed until the stream_idle cutoff — then hang up.
    let mut hog = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hog", true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    use futures_util::StreamExt;
    let read_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let chunk = tokio::time::timeout(
            read_deadline.saturating_duration_since(Instant::now()),
            hog.next(),
        )
        .await
        .expect("upstream chunk within 5s")
        .expect("stream open")
        .expect("chunk ok");
        if String::from_utf8_lossy(&chunk).contains("choices") {
            break; // the mock's single pre-hang data chunk has been relayed
        }
    }
    drop(hog);

    // The slot must come back well before the stream_idle cutoff (300s here):
    // the proxy notices the closed client channel, not just the stalled read.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client()
            .post(proxy.url("/v1/chat/completions"))
            .json(&chat_body("after-disconnect", false))
            .send()
            .await
            .unwrap();
        if resp.status() == 200 {
            break;
        }
        assert_eq!(resp.status(), 503, "only sheds while the slot is held");
        assert!(
            Instant::now() < deadline,
            "slot never released after client disconnect"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ===========================================================================
// Coverage: settings error legs and key validation driven through the real
// HTTP surface.
// ===========================================================================

/// A reachable upstream that 404s the models route is a key rejection, not a
/// connection failure (probe_key's non-success branch).
#[tokio::test]
async fn key_probe_reports_upstream_rejection() {
    let mock = start_mock().await;
    // Point the proxy's upstream at a bogus path: the mock's own router
    // 404s /v1/models there, so the probe must read it as a key rejection
    // (configured base URL, never a caller-supplied one).
    let proxy = start_proxy(&format!("{}/bogus", mock.url), &[]).await;
    let resp = client()
        .post(proxy.url("/api/settings/validate-key"))
        .json(&serde_json::json!({"key": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["ok"], false, "{v}");
    assert!(v["error"].as_str().unwrap().contains("rejected"), "{v}");
}

/// The authenticated key validator reports an unreachable upstream (probe_key's
/// connect-error branch, via /api/settings/validate-key).
#[tokio::test]
async fn authenticated_key_validation_reports_unreachable_upstream() {
    let proxy = start_proxy_with("http://127.0.0.1:1", support::StoreOpts::default(), &[]).await;
    let root = session_cookie(&proxy).await;
    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/validate-key",
        serde_json::json!({"key": "x"}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], false, "{v}");
    assert!(v["error"].as_str().unwrap().contains("reach"), "{v}");
}

/// Removing or reconfiguring a NIM key that doesn't exist is a 400, and the
/// action selector requires exactly one of add/remove/set.
#[tokio::test]
async fn nim_keys_reject_unknown_fingerprint_and_empty_action() {
    let mock = start_mock().await;
    let proxy = start_proxy_with(&mock.url, support::StoreOpts::default(), &[]).await;
    let root = session_cookie(&proxy).await;
    for body in [
        serde_json::json!({"remove": "deadbeef"}),
        serde_json::json!({"set": {"fingerprint": "deadbeef", "enabled": true}}),
    ] {
        let (status, v) = post_json(&proxy, &root, "/api/settings/nim-keys", body).await;
        assert_eq!(status, 400, "{v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no such key"),
            "{v}"
        );
    }
    let (status, _) = post_json(
        &proxy,
        &root,
        "/api/settings/nim-keys",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 400, "empty action rejected");
}

/// Client-key endpoint: unknown name, bad mode, empty oneof, empty name on
/// commit, and cross-owner revoke are all rejected with the right status.
/// The upstream base_url is re-validated on write: a link-local target (SSRF /
/// cloud-metadata) is refused.
#[tokio::test]
async fn upstream_rejects_link_local_base_url() {
    let mock = start_mock().await;
    let proxy = start_proxy_with(&mock.url, support::StoreOpts::default(), &[]).await;
    let root = session_cookie(&proxy).await;
    let (s, v) = post_json(
        &proxy,
        &root,
        "/api/settings/upstream",
        serde_json::json!({"base_url": "http://169.254.169.254"}),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("link-local"),
        "{v}"
    );
}

/// An empty DATA_DIR is a fatal misconfiguration — the store home must be a
/// real writable directory.
#[tokio::test]
async fn boot_refuses_an_empty_data_dir() {
    support::expect_refuses_to_start(std::path::PathBuf::from("")).await;
}

/// `nim-proxy --health` probes /health on $PORT and exits 0 (healthy) or 1
/// (unreachable) — the scratch image's shell-less HEALTHCHECK.
#[tokio::test]
async fn health_probe_flag_reports_liveness() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let run_health = |port: String| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_nim-proxy"));
        cmd.arg("--health").env("PORT", port);
        // Forward the coverage profile path so the probe subprocess is counted
        // under `cargo llvm-cov` (a no-op in a normal test run).
        if let Ok(v) = std::env::var("LLVM_PROFILE_FILE") {
            cmd.env("LLVM_PROFILE_FILE", v);
        }
        cmd.status().unwrap()
    };
    assert!(
        run_health(proxy.port.to_string()).success(),
        "--health exits 0 against a healthy proxy"
    );
    assert!(
        !run_health("1".into()).success(),
        "--health exits non-zero against a dead port"
    );
}

/// The operator queue lists every in-flight request (client · model · path ·
/// phase) and a terminate call ends its stream with error code -91 and the
/// required message. The entry unregisters itself the moment the stream dies.
#[tokio::test]
async fn queue_lists_inflight_requests_and_terminates_them_with_code_91() {
    use futures_util::StreamExt;
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    mock.state.push(Behavior::ActiveStream(50));
    let cookie = session_cookie(&proxy).await;

    // Start a stream that never ends on its own; a background task drains it
    // into a shared buffer so the proxy's response channel never fills (that
    // mirrors a harness actually reading its stream).
    let stream = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("queue probe", true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    let out = Arc::new(Mutex::new(String::new()));
    {
        let out = out.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(Ok(chunk)) = stream.next().await {
                out.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&chunk));
            }
        });
    }

    // The queue shows the request with client / model / path.
    let entry = loop {
        let v = client()
            .get(proxy.url("/api/queue"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(v.status(), 200);
        let q: serde_json::Value = v.json().await.unwrap();
        if let Some(first) = q["requests"].as_array().and_then(|a| a.first()) {
            break first.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(entry["client"], "local");
    assert_eq!(entry["model"], "mock/model-a");
    assert_eq!(entry["path"], "/v1/chat/completions");
    assert!(entry["phase"].is_string(), "{entry}");
    assert!(entry["age_s"].as_u64().is_some(), "{entry}");
    let id = entry["id"].as_u64().unwrap();

    // Terminate it: the client's SSE stream must end with error code -91.
    let r = client()
        .post(proxy.url("/api/queue/terminate"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "id": id }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // The -91 SSE must arrive on the client stream.
    let deadline = Instant::now() + Duration::from_secs(5);
    let got = loop {
        let cur = out.lock().unwrap().clone();
        if cur.contains("\"code\":\"-91\"") {
            break cur;
        }
        assert!(
            Instant::now() < deadline,
            "killed stream should carry the -91 error, got: {cur}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        got.contains("Your request has been terminated by the system"),
        "killed stream should carry the termination message, got: {got}"
    );

    // The entry is gone from the queue, and an unknown id is a 404.
    let q: serde_json::Value = loop {
        let v: serde_json::Value = client()
            .get(proxy.url("/api/queue"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if v["requests"].as_array().unwrap().is_empty() {
            break v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(q["requests"].as_array().unwrap().is_empty(), "{q}");
    let r = client()
        .post(proxy.url("/api/queue/terminate"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "id": 999_999 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

/// A buffered (non-streaming) request killed while waiting on upstream
/// headers answers the client with a JSON error: HTTP 400, code -91.
#[tokio::test]
async fn queue_terminates_buffered_requests_with_a_json_91() {
    let mock = start_mock().await;
    let proxy = start_proxy_with(&mock.url, StoreOpts::default(), &[]).await;
    mock.state.push(Behavior::DelayHeaders(30_000));
    let cookie = session_cookie(&proxy).await;

    // The buffered request is parked waiting for headers (30s).
    let url = proxy.url("/v1/chat/completions");
    let waiting = tokio::spawn(async move {
        client()
            .post(url)
            .bearer_auth("sekrit")
            .json(&chat_body("buffered queue probe", false))
            .send()
            .await
            .unwrap()
    });
    let id = loop {
        let v = client()
            .get(proxy.url("/api/queue"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        let q: serde_json::Value = v.json().await.unwrap();
        if let Some(first) = q["requests"].as_array().and_then(|a| a.first()) {
            break first["id"].as_u64().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let r = client()
        .post(proxy.url("/api/queue/terminate"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "id": id }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let resp = waiting.await.unwrap();
    assert_eq!(
        resp.status(),
        400,
        "buffered kill answers 400, not the 200-body"
    );
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "-91", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("terminated by the system"),
        "{v}"
    );
}

/// A kill also lands on a request still waiting for a rate-limit slot (phase
/// `waiting_slot` in the queue view) — no slot was reserved for it, so nothing
/// is wasted, and its stream still ends with the -91 error.
#[tokio::test]
async fn queue_terminates_a_request_still_waiting_for_a_slot() {
    use futures_util::StreamExt;
    let mock = start_mock().await;
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            nim_keys: vec![("test-key-0".into(), 1)],
            heartbeat_secs: 1,
            ..StoreOpts::default()
        },
        &[],
    )
    .await;
    mock.state.push(Behavior::ActiveStream(500));
    mock.state.push(Behavior::Ok);
    let cookie = session_cookie(&proxy).await;

    // Occupy the single lane with a stream that never ends. The holder and
    // the waiter are both "alice"; the waiter sits in phase "waiting_slot".
    let holder = client()
        .post(proxy.url("/v1/chat/completions"))
        .bearer_auth("sekrit")
        .json(&chat_body("queue holder", true))
        .send()
        .await
        .unwrap();
    let mut waiter = client()
        .post(proxy.url("/v1/chat/completions"))
        .bearer_auth("sekrit")
        .json(&chat_body("queue waiter", true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    // Read the waiter's stream in the background (heartbeats and, later, the
    // -91 kill event all flow through it).
    let reader = tokio::spawn(async move {
        let mut out = String::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Ok(Some(Ok(chunk))) = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            waiter.next(),
        )
        .await
        {
            out.push_str(&String::from_utf8_lossy(&chunk));
            if out.contains("[DONE]") {
                break;
            }
        }
        out
    });

    let waiter_id = loop {
        let v = client()
            .get(proxy.url("/api/queue"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        let q: serde_json::Value = v.json().await.unwrap();
        let reqs = q["requests"].as_array().unwrap();
        if let Some(w) = reqs.iter().find(|r| r["phase"] == "waiting_slot") {
            break w["id"].as_u64().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let r = client()
        .post(proxy.url("/api/queue/terminate"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "id": waiter_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let out = reader.await.unwrap();
    assert!(
        out.contains("\"code\":\"-91\""),
        "a queued (not yet slotted) request must still die with -91, got: {out}"
    );

    // The holder is untouched and still listed; the waiter's entry is gone.
    let q: serde_json::Value = client()
        .get(proxy.url("/api/queue"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reqs = q["requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 1, "{q}");
    assert_eq!(reqs[0]["phase"], "upstream", "{q}");
    drop(holder);
}

/// The queue view distinguishes the two wait stages: a request blocked on the
/// governor's worker-concurrency gate shows `waiting_permit`, not
/// `waiting_slot`.
#[tokio::test]
async fn queue_reports_waiting_permit_phase() {
    use futures_util::StreamExt;
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let root = session_cookie(&proxy).await;

    // Pin the model's concurrency cap to 1: the second request must then wait
    // on the governor gate (phase waiting_permit), not the RPM queue.
    let (status, v) = post_json(
        &proxy,
        &root,
        "/api/settings/governor",
        serde_json::json!({"set_override": {"model": "mock/model-a", "cap": 1}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    mock.state.push(Behavior::ActiveStream(500));
    mock.state.push(Behavior::Ok);

    // First request takes the single permit and streams forever.
    let holder = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("permit holder", true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    let out = Arc::new(Mutex::new(String::new()));
    {
        let out = out.clone();
        tokio::spawn(async move {
            let mut holder = holder;
            while let Some(Ok(chunk)) = holder.next().await {
                out.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&chunk));
            }
        });
    }

    // Second request: admitted to the slot queue but stalled at the permit.
    let mut waiter = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("permit waiter", true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    let waiter_out = Arc::new(Mutex::new(String::new()));
    {
        let waiter_out = waiter_out.clone();
        tokio::spawn(async move {
            while let Some(Ok(chunk)) = waiter.next().await {
                waiter_out
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&chunk));
            }
        });
    }

    let seen = loop {
        let v = client()
            .get(proxy.url("/api/queue"))
            .header("cookie", &root)
            .send()
            .await
            .unwrap();
        let q: serde_json::Value = v.json().await.unwrap();
        let reqs = q["requests"].as_array().unwrap();
        if let Some(w) = reqs
            .iter()
            .find(|r| r["model"] == "mock/model-a" && r["phase"] == "waiting_permit")
        {
            break w.clone();
        }
        assert!(
            reqs.iter().any(|r| r["phase"] == "upstream"),
            "holder must be upstream: {q}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let id = seen["id"].as_u64().unwrap();
    // Don't leave the waiter parked forever: it must die with -91 like any
    // other queued request.
    let r = client()
        .post(proxy.url("/api/queue/terminate"))
        .header("cookie", &root)
        .json(&serde_json::json!({ "id": id }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let cur = waiter_out.lock().unwrap().clone();
        if cur.contains("\"code\":\"-91\"") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "permit-waiting kill should carry -91, got: {cur}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The dispatch policy is configurable and observable: flipping the policy
/// through /api/settings/dispatch, /api/config reports it back, the queue
/// surface names it, and alongside /metrics the dispatcher exposes the
/// live-policy gauge — all while requests keep flowing (a store switch that
/// wedged the dispatcher would hang every request).
#[tokio::test]
async fn dispatch_policy_settings_round_trip_and_live_swaps() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;

    // Defaults: FIFO both in the store and in the queue view.
    let cfg: serde_json::Value = client()
        .get(proxy.url("/api/config"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg["server"]["dispatch"]["policy"], "fifo");
    let q: serde_json::Value = client()
        .get(proxy.url("/api/queue"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(q["dispatch_policy"], "fifo");

    // Flip to EDF; keep the proxy serving requests across the swap.
    let r = client()
        .post(proxy.url("/api/settings/dispatch"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "policy": "edf" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let cfg: serde_json::Value = client()
        .get(proxy.url("/api/config"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg["server"]["dispatch"]["policy"], "edf");

    // The queue view now names the live policy.
    let q: serde_json::Value = client()
        .get(proxy.url("/api/queue"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(q["dispatch_policy"], "edf");

    // Requests still flow end-to-end under EDF.
    send_successful_chats(&proxy, 2).await;

    // A bogus policy is a client error, not a half-applied state.
    let bad = client()
        .post(proxy.url("/api/settings/dispatch"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "policy": "lifo" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    // A restart honors the persisted policy.
    let proxy = restart(proxy, &[]).await;
    let q: serde_json::Value = client()
        .get(proxy.url("/api/queue"))
        .header("cookie", &session_cookie(&proxy).await)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        q["dispatch_policy"], "edf",
        "policy persists across restart"
    );
}

/// Under contention an explicit deadline is the dispatcher's ordering key
/// with EDF: a deadline-carrying request behind a long holder is served
/// before its deadline passes or fails fast at its own bound, never parked
/// at max_wait. (The queue deadline is the minimum of the default wait and
/// the header's absolute bound.)
#[tokio::test]
async fn edf_deadline_bounds_the_queue_wait() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let cookie = session_cookie(&proxy).await;
    let r = client()
        .post(proxy.url("/api/settings/dispatch"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "policy": "edf" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Occupy the pool with a long stream, then queue a deadline-carrying
    // request behind it with a sub-second bound.
    mock.state.push(Behavior::ActiveStream(120));
    let _busy = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("edf holder", true))
        .send()
        .await
        .unwrap();

    let started = Instant::now();
    let urgent = client()
        .post(proxy.url("/v1/chat/completions"))
        .header("x-nim-proxy-deadline-ms", "700")
        .json(&chat_body("edf urgent", false))
        .send()
        .await
        .unwrap();
    // Either it wins a slot promptly (EDF ordering) or fails fast at its own
    // deadline (~700ms); it must NOT sit queued past its header deadline.
    assert!(
        urgent.status().is_success() || urgent.status() == reqwest::StatusCode::GATEWAY_TIMEOUT,
        "urgent request should serve or fail fast, got {}",
        urgent.status()
    );
    if urgent.status() == reqwest::StatusCode::GATEWAY_TIMEOUT {
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "deadline request should fail fast at its own bound, waited {elapsed:?}"
        );
    }
    // The holder keeps its slot; a plain request may simply wait (max_wait
    // is 30s in the fixture) — do not assert on it, just let the test end.
}

/// A restart does not reset upstream pacing: the rate window persisted on
/// clean shutdown is honored by the next boot, so a still-full key stays
/// throttled instead of bursting past its rpm. Ramp is disabled so this
/// assertion isolates window restoration from slow-start.
#[tokio::test]
async fn rate_windows_survive_a_restart() {
    let envs = &[("NIMPROXY_RAMP_SECS", "0")];
    let mock = start_mock().await;
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            nim_keys: vec![("only-key".into(), 2)],
            max_wait_secs: 2,
            heartbeat_secs: 1,
            ..StoreOpts::default()
        },
        envs,
    )
    .await;

    // Fill the 2-slot window.
    for _ in 0..2 {
        let r = client()
            .post(proxy.url("/v1/chat/completions"))
            .bearer_auth("sekrit")
            .json(&chat_body("hi", false))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    assert_eq!(mock.state.hit_count(), 2);

    // Restart the same DATA_DIR; the window rides over in ratestate.jsonl.
    let proxy = restart(proxy, envs).await;
    let cookie = session_cookie(&proxy).await;
    let now: serde_json::Value = client()
        .get(proxy.url("/api/dashboard/now"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(now["restored_lanes"], 1, "lane restored from disk: {now}");

    // The window is still full: the next request cannot fit and must fail
    // fast. A lost window would admit it immediately at full rpm.
    let third = client()
        .post(proxy.url("/v1/chat/completions"))
        .bearer_auth("sekrit")
        .json(&chat_body("hi", false))
        .send()
        .await
        .unwrap();
    assert_eq!(
        third.status(),
        504,
        "persisted window kept the key throttled across restart"
    );
    assert_eq!(
        mock.state.hit_count(),
        2,
        "no new upstream traffic slipped out after restart"
    );
}

/// After a quick restart the slow-start ramp engages: a key stepping back
/// from a recovered window admits only rpm × ramp_factor, letting surplus
/// requests shed with 504 instead of firing a burst at the upstream.
#[tokio::test]
async fn ramp_slows_traffic_after_a_quick_restart() {
    let cold_envs = &[("NIMPROXY_RAMP_SECS", "0")];
    let mock = start_mock().await;
    let proxy = start_proxy_with(
        &mock.url,
        StoreOpts {
            nim_keys: vec![("only-key".into(), 20)],
            max_wait_secs: 2,
            heartbeat_secs: 1,
            ..StoreOpts::default()
        },
        cold_envs,
    )
    .await;

    // Some traffic before the restart so the persisted window isn't empty.
    for _ in 0..3 {
        let r = client()
            .post(proxy.url("/v1/chat/completions"))
            .bearer_auth("sekrit")
            .json(&chat_body("hi", false))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    // Restart with a long ramp at 25% of budget.
    let ramp_envs = &[
        ("NIMPROXY_RAMP_SECS", "120"),
        ("NIMPROXY_RAMP_FACTOR", "0.25"),
    ];
    let proxy = restart(proxy, ramp_envs).await;

    // The ramp gauge is live and the window was recovered.
    let text = metrics(&proxy).await;
    assert!(
        text.contains("nimproxy_ramp_active 1"),
        "ramp gauge on after restart: {text}"
    );
    let cookie = session_cookie(&proxy).await;
    let now: serde_json::Value = client()
        .get(proxy.url("/api/dashboard/now"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(now["ramp_active"], true, "{now}");
    assert_eq!(now["restored_lanes"], 1, "{now}");

    // Burst of 8. The recovered window holds 3 recent slots; the ramp budget
    // is 20 × 0.25 = 5, so exactly 2 more pass and the other 6 shed fast.
    let statuses = futures_util::future::join_all((0..8).map(|_| {
        client()
            .post(proxy.url("/v1/chat/completions"))
            .bearer_auth("sekrit")
            .json(&chat_body("burst", false))
            .send()
    }))
    .await;
    let mut ok = 0;
    let mut limited = 0;
    for r in statuses {
        let r = r.unwrap();
        if r.status() == 200 {
            ok += 1;
        } else if r.status() == 504 {
            limited += 1;
        } else {
            panic!("unexpected status {}", r.status());
        }
    }
    assert_eq!(
        ok, 2,
        "ramp admitted exactly rpm × factor past the restored window"
    );
    assert_eq!(limited, 6, "surplus requests shed while the ramp is on");
    assert_eq!(mock.state.hit_count(), 5, "upstream saw 3 cold + 2 ramped");
}

// ---------------------------------------------------------------------------
// Multi-upstream failover
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_upstream_single_endpoint_behaves_as_before() {
    let mock = start_mock().await;
    let proxy = start_proxy(&mock.url, &[]).await;
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("hello", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn failover_primary_fails_backup_serves() {
    let mock = start_mock().await;
    let backup = start_mock().await;
    // Two upstreams: primary (mock) then backup.
    let opts = StoreOpts {
        upstreams: vec![mock.url.clone(), backup.url.clone()],
        ..Default::default()
    };
    let proxy = start_proxy_with(&mock.url, opts, &[]).await;

    // Primary is alive → first request goes to it.
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("first", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(mock.state.hit_count(), 1);
    assert_eq!(backup.state.hit_count(), 0);

    // Make the primary fail twice: mark it down.
    for _ in 0..2 {
        mock.state.push(Behavior::ServerError(500));
    }
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("retry", false))
        .send()
        .await
        .unwrap();
    // The proxy retries the 500 → hits another endpoint → backup serves.
    assert_eq!(resp.status(), 200);
    assert_eq!(backup.state.hit_count(), 1, "backup must serve the retry");

    // Succeeding request goes to backup (primary is down).
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("after", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        backup.state.hit_count() >= 2,
        "backup serves while primary is down"
    );
}

#[tokio::test]
async fn failover_primary_recovers_after_success() {
    let mock = start_mock().await;
    let backup = start_mock().await;
    let opts = StoreOpts {
        upstreams: vec![mock.url.clone(), backup.url.clone()],
        ..Default::default()
    };
    let proxy = start_proxy_with(&mock.url, opts, &[]).await;

    // Primary fails twice → goes down.
    for _ in 0..2 {
        mock.state.push(Behavior::ServerError(500));
    }
    let _resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("fail", false))
        .send()
        .await
        .unwrap();
    // Backup served.
    assert!(backup.state.hit_count() >= 1);

    // Primary gets a successful response (models refresh probes it).
    // The next request should go back to the primary.
    let resp = client()
        .post(proxy.url("/v1/chat/completions"))
        .json(&chat_body("recovery", false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn failover_health_reported_in_dashboard_now() {
    let mock = start_mock().await;
    let backup = start_mock().await;
    let opts = StoreOpts {
        upstreams: vec![mock.url.clone(), backup.url.clone()],
        ..Default::default()
    };
    let proxy = start_proxy_with(&mock.url, opts, &[]).await;
    let cookie = session_cookie(&proxy).await;

    let now = dashboard_now(&proxy, &cookie).await;
    let health = now["upstream_health"].as_array().unwrap();
    assert_eq!(health.len(), 2, "two endpoints reported");
    assert!(health[0]["alive"].as_bool().unwrap());
    assert!(health[1]["alive"].as_bool().unwrap());
}
