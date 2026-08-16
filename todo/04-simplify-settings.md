# Step 4: 简化 settings.rs

## 目标

移除用户管理、客户端 key 管理、账户设置相关的 API handler。

## 具体改动

1. 删除以下 handler 函数：
   - `clients` — 客户端 API key 管理
   - `users` — 用户管理
   - `account` — 密码更改
   - `setup_page` / `setup_submit` / `setup_validate_key` — 首次运行向导

2. 简化 `api_config`：
   - 不再返回 `users`、`client_keys`、`server` 中的用户相关字段
   - 始终返回 `mode: "open"`

3. 简化 `commit` 函数：
   - 移除用户/角色相关的校验

4. 删除 `setup.html` 的 `include_str!` 引用

## 保留的 handler

- `nim_keys` — NIM API key 管理（核心功能）
- `upstream` — 上游配置
- `limits` — 速率限制
- `pricing` — 定价
- `history` — 历史记录
- `governor_cfg` — 模型压力治理
- `recovery` — 重启恢复
- `dispatch_cfg` — 调度策略
- `validate_key` — key 验证

## 验证

- `cargo build` 通过