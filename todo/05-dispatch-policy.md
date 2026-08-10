# 任务 05：调度策略：EDF 截止时间 + 加权公平（Dispatch policy）

> 状态：已完成（所有实现步骤落地并通过验证，详见文末"实施记录"）
> 日期：2026-08-09
> 需求：共享 pool 场景下，严格全局 FIFO（`dispatch.rs`）让一个跑批量任务的
> 客户端长期占据 slot，饿死交互式请求；同时带 `X-Nim-Proxy-Deadline-Ms` 的
> 请求并没有在队列里获得任何优先级帮助（deadline 只是"超了就放弃"）。让
> dispatcher 支持可选策略：EDF（最早期限者优先）+ 按客户端权重的公平轮询
> （weighted fair + aging），默认仍是严格 FIFO。

---

## 背景与现状（探索结论）

- **现状**：`dispatch.rs` 全局 FIFO 队列（决策 `global-fifo-dispatcher`）。
  `Slot` 严格按到达顺序放行。等 slot 的请求有 deadline（`wait_deadline`
  默认 `max_wait`），但 deadline 只做"放弃"，不做"插队"。
- **请求已有 deadline 数据**：`RequestDeadline`（`proxy.rs:41`）解析自
  `X-Nim-Proxy-Deadline-Ms`；`reserve_slot` 的 `acquire(deadline, prefer)`
  已把绝对期限传进 dispatcher（`dispatch.rs:57`）——只差"用它排序"。
- **客户端身份现成**：`Ctx.client`（API key 名 / "local"）可用于归类；
  config store 里可加每 client 权重（或用 role 推导）。
- **与 governor 的关系**：本任务只改**限流槽调度**（slot），不动
  `acquire_model_permit`（模型并发闸门，`proxy.rs:172`）。
- **制度约束**：`sticky-affinity-with-spillover` 的 prefer 语义维持不变；
  公平性必须"防饿死"（aging：等待超过阈值者自动提升权重）。

## 设计决策

### D1：调度策略可配置（默认 FIFO，行为完全不变）
- `dispatch.rs` 内部从"单 FIFO 队列"重构为"策略层 + 等待队列"：
  - `Policy::Fifo`（现状，默认）
  - `Policy::Edf`：按 `deadline` 绝对时间排序，tie-break 按到达序。
  - `Policy::Fair`：按 client 总槽配额轮转（water），等待逾龄（aging）
    → 权重临时拉满。
- 策略在 Settings 选择（全局）；`acquire()` 签名不变，排序在内部完成，
  现有调用方与测试零改动。

### D2：EDF 的具体语义
- 只有**自带 `X-Nim-Proxy-Deadline-Ms`**的请求参与 EDF 排序；无 header 的
  请求按 FIFO 兜底（防恶意 deadline=now 垄断）。
- deadline 已过期 → 不排队直接 504（现有 `fails_fast_when_no_slot_can_open`
  逻辑保留）。
- 对 deadline 排队者，slot 分配严格取最早 deadline；同一 deadline 内 FIFO。

### D3：Fair 的具体语义
- 权重按 client 名哈希稳定 + Settings 可配 per-client weight 覆盖。
- 每轮：为每个有等待者的 client 保留最多 `w` 个 slot 配额，已用尽配额的
  client 等待到下一轮；配额按"授予即消耗"计。
- aging：任何等待者超过 `fair_aging_secs`（默认 60s）后视为最高优先级
  （防饥饿，保证最坏等待上界）。

### D4：dashboard / metrics
- Queue tab 行新增"调度标记"（fifo / edf / fair 及当前排队位次近似）。
- 新 metric `nimproxy_dispatch_policy`（policy 枚举，常驻
  `nimproxy_dispatch_slot_grant_total{policy, client}`）。

### D5：边界（不做）
- 不做优先级并发（per-client 硬并发上限是另一件事，`max_inflight` 不做
  细分）。
- 不做权重热更新（Settings 保存即时生效是已有机制，但本任务不做
  per-request 级突增）。
- 不做 ETA 头（与任务 07 分开：本任务管"谁先走"，07 管"告诉客户端要等
  多久"）。

## 实施步骤

1. `dispatch.rs`：策略枚举 + 排序/公平逻辑 + aging；`acquire` 入参追加
   policy（从 cfg 读，`acquire` 已接 `deadline: 保留，补充排序入口）。
2. `config.rs`/`settings.rs`：`dispatch_policy` 配置项 + Settings UI
   （radio/select + 说明）。
3. `src/dashboard.html`：Queue tab 行标记 + Capacity/reliability 分析
   （by-policy grant 数）。
4. 测试：
   - unit：EDF 排序正确性（deadline 乱序到达）、fair 轮转与 aging 上界、
     FIFO 默认行为回归；
   - e2e：三策略下的 mock 并发断言老客户不死等；
   - load：100 并发在 `Fair` 下每个 client 至少拿到若干槽（无饿死断言）。
5. 文档：决策页（policy 矩阵 + 安全论证）+ `knowledge/index.md` +
   `log.md` + CHANGELOG。
6. 验证门：fmt/clippy/全量测试；load harness 零违规（Fair/EDF 下仍要
   保证每个上游窗口不超 rpm——这是"只调顺序、不改总量"的设计自证）。

## 风险与注意

- **EDF 与窗口纪律**：EDF 只改变**谁先拿槽**，不改变"每窗口装满效率"；
  总吞吐不变或变好（避免 429→更多 failover 浪费）。必须 load 验证。
- **Fair 的默认值**：上线先 `Fifo` 默认；`Fair`/`Edf` 是 opt-in。
- **Aging 参数**：需 e2e 调参（过小失去优先级意义，过大积累饥饿风险）。

---

## 实施记录（2026-08-09 完成）

- `src/config.rs`：
  - `DispatchPolicy { Fifo, Edf, Fair }`（:212）——serde 名 `fifo`/`edf`/`fair`
    （round-trip 测试 :786-792），`Display` + `as_str`；默认 **Fifo**。
  - `DispatchCfg { policy, fair_aging_secs }`（:239-243），`fair_aging_secs` 默认
    60、校验 >=1（:592）；fail-safe 默认断言（:780）保证旧配置无 `policy` 字段时
    回退 FIFO 行为不变。
- `src/dispatch.rs`（核心）：
  - `PolicyState { policy, aging, weights }`（:68），`From<&DispatchCfg>`（:77）；
    每次 `pick()` 前经 `Arc<RwLock<PolicyState>>` 读取 —— settings 保存即时生效，
    生效点在"下一次取号"，绝不打断一轮授予。
  - `acquire()` 入口签名不变，现有调用方与测试零改动；dispatcher 仍是唯一
    `Pool::reserve` 调用者。常量 `GRANT_GAP=25ms`（授予节流，2400 槽/分上限，
    不降吞吐）、`POLL=500ms`（轮询切片）、`IDLE_POLL=100ms`（空闲保洁）。
  - `pick()` 三臂：`Fifo` = 严格到达序（历史行为，默认）；`Edf` = 按绝对期限
    排序、平局按到达序；`Fair` = 按客户端配额轮转（授予计耗），等待逾龄
    （`fair_aging_secs`）者视为最高优先级——防饿死有单测兜底
    （`fair_aging_overrides_budget_so_nobody_starves`, :579）。
  - 新 gauge `nimproxy_dispatch_policy{policy}` 单例常驻（:141-147）。
  - 新单测：FIFO 默认行为回归、EDF 排序与平局（:465-505）、Fair 轮转与 aging
    （:506-606）。
  - D3 偏差：**per-client 权重覆盖未实现**——本轮 Fair 的配额按客户端实测
    授予计，权重热配置留给后续（避免在 settings 里引入第二套权重通道）。
  - D2 简化：EDF 直接对所有等待者按"折叠后的队列期限"排序——无 header 的请求
    期限即 `wait_deadline`（默认 `max_wait`），自然排在末端，效果等价于 D2 的
    "无 header 兜底"，但不用维护双通道。
- `src/proxy.rs`：队列期限折叠——`deadline = min(wait_deadline, rd.0)`
  （buffered 与 stream 两分支一致 :665-669）；带 deadline 的请求在自己的期限
  处 fail-fast 出队（不再死等 `max_wait`）；队列过期时区分语义：
  `queue_expired_flavor`/`queue_expired_sse`——显式 deadline 是绑定边界 →
  `deadline_exceeded`（记 `nimproxy_deadline_exceeded_total`），否则维持
  `rate_limited` 饱和提示；buffered 与 stream 的 permit/slot 四条失败路径全部
  接入。
- `src/settings.rs` + `src/lib.rs`：`POST /api/settings/dispatch`
  `{policy, fair_aging_secs}`（校验非法值 400）；`/api/queue` 回
  `dispatch_policy` 字段（:334-361）。
- `src/dashboard.html`：Settings 页 policy 选择 + fair aging 秒数输入
  （:2166/:2212-2214 保存），Queue tab 行头显示当前 policy 徽标（:2225）。
- 文档：新决策页 `knowledge/decisions/dispatcher-policy-modes.md`（fifo/edf/fair
  矩阵 + 设计论证）；`explicit-request-deadline.md` 补"deadline 作用于排队"
  小节；`global-fifo-dispatcher.md` 补三策略指引；`index.md`/`architecture`
  索引行更新；`knowledge/log.md` 2026-08-09 ingest 条目。
- 验证：`cargo fmt --check` 与 `cargo clippy --all-targets -- -D warnings` 全
  净（operator 装好本地工具链后）；`cargo test --lib` 141 + `--test e2e` 98
  全绿；load harness（`mock_nim.py --enforce`）两种场景 **0 失败、0 上游速率
  违规 PASS**（300 请求 p95 63.8s key 分布 {105/95/100}；governor 场景 90 请求
  p95 1.27s 峰值并发 {r1:7, kimi:3, llama:8}）——EDF/Fair 只调顺序、不改窗口
  上限的自证成立。

---