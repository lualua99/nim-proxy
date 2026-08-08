# 任务 01：可用模型查看页（Model Catalog）

> 状态：已规划，待实施
> 日期：2026-08-09
> 需求：在 dashboard 侧边栏新增一个"可用模型查看页"，方便操作员了解 NIM 上游当前可用的模型列表。

---

## 背景与现状（探索结论）

- **上游目录**：NIM `/v1/models` 严格遵循 OpenAI 最小 schema，每条仅
  `id` / `object` / `created` / `owned_by` 四字段（无描述、无能力标记）。
  依据：`knowledge/research/nim-models-endpoint-schema.md`。
- **代理侧现状**：`src/proxy.rs:1023-1057` 的 `models()` 处理 `/v1/models`：
  缓存命中（`state.models_cache`，TTL 默认 600s）零成本返回；过期后经
  限流调度（`reserve_slot`）向 NIM `fetch_models()` 刷新一次并写回缓存。
  响应体原样透传（`src/proxy.rs:1062-1071`）。
- **dashboard 现状**：侧边栏 6 个 tab（Overview · Models · Clients ·
  Reliability · Capacity · Settings），`src/dashboard.html`。现有 **Models
  tab 是代理侧性能遥测**（按 metrics label 聚合），**不是 NIM 可用模型
  列表**——当前全产品没有任何 UI 展示上游目录。
- **无现成 API**：13 个 `/api/*` 端点全部 session 认证，但没有模型列表
  端点。dashboard 拿不到缓存的目录。
- **视觉身份基础设施齐全**：`publisher()`（:1160）、`prettyName()`
  （:1167）、`chipHtml()`（:1174，LobeHub CDN logo + 品牌色 monogram
  回退）、`mcard` 卡片样式（:257）。新页面可直接复用，无需上游描述。
- **tab 机制**：新增 tab = 侧边栏加 `<button data-tab>` + `<main>` 下加
  `<section id="tab-xxx">`（默认 hidden）+ `RENDERERS` 条目 + `TITLES`
  条目 + 同名渲染函数。深链 hash、窄屏 icon rail、`aria-selected` 均自动
  生效（`:2538-2551`）。

## 设计决策

### D1：新入口：侧边栏新增第 7 个 tab（data-tab="catalog"）
- 位置：Overview 之后、Models 之前（或紧邻 Models）。
- 命名：**Catalog**（label），`TITLES["catalog"]` = "Model Catalog"。
- 与现有 Models tab 共存的命名区分：Models=性能遥测，Catalog=可用模型
  目录。若担心混淆，备选名 "Available"。
- 使用新的水墨图标（内联 SVG，16×16 stroke 风格，与现有图标一致）。

### D2：数据源：新增 session 认证的 `GET /api/models`
- 逻辑（复用而非复制）：把 `proxy::models()` 的"缓存检查 + 刷新"核心
  抽成共享函数（如 `models_catalog(state, cfg) -> CatalogResult`），
  `/v1/models`（受客户端 key 认证）与 `/api/models`（受 session 认证）
  共用同一份缓存与刷新路径。
- `/api/models` 响应 JSON：
  `{ "models": [ {id, created, owned_by}, ... ], "cached_at": unix, "ttl_secs": n }`
  —— 从缓存字节 parse 一次（缓存是上游原始 body，字段即 schema 四件套）。
- 缓存未命中/过期时的行为：同步触发一次上游 fetch（受限流预算约束，
  与 `/v1/models` 路径相同），成功后更新共享缓存并返回；失败返回
  `502`（带错误信息）。
- 权限：任意已登录用户可见（与 `/api/dashboard` 同级），不输出任何
  密钥/敏感信息。
- 可选 `?refresh=1`：操作员手动强制刷新时绕过 TTL 强制 fetch 一次
  （消耗一次限流预算；计划内默认提供，实施时以按钮触发）。

### D3：前端页面形态（`renderCatalog()`）
- 独立于 `/api/dashboard` 3s 轮询：进入 tab 时加载一次 + 手动"刷新"
  按钮（含 cached_at 时间戳文案，如 "Cached 12:03:45 · 刷新"）。
- 内容（自上而下）：
  1. 头部 KPI：模型总数、缓存时间。
  2. 模型卡片网格（复用 `.gcards`/`.mcard` 风格，或轻量表格）：每模型
     显示 publisher chip + prettyName + **完整原始 id**（可复制，目录的
     核心用途是拿 id 来配 harness）+ owned_by + created 本地时间（无值
     显示 —）。
  3. 空态："目录为空（上游未返回模型）"；错误态：fetch 失败原因。
- 所有动态值经现有 `esc()` 转义（XSS 不变量，见
  `knowledge/decisions/input-sanitizing-and-xss.md`）。

### D4：不做的事（边界）
- 不做"目录→性能"联动过滤（性能模型来自 metrics label，上游 id 未必
  一一对应，价值低且复杂）。
- 不解析/不展示描述、上下文长度等不存在的字段（知识库已确认 schema）。
- 不改 `/v1/models` 透传行为（客户端 API 兼容）。

## 实施步骤

1. **Rust 侧**
   - `src/proxy.rs`（或新 `src/catalog.rs`）：抽出共享目录逻辑
     `catalog(state, cfg, force_refresh)` → 结构体 {models, cached_at}。
     `models()` 改为调用它；新增 `handle_models_api()`。
   - `src/lib.rs`：注册 `.route("/api/models", get(...))` 于
     `require_session` 之下。
   - 确保 `probe_key` / setup 复用不受影响。
2. **前端 `src/dashboard.html`**
   - 侧边栏 `<button data-tab="catalog">` + 图标 + `TITLES` 条目。
   - `<section id="tab-catalog">`（默认 hidden）+ `RENDERERS.catalog`
     与 `renderCatalog()`（fetch `/api/models` → 渲染；刷新按钮；
     错误/空态）。
   - 复用 `publisher()/prettyName()/chipHtml()/esc()`。
3. **测试**
   - unit：目录解析（缺字段容错、created 时间格式化）、刷新失败分支。
   - e2e：session 认证（未登录 401）、登录后 200、缓存命中不再打上游、
     `?refresh=1` 强制重取（mock_nim 断言请求计数）。
4. **安全与合规**
   - 新端点检查：require_session 下、无密钥泄露、所有动态值 esc。
   - `src/dashboard.html` 无字符串拼接 `innerHTML` 绕过 esc 的新 sink。
5. **文档与知识库（同 PR 维护）**
   - `knowledge/decisions/` 新决策页（或并入 dashboard 架构页）：
     catalog 视图设计、共享缓存路径。
   - `knowledge/index.md` 索引行 + `knowledge/log.md` 日期条目。
   - `CHANGELOG.md` Unreleased 新增条目。
   - 若 UI 语义变化影响 `README` runbook，同步更新。
6. **验证门**
   - `cargo fmt`、`cargo clippy --all-targets -- -D warnings`、
     `cargo test`（unit + e2e 全绿）。
   - 手动：登录 dashboard → 点击新 tab → 看到 mock 上游模型列表 →
     刷新按钮 → 缓存时间戳更新。

## 风险与注意

- **限流预算**：目录刷新走上游限流调度；高频 `?refresh=1` 会消耗速率。
  前端刷新按钮不加节流以外不需要；不做自动定时刷新。
- **命名混淆**：Catalog vs Models tab 需在页面副标题/图标上区分清楚，
  避免运维困惑。
- **缓存的共享**：dashboard 刷新目录也会顺带更新 `/v1/models` 客户端
  缓存（行为一致，无破坏）。