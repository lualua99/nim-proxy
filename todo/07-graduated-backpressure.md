# 任务 07：渐进式背压：排队 ETA + 带 Retry-After 的拒绝（Graduated backpressure）

> 状态：已完成
> 日期：2026-08-09
> 完成日期：2026-08-10
> 需求：现在的熔断是"冷"的——`max_inflight` 超限直接 503
> （`proxy.rs:505-519`，`overloaded()` 在 `:1380`，无重试建议），而限流队列
> 里排队的人只有心跳、没有任何 ETA 信息。把"排队"与"拒绝"变成一条梯度：
> 等得起就等（并让客户端知道要等多久），等不起就 503 + 真实的
> `Retry-After` 建议，agent 自行退避而不是死等或立即撞墙重试。

---

## 背景与现状（探索结论）

- **现状两个极端**：
  1. `max_inflight` 熔断（`proxy.rs:505-519`）→ `overloaded()`（`:1380`）
     冷 503，无 `Retry-After`；
  2. 限流队列（`reserve_slot`）→ 心跳 + 无限等待到 `wait_deadline`，
     客户端无从知道排队多久。
- **ETA 可算**：队列数学已全在手上——lane 窗口的 `sent` 时间戳（能算出
  多少秒后可放行）、队内顺序（dispatcher FIFO）、`max_wait` 截止。
  每个排队请求的等待时间 = 排在其前的窗口释放时间，可精确到秒。
- **已有的 deadline 通路**：`RequestDeadline`（`proxy.rs:41`）与 dispatcher
  的 `acquire(deadline, …)` 齐备——ETA 就是"从 queue head 数 slot+窗口"。
- **Error-code 协议约定**：现有错误信封用 `code` 字符串（如 `-91`、
  `deadline_exceeded`）；新增可辨别的 code（如 `backpressure`）不破坏
  现有 harness。
- **非流式无心跳**（已知局限）：非流式 buffered 请求无法边等边发 heartbeat
  （`buffered()`，`proxy.rs:714`）——它最需要"拒绝+Retry-After"兜底路径。

## 设计决策

### D1：ETA 计算器（dispatcher 内）
- `dispatch.rs` 新增 `pub fn eta(&self, prefer: Option<usize>) -> Option<Duration>`：
  从 lane 窗口 timestamp 计算"最短要等多久才有空槽"（取 prefer lane 与
  最空闲 ready lane 的较快者；无槽则按窗口内最早释放时间估计）。
- 纯只读，无锁竞争化；O(1)~O(lanes) 计算，不进热路径（每请求一次即可）。
- 大队列时的保守性：ETA 只作为"建议值"，不承诺——文档要写明是 estimate
  （排队中释放会动态变化）。

### D2. 服务端响应层级（三个梯度）
1. **排队 < 阈值**（`backpressure_eta_threshold` 默认 20s）：现状行为
   （心跳 / 静默排队），但流式 200 响应头推送
   `X-Nim-Proxy-Eta: <secs>`（响应头在第一次 SSE 帧前可发）。
2. **排队 >= 阈值 且无显式 deadline**：拒绝而非排队——
   `429`（语义让 harness 走通用退避）或 `503`？——选 **503**（服务暂时
   不可用，与 `max_inflight` 一致）+ `Retry-After: <eta_secs>` +
   错误为 `{"code":"backpressure","message":"queue full; retry after N s"}`，
   支持非流式与流式（流式在尚未 200 提交前即可拒绝）。
3. **有显式 deadline 的请求**：不丢进背压路径；继续排队（EDF 下优先），
   由 deadline 自己兜底——背压拒绝会让 deadline 请求失去唯一可行路径。

### D3. 可配置性
- Settings 新增：`backpressure_enabled`（默认关——先落地再默认开启的
  分两步）、`backpressure_queue_threshold_eta_secs`（默认 20）、
  `Retry-After` 是否发送（跟随 enabled）。

### D4. 前端 / metrics
- 新 metric `nimproxy_backpressure_total{kind: eta|reject}` 与
  `nimproxy_rejected_eta_seconds`（histogram）。
- Overview/Reliability：小型 KPI 展示"估算等待 vs 实际等待"精度（ETA 的
  校准指标）。

### D5. 边界（不做）
- 不做全量非 SSE 能力（无需流式也能拒绝——本功能不依赖 SSE）。
- 不做 `X-Nim-Proxy-Eta` 的强制承诺（非 SLA）。
- 不做队列容量上限 + 溢出丢弃（与 max_inflight 正交；本任务只多一个
  "排队太久就建议重试"）。

## 实施步骤

1. `dispatch.rs`：`eta()` 函数 + unit 测试（空队列、满 lane、混合）。
2. `proxy.rs`：`reserve_slot` 前检查背压（`backpressure_enabled`）→ 503 +
   `Retry-After` 响应；streaming 路径在 200 提交前拦截（`:846-864` 前）。
3. `config.rs`/`settings.rs`：三个配置项 + Settings UI。
4. metrics + dashboard KPI。
5. e2e 测试：
   - a) mock 打满 → 无 deadline 请求收到 503+Retry-After≈当时 ETA；
   - b) 有 deadline 请求不受背压影响；
   - c) 阈值内请求保持心跳行为，`X-Nim-Proxy-Eta` 头在流式响应中出现；
   - d) 关闭 backpressure 行为回归现状（回归基线）。
6. 文档：决策页 + index/log + CHANGELOG。
7. 验证门：fmt/clippy/全量；load harness（mock `--enforce` 下背压开启后
   零违规且 100 客户端无死等超时）。

## 风险与注意

- **ETA 偏差**：只要调度顺序有优先级（任务 05 的 Fair/EDF），ETA 也只是
  估计；文档明示"估算，非承诺"。
- **429 vs 503**：拒绝语义避免与 upstream 429 混淆，守住"客户端只见 200
  心跳"的主承诺 —— 背压是**代理层**的反馈，不冒充上游。
- **与 isolate 相关**：背压与 deadline 的交互必须先于 205 的 fault-less
  桥接（无 deadline 的大等待者）。

---

## 实施记录

### 2026-08-10 — 全部完成

**核心代码**：
- `src/proxy.rs`：排队前检查背压（`backpressure_enabled`，默认关，Settings → Server 实时开关）——估算队列等待超过 `backpressure_queue_threshold_eta_secs`（默认 20s）且无显式 deadline 的请求，在加入队列前即以 `503` + `Retry-After: <secs>` + `{"code":"backpressure"}` 拒绝；低于阈值的保持心跳行为，流式响应推 `X-Nim-Proxy-Eta: <secs>` 信息头。带 `X-Nim-Proxy-Deadline-Ms` 的请求豁免（已有绑定边界）。
- `src/config.rs`/`src/settings.rs`：`backpressure_enabled`、`backpressure_queue_threshold_eta_secs` 配置项与 Settings UI。
- metrics：`nimproxy_backpressure_total{kind=eta|reject}`、`nimproxy_backpressure_rejected_eta_seconds`；Reliability 页新增"Backpressure (503)"行。
- 决策页 `knowledge/decisions/graduated-backpressure.md`（ADR）；`knowledge/index.md` 与 `knowledge/log.md` 更新；CHANGELOG [Unreleased] Added 条目。
- 验证：fmt/clippy 干净，全量测试（164 lib + 111 e2e）通过。

---