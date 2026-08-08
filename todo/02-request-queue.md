# 任务 02：请求队列（Request Queue）+ 管理员终止请求

> 状态：已完成（所有实现步骤落地并通过验证，详见文末"实施记录"）
> 日期：2026-08-09
> 需求：dashboard 侧边栏新增"请求队列"入口：查看全部用户的在途请求（哪个用户请求了什么模型），
> 允许管理员终止任意请求（包括自己的），被终止的请求收到错误码 `-91`"Your request has been
> terminated by the system"。

---

## 背景与现状（探索结论）

- **无 per-request 注册表**：`/v1/*` 统一进 `proxy::handle()`（`src/proxy.rs:479`），分流到
  `buffered()`（:692，非流式）或 `streaming()`（:769，spawned task + mpsc，先提交 200 SSE）。
  现有在途计量只有 `AppState.inflight`（`AtomicUsize`，仅做 `max_inflight` 熔断）与 gauge
  `nimproxy_active_requests`（:703/:790）——没有任何"这条请求是谁、在哪个阶段、如何找到它
  并取消它"的实体。
- **请求身份数据齐全**：`handle()` 早期的 `Ctx {client, model, path, started}`（:26-32，
  解析于 :565-570）就是需求要展示的全部字段：
  - `client` = API key 名（keyed 模式）或 `"local"`（open 模式）——这就是"哪个用户"。
  - `model` = 经 `label_model`/`sanitize_label` 消毒过的模型 id（:89-93）——"请求了什么模型"。
  - `path`、`started` 直接可用。队列只需把这些字段登记进注册表。
- **取消基础设施已可复用**：
  - 流式主循环已有 `tokio::select! { _ = tx.closed() => …, read = upstream_read => … }`
    （:927-941）——一个终止信号天然是第三个分支。
  - `reserve_slot`（:140-166）与 `acquire_model_permit`(:172-205) 都接受 `on_wait` 心跳回调，
    返回 `false` 即放弃——心跳处顺带查终止标志即实现"排队中被杀"。
  - 非流式 `buffered()` 的 `upstream_request(..).timeout(..).send()`（:730-733）可套
    `tokio::select!` 加一个终止分支（reqwest future 可安全取消，连接直接断开）。
- **错误信封已有模板**：`proxy_error_json`（:1255）输出
  `{"error":{message,type:"proxy_error",code:"-91"}}`；`sse_error_with_code`（:1261，用于已提交
  200 的流内错误事件，先 `data:` 事件再 `data: [DONE]`）。`code` 是字符串字段，`"-91"` 直接可用。
- **管理角色体系现成**：`Role::{Superuser,Admin,User}`，`Role::is_admin()`（`src/config.rs:220-225`）；
  现有 admin-only 端点模式 = `require_session`（`auth.rs:362`）注入 `Identity(username)` +
  `settings.rs` 的"role_of → forbidden → mutate"骨架（:296、:565-611）。"管理员断开用户请求"沿用同一套。
- **dashboard tab 机制（task 01 已验证落地）**：侧边栏 `<button role="tab" data-tab>`（html:378-386，
  Catalog 是 :380 的先例）+ `<section id="tab-xxx" hidden>`（overview :427、catalog :456 先例）+
  `TITLES`（:1226）与 `RENDERERS` 条目（:2343）+ 同名渲染函数（`renderCatalog`/`loadCatalog`
  :2371/:2401 先例）；深链 hash、窄屏 icon rail、
  `aria-selected` 自动生效。队列页可完全复制 Catalog 的做法。`POLL_MS = 3000`（:723）。
- **实时轮询现成**：`pollNow()`（:2531）每 3s 拉 `/api/dashboard/now`；队列页若作为独立 tab，
  可在可见时自轮询 `/api/queue`（Catalog 是进入时拉取 + 手动刷新，队列需要定时刷新）。
