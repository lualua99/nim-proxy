# Step 11: cargo fmt && cargo clippy

## 目标

确保代码风格和 lint 通过 CI 检查。

## 命令

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## 修复

- 修复所有 clippy 警告
- 运行 `cargo fmt` 格式化代码