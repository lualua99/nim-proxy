# Step 8: 简化 setup.html

## 目标

移除首次运行向导页面。单人本地使用时不再需要。

## 具体改动

1. 删除 `src/setup.html` 文件

2. 删除 `src/settings.rs` 中对 `include_str!("setup.html")` 的引用

3. 删除 `src/lib.rs` 中的 `/setup` 路由

## 注意

- 确保 dashboard 不再重定向到 `/setup`
- 确保 `/v1` 不再返回 503 setup_required

## 验证

- `cargo build` 通过