# Step 9: 重构 dashboard.html

## 目标

简化前端，移除多余标签页，合并 Settings 为一个页面。

## 具体改动

### 侧边栏导航（第 407-416 行）

从 7 个标签页减少到 4 个：

| 操作 | 标签 | 理由 |
|---|---|---|
| 保留 | Overview | 核心监控 |
| 隐藏 | Queue | 单人不需要排队管理 |
| 保留 | Catalog | 查模型 ID 有用 |
| 保留 | Models | 性能数据 |
| 删除 | Clients | 单人无意义 |
| 保留 | Reliability | 可用性数据 |
| 删除 | Capacity | 容量规划对单人无用 |
| 保留但简化 | Settings | 见下方 |

### Settings 页面（第 700-705 行）

从 4 个子标签合并为 1 个页面：

- 删除 `renderUsers()` — 用户管理
- 删除 `renderAccount()` — 密码修改
- 简化 `renderAccess()` — 只保留 NIM key 管理，移除客户端 API key 和连接模式
- 合并 `renderAccess()` 和 `renderServer()` 到同一个页面

### 其他修改

- 删除 `TITLES` 中的 `queue`、`clients`、`capacity`
- 删除 `SICONS` 中的 `users`、`account`
- 更新 `renderSettings()` 中的子标签列表
- 调整选中标签的默认行为

## 验证

- 手动检查 dashboard 显示正常
- 所有 API 调用路径正确