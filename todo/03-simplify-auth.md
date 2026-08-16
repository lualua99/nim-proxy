# Step 3: 简化 auth.rs

## 目标

移除整个认证模块。单人本地使用不需要会话 cookie、登录/登出、PBKDF2 密码哈希。

## 具体改动

1. 删除以下公共类型/函数：
   - `Admin` 结构体
   - `Identity` 结构体
   - `require_session` 中间件
   - `login_page` / `login_submit` / `logout` handler
   - `ct_eq` / `sha256_hex` / `pbkdf2_sha256` 等工具函数

2. 添加一个无操作的 `Identity` 替代：
   - 用一个简单的 `Identity(String)` 提取或硬编码一个占位值

3. 或者完全删除 `auth.rs` 模块，在 `lib.rs` 中移除 `mod auth;`

## 注意

- `proxy.rs` 中可能用到了 `Identity` 来获取客户端名称
- `settings.rs` 中所有 handler 都用了 `Identity` 提取
- 仪表盘 SSE 流中用到了 `Identity`

## 方案选择

**推荐方案**: 完全删除 auth.rs，在 lib.rs 中创建一个简单的 `Identity` 提取，始终返回 `"local"` 占位用户名。

## 验证

- `cargo build` 通过