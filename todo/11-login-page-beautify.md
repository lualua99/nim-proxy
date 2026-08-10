# 任务 11：登录页面美化（与仪表盘风格一致）

> 状态：已完成
> 日期：2026-08-10
> 需求：`/login` 页面目前使用系统字体、蓝色按钮、无品牌感，与精致的
> `dashboard.html` 风格严重不匹配。需要将登录页面的视觉设计对齐到仪表盘
> 的靛蓝品牌色系、Inter 字体、深色/浅色主题和卡片设计语言。

---

## 背景与现状

- 登录页面在 `src/auth.rs:580-623` 的 `login_html()` 函数中以内联方式生成
  （`Response` 返回 `text/html`），并无独立 HTML 文件。
- 当前设计：系统字体，`#2a78d6` 蓝色按钮，基本 `prefers-color-scheme` 媒体查询，
  300px 宽卡片，无品牌标识，无动画。
- 仪表盘（`dashboard.html`）使用：Inter 字体（Google Fonts）、靛蓝品牌色
  （`#6366F1` 浅色 / `#818CF8` 深色）、CSS 变量主题系统（`data-theme` +
  `localStorage('np-theme')`）、径向渐变背景、精细的卡片阴影和圆角。
- 无测试覆盖 `login_html` 的 HTML 内容，改动安全。
- 安全不变式：`{err}` 是固定字符串，无用户输入进入 `innerHTML`，XSS 防线
  不变。

## 设计决策

### D1：品牌一致性
- 复用仪表盘的 base64 徽标（68×68 PNG，与侧边栏相同）。
- 卡片顶部显示 logo + "**NIM** Proxy" wordmark（与仪表盘侧边栏相同的品牌色）。
- 品牌色：`#6366F1`（浅色）/ `#818CF8`（深色）。

### D2：主题系统与仪表盘共享
- 在 `<head>` 中内联脚本，读取 `localStorage('np-theme')` 设置
  `data-theme`，防止页面闪烁。
- 定义 `:root` 浅色变量和 `:root[data-theme="dark"]` 深色变量，配色与
  仪表盘一致。
- 添加主题切换按钮（月/日图标），与仪表盘共用 `np-theme` localStorage 键。

### D3：卡片与背景
- 居中布局；`max-width: 400px`，`width: 92%`。
- 卡片样式与仪表盘 `.card` 一致：`border-radius: 14px`，`box-shadow`。
- 浅色下 `radial-gradient(1100px 400px at 82% -14%, rgba(99,102,241,0.08), transparent 66%)`
  与仪表盘 `.main` 一致。

### D4：输入框与按钮
- 输入框匹配仪表盘 `.sin` 类：`border-radius: 8px`，`border: 1px solid var(--card-border)`，
  焦点时 `border-color: var(--brand-ring)` 和 `box-shadow: 0 0 0 3px var(--ring-soft)`。
- 按钮：靛蓝品牌色，`border-radius: 99px` 药丸形状，`font-weight: 600`。
- 输入框左侧添加 SVG 图标（用户和锁图标）。

### D5：微交互
- 卡片 `@keyframes fadeIn` 0.3s 淡入。
- 按钮 `transition: background 0.15s`，输入框 `transition: border-color 0.15s, box-shadow 0.15s`。

## 实施步骤

1. 提取仪表盘的 base64 徽标字符串，在 `auth.rs` 中定义为常量。
2. 重写 `login_html()` 的内联 `<style>` 块，包含完整的主题系统（浅色/深色
   CSS 变量、Inter 字体链接、径向渐变背景、卡片/输入/按钮样式）。
3. 重写 HTML 结构：品牌标识、主题切换按钮、带图标的输入框、靛蓝按钮、
   错误消息使用仪表盘 `.chip.bad` 样式。
4. `cargo test` 全量测试通过。
5. `cargo fmt && cargo clippy --all-targets -- -D warnings` 零警告。

## 不改动的内容

- 登录逻辑、会话管理、节流、验证 —— 完全不变。
- 路由 `/login`、`POST /login`、`/logout` —— 不变。
- 错误处理 —— `error` 参数传递方式不变，只改变样式。
- 安全不变式 —— 无新 XSS 向量。

---

## 实施记录

- 在 `src/auth.rs` 新增 `LOGO_PNG_B64` 常量（复用仪表盘同一 68×68 base64 徽标，
  与侧边栏一致），登录卡片顶部显示 logo + "**NIM** Proxy" wordmark。
- 重写 `login_html()` 的内联 `<style>` 与 HTML 结构：
  - 主题系统：`<head>` 内联脚本读取 `localStorage('np-theme')` 设 `data-theme` 防闪烁；
    `:root` / `:root[data-theme="dark"]` CSS 变量与仪表盘同色系（`#6366F1` / `#818CF8`）；
    右上角月/日主题切换按钮共用 `np-theme` 键。
  - 布局：居中 `max-width:400px`、`border-radius:14px`、`--shadow-card`；
    浅色下 `radial-gradient(1100px 400px at 82% -14%, rgba(99,102,241,0.08), transparent 66%)` 与 `.main` 一致。
  - 输入框带用户/锁 SVG 图标，`border-radius:8px`、焦点品牌色描边 + `--ring-soft` 光晕。
  - 提交按钮靛蓝药丸形（`border-radius:99px`，`font-weight:600`）。
  - 错误提示改用仪表盘 `.chip.bad` 样式。
  - 微交互：卡片 `fadeIn` 0.3s 淡入，按钮/输入框 `transition`。
- 安全不变式保持：`{err}` 仍为固定字符串，无用户输入进入 HTML，无新 XSS 向量。
- 登录逻辑、会话、路由、错误处理完全未改。
- 验证：`cargo test` 284 项全过（173 单元 + 111 e2e）；
  `cargo fmt` 与 `cargo clippy --all-targets -- -D warnings` 零警告。