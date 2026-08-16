# Step 2: 简化 config.rs

## 目标

移除多用户/多角色相关的结构体和字段。

## 具体改动

1. `StoredConfig` 结构体：
   - 移除 `users: Vec<User>` 字段
   - 移除 `client_auth: ClientAuth` 字段（或简化）

2. 删除以下结构体/枚举：
   - `User` 结构体
   - `Role` 枚举（superuser/admin/user）
   - `ClientAuth` 结构体
   - `ClientKey` 结构体
   - `Mode` 枚举（keyed/open — 永远 open）

3. 删除相关方法：
   - `StoredConfig::superuser()`
   - `StoredConfig::user()`
   - `StoredConfig::pool_specs()` 中的所有权过滤
   - 所有与 `Role` 相关的方法

4. 简化 `RuntimeConfig` 生成：
   - `clients` 字段永远为 `None`（open 模式）
   - 移除 `setup_required` 的检查（不再需要 superuser）

## 验证

- `cargo build` 通过