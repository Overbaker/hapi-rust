## ADDED Requirements

### Requirement: 真实栈基线
系统 SHALL 提供一条命令，在 india-vps 上对三组基线（上游 `37446bcf`、deploy/rust 的 TS 优化版、本变更产物）产出 JSON+Markdown 报告，含硬件与 commit 元数据。

#### Scenario: 指标齐全
- **WHEN** 运行报告命令
- **THEN** 报告含：LCP（500 消息会话）、100 chunk 流式 p50/p95/p99 帧时与 >50ms 长任务数、2k 消息滚动帧时、10 会话下 hub+runner+wrapper+agent 子进程的 PSS 总和与占比、TTFB 与 prompt→首 token、SSE gzip 前后字节数

### Requirement: 诚实归因
报告 MUST 同时给出含 agent 子进程的总内存，并 MUST 标注 Rust 无法改善的部分（agent 子进程、LLM 首 token、RTT、DOM 布局）；微基准结果 MUST 与端到端结果分开列示。

#### Scenario: 归因
- **WHEN** 报告内存倍数
- **THEN** 同时列出含子进程总 PSS 与 Rust 无法改善项

### Requirement: Gated 项的决策
CLI wrapper 内存 Rust 化与 hub-rs 只读 sidecar 仅在测量满足 design.md 门槛时才进入实现：wrapper PSS 之和 ≥ 栈总 PSS 的 25%（且惰性导入/compile 优化无法降低 ≥40%）；hub 在 3x 预期峰值下 p99 广播延迟 ≥ 20 ms 或 CPU ≥ 25% 单核。否则 MUST 记录"不实施"结论。

#### Scenario: 未过门槛
- **WHEN** 测量低于门槛
- **THEN** 记录“不实施”结论且不引入对应实现

### Requirement: 数据安全
任何 Rust 组件 MUST NOT 向 `~/.hapi/hapi.db` 写入 TS hub 不可读内容；对其只读打开时 `user_version != 36` MUST 拒绝。

#### Scenario: 版本不符
- **WHEN** 只读打开的库 `user_version != 36`
- **THEN** 拒绝打开且不写入任何内容
