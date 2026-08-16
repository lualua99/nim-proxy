# Step 1: 理解依赖关系

阅读所有关键源文件，理清模块间依赖，确定改动范围。

## 需要阅读的文件

- `src/lib.rs` — 模块声明和 AppState 结构体
- `src/main.rs` — 路由注册、启动逻辑（实际在 `lib.rs` 的 `run()`）
- `src/auth.rs` — 认证模块（将被移除）
- `src/config.rs` — 配置结构体（将被简化）
- `src/proxy.rs` — 代理处理（移除客户端 key 校验）
- `src/pool.rs` — key 连接池（移除所有权隔离）
- `src/settings.rs` — 设置 API handler（移除用户相关）
- `src/dashboard.html` — 前端（移除多余标签页）
- `src/setup.html` — 首次运行向导（将被移除）

## 关键依赖链

- `auth.rs` 被 `lib.rs` 引用：`use auth::{Admin, Identity}`
- `config.rs` 被几乎所有模块引用
- `settings.rs` 依赖 `auth.rs` 和 `config.rs`
- 测试文件 `tests/` 中的 e2e 测试也依赖 auth 流程

## 输出

### 模块依赖图（关键事实）

- `auth.rs` 只被三处引用：`lib.rs`（`Admin`/`Identity`、`require_session` 中间件、`/login /logout` 路由）、`settings.rs`（密码哈希、会话、`Identity` 提取器）、`proxy.rs`（`sha256_hex`/`ct_eq`/`setup_required_json`）。删除它 → 全部解耦。
- `config.rs` 定义 `User`/`Role`/`ClientAuth`/`ClientKey`/`Mode`，`NimKey.owner`，`Config.clients`，及所有认证相关校验。`runtime()` 把这些映射进 `Config`。
- `pool.rs` **没有 owner 概念**（`LaneSpec` = key/rpm/enabled）——所有权隔离实际在 config 校验 + settings handler 里，pool 本体几乎不用动。
- `dispatch.rs`/`governor.rs`/`history.rs`/`upstream.rs`/`registry.rs`/`ratestate.rs` 均**不依赖 auth/config 的用户部分**（已逐个 grep 确认）。
- `getrandom` 在 `history.rs`（boot ID）仍用 → 保留该依赖；`hmac`/`sha2`(正式依赖)/`subtle` 只被 auth 用 → 可删。
- e2e 测试（`tests/e2e.rs` 127 个测试 + `tests/support/mod.rs`）大量走 setup/login/session/client-key 流程；`support` 有 `TEST_USER`/`TEST_PASSWORD`、`login_as()`、`complete_setup()`、带用户和 `client_auth` 的 fixture。

### 各模块具体改动清单

**Cargo.toml（伴随步骤 3）**
- 删 `[dependencies]`: `hmac`、`sha2`、`subtle`。
- `getrandom` 保留（history boot ID）。
- 若 e2e fixture 不再哈希 client secret，`[dev-dependencies] sha2` 也可删；`[profile.dev.*]` 里 sha2/hmac/digest 加速项随后清理。

**config.rs（步骤 2）**
- 删 `User`、`Role` 及 `StoredConfig.users` / `superuser()` / `user()`。
- 删 `ClientAuth`、`ClientKey`、`Mode` 及 `StoredConfig.client_auth`。
- `NimKey.owner` 字段删除（结构体变为 key/enabled/rpm）。
- `runtime()`：删 `clients` 映射与 `Config.clients` 分支。
- `validate()`：删用户名/角色/超管唯一性、client key 校验、owner 存在性、超管必须拥有 ≥1 个启用 key（pool floor）等所有权/pool-floor 校验；`NimKey` 保留 key/duplicate/rpm 校验。
- `Config`（lib.rs 里）删 `clients` 字段。
- 兼容性：serde 默认忽略未知字段，遗留 store 里的 `users`/`client_auth`/`owner` 会被静默忽略，无需迁移。

**auth.rs（步骤 3）**
- 整个文件删除；`lib.rs` 删 `mod auth;`；删 `warn_legacy_env` 中的 `ADMIN_PASSWORD`/`PROXY_API_KEYS`/`INSECURE_NO_AUTH`（若有）。

**proxy.rs（步骤 5）**
- `handle()`：删 `setup_required` 门 + `auth::setup_required_json()` 分支。
- 客户端 key 校验块（约 569–593 行）整体替换为 `let client = "local".to_owned();`——`Ctx.client` 标签、metrics、日志、registry、dispatch waiter 全部继续用 "local"（仪表盘按 client 聚合视图仍工作）。
- 删 `unauthorized()`、`nimproxy_unauthorized_total` 计数（不再可能发生）；`auth::sha256_hex`/`ct_eq` 引用删除。
- 其余（限速/重试/故障转移/SSE心跳/governor/request-shape/metrics）不变。