- **隐私红线**：`request-shape-metrics` 决策确立"计数与元数据、绝不记录内容"。队列只列
  id/client/model/path/started/阶段/时长，绝不进 prompt/tools/usage 内容。
- **测试基建**：`tests/support/mod.rs` 脚本化 mock 支持 `Behavior::Hang`、`Behavior::DelayHeaders(n)`
  （在途等待）、`Behavior::ActiveStream(n)`（流式进行中）；`support::login()` 取 session cookie；
  `Behavior` 队列消费式注入。e2e 已有 dashboard 轮询与认证测试模板。

## 设计决策

### D1：新入口：侧边栏新增第 8 个 tab（data-tab="queue"）
- 需求原文"overview 侧边栏新增请求队列"按 task 01 先例解释为：侧边栏新增条目，紧邻 Overview。
  备选（嵌入 Overview 页面加卡片）若更符合预期，实施时只需把渲染函数挂进 `tab-overview` 而非
  新 section（代价一行），设计上不冲突。
- 命名：**Queue**（label），`TITLES["queue"]` = "Request Queue"。新水墨内联 SVG 图标（16×16
  stroke 风格，队列三横线）。
- 权限：仅管理员可见。侧边栏按钮默认隐藏，`/api/dashboard/now` 响应新增 `"role"` 字段
  （`superuser | admin | user`），前端据此显示/隐藏按钮；后端 `/api/queue*` 一律 403 兜底。

### D2：数据源：新增 `src/registry.rs` 在途请求注册表（内存态）
- `AppState.registry: Arc<RequestRegistry>`。
- `RequestRegistry { next_id: AtomicU64, inner: RwLock<HashMap<u64, Entry>> }`。
- `Entry { id, client, model, path, started: Instant, phase: &'static str, kill: watch::Sender<bool> }`。
  phase 取值：`queued`（等 slot / 等 permit）/ `upstream`（已发出）/ `retry`。不做阶段枚举之外的
  东西，`started` 与 `elapsed` 由前端算。
- 注册发生在 `handle()` 解析出 `Ctx` 之后；`Enter` 传 `kill` 的 `watch::Receiver<bool>` 进
  `buffered()`/`streaming()`；请求结束（任意路径）Drop guard 自动从 map 摘除（scopeguard 模式，
  与 `dispatch::scopeguard` 同族）。id 单调递增，不复用——注册表无持久化，开机即空。
- 队列展示的 client/model 就是 `ctx.client`/`ctx.model`（已消毒），无需新消毒逻辑，但前端
  **所有动态值仍必须过 `esc()`**（XSS 不变量）。

### D3：终止语义：kill 信号 + 检查点
- 终止 = `kill.send(true)`；请求侧所有检查点均以"信号已消费即结束"为准：
  1. 流式主循环（:969 select）加第三个分支 `kill.changed()` → 回 `-91` SSE 再退出；
  2. 流式的 `on_wait` 心跳 closures（:806-829，reserve_slot 与 acquire_model_permit 两处）里
     追加 `*kill.borrow()` 检查，false 即静默退出（等 slot 中被杀）；
  3. `buffered()` 的 `upstream_request().send()` 外包 `tokio::select!` 加杀分支；
  4. 拿到 slot 后、发请求前再查一次（竞态窗口可接受：拒绝后到真发送只有 µs 级，漏杀一次
     请求消耗一个窗口计数；已在文档记录）。
- 被终止请求：`record_request(ctx, "terminated")`，tracing `warn`/`info` 记录
  `terminated by <admin> (req <id>)` 可供审计；新增 counter `nimproxy_terminated_total`
  （`by` label = admin 用户名，用户量有界，cardinality 安全）。
- 响应形态（需求"返回错误码 -91"）：
  - 流式（已 200 提交）：`data: {"error":{"message":"Your request has been terminated by the system","type":"proxy_error","code":"-91"}}\n\ndata: [DONE]\n\n`
  - 非流式：HTTP 400（不可重试，终止是人为决策）+ 同 JSON 信封。
  - 客户端语义：OpenAI 兼容错误信封、`code` 字符串 `"-91"`，harness 能透出给用户。

