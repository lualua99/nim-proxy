# 任务 04：自适应车道校准（Self-calibrating lanes）

> 状态：已完成（所有实现步骤落地并通过验证，详见文末"实施记录"）
> 日期：2026-08-09
> 需求：NIM 的 ~40 RPM 不是硬 SLA，而是"模型 / 账号 / 流量类型相关"的软上限，
> 且存在账号级限额与 429 后的指数退避锁死。当前每把 key 的窗口宽度固定为配置的
> rpm，无法利用实测容量、也无法回避被限得更狠的 key 对整池的拖累。让 proxy 从
> 上游观测（429 + Retry-After + 锁死事件）自动校准每 lane 的实际安全吞吐。

---

## 背景与现状（探索结论）

- **上游事实（2026-07 论坛核实）**：40 RPM 是社区基准而非保证；限额取决于
  模型、用例、当前总流量；存在**账号级**限额（换新 key 也一样被限），触发后
  每次重试都会延长锁定期（指数退避）。模型不同，限额也不同。
- **现状**：`src/pool.rs` 每 lane 一个 `VecDeque<Instant>` 窗口 + `cooldown_until`
  。`reserve(prefer)` 只在 `sent.len() < rpm` 放行，rpm 来自
  `NimKey.rpm`（config store，默认 40）。bench（`penalize`）只处理"上游刚拒绝"
  的短时退避，不改变窗口宽度——容量假设一成不变。
- **窗口余量的机会**：若实测上限 > 配置 rpm，窗口有 隐藏空余（浪费容量）；若
  实测上限 < 配置 rpm，固定宽度必然持续吃 429（靠 failover 吸收，浪费其它 key）。
- **已有观测点**：`penalize`/`bench` 已记录 429 与 Retry-After；`retryable()`
  分支（`proxy.rs:766`）驱动 failover；metrics 已有 `lane_benched_total`
  （lane, status）。缺的是把这些事件折算成**长期容量估计**。
- **安全约束**：项目的核心承诺是"零上游违规"（load test 硬要求）。任何校准
  必须**保守**：只能降不能放（除非有充分证据），需要防抖与最大衰减下限。

## 设计决策

### D1：每 lane 一个长期"校准因子"（calibration factor）
- `Lane { sent, cooldown_until, cal: Calibration }`。
- `Calibration { factor: f64 (默认 1.0, 下限 ~0.6, 上限 1.0), events: VecDeque<(Instant, Signal)>, updated: Instant }`。
- 有效窗口宽度 = `rpm * factor`（低位取整）；**factor 只降不升快**，提升走
  慢速试探（每补测间隔观察一段时间无 429 才 +0.01）。
- 事件来源：`penalize(lane, 429/5xx/Retry-After)` 时 `degrade(factor *= 0.9, 下限
  0.6)`；连续 N 小时无事件时 `probe_up()` 试探性 +0.01。
- 429 的 `Retry-After` 绝对值参与 cooldown；锁死特征（连吃 2+ 个 429 且
  Retry-After >= 30s）一次性降 0.5 下限档。

### D2：校准只作用在"空转头"（admission），不影响已入窗口
- `reserve()` 的准入判断由 `sent.len() < rpm` 改为
  `sent.len() < effective_rpm(lane)`；已戳入窗口的 slot 不清零——校准只改
  准入率，不改已批准承诺。

### D3：dashboard / metrics 暴露实测容量
- Capacity tab 新增每 lane "实测吞吐（近期 N 分钟）vs 校准额度" 仪表；新增
  metric `nimproxy_lane_calibration`（lane, 值=factor 或 effective rpm），
  `lane_benched_total` 已有。
- Settings 可控性：提供"手动锁死某 lane 校准值"或"关闭自适应（固定 rpm）"
  开关，默认开。

### D4：边界（不做）
- 不做账户级限额探测（无法也不该主动打探他人账户）。
- 不做跨 key 容量迁移（那是 dispatcher/轮询的范畴）。
- 不做扩扩容：factor 上限 1.0——**绝不 超过**配置 rpm，只纠正过度/不足，
  把"少用"让回 40，把"多用"拉回实测。

## 实施步骤

1. `src/pool.rs`：`Calibration` 结构 + `degrade/probe_up/snapshot`；`reserve`
   与 `try_take` 改用 effective rpm；`rebuild` 时按 key-string 保留校准状态。
2. `src/metrics.rs`（或 pool 内）：`lane_calibration` gauge。
3. `src/dashboard.html`：Capacity tab 仪表 + Settings 开关。
4. 测试：
   - unit：degrade 下限、probe 恢复周期、窗口数学（factor 换算）、rebuild
     保留；
   - e2e/load：mock `--enforce` 下，把 mock 的真实限流改成"40% 时开始
     429"，验证 proxy 自动降采样 → 后续零违规；恢复后试探回到 1.0。
5. 文档：决策页 + `knowledge/index.md` + `log.md` + CHANGELOG。
6. 验证门：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、
   `cargo test`；load harness 零违规。

## 风险与注意

- **保守性**：降得快、升得慢；上限 1.0 封死，"校准"不是"加速"。
- **与 benching 的关系**：bench(10s) 是短时惩罚，calibration 是长期容量——
  两个维度并存，不互相覆盖。
- **观测粒度**：429 事件量小（本来就少），需要较长观测窗才有统计意义；初期
  以分钟级聚合，thresholds 需要 load test 调参。

---

## 实施记录（2026-08-09 完成）

- `src/pool.rs`（核心）：
  - `Calibration { factor, locked, last_event, consecutive_locks }`（每个
    `Lane` 挂 `calibration: Mutex<Calibration>`）；常数 `CAL_INITIAL=1.0`、
    `CAL_FLOOR=0.2`、`CAL_LOCKOUT_FLOOR=0.1`、`CAL_DECAY=0.9`、
    `CAL_LOCKOUT_FACTOR=0.5`、`LOCKOUT_RETRY_AFTER=30`、`PROBE_STEP=0.01`。
  - `observe(signal)`：429/5xx 每次 `factor *= 0.9`；`Retry-After >= 30s`
    计入连续锁死计数，每第 2 次再加乘 ×0.5 并置 `locked`（锁死下限 0.1）；
    锁死 flag 在静默期满的探针处清除。
  - `maybe_probe(now) -> bool`：`factor < 1.0` 且距上次事件满
    `PROBE_INTERVAL`（默认 3600s，`NIMPROXY_CALIBRATION_PROBE_SECS` 可调，
    `LazyLock` 惰性读取）时 +0.01 回升；探针本身记一次"事件"防止快冲。
  - `effective_rpm(rpm) = max(1, floor(rpm * factor))`——**永不归零**
    （0 会锁死 `reserve` 的窗口数学）。
  - `reserve()`/`try_take()` 全部按 `effective_rpm` 判定准入与窗口数学；
    `reserve` 每轮访问触发一次 `maybe_probe` 并回发 gauge。
  - `rebuild()` 按 key-string 保留 `Calibration`（与窗口/cooldown 同通道），
    重配置的 key 不丢上游教过的教训。
  - 新公共 API:`Pool::observe(lane, Signal)`（长期记忆,与 `penalize` 的
    短期 bench 分离）、`calibration_factors()`、`LaneStat.cal_factor`。
  - 新增 unit 测试:`observe_rate_limit_shrinks_admission`（1 次 429 后限到
    9）、`lockout_signature_cuts_far_below_floor`（4 次 30s 锁后 factor < 0.2
    且 ≥ 0.1、仍保留 1 个 slot）、`server_error_shrinks_without_lockout`、
    `rebuild_carries_calibration_for_kept_keys`。
- `src/proxy.rs`: `bench(slot, status, backoff, signal: Option<Signal>)`——
  真实 429/5xx 走 `upstream_signal()`(429 → `RateLimited{retry_after}`、
  5xx → `ServerError`)喂 `observe`;连接错误只 bench 不校准。新增
  `retry_after_secs`/`upstream_signal` 辅助;signal 在 `resp.text()` 消费前
  捕获(3 处调用点:非流式、流式、models 目录)。
- `src/settings.rs` + `src/dashboard.html`:key 行状态显示
  `in_window / effective rpm`(有效值 = `rpm × factor` 取整),factor < 1 时
  附 `×0.xx`;Lane meters 的 util 与 Now rpm 列改按 `laneEff(i)` 计算;每
  级 gauge `nimproxy_lane_calibration` 在收缩/探针变化时发布。
- 文档:`knowledge/architecture/key-pool.md` 增补 Calibration 章节、
  `knowledge/log.md` 2026-08-09 ingest 条目。
- 验证:`cargo check --all-targets` 零警告;`cargo test` 129 unit + 96 e2e
  全绿;load harness(`mock_nim.py --enforce`,100 客户端 × 3 请求):
  **0 失败、0 上游速率违规 PASS**(校准只降不升,冷启动 factor=1.0 行为与
  之前一致)。rustfmt/clippy 本机不可用(系统包工具链),CI 仍强制执行。

---