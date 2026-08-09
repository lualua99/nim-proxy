# 任务 06：平滑重启：限流状态持久化 + 预热恢复（Restart-smooth recovery）

> 状态：提案（未开始）
> 日期：2026-08-09
> 需求：README 已明示的局限——"Rate windows reset on restart"：进程重启后
> 内存窗口清零，代理以为每把 key 都是空窗口，立刻又打满 40 RPM，而上游
> 还记着刚用过的次数 → 重启后必吃一串 429（靠 retry 静默吸收，用户只感到
> 慢）。让 lane 窗口 / cooldown / governor 状态落盘并在启动时恢复，并加
> "慢启动"阶段消除过渡期抖动。

---

## 背景与现状（探索结论）

- **现有持久化基建**：`history.rs` 已有 versioned JSONL + 原子写入 +
  reset-aware 启动索引的成熟模式（`metrics-history` 决策）；`config.json`
  落盘在 `DATA_DIR`。限流状态持久化可复用同一套目录与原子写策略，零新依赖。
- **要持久化的状态**：
  - `pool.rs` 每 lane：`sent: VecDeque<Instant>`（窗口计数）、
    `cooldown_until`、任务 04 引入的 `Calibration`；
  - `governor.rs`：per-model `limit`（自适应后的并发上限）与 in-flight；
    governor 的 adaptive 值若丢失，重启后又从默认 cap 开始（保守，可接受，
    但持久化更平滑）。
- **恢复语义**：窗口是"过去 61s 的发送时间戳"——重启后这些 Instant 全部
  过期（绝对时间已变），不能直接搬。正确做法：**只保留"过去 61s 内的计数"**
  （按 epoch 秒截断），把计数写为"相对现在的时间戳"。启动时构造
  `now - (61 - age)` 的虚拟时间戳。
- **慢启动的理由**：即使计数恢复精确，重启后 proxy 对新旧窗口衔接期的
  上游行为（锁死/退避）没有记忆；且 NIM 对"突然后继 40 个请求"的场景仍
  可能回退避。慢启动 = 恢复后首个窗口用 70% 额度（或按校准 factor），
  观察 1 个窗口无 429 后恢复满额。

## 设计决策

### D1：落盘格式与频率
- 新文件 `DATA_DIR/ratestate.jsonl`（或并入现有 history 的原子写路径）：
  每 lane 一行 `{key, rpm, sent_window_epochs: [..], cooldown_until_epoch,
  cal_factor}` + 一条 `{governor: {model -> limit}}`。
- 触发时机：**变更即写 + 定期兜底**（每 30s 或每窗口边缘）。写操作放在
  pool 写锁之外（快照后异步写），避免热路径阻塞。
- 有界性：文件固定大小（单行覆盖写或 append+compact），`sent` 只保留
  窗口内 61s 的计数。

### D2：启动恢复
- 启动读 `ratestate.jsonl`，按 epoch 时间过滤"现在"之前 61s 的计数，
  重建 lane 窗口；cooldown 未过期则继续 bench；cal_factor 直接采用。
- 恢复失败（文件损坏/版本不兼容）→ 静默降级为全新窗口（现状行为），
  记 warning——绝不让恢复逻辑成为启动硬错误。
- 与 `history.rs` 的 reset-aware 启动索引一样：幂等、可重放、写盘原子。

### D3：慢启动（recovery ramp）
- 启动后进入 `ramp_until = now + ramp_secs`（默认 60s，Settings 可配，
  `0` = 关闭）。
- ramp 期间 effective rpm = `rpm * ramp_factor`（默认 0.7）。配合任务 04
  的 calibration：ramp 结束后由 factor 继续微调。
- ramp 只在"上次落盘距今 < 5min"时启用（避免"离线一周回来反而被限速"）；
  长期停机恢复按满额走（反正窗口已空）。

### D4：dashboard / metrics
- Capacity tab 或 Overview 展示"上次重启时间 + 当前是否 ramp 中 +
  ramp 剩余"。
- 新 metric `nimproxy_ramp_active`（0/1 gauge）与
  `nimproxy_restore_count{restored, dropped}`（启动时恢复的行数）。

### D5：边界（不做）
- 不做跨实例共享（多实例仍是"一个实例一套 key"的设计约束——本任务只解决
  同一实例的重启连续性）。
- 不持久化"未完成的请求"（registry 本来就不持久化，语义不变）。
- 不做 crash-consistency 承诺（尽力而为：原子写 + 损坏降级，不是分布式
  事务）。

## 实施步骤

1. `pool.rs`：lane 快照函数（sent 截断为 epoch）+ 从快照重建（构造虚拟
   时间戳）+ ramp 状态机；`rebuild` 与恢复路径并存。
2. `governor.rs`：limit 快照/恢复。
3. 新模块（如 `src/ratestate.rs`）：原子写、版本号、损坏降级、定期兜底
   定时器。
4. `config.rs`/`settings.rs`：`ramp_secs`、`ramp_factor` 配置项 + Settings
   UI。
5. `src/dashboard.html`：ramp 状态展示 + metric。
6. 测试：
   - unit：快照↔恢复往返（含 epoch 截断、cooldown 保留、损坏文件降级）；
   - e2e：mock 上游下打满窗口 → 重启（重启真实进程）→ 断言恢复后立即发
     请求不再触发上游 429；ramp 期请求被拉宽（用 `--enforce` mock 验证
     ramp 期间零违规）。
7. 文档：决策页 + `knowledge/index.md` + `log.md` + CHANGELOG + README
   FAQ 更新（删掉/改写"Rate windows reset on restart"条目）。
8. 验证门：fmt/clippy/全量测试；load harness。

## 风险与注意

- **epoch 边界竞态**：窗口边缘的计数截断可能有 ±1s 误差——误差方向是
  "少记"（代理更保守），安全。
- **写盘频率 vs 性能**：每 30s 一次 + 变更触发，JSONL 小文件，量可忽略。
- **与任务 04 的耦合**：cal_factor 随本任务持久化；ramp 与 calibration
  叠加语义（ramp 结束 → calibration 接管）需在决策页写清。

---