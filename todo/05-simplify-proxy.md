# Step 5: 简化 proxy.rs

## 目标

移除客户端 API key（`npk_`）校验，永远使用 open 模式。

## 具体改动

1. 查找并移除客户端 key 校验逻辑：
   - 定位 `Authorization: Bearer npk_...` 的校验代码
   - 移除 `Config::clients` 的检查

2. 简化 `handle` 函数：
   - 不再要求客户端提供 bearer token
   - 客户端标签始终使用 `"local"` 或提取的模型名

3. 移除 `Ctx` 中不必要的客户端认证字段

## 注意

- `proxy.rs` 的 `handle` 函数是主入口，改动需要谨慎
- 保留 sanitize_label 等安全函数
- 保留速率限制、重试、心跳等核心逻辑

## 验证

- `cargo build` 通过