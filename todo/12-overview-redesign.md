# 任务 12：Overview 界面重新布局（现代 SaaS 极简风）

> 状态：已完成
> 日期：2026-08-10
> 需求：Overview 界面视觉风格从"灰卡片平铺"升级为 Vercel/Linear 式现代 SaaS
> 极简风，提升颜值和信息密度。

---

## 视觉设计稿

```
┌─────────────────────────────────────────────────────────────────┐
│  Overview                                          [range pills]│
│                                                                  │
│  ┌──────┐  ┌──────┐  ┌──────┐  ┌──────┐  ┌──────┐  ┌──────┐  │
│  │ 448K  │  │ 1.2M  │  │ 99.8% │  │ $142  │  │  12  │  │  3   │  │
│  │ Req   │  │ Tok   │  │ Suc   │  │Saved  │  │Active│  │Queue │  │
│  │ ▁▂▃▅▇ │  │ ▁▃▂▄▆ │  │ ▆▇▇▇▇ │  │ ▁▂▃▅▇ │  │  now │  │  now │  │
│  │ +5.2%  │  │ -1.1%  │  │ +0.1%  │  │ +12%  │  │      │  │      │  │
│  └──────┘  └──────┘  └──────┘  └──────┘  └──────┘  └──────┘  │
│                                                                  │
│  ┌───────────────────────────────┐  ┌─────────────────────────┐ │
│  │ Traffic (requests/min)  1.2K  │  │  Capacity   68% ██████  │ │
│  │  ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄   │  │  Success    99% ██████ │ │
│  │  ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄   │  │  ────────────────       │ │
│  │  ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄   │  │  Benches  0 ✓           │ │
│  │  ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄    │  │  Shed     0 ✓           │ │
│  │  09:00                now    │  │  401      0 ✓           │ │
│  └───────────────────────────────┘  │  Logins   0 ✓           │ │
│                                    └─────────────────────────┘ │
│                                                                  │
│  ┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐ │
│  │ TTFT  /min        │ │ Gen speed  /min  │ │ TPOT  /min       │ │
│  │  ████▲            │ │  ████▼           │ │  ████▲           │ │
│  │ ██  ██            │ │ ██  ██           │ │ ██  ██           │ │
│  │ p50/p95 迷你图    │ │ p50/p95 迷你图   │ │ p50/p95 迷你图   │ │
│  │ 320ms  +3%        │ │ 45/s  -2%        │ │ 80ms  +5%        │ │
│  └──────────────────┘ └──────────────────┘ └──────────────────┘ │
│                                                                  │
│  ┌────────────────────┐ ┌────────────────────┐                  │
│  │ Top models         │ │ Top harnesses      │                  │
│  │ ◆ Meta-Llama ████  │ │ ◆ claude-code ████ │                  │
│  │ ◆ DeepSeek  ████   │ │ ◆ aider      ███  │                  │
│  │ ◆ Mistral   ██     │ │ ◆ opencode   ██   │                  │
│  │ ◆ Qwen      ██     │ │ ◆ cursor     █    │                  │
│  │ ◆ Gemini    █      │ │ ◆ cline      █    │                  │
│  └────────────────────┘ └────────────────────┘                  │
└─────────────────────────────────────────────────────────────────┘
```

## 与现状的核心区别

| 维度 | 现状 | 方案 |
|------|------|------|
| KPI 行 | 4 张 hero 大卡，Active/Queued 埋在健康区 | 6 张紧凑卡，关键数字一目了然，第一张品牌色强调 |
| 健康面板 | 2 个 ring gauge 占空间，告警指标在底部 | 紧凑横向进度条（Capacity/Success）+ 4 行告警，信息密度翻倍 |
| 性能区 | 静态 track 条 + 圆点，只能看当前值 | 迷你线图展示窗口内趋势，p50/p95 双线，带变化方向 |
| 卡片质感 | 全是灰色卡片，无主次 | 第一张 KPI 带品牌底色，hover 上浮效果，层次分明 |

## 具体改动

### 1. CSS 新增（`dashboard.html` `<style>` 内）

- `--card-emphasis` 变量：带品牌色背景的强调卡片
- `.kpi` 紧凑变体（高度 110px，之前 126-132px）
- `.kpi` hover 效果：`transform: translateY(-1px)` + 阴影加深
- `.health-bar` 类：紧凑横向进度条（替代 ring gauge）
- `.health-row` 类：告警指标行（✓/✗ 状态色）
- `.perf-mini` 类：迷你性能图容器（120px 高，替代 track 条）
- 卡片间距微调：`14px → 16px`，区块间留白增大

