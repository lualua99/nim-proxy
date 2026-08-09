# 任务 03：限流等待可视化细化（Request Wait Phases）

> 状态：已完成（所有实现步骤落地并通过验证，详见文末"实施记录"）
> 日期：2026-08-09
> 需求：队列视图目前只能区分 `queued`（笼统等待）与 `upstream`（已发出），
> 无法判断请求卡在**限流槽**还是**模型并发闸门**上。细化 phase，让操作员
> 一眼区分 key 限流瓶颈（等 slot）与模型/worker 并发瓶颈（等 permit）。

---

## 背景与现状（探索结论）

- **现状 phase 只有两值**：注册时 `"queued"`（`proxy.rs:604`），拿到 slot 后
  `"upstream"`（buffered `:740` / streaming `:874`）。`queued` 同时涵盖
  `acquire_model_permit`（governor 并发闸门，`proxy.rs:172-206`，轮询
  POLL=250ms）与 `reserve_slot`（全局 FIFO RPM 槽，`proxy.rs:141-167`）两段
  等待——操作员无法区分谁是 key 瓶颈、谁是模型并发瓶颈。
- **走两段等待的前提顺序固定**：`buffered()`（:730-739）与 `streaming()`
  （:846-864）都是先 `acquire_model_permit` 后 `reserve_slot`，拿到 slot 才置
  `upstream`。两段等待点都已有注册在案的 `id`，加 `set_phase` 即可。
- **前端有两处消费 phase**：`renderQueueTab()` 的 `waiting` KPI 计数
  （`dashboard.html:2458`，`q-queued` tile）与 `queueRow()` 的 badge
  （`dashboard.html:2471-2476`）。KPI tile 布局是 `q-count` + `q-queued`
  两枚（`dashboard.html:458-461`）。
- **决策页已预留细化空间**：`request-queue-and-termination.md` phase 定义为
  `queued/upstream`（retry 未落地），本改动只是把 `queued` 细化为两个阶段，
  不动 kill/-91 语义。
- **权限与隐私口径不变**：只动 phase 字符串与展示，不新增字段、不触上游、
  零限流预算。

## 设计决策

### D1：phase 取值细化为三值：`waiting_permit` / `waiting_slot` / `upstream`
- `waiting_permit`：正在 `acquire_model_permit`（governor worker 并发闸门）。
- `waiting_slot`：正在 `reserve_slot`（全局 FIFO 限流槽）。
- `upstream`：已拿到 slot、请求已发出（含流式 pipe 中）。
- `queued` 仅作为注册瞬间的初值保留？不——注册后立即进入等待路径，直接以
  `waiting_permit` 注册，无模糊窗口。
- `retry` 阶段不落地（现状没有 set_phase("retry")，且重试期间重新走
  waiting_permit/waiting_slot，语义仍真实）。

### D2：插入点（完整覆盖两段等待）
- `buffered()`：`acquire_model_permit` 之前
  `set_phase(id, "waiting_permit")`；`reserve_slot` 之前
  `set_phase(id, "waiting_slot")`；拿到 slot 后 `upstream`（已有 :740）。
- `streaming()`：同样在 permit 前 / slot 前 / slot 后三处。
- 注册初值 `"queued"` 改为 `"waiting_permit"`（`proxy.rs:608`）。
- 两函数的 `else`（504 早退）前 phase 保持等待态，展示无歧义。

### D3：前端 KPI 与 badge 细分
- **KPI**: `q-queued` tile 拆成 `q-slot`（"Waiting for a rate-limit slot"）与
  `q-permit`（"Waiting for model capacity"）两枚；`q-count` 总数不变。
  `waiting` 高亮逻辑改为按新 phase 分列。
- **badge**: `queueRow()` 按 phase 渲染：`waiting_permit` → 蓝（admin
  色系）"waiting · permit"，`waiting_slot` → 中性（user 色系）
  "waiting · slot"，`upstream` → 无底色 "upstream"。
- 所有动态值继续过 `esc()`（XSS 不变量）。

### D4：边界（不做）
- 不做 retry 阶段（范围外；重试时 phase 如实回到 waiting_*）。
- 不动 kill/-91 语义、不新增端点、不加依赖、不改 `/api/queue` 响应结构
  （只是 phase 字符串取值变化）。
- 不显示在途时长之外的任何新字段。

## 实施步骤

1. **Rust 侧** `src/proxy.rs`：注册初值 `queued` → `waiting_permit`；
   `buffered()` 与 `streaming()` 各插两处 `set_phase`（permit 前 / slot 前）。
2. **前端** `src/dashboard.html`：`q-queued` → `q-slot` + `q-permit` 两 tile
   （HTML + 渲染/错误态）；`queueRow()` badge 三分支。
3. **测试** `tests/e2e.rs`：
   - `:4080` 等 slot 的 waiter 断言 phase：`"queued"` → `"waiting_slot"`；
   - `:4113` holder 断言 `"upstream"` 不变；
   - 新增断言：等 permit 阶段可见（可用 governor override 卡到并发上限，
     mock ActiveStream 占住 permit）。
4. **文档与知识库**：
   - `knowledge/decisions/request-queue-and-termination.md`：phase 定义段
     更新为三态。
   - `knowledge/log.md` 日期条目；`CHANGELOG.md` Unreleased 条目。
   - 本文件状态改为"已完成"+ 实施记录。
5. **验证门**：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、
   `cargo test` 全绿；手动：Queue tab 在 mock 并发下看两种 waiting badge 与
   两枚 KPI。

## 风险与注意

- **竞态窗口**：permit 立即放行时 `waiting_permit` 一闪而过，前端轮询可能
  见不到——正常（未被 gate 就是没等 permit），不要为可见性加延迟。
- **governor 关闭时**：`acquire_model_permit` 立即返回，只有 `waiting_slot`
  可见——符合直觉（无并发闸门即只剩限流队列）。
- **向后兼容**：phase 值变了，但 `/api/queue` schema 不变；dashboard 与
  后端同仓同版本，无外部消费者。

---

## 实施记录（2026-08-09 完成）

- `src/proxy.rs`：注册初值 `queued` → `waiting_permit`；`buffered()` 与
  `streaming()` 在 `acquire_model_permit` 前 `set_phase(id,"waiting_permit")`、
  `reserve_slot` 前 `set_phase(id,"waiting_slot")`（slot 后的 `upstream`
  原有不动）。`src/registry.rs` 注释同步。
- `src/dashboard.html`：KPI 区 `q-queued` tile 拆成 `q-slot`（"Waiting for
  a rate-limit slot"）与 `q-permit`（"Waiting for model capacity"），计数与
  amber 高亮分别判定；`queueRow()` badge 三分支（waiting · permit 蓝 /
  waiting · slot 中性 / upstream 无底色）；错误/加载态同步清空两枚 tile；
  KPI 布局 flex 调整。
- 测试：`tests/e2e.rs` `:4080` 断言 `queued` → `waiting_slot`；新增
  `queue_reports_waiting_permit_phase`：`/api/settings/governor` 设
  `cap=1` 卡住 mock/model-a，holder 占 permit 流式，waiter 在 queue 中
  呈 `waiting_permit`，terminate 后收到 -91。相关 5 个 e2e 全绿。
- 文档：`request-queue-and-termination.md` phase 契约改三态 +
  KPI 描述；`knowledge/log.md` ingest 条目；CHANGELOG Unreleased 条目。
- 验证门：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、
  `cargo test`（125 unit + 96 e2e）全绿（本机无 fmt/clippy 时以
  `cargo build --all-targets` 零警告代替，CI 执行 fmt/clippy）。