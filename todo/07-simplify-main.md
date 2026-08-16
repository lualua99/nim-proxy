# Step 7: 简化 lib.rs (run 函数)

## 目标

移除 auth 中间件、setup 路由、登录/登出路由。

## 具体改动

1. 移除 `AppState` 中的：
   - `admin: Admin` 字段
   - `setup_required: AtomicBool` 字段

2. 删除路由注册：
   - `/login`（GET/POST）— 不再需要登录页
   - `/logout`（POST）— 不再需要登出
   - `/setup`（GET/POST）— 不再需要设置向导
   - `/setup/validate-key` — 不再需要

3. 移除 `protected` 路由组上的 `auth::require_session` 中间件

4. 简化 `dashboard_now_payload`：
   - 移除 `username` 参数，不再需要 role
   - 移除 `role` 字段返回

5. 简化 `api_dashboard_now` 和 `api_dashboard_stream`：
   - 移除 `Extension(Identity(...))` 提取
   - 直接调用 `dashboard_now_payload`

6. 移除 `api_queue` 和 `api_queue_terminate` 中的 `operator_check` 调用

7. 移除 `operator_check` 函数

8. 移除 `Admin::new(trust_proxy)` 调用

9. 简化启动日志：
   - 不再输出 "SETUP REQUIRED" 警告
   - 不再输出 dashboard auth 状态

## 验证

- `cargo build` 通过