### 2. HTML 骨架变更（`src/dashboard.html` overview section）

- `#tab-overview` 内重构：
  - KPI 行：`div.tiles` → 6 个 `.kpi.compact` 容器（id 不变以兼容 JS）
  - 第二行：`div.row` 改为 flex:2 / flex:1 非对称
  - 健康面板：去掉 `#o-rings` + `#o-health`，改为 `#o-health-panel`
  - 性能区：`div.perf3` → 3 个 `#o-ttft` `#o-tps` `#o-tpot` 迷你图容器
  - 底部：`div.grid2` 保留，数据不变

### 3. JS 渲染函数变更（`renderOverview`）

- `kpiCards` 调用：第 1 个 KPI 传 `{ emphasis: true }` 参数，JS 加类
- 健康面板：新函数 `renderHealthPanel(el, c)`，用紧凑 bars + 告警行替代 ring + metricRow
- 性能区：复用 `quantSeries` + `lineChart`（120px 高），替代 `perfBlock`
- 其余函数（`leaderList`、`lineChart`）不动

### 4. 不变的内容

- 所有数据指标、聚合逻辑、`c` 对象字段
- 图表渲染函数 `lineChart`、`stackChart`、`leaderList`、`kpiCards`
- 时间范围选择、主题切换、sidebar
- 其余 tab（queue/catalog/models/clients/reliability/capacity/settings）完全不动

## 实施步骤

1. 新增 CSS 类（紧凑 KPI、健康条、迷你图容器、hover 效果）
2. 重构 `#tab-overview` HTML 骨架
3. 重写 `renderOverview` 中的健康面板渲染（新函数 `renderHealthPanel`）
4. 重写性能区渲染（`lineChart` 替代 `perfBlock`）
5. 调整 KPI 第一张卡片为强调样式
6. `cargo test` 全量测试通过
7. `cargo fmt && cargo clippy --all-targets -- -D warnings` 零警告

## 风险

- 仅改 `renderOverview` 和 `#tab-overview`，不碰其他 tab，不影响其他功能
- 图表复用现有 `lineChart` 函数，无新渲染逻辑
- 安全不变式：不引入新 innerHTML 注入点

---

## 实施记录

**2026-08-10** — 全部实施完成

### 改动摘要

**CSS 新增** (`src/dashboard.html` `<style>`):
- `.kpi.compact` — 高度 110px，hover 上浮效果 (`translateY(-1px)` + `shadow-pop`)
- `.kpi.emphasis` — 品牌色渐变背景，白色文字，用于第一张 KPI
- `.health-bar` / `.health-row` — 紧凑横向进度条 + 告警指标行（✓/✗ 状态色圆点）
- `.perf-mini` — 迷你线图容器
- `#tab-overview` 内间距从 14px 改为 16px

**HTML 骨架** (`#tab-overview`):
- KPI 行：6 个紧凑 KPI（Requests / Tokens out / Success rate / Dollars saved / Active now / Queue）
- 第二行：flex 2:1 非对称布局（Traffic 图 + 健康面板）
- 健康面板：`#o-rings` + `#o-health` → `#o-health-panel`（紧凑进度条 + 告警行）
- 性能区：`div.perf3` → 3 个 `#o-ttft` `#o-tps` `#o-tpot` 迷你图容器
- 底部 `div.grid2` 保留不变

**JS 变更** (`renderOverview`):
- `kpiCards` 调用：6 张 KPI，第一张传 `{ emphasis: true }`
- 新函数 `renderHealthPanel`：紧凑 bars + 告警行替代 ring gauge + metricRow
- 性能区：`quantSeries` + `lineChart`（120px 高）替代 `perfBlock` 静态 track 条
- `kpiCards` 函数：支持 `k.emphasis` 标志位

**不变的内容**：所有数据指标、聚合逻辑、`c` 对象字段、`lineChart`/`stackChart`/`leaderList`/`kpiCards` 函数签名、其余 tab（queue/catalog/models/clients/reliability/capacity/settings）

**验证**：`cargo test` 284 例全通过，`cargo fmt && cargo clippy` 零警告