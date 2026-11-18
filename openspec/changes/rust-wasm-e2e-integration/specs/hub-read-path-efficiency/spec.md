## ADDED Requirements

### Requirement: 消息页免解析拼接
`GET /api/sessions/:id/messages` 与导出路径 MUST 返回与改动前 JSON 语义等价的响应，且在不对每条消息 `JSON.parse`/`stringify` 往返的情况下构造响应体。

#### Scenario: 响应等价
- **WHEN** 对同一份种子库（含 64 KB 截断工具结果、TEXT 与 zstd BLOB 两种存储）分别用新旧实现请求
- **THEN** 解析后的 JSON 深度相等，且 `Content-Length`/大小估计字段一致

### Requirement: 终端回滚缓冲 O(chunk)
终端回滚缓冲追加 MUST 为 O(chunk 大小)，缓冲上限字节数 MUST 不变；满载后追加不得整体复制缓冲。

#### Scenario: 洪泛下事件循环延迟
- **WHEN** 对 hub 终端灌入 200 MB 连续输出并并发发送 SSE 消息
- **THEN** 事件循环延迟 p99 < 20 ms 且并发 SSE 广播延迟不劣化

### Requirement: 先测量门槛
每项优化 MUST 先在种子数据上给出改动前基线；实测收益低于 design.md 所列 kill criterion 时 MUST 放弃并保留原实现。

#### Scenario: 收益不足
- **WHEN** 实测收益低于 kill criterion
- **THEN** 放弃该优化并保留原实现