**pool.rs（步骤 6）**
- 确认无 owner 概念，结构上**无需改动**；enabled/carrier、校准、ramp、sticky 均与所有权无关，保留。步骤 6 实际是 config/settings 里所有权逻辑的移除（见步骤 2/4）。

**settings.rs（步骤 4 + 7）**
- 删 handler：`setup_page`、`setup_submit`、`setup_validate_key`、`validate_key`、`clients`、`users`（含 `AddUser`/`ResetPassword`/`SetRole`/`parse_role`）、`account`（含 `apply_password_change`/`PasswordChangeErr`）；`probe_key` 与 `auth` 相关工具如随客户端密钥删除。
- 所有其余 handler 去掉 `Extension(Identity(...))` 参数与角色检查（`role_of`/`stale_session`/`forbidden`/`admin_section!` 宏内的 role 分支、`operator_check` 相关）。
- `api_config`：删 `role`/`users`/`client_keys`/`mode` 字段与 `admin_view` 过滤；保留 `nim_keys`（含 last4/fingerprint/lane 状态）与 server 段（server 自体设置全量直接返回）。
- `last4`/`fingerprint` 保留（NIM key 展示用）；`mint_client_secret`/`sha256_hex`/`mint_session_cookie` 等删除。
- `commit()` 本身不动（validate 已改）。

**lib.rs（步骤 7）**
- 删 `use auth::{Admin, Identity}`、`use config::Role`；`mod auth;`。
- `Config.clients` 字段删。
- `AppState`：删 `admin: Admin`、`setup_required: AtomicBool`。
- `run()`：删 `trust_proxy` 读取/env `TRUST_PROXY`、`setup_required = stored.superuser().is_none()`、setup 相关日志、`Admin::new`、路由层；`warn_legacy_env` 列表清理。
- handler 签名：`api_dashboard_now` / `api_dashboard_stream` / `api_queue` / `api_queue_terminate` 去掉 `Extension(Identity)`；`api_queue`/`api_queue_terminate` 去掉 `operator_check`（直接放行）；`dashboard_now_payload` 去掉 username 及 store 里的 role 查找。（注：`api_capacity_model` 随 what-if 模拟器一并删除。）
- 路由：`/login`、`/logout`、`/setup`、`/setup/validate-key`、`/api/settings/clients`、`/api/settings/users`、`/api/settings/account` 删除；`require_session` 中间件及 "protected" 分组整体删除，仪表盘路由直接并入主 Router（本地单用户，无需会话门）。

**setup.html（步骤 8）**
- 整个文件删除。

**dashboard.html（步骤 9）**
- Settings 子导航：删 `users`、`account` 两个子页与 `renderUsers`/`renderAccount`；`setnav` 只剩 access(简化)/server。
- Access & keys：删 `/api/settings/clients` 调用、客户端密钥列表/创建、open/keyed 模式切换；保留 NIM keys 管理。
- Header/Overview：删 role 徽章、`SET.username/role/users/mode/client_keys` 字段、`auth` 字段引用；Overview "Shed · 401 · Logins" 行的 401/logins 指标（`authorized_total` 不再产生）移除。
- 去掉对 `/login` 的 401 跳转逻辑（已无登录页）。

**tests（步骤 10）**
- `tests/support/mod.rs`：删 `TEST_USER/TEST_PASSWORD`、`login_as()`/`login()`/`complete_setup()`、用户与 client_auth fixture；二进制以"已配置、/v1 open、仪表盘不设门"方式启动；`client` 标签固定 "local"。
- `tests/e2e.rs`：删所有 setup-claim、login/logout、session-required、metrics 凭据、RBAC（admin/user/superuser）、client 密钥 mode、用户管理、孤儿收养类测试（约三分之一）；保留限速/重试/故障转移/governor/history/streaming 覆盖。
- 新增/改写最小启动路径测试（无 setup 直接可用，`/v1` 不以 "local" 拒绝）。

**后续注意**
- 代码改完按 AGENTS.md 维护 `knowledge/`：`auth-posture-and-dashboard-password`、`ui-managed-config-store`、`client-auth`(architecture)、`sharing-with-friends`(ops)、`configure-env`、`dashboard`(architecture) 等页面需标注/更新或删除，并在 `knowledge/log.md` 记 ingest。
- 每次完成后更新本清单改动状态。