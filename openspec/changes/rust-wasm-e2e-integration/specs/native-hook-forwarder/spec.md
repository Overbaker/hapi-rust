## ADDED Requirements

### Requirement: 原生 hook 转发器
Claude hook 与 `hapi job` 的短命调用 SHALL 由独立 Rust 二进制转发到本机 runner/hub；其输入输出 MUST 与现有 TS 转发器字节级兼容，且找不到二进制时 MUST 自动回落到 TS 实现。

#### Scenario: 冷启动
- **WHEN** 在 india-vps 上连续 200 次冷调用
- **THEN** p50 冷启动 ≤ 10 ms 且瞬时 RSS ≤ 8 MB（对比 TS 约 65 ms / 60 MB）

#### Scenario: 失败语义
- **WHEN** 本机端点不可达
- **THEN** 退出码与错误输出与 TS 实现一致，且不阻塞 agent 超过既有超时

### Requirement: Kill criterion
若以 `/bin/sh`+`curl` 或常驻本机端点即可达到同等冷启动（≤10 ms），本能力 MUST 放弃 Rust 实现。

#### Scenario: 对照达标
- **WHEN** `sh`+`curl` 方案冷启动 ≤10 ms
- **THEN** 不实施 Rust 二进制
