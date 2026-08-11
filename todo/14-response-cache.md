# 任务 14：幂等请求响应缓存（Response Cache for Idempotent Requests）

> 状态：已完成
> 日期：2026-08-11
> 需求：对非流式、幂等的 `POST /v1/chat/completions` 与 `POST /v1/embeddings`
> 请求做 TTL 缓存，命中时跳过限流队列与上游请求，节省 RPM 预算。缓存键基
> 于请求语义（model + messages + 采样参数），配以可配置的 TTL 和最大条目数。

---

## 背景与现状（探索结论）

- **限流瓶颈**：每条 key 40 RPM。非流式请求（buffered 路径）在等待 slot 时
  无法发送心跳（见 FAQ：「Non-streaming requests can't be heartbeated (no
  wire format for it) — they wait silently through pacing/retries」）。如果
  同一请求（如 embeddings、固定 prompt 的 classification）被反复提交，每次
  都消耗一个 RPM slot，而实际响应完全可复用。
- **现有缓存**：`proxy.rs` 的 `models_cache`（`Mutex<Option<(Instant, Bytes)>>`）
  仅缓存 `GET /v1/models`，TLT 10 分钟，单飞刷新。这是唯一的响应缓存。
- **请求处理流程**（`proxy.rs:handle()`）：
  1. setup gate → 2. in-flight cap → 3. client auth → 4. body parse → 5.
     Ctx 构建 → 6. `/v1/models` 快路径 → 7. stream 标记 → 8. 亲和性哈希
     → 9. 注册 operator queue → 10. 记录 shape metrics → 11. usage injection
     → 12. 背压门 → 13. 分支到 `buffered()` 或 `streaming()`。
  - 缓存插入点：在 7（stream 标记）之后、11（usage injection）之前。对
    `stream=false` 且 path 允许缓存的请求，先查缓存；命中则直接返回，跳过
    8-13 全部流程（包括 slot 获取）。
- **`buffered()` 路径**（`proxy.rs:1470+`）：
  - `acquire_model_permit()` → `reserve_slot()` → `upstream_request()` →
    `relay()`。
  - 缓存命中可跳过整个 retry loop，直接返回 `relay()` 等效结果。
- **Embeddings 端点**：`/v1/embeddings` 天然幂等，且 NIM 无免费的 embedding
  模型，但 proxy 的通用架构允许用它。缓存对 embeddings 的收益最高（输入同
  即输出同）。
- **已有依赖**：`sha2` 已存在（用于密码哈希和 client key 摘要），可直接用
  于缓存键哈希；`moka` 未引入，需要新增。

## 设计决策

### D1：缓存范围（只缓存非流式 + 幂等端点）

- 只缓存 `stream=false` 的请求（`wants_stream == false`）。流式请求的响应
  是 SSE 事件流，不可整体缓存。
- 允许缓存的 path：`/v1/chat/completions`、`/v1/embeddings`。
- 默认不缓存 `/v1/completions`（legacy，NIM 可能不支持），但配置可扩展。
- 不缓存带 `X-Nim-Proxy-Deadline-Ms` 的请求（deadline 语义暗示请求是一次
  性的、不可缓存）。

### D2：缓存键（语义哈希）

- 缓存键 = `SHA256(model + path + canonical_body)`，其中 `canonical_body`
  是 body 的规范 JSON 序列化（`serde_json::to_string` 产生稳定输出）。
- 之所以包含整个 body 而非子集，是因为：
  - 子集提取复杂且容易漏掉参数（如 `seed`、`stop` 序列、`response_format`）。
  - 全 body 哈希简单且安全（不会错误命中）。
  - 代价是 `frequency_penalty`、`presence_penalty` 等参数即使不变也
    进键——但大多数 harness 的请求是重复的，所以仍然有高命中率。
- 长度：`model` + `path` + body 哈希 = 约 100 字节，作为缓存键内存开销可接受。

### D3：缓存存储（moka）

- 使用 `moka::sync::Cache`（非 `future` 版，因为缓存操作是纯内存的，无
  需异步；`sync` 版内部用 `crossbeam` channel 做并发，足够快）。
