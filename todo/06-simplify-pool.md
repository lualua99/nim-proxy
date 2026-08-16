# Step 6: 简化 pool.rs

## 目标

移除 key 所有权隔离逻辑（哪个 key 属于哪个用户）。

## 具体改动

1. 查找并移除：
   - `NimKey` 中的 `owner` 字段相关逻辑
   - 用户级别 key 可见性的过滤
   - `guarded` 属性（保证 superuser 至少有一个 key 的逻辑）

2. 简化 `pool_specs()`：
   - 所有 enabled 的 key 都加入池，不按用户过滤

## 验证

- `cargo build` 通过