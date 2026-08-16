# 单人本地化改造任务清单

## 总体目标

将 nim-proxy 从多用户共享代理改造成**单人本地使用的简化版本**，移除所有认证/用户/权限/多租户相关功能，保留核心的限速、心跳、重试、故障转移、模型压力治理等能力。

## 执行顺序

1. [x] **理解依赖关系** — 阅读源码，理清模块间依赖
2. [x] **简化 config.rs** — 移除 User/ClientAuth 等结构体
3. [x] **简化 auth.rs** — 移除整个认证模块
4. [x] **简化 settings.rs** — 移除用户/client key handler
5. [x] **简化 proxy.rs** — 移除客户端 key 校验
6. [x] **简化 pool.rs** — 移除 key 所有权隔离（确认 pool 本就无所有权，无改动）
7. [x] **简化 lib.rs** — 移除 auth 中间件和路由
8. [x] **移除 setup.html** — 不再需要向导
9. [x] **重构 dashboard.html** — 简化标签页和设置
10. [x] **编译测试** — cargo build && cargo test
11. [x] **fmt & clippy** — 代码风格清理

## 核心保留功能

- 每 key 滑动窗口限速（40 RPM）
- SSE 心跳保活
- 多 key 负载均衡
- 429/5xx 自动重试
- 故障转移
- 模型压力治理
- 仪表盘监控（简化版）
- 历史记录

## 状态

✅ 全部完成（2026-08-14）：`cargo build`、`cargo test`（135 单元 + 77 e2e）、
`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 均通过；
`knowledge/` 已同步（新增 single-user-local-build 决策页，auth/client 相关页面标注废弃）。