### D4：后端 API（挂在 `require_session` 之下，服务端 admin 检查）
- `GET /api/queue` → `{ "requests": [ {id, client, model, path, started_at, age_s, phase} ] }`；
  非 admin → 403 `{"error":{"code":"forbidden"}}`（沿用 settings 骨架）。
- `POST /api/queue/terminate`，body `{"id": <u64>}` → `{"ok": true}`；id 早已不存在 → 404。
  管理员可终止任何请求，**包括自己的**。
- 两个端点都只读注册表，不经 store、不经上游，零限流预算。

### D5：前端形态（`renderQueue()`）
- 进入 tab 起轮询 `/api/queue`（2s，仅本 tab 可见时；离开 tab 停）。可复用 Catalog 的
  fetch-while-visible 骨架。
- 内容（自上而下）：
  1. 头部 KPI：在途总数、排队等待数（phase=queued）。
  2. 表格/列表：年龄（started 起算）· client（monogram chip 复用 `clientChip`）· model
     （`prettyName` + publisher chip，复用 Catalog 基建）· path · phase 标签 · **终止按钮**
     （title="终止该请求"；点击一次即 POST，成功本地移除行 + 提示）。
  3. 空态："当前没有在途请求"；错误态：403 → "需要管理员权限"。
- 该 tab 数据独立于 3s report，不受窗口/时间选择器影响。

### D6：边界（不做）
- 不做内容级展示（prompt、usage、tools）——只列元数据，保持 `request-shape-metrics` 隐私口径。
- 不做历史持久化：队列只代表当前在途；重启即空。
- 不做批量终止、不做按 client 过滤（v1 范围外）。
- 不触碰 `/v1/*` 透传行为与限流分配逻辑；杀进程不改变 slot 分配纪律（排队中被杀自然释放，
  已在途被杀浪费到已发出瞬间）。
- 不引入新依赖（watch channel 在 tokio "sync" feature 内，Cargo.toml:23 已启用）。

## 实施步骤

1. **Rust 侧**
   - 新建 `src/registry.rs`：`RequestRegistry`、`Entry`、`register()`/`remove()`、
     `terminate(id)`、`snapshot()`；无缝 Drop 自动摘除。
   - `src/lib.rs`：`AppState.registry` 初始化；`.route("/api/queue", get(...))` 与
     `.route("/api/queue/terminate", post(...))` 挂回 `require_session` 的 protected 路由；
     `/api/dashboard/now` 响应加 `"role"`（从 `Identity` 读）。
   - `src/proxy.rs`：`handle()` parse Ctx 后登记进注册表，把 kill Receiver 传进
     `buffered()`/`streaming()`；补 `is_terminated` 检查点与 `-91` SSE/JSON 信封与 `record_request`
     ("terminated") 与 counter；确认 `parse_request_deadline` 等早退路径都能正常摘除登记。
2. **前端 `src/dashboard.html`**
   - 侧边栏 `<button data-tab="queue">` + 图标 + `TITLES` 条目；非 admin 时按钮隐藏
     （`/api/dashboard/now` 的 `role`）。
   - `<section id="tab-queue">`（默认 hidden）+ `renderQueueTab()`：KPI、表格、终止按钮、空态/403
     态；可见时 2s 轮询。
   - 所有动态值过 `esc()`；无新增绕过 esc 的 innerHTML sink。