- 配置：
  - `max_capacity`：默认 1024 条目（可配置，Settings 中加 `cache_max_entries`）。
  - `time_to_live`：默认 60 秒（可配置，Settings 中加 `cache_ttl_secs`）。
  - 不设 `time_to_idle`（语义上 TTL 就够了）。
- 存储值：`(Bytes, StatusCode, HeaderMap)`——完整的响应体 + 状态码 + 响应头
  （content-type 等）。`HeaderMap` 在 cache 中只保留 `content-type` 和
  `content-length` 子集，不保留上游的 `date`、`x-request-id` 等易变头。
- 线程安全：`moka::sync::Cache` 是 `Send + Sync`，存在 `AppState` 中。

### D4：缓存逻辑（hit → 短路；miss → 写入）

- 在 `handle()` 中，body 解析后、usage injection 前：
  ```
  if !wants_stream && is_cacheable_path(&path) && request_deadline.is_none() {
      let key = cache_key(&model, &path, &body);
      if let Some(hit) = state.response_cache.get(&key) {
          // 记录 cache hit 指标
          // 直接返回缓存的响应（用 axum::response::Response::builder）
      }
      // 继续正常流程，但将 key 传给 downstream
      // 在 buffered() 成功返回后，用 key 写入缓存
  }
  ```
- 写入点：`buffered()` 成功调用 `relay()` 后、返回前，将 `(resp, status, headers)` 写入 `state.response_cache`。
- 不缓存上游错误响应（status >= 400）。

### D5：指标

- 新增 Prometheus counter：
  - `nimproxy_cache_hits_total{path}` — 缓存命中次数
  - `nimproxy_cache_misses_total{path}` — 缓存未命中次数
  - `nimproxy_cache_entries` — 当前缓存条目数（gauge，moka 原生支持
    `entry_count()`）
- 在 dashboard 中择机展示（可选，后续可加）。

### D6：配置

- Settings 新增段：
  - `response_cache_ttl_secs: u64`（默认 60，设为 0 可禁用缓存）
  - `response_cache_max_entries: u64`（默认 1024）
- 环境变量不新增（走 Settings store）。

### D7：边界（不做）

- 不做流式缓存（`stream=true` 的 SSE 流不可整体缓存）。
- 不做请求体超集缓存（如不同 `temperature` 的请求共享缓存）。
- 不做预热（缓存按需填充）。
- 不做持久化（重启后缓存清空，逐步重建）。
- 不做分布式缓存（单实例内存缓存，与多实例不共享——与 rate window 同一
  限制，但短期内可接受）。
- 不做上游缓存控制头（`Cache-Control`、`ETag`）的解析——NIM 不返回
  这些头，按简单 TTL 即可。

## 实施步骤

### 1. 依赖
- `Cargo.toml`：添加 `moka = { version = "0.12", features = ["sync"] }`

### 2. 配置定义
- `config.rs`：`Config` 结构体增加 `response_cache_ttl_secs: u64`（默认 60）
  和 `response_cache_max_entries: u64`（默认 1024）。
- `config.rs`：`SettingsKeys` / `SettingsValues` 同步更新（序列化/反序列化）。
- 迁移（`upgrade_config` 函数中填充默认值）。
- 限定 `response_cache_ttl_secs` 值域为 0..86400（0 表示禁用）+ 合法性校验
  （`validate_settings` 中）。

### 3. AppState / 缓存初始化
- `lib.rs`：`AppState` 增加 `response_cache: moka::sync::Cache<String, (Bytes, StatusCode, HeaderMap)>` 或包装类型。
- 初始化时根据 `cfg.response_cache_max_entries` 和 `cfg.response_cache_ttl_secs` 构建 cache。

### 4. 缓存键与缓存逻辑
- 新建 `src/cache.rs`（或内联在 `proxy.rs` 中）：
  - `cache_key(model: &str, path: &str, body: &[u8]) -> String`：SHA256 哈希，hex 输出。
  - `is_cacheable_path(path: &str) -> bool`：白名单 `["/v1/chat/completions", "/v1/embeddings"]`。
  - `ResponseCache` 结构体（包装 moka cache）+ `get`/`set`/`configure` 方法。
