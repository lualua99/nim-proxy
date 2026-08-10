# 任务 13：实时仪表盘推送（SSE/WebSocket）

> 状态：已完成
> 日期：2026-08-10
> 需求：仪表盘目前每 3 秒轮询 `/api/dashboard/now`，改用 SSE 或 WebSocket 推送实时数据，减少轮询开销和延迟。

---

## 背景与现状

- **现状**：`src/dashboard.html` 中 `pollNow()` 每 3 秒发起一次 `GET /api/dashboard/now`，返回完整 JSON（含所有通道状态、度量快照、历史尾巴等）。每次响应 ~2-10 KB，对 50+ 通道可能更大。
- **服务端现状**：`src/lib.rs:274-361` 的 `api_dashboard_now` 每次请求需获取池快照、Prometheus 渲染、历史快照、上游健康状态等，涉及多次锁获取和 JSON 序列化。
- **轮询问题**：
  - 每 3 秒的全量 JSON 序列化在大量客户端时增加 CPU 开销。
  - 数据在两次轮询之间发生变化时，仪表盘更新有 0-3 秒的延迟。
  - 移动端/弱网下，每 3 秒的请求增加电池和带宽消耗。
- **axum 原生支持**：axum 0.8 原生支持 SSE（`axum::response::sse`）和 WebSocket（`axum::extract::ws`），无需额外依赖。

## 设计决策

### D1：推荐 SSE 而非 WebSocket

- **理由**：
  - SSE 是单向（服务端→客户端），与仪表盘需求完全匹配（仪表盘从不向服务端发送实时数据）。
  - SSE 通过标准 HTTP 工作，无需升级握手，穿透所有代理/负载均衡器。
  - 浏览器原生支持 `EventSource` API，自动重连，无需 JS 库。
  - 现有 CSP 配置已允许 `connect-src 'self'`，无需修改。
- WebSocket 适用于双向通信（如仪表盘操作实时同步），但当前无此需求。

### D2：增量推送，而非全量快照

- 每 3 秒推送完整 JSON 是冗余的。改为推送 **增量更新**：
  - 初始连接时发送一次全量快照（`{"type": "snapshot", ...}`）。
  - 后续推送仅包含变化的字段（`{"type": "delta", "window_fill": 42, "per_lane": [...], ...}`）。
  - 心跳每 30 秒发送一次，保持连接存活（`{"type": "heartbeat"}`）。
- 这需要服务端追踪上次推送的状态并计算差异。复杂度较高，可作为 v2 优化。

### D3：简单方案（v1）— 保持全量推送，改用 SSE 替代轮询

- 服务端新增 `GET /api/dashboard/stream` 端点，以 SSE 形式每 3 秒推送一次 `api_dashboard_now` 的 JSON 响应。
- 客户端 `EventSource` 监听 `/api/dashboard/stream`，`onmessage` 更新仪表盘。
- 保留 `/api/dashboard/now` 作为回退（SSE 断开时降级到轮询）。
- 这是最小可行方案，改动最小，风险最低。

### D4：服务端推送间隔

- 推送间隔保持 3 秒（与当前轮询间隔一致），避免过快更新导致客户端重绘抖动。
- 可配置（通过 `Settings` → `Dashboard` 添加 `push_interval_secs` 字段）。
- 默认 3 秒；最小 1 秒，最大 30 秒。

### D5：连接管理

- 每个 SSE 连接对应一个 `tokio::spawn` 任务，持有 `AppState` 的 `Arc` 克隆。
- 客户端断开时（`tx.send()` 返回 `Err`），任务自动退出，无泄漏。
- 服务端重启时，客户端 `EventSource` 自动重连（内置重连机制）。
- 连接数限制：通过 `max_inflight` 不直接适用，但可新增 `max_sse_connections` 防止资源耗尽（默认 100）。

### D6：Session 认证

- SSE 端点同样受 `require_session` 中间件保护，与现有仪表盘 API 一致。
- `EventSource` 不支持自定义请求头，session cookie 自动携带（与现有认证机制相同）。

## 实施步骤

### Rust 侧

1. **`src/lib.rs`**：新增 `api_dashboard_stream()` 处理函数：
   - 使用 `axum::response::sse::Sse` 返回类型。
   - 创建 `tokio::sync::broadcast` 或每连接独立 `tokio::sync::mpsc` 通道。
   - 每 3 秒向通道发送 `api_dashboard_now` 的 JSON 序列化结果。
   - 客户端断开时自动退出。
   - 注册路由：`.route("/api/dashboard/stream", get(api_dashboard_stream))` 于 `require_session` 之下。
2. **连接数限制**：在 `AppState` 中新增 `AtomicUsize` 追踪活跃 SSE 连接数，超过阈值时拒绝新连接。

### 前端 `src/dashboard.html`

3. **`pollNow()` 修改**：
   - 尝试创建 `EventSource("/api/dashboard/stream")`。
   - `onmessage`：解析 JSON 并调用现有的 `updateDashboard()` 渲染逻辑。
   - `onerror`：SSE 断开时降级到轮询（`pollNow()` 的现有逻辑）。
   - 页面离开（`visibilitychange`）时关闭 `EventSource`，返回时重新连接。
4. **保留 `/api/dashboard/now` 轮询路径**：作为 SSE 断开时的降级方案。

### 测试

5. **e2e 测试**：新测试验证 SSE 端点：
   - 未登录返回 401。
   - 登录后返回 `content-type: text/event-stream`。
   - 3 秒内收到至少一条 `data: {...}` 消息。
   - 客户端断开后服务端任务自动退出（无泄漏）。
6. **单元测试**：`api_dashboard_stream` 在连接数超限时返回 503。

### 安全与合规

7. 新端点检查：`require_session` 下、无新数据泄露风险。
8. CSP 更新：`connect-src 'self'` 已覆盖 SSE（SSE 使用标准 HTTP 连接），无需修改。

### 文档与知识库（同 PR 维护）

9. `knowledge/decisions/` 新决策页：SSE 实时推送的设计选择、增量 vs 全量权衡。
10. `knowledge/index.md` 索引行 + `knowledge/log.md` 日期条目。
11. `CHANGELOG.md` Unreleased 新增条目。

### 验证门

12. `cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test`（unit + e2e 全绿）。
13. 手动：登录 dashboard → 确认页面正常加载 → 打开浏览器 DevTools Network tab → 确认存在 `EventSource` 连接到 `/api/dashboard/stream` → 每 3 秒收到 `data:` 消息 → 关闭服务端 → 客户端自动降级轮询 → 重启服务端 → 客户端恢复 SSE。

## 风险与注意

- **EventSource 限制**：`EventSource` 不支持自定义请求头，但 session cookie 自动携带，认证无障碍。
- **SSE 连接数**：每个浏览器标签页建立一个连接，多标签页时需注意连接数上限。
- **增量推送复杂度**：v1 不做增量，保持全量推送。增量推送需要 diff 算法（如 JSON Patch），增加维护复杂度，且收益在高频更新时才显著。
- **浏览器兼容性**：`EventSource` 在所有现代浏览器中支持，IE 不支持（但仪表盘已不兼容 IE）。
- **与现有轮询的切换**：SSE 断连时降级到轮询，轮询间隔保持 3 秒，用户体验无缝。

---

## 实现记录

### Rust 侧 (`src/lib.rs`)

- 提取 `dashboard_now_payload(state, username) -> serde_json::Value` 共享给
  `api_dashboard_now` 和 `api_dashboard_stream`。
- 新增 `GET /api/dashboard/stream`，使用 `axum::response::sse::Sse`，
  每 3 秒通过 `mpsc` 通道推送 JSON 快照，15 秒 keepalive。
- `AppState` 新增 `sse_connections: AtomicUsize`，上限 100 连接，
  超限返回 503 `too_many_streams`。
- 路由注册在 `require_session` 下。

### 前端 (`src/dashboard.html`)

- 提取 `pollNow()` 处理逻辑为 `handleNowPayload(next)`。
- 新增 `startSSE()` 使用 `EventSource('/api/dashboard/stream')`，
  `onmessage` 调用 `handleNowPayload`，`onerror` 降级到轮询。
- 新增 `visibilitychange` 监听，tab 隐藏时关闭 SSE，返回时重连。
- 启动时先执行一次 `pollNow()` 确保首屏实时，然后 `startSSE()`。

### 测试 (`tests/e2e.rs`)

- `dashboard_sse_stream_requires_auth` — 未登录 401。
- `dashboard_sse_stream_returns_event_stream` — content-type 验证。
- `dashboard_sse_stream_receives_data_within_timeout` — 3 秒内收到 `data:`。
- `dashboard_sse_stream_connection_limit_returns_503` — 超限返回 503。

### 验证

- `cargo fmt`、`cargo clippy --all-targets -- -D warnings` 通过。
- `cargo test` 单元 + e2e 全绿（4 个新 SSE 测试 + 原有全部测试通过）。