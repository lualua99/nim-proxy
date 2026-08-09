# 任务 08：多上游端点故障转移（Multi-upstream failover）

> 状态：提案（未开始）
> 日期：2026-08-09
> 需求：当前 upstream 是单一固定 URL（`cfg.base_url`）。NIM 全站限流 /
> 宕机 / 账号被锁时，整个 pool 一起死。支持一份**有序上游端点列表**：
> 主端点可用就用它，探测失败自动切到备选（如自建 NIM 副本、别的 region /
> 兼容端点），恢复后回切。零限流预算成本——复用现有 models-cache 路径做
> 烟雾探测。

---

## 背景与现状（探索结论）

- **现状**：`cfg.base_url` 单值；`fetch_models()`（`proxy.rs:1245`）与
  `upstream_request` 全部指向它。`models_cache`（`lib.rs:99`）TTL 缓存 + 
  单飞刷新；`fetch_models` 已带 key 权限。
- **已有失败语义**：lane-level failover 只覆盖"429/5xx/connect error 时
  换 key"（`key-pool` 决策），不覆盖"端点全挂"。
- **探测零成本的思路**：`/v1/models` 刷新路径（`:1140`）本身就把一次请求
  打到上游——聚合长期成功/失败统计即可得到**端点的健康状态**，不需要额外
  心跳请求（不烧 RPM）。现状每次 TTL 到期只刷一个端点；改成按健康状态选
  主/备即可。
- **设计目标形态**：
  ```
  upstreams:
    - https://integrate.api.nvidia.com            # 主（默认）
    - https://selfhosted-nim.local:8080/v1        # 备（自建 NIM）
  ```
  顺序 = 优先级；列表变化可由 Settings 热更新（现有 rebuild 模式）。
- **与既有决策的关系**：`streaming-pipeline` 的 failover 语义（429/5xx 换
  key）不动；本任务在其上加"换端点"。`usage-injection` 等 pass-through
  不变量按端点无关（依然透传）。

## 设计决策

### D1：端点健康状态机（per-endpoint）
- `UpstreamState { url, alive: bool, last_try: Instant, failures: u32,
  last_success: Option<Instant> }`。
- 判定：`fetch_models` 或任一 `/v1/*` 实际请求对该端点的成功/失败都会
  计入（被动观测）；连续 2 次失败 → `down`；成功一次 → `up`。
- `down` 端点在 `cooldown = 60s` 内不选中（尝试探测间隔由 models TTL
  驱动，天然有节流）。

### D2. 选择算法（简单为主）
- 每次“需要选端点”时（首次请求、lane failover 该端点已 down）：
  - 若当前主端点 alive → 用它；
  - 否则按顺序第一个 alive；全 down → 用列表第一个（宁可错报 429 也
    不拒绝对外服务），并记录 `all_down` 事件。
- **请求级流派**：一个端点失败不会让同请求在另一端点重试同一 body？——会。
  因为 NIM 同 API 无状态、SSE 重放安全；但**要限制** 总重试次数（沿用
  现有 retry 计数，改成 per-endpoint 重试 1 次）。
- 写回 `nimproxy_upstream_endpoint_health` gauge 与
  `nimproxy_upstream_failover_total{from,to}` counter。

### D3. 配置
- Settings 增加 "Upstream endpoints" 段：有序 URL 列表（可加/删/排序/
  设为默认），兼容现有单一 `base_url`（迁移：一个 URL = 仅主端点）。
- 环境变量 `UPSTREAM_URL`（现有）保持单值；列表只走 Settings store。

### D4. 边界（不做）
- 不做跨上游的请求复制（仅故障转移，不并发）。
- 不做健康检查专用定时器（用现有 TTL 被动观测），保持零 RPM 预算。
- 不做 per-model 端点路由（按端点整体走）。

## 实施步骤

1. `config.rs`：`upstreams: Vec<String>`（默认 `[base_url]`），迁移逻辑。
2. 新模块（如 `src/upstream.rs`）：状态机 + select + 观测记录。
3. `proxy.rs`：`models()` 与 `upstream_request` 的 base_url 改用
   `select_upstream()`；
4. `settings.rs` + dashboard Settings UI（可排序列表编辑器）。
5. metrics + e2e：
   - a) 主端点 500 / connect error / 429 全挂 → 备选端点接管；
   - b) 恢复主端点 → 回切（按 alive 状态）；
   - c) 双端点都被 mock 拒绝 → `max_down` 事件 + 对外仍 503 而非 crash；
   - d) 单端点（默认）行为回归现状。
6. 文档：决策页 + index/log + CHANGELOG + README（配置说明）。
7. 验证门：fmt/clippy/全量测试。

## 风险与注意

- **限流预算**：不做主动 healthcheck---只复用 TTL 刷新与真实请求被动观测，
  与 models-cache 相同的“零成本”。
- **端点不可用时长**：全 down 时指标/日志要非常明显（dashboard Reliability
  加一个"端点"行）。
- **顺序即优先级**：UI 编辑要支持排序，避免"后端固定优先"的黑魔法。

---