- `proxy.rs`：
  - `handle()` 中加缓存查入逻辑（见 D4）。
  - `buffered()` 中加缓存写入逻辑。
  - 处理 `response_cache_ttl_secs == 0`（禁用）的情况。

### 5. 缓存写入
- `buffered()` 在 `relay()` 成功后，检查缓存是否启用、key 是否非空，然后
  `cache.set(key, (status, headers_subset, body))`。
- 只缓存 `relay()` 成功（status 2xx）的响应。
- 不缓存 `relay()` 出错或被 deadline 截断的响应。

### 6. 指标
- `metrics.rs`（或 `proxy.rs` 顶部）：
  ```rust
  metrics::describe_counter!("nimproxy_cache_hits_total", "Response cache hit count");
  metrics::describe_counter!("nimproxy_cache_misses_total", "Response cache miss count");
  metrics::describe_gauge!("nimproxy_cache_entries", "Number of entries in the response cache");
  ```
- cache hit/miss 处 `metrics::increment_counter!`。
- 定期（或 lazily）更新 `nimproxy_cache_entries` gauge。

### 7. 前端 Settings
- `src/dashboard.html`：Settings 页面增加 "Response caching" 段：
  - TTL 输入（秒，0 = 禁用）。
  - 最大条目数输入。
  - 保存时走现有 `POST /api/settings/...` 路径。

### 8. 测试
- **单元测试**（`src/cache.rs` 或 `tests/`）：
  - `cache_key` 对相同输入产生相同输出。
  - `cache_key` 对不同输入产生不同输出。
  - `is_cacheable_path` 对 `/v1/chat/completions` 返回 true，对 `/v1/embeddings` 返回 true，对 `/v1/*` 其他路径返回 false。
- **集成测试**（`tests/e2e.rs` 或 `tests/support/mod.rs`）：
  - 发送两次相同的非流式请求 → 第二次应命中缓存（可在 mock 中计数请求数）。
  - 缓存禁用时（`cache_ttl_secs = 0`）→ 两次都走上游。
  - 缓存过期后（TTL 已过）→ 第二次走上游。
  - 流式请求不缓存（即使相同 body）。
  - 带 `X-Nim-Proxy-Deadline-Ms` 的请求不缓存。
  - 缓存后的请求在 dashboard 指标中显示 cache hit。

### 9. 文档
- `knowledge/decisions/response-cache.md`：决策页（本文件内容整理）。
- `knowledge/index.md`：添加决策页条目。
- `knowledge/log.md`：添加日志条目。
- `CHANGELOG.md`：记录新功能。

### 10. 验证门
- `cargo fmt` — 无变更。
- `cargo clippy --all-targets -- -D warnings` — 无新增警告。
- `cargo test` — 全部通过（含新测试）。
- 手动验证：docker compose 启动，Settings 中确认缓存配置可见，两次相同
  请求确认命中（通过日志或指标）。

## 风险与注意

- **缓存键冲突**：SHA256 碰撞概率极低，可忽略。
- **内存膨胀**：`max_entries = 1024`，每个条目约 1-10 KB（视响应大小），
  总计约 1-10 MB，可接受。`max_entries` 可设上限 10000，人均无压力。
- **缓存过期后的突发**：TTL 到期后，所有并发请求都会 miss 并同时打上游。
  但 NIM 的 RPM 限流会自然节流（每个请求独立排队等待 slot），不会造成
  比正常情况更差的突发。
- **缓存与代理的透传承诺**：README 说「Bodies are forwarded untouched」
  （除 usage injection）。缓存直接返回本地响应，不经过上游——**这在语义上
  是正确的**（因为缓存的是上游的原始响应），但需要文档明确说明"缓存命中
  时不会经过上游"。
- **Settings 热更新**：TTL/max_entries 变更时，需要重建缓存。实现方式：
  在 `configure` 路径中 `drop` 旧 cache、`new` 新 cache。moka 的 `new` 是
  轻量的。
- **非 2xx 不缓存**：上游错误（429、500 等）不应缓存，因为：
  - 429 是瞬态的，缓存后会错误地拒绝后续请求。
  - 5xx 可能是瞬态错误。
  - 2xx 成功响应才是缓存的目标。

---