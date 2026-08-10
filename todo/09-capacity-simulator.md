# 任务 09：Capacity 模拟器（What-if simulator）

> 状态：已完成
> 日期：2026-08-10
> 需求：dashboard 已能看"历史利用率 + 现在饱和度 + 峰值短欠水"，但无法回答
> "如果我再加 2 把 key / 砍掉 3 个客户端 / 把某模型并发闸门放宽，排队延迟会
> 变成多少"。做一个纯前端计算 + 后端数据接口的 what-if 模拟器：把现有真实
> 观测（per-key 实测吞吐、request-shape 分布、governor 上限）喂进一个小的
> 排队模型，滑块即调即算。

---

## 背景与现状（探索结论）

- **原料全在**：
  - per-key rpm + 实测吞吐（任务 04 后更准）；
  - request-shape 指标（`request-shape-metrics` 决策：messages / tools /
    max_tokens / temperature 直方图，`nimproxy_request_*`）→ 可推每个
    client 的"请求速率 × 时长"分布；
  - `queue_wait_seconds`、`ttft`、`tokens_per_second` 等历史（
    `metrics-history`）→ 参数化排队模型（M/M/c 或简单流体模型）的输入；
  - governor cap（`nimproxy_model_limit` / `model_inflight`）→ 并发闸门
    的瓶颈项。
- **现状缺口**：没有任何"预测/外推"能力；Capacity tab 只回放历史。
- **技术选型约束**：无前端构建步骤（dashboard.html 单文件内嵌），模拟器
  必须纯 JS + 后端少量参数接口，不能引图表库（chart 已自绘）。

## 设计决策

### D1：模拟器形态
- Capacity tab 新增 "What-if" 区：滑块/输入组：
  - key 数（每把 rpm 可调）
  - 客户端数 / 每客户端请求速率（或从观测自动取"当前值"）
  - 模型并发闸门 cap（governor 覆盖值）
  - 每请求平均处理时长（从观测取，或手动覆盖）
- 输出（即时计算，纯前端）：
  - 预期平均排队延迟 / P95
  - 预期吞吐（RPM 上限）
  - 瓶颈标识（key 窗口 / 模型闸门 / 客户端请求速率）
  - 对比当前实际（把"现在"也画成一条基线）

### D2. 后端数据接口
- 新增 `GET /api/dashboard/capacity-model`（session 保护，admin？不——
   普通 user 也能看 Capacity tab，保持和现有 tab 权限一致）：
  返回 `{ per_key: {rpm, observed_rpm}, per_client: {rate, msg_dist},
  per_model: {cap, inflight}, history_sample: queue_wait p50/p95 }`。
- 数据全部来自现有 registry/metrics/history 的聚合——**不存任何新字段、
  不碰内容**（隐私不变量）。

### D3. 模型
- 优先用**流体/确定性模型**（不引入随机排队理论依赖）：吞吐 = min(总
  rpm 上限, 模型闸门上限, 客户端请求速率)；延迟 = 队列长度 / 服务速率，
  服务速率用观测的每请求上游时长。复杂排队理论作为后续可选项。
- 前端实现：~100 行纯 JS，复用现有 `fmt()`/tile 样式，无新依赖。

### D4. 边界（不做）
- 不做蒙特卡洛/随机模拟（第一版确定性即可，文档注明局限）。
- 不做模拟结果持久化（每次实时计算）。
- 不做"应用模拟结果"（只是咨询工具，不写回任何配置）。
- 不做按模型细分的延迟分解（第一版聚合级别，后续可加）。

## 实施步骤

1. `src/lib.rs`/`metrics.rs`：`/api/dashboard/capacity-model` 端点（聚合
   现有 metrics + history）。
2. `src/dashboard.html`：Capacity tab 新面板 + 滑块 + 实时计算（纯 JS）。
3. 测试：
   - e2e：端点鉴权（session 要求）、返回结构稳定（空数据时不 500）；
   - unit：模型数学（rpm 上限 vs 并发闸门 vs 客户端速率 的三取小）、
     边界（key=0 → 不崩溃提示）。
4. 文档：决策页（模型选择 + 局限）+ index/log + CHANGELOG。
5. 验证门：fmt/clippy/全量测试。

## 风险与注意

- **模型过度承诺**：文案必须写"近似模拟，用于趋势判断，不是 SLA 承诺"；
  与现有 "estimate/not promise" 口径一致。
- **隐私口径**：接口只导出聚合计数，无任何 per-request 内容字段。
- **与任务 04/05 联动**：实测吞吐与调度策略会改变服务速率——模拟器参数
  应取"最近观测值"而非静态配置，否则模拟与现状漂移。

---