3. **测试**
   - unit：`registry` 注册/摘除/Drop 清理、`terminate` 命中/未命中、snapshot 排序。
   - e2e（沿用 `Behavior::Hang` / `DelayHeaders` / `ActiveStream` + `support::login`）：
     a) 未登录 GET /api/queue → 401；登录后 200；
     b) user 角色 → 403；admin → 200；
     c) `ActiveStream` 在途：queue 里出现该请求（client=提交 key 名（如 `e2e`）、model、phase）；
        terminate → 客户端收到含 `code:"-91"` 的 SSE 错误并结束；
     d) 非流式（DelayHeaders）terminate → 收到 400 + `-91` 信封，请求体未到达上游（kill 生效）；
     e) 已结束请求再 terminate → 404；
     f) 管理员终止自己的请求也被允许（不做自身豁免）。
4. **安全与合规**
   - require_session 下、admin 检查放服务端；`/api/queue` 不含密钥/敏感字段；前端 esc；
   - 观察 request-shape 口径无变化。
5. **文档与知识库（同 PR 维护）**
   - `knowledge/decisions/` 新决策页：队列注册表 + 终止语义（-91 契约、in-flight 唯一标识设计）。
   - `knowledge/index.md` 索引行 + `knowledge/log.md` 日期条目。
   - `CHANGELOG.md` Unreleased 新增条目；若 README runbook 涉及操作员可见面变更同步更新。
6. **验证门**
   - `cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全绿。
   - 手动：登录 dashboard → Queue tab → mock 上游挂起请求可见 → 终止 → 客户端收到 -91；
     user 角色登录看不到该 tab。

## 风险与注意

- **竞态**：拿到 slot 后、发出前被杀 → 浪费一次窗口计数（罕见，文档记录即可；也可考虑给
  `Slot` 补 `stamp` 以支持 release，作为可选增强）。
- **心跳粒度**：排队中被杀的最长响应时间 ≈ heartbeat（默认小几百 ms），可接受。
- **role 字段外溢**：`/api/dashboard/now` 新增 `role` 是把角色暴露给全部会话（已被 require_session
  保护），仅影响 UI 归隐，无权限面扩大（服务端仍强制 403）。
- **404 vs 幂等**：terminate 已结束/已死请求返回 404 而非吞掉——便于前端判定"已消失即成功"。
- **命名语义**："Model"=性能遥测，"Catalog"=上游目录，"Queue"=在途请求；页面副标题要写清避免混淆。
---

## 实施记录（2026-08-09 完成）

- `src/registry.rs` 新建：`RequestRegistry`（RwLock<HashMap> + AtomicU64 单调 id）、
  `Entry { id, client, model, path, started, phase, kill: watch::Sender<bool> }`、
  `register/remove/terminate/snapshot`；Drop guard（scopeguard 模式）自动摘除；4 个 unit 测试。
- `src/lib.rs`：`AppState.registry`；`GET /api/queue` + `POST /api/queue/terminate` 挂在
  `require_session`（403 非 admin）；`/api/dashboard/now` 响应加 `role`。
- `src/proxy.rs`：`handle()` 登记；终止检查点 = 流式 select 第三分支 + `on_wait` 心跳闭包
  `*kill.borrow()` 静默退出 + `buffered()` 的 send 外包 select；`-91` SSE 信封（`terminated_sse()`）
  与 buffered 400 JSON；`record_request(..,"terminated")` + `nimproxy_terminated_total{by}`。
- `src/dashboard.html`：Queue tab（data-tab="queue"，非 admin 隐藏）、renderQueueTab（KPI + 
  表格 + 终止按钮 + 空态/403 态）、2s 可见时轮询；动态值全部 `esc()`。
- e2e 4 个新测试：401 门卫 / 非 admin 403 + role 字段 / 流式列出+终止→-91+404 / buffered
  终止→400 -91 / 排队中终止。全量 220 测试绿。
- 文档：`knowledge/decisions/request-queue-and-termination.md`、`index.md` 行、`log.md` 条目、
  CHANGELOG Unreleased。
- 验证门：本机无 rustfmt/clippy（WSL 源码 tarball 工具链），以 `cargo build --all-targets`
  零警告代替；`cargo test` 125 unit + 95 e2e 全绿。CI 将执行 fmt/clippy。
