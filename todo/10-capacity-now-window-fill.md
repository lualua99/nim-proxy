# 任务 10：Capacity used · Now 环图改为窗口占用率（Window fill）

> 状态：已完成
> 日期：2026-08-10
> 实施记录：see `knowledge/log.md` [2026-08-10] ingest — window-occupancy ring gauge
> 需求：dashboard Overview 的 "Capacity used · Now" 环图在低流量下严重虚高——只发 1 个请求
> 就显示 50% 占用。根因是环图用 3 秒轮询差值的瞬时速率外推 RPM，而非窗口实际占用。

---

## 背景与现状（探索结论）

- **环图计算**（`src/dashboard.html`）：
  - `nowRpm` 由两次 `/api/dashboard/now` 轮询（3s 间隔）的 `nimproxy_requests_total`
    计数器差值经 `rate()` 外推（`dashboard.html:823-826`，`(cur-prev)/((curT-prevT)/60000)`）。
  - `capacity = poolCapacity()`（`:820`）= `cfg.capacity_rpm ?? cfg.lanes * cfg.rpm`。
  - 环图 `capRatio = rpmNow / capacity`（`:1317`），副标题
    `"${fmt(rpmNow)} / ${fmt(capacity)} rpm · N lanes"`（`:1395`）。
- **问题复现**：1 把 key（40 RPM），3 秒内发 1 个请求 → 1/(3/60) = 20 RPM → 20/40 = **50%**。
  单次请求被放大成 20 RPM 的投影，窗口实际只用了 1/40 槽位。
- **后端已有精确数据**：`Pool::lane_stats()`（`src/pool.rs:388-412`）返回 `LaneStat.in_window`
  = 61 秒窗口内实际发送计数（`sent.iter().filter(|t| now - **t < WINDOW).count()`），
  以及 `effective_rpm`（经校准/ramp 后的实际准入额度）。`api_dashboard_now`
  （`src/lib.rs:274`）目前只回 `lanes`/`rpms`/`capacity_rpm`，**未回 per-lane 窗口填充**。
- **RPM 指标仍有用**：历史趋势图（`o-traffic`）与 "Requests now" 文字（`o-reqnow`）都依赖
  `rpmNow`，属"吞吐量"语义，保留不动；环图应改用"窗口占用率"语义。

## 设计决策

### D1：指标分工（两个指标各司其职）
- **`rpmNow`（瞬时 RPM，现有）**：继续喂历史趋势图 + "Requests now (Z rpm)" 文字。
  —— 吞吐量语义，不受影响。
- **`windowFill`（窗口占用，新增）**：喂环图比率 + 副标题。数据源 = 后端
  `pool.lane_stats().in_window` 之和，与轮询间隔、瞬时速率完全解耦。
  —— 窗口真实填充率语义，解决低流量虚高。

### D2：后端扩展 `api_dashboard_now`（`src/lib.rs`）
- 响应新增：
  - `per_lane: [{ key, in_window, effective_rpm, rpm }]`（来自 `pool.lane_stats()`）——
    为后续"逐 lane 窗口占用"预留，当前环图只用聚合。
  - `window_fill: usize` = `Σ in_window`（所有 enabled lane）。
  - `window_capacity: usize` = `Σ effective_rpm`（每 lane 校准后的实际准入额度，
    `max(1, floor(rpm*factor))`，与 `pool::try_take` 的准入口径一致）。
- 复用 `pool.lane_stats()`，零新增锁、零上游、零限流预算。

### D3：前端环图改用窗口占用（`src/dashboard.html`）
- `applyNowConfig()` 读取 `window_fill` / `window_capacity`（存 cfg 或独立变量）。
- `render()` 环图：
  - `capRatio = window_capacity > 0 ? window_fill / window_capacity : 0`。
  - 副标题改为 `"${fmt(window_fill)} / ${fmt(window_capacity)} slots · N lanes"`。
  - `capColor` 阈值（0.9 红 / 0.7 琥珀）逻辑不变。
- `o-reqnow` 文字继续保持 `"${fmt(rpmNow)} rpm now"`——RPM 信息不丢，仅从环图移走。
- 空态/无请求时 `window_fill = 0` → 环图 0%，不再虚报。

### D4：边界（不做）
- 不改 `pool.rs` 逻辑（只读 `lane_stats()`，不新增方法）。
- 不改历史趋势图 / `o-reqnow` 的 RPM 语义。
- 不做 per-lane 单独环图（v1 只做聚合口径，`per_lane` 字段先给数据）。
- 不改 `capacity_rpm` 在场的历史 rollup 口径（`history.rs` 不动）。

## 实施步骤

1. **Rust 侧** `src/lib.rs`：`api_dashboard_now` 调 `pool.lane_stats()`，
   聚合 `window_fill` / `window_capacity`，响应加 `per_lane`、`window_fill`、
   `window_capacity`。
2. **前端** `src/dashboard.html`：
   - `applyNowConfig()` 读新字段；
   - `render()` 环图 `capRatio` 与副标题改用 window fill；
   - `o-reqnow` 文字保留 RPM。
3. **测试** `tests/e2e.rs`：
   - `api_dashboard_now` 返回 `window_fill`/`window_capacity`/`per_lane` 且结构稳定；
   - 低流量（1 请求）→ `window_fill` 反映真实窗口计数而非速率外推；
   - 空态 `window_fill = 0`。
4. **文档与知识库**：
   - 若无对应决策页，决策页并入 dashboard 架构页（`knowledge/architecture/dashboard.md`）
     或新增小节；`knowledge/log.md` 日期条目；`CHANGELOG.md` Unreleased 条目。
   - 本文件状态改"已完成"+实施记录。
5. **验证门**：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、
   `cargo test` 全绿；手动：发 1 个请求看环图 ≈ 0%，打高流量看接近满。

## 风险与注意

- **口径一致性**：`window_capacity` 用 `effective_rpm`（校准后）而非配置 `rpm`，
  与 `try_take` 准入一致，避免"校准后能放行但容量显示虚满"。文档要写清。
- **与 ramp 的交互**：ramp 期间 `effective_rpm` 已含 ramp_factor，`window_capacity`
  自动反映慢启动额度，无需额外处理。
- **向后兼容**：新增字段，dashboard 与后端同仓同版本，无外部消费者破坏。