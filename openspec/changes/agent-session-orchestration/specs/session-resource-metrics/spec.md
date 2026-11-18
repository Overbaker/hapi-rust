## ADDED Requirements

### Requirement: 空闲 CPU A/B
系统 SHALL 提供可复现脚本，在同一 commit 下比较两个 Bun 版本的空闲 wrapper 的 CPU%、唤醒次数/s 和 PSS，并按判定规则得出结论：Bun 版本是主因就锁定较好的版本，否则修复 HAPI 侧的定时器或 watcher。

#### Scenario: 空闲指标
- **WHEN** 采用结论后的 wrapper 空闲 10 分钟
- **THEN** 60 s 均值 ≤0.3% CPU 且 ≤5 次唤醒/s；若受 Bun 运行时限制而达不到，则在报告中注明

### Requirement: 启动与常驻内存
惰性导入与 bytecode 编译 SHALL 各自只在冷启动或常驻 PSS 改善 ≥10%、且另一项劣化不超过 5% 时保留；报告给出每个 flavor 改动前后的 p50/p95 与 PSS。

#### Scenario: 无头 wrapper 不加载 UI
- **WHEN** runner 以无 TTY 方式启动 claude wrapper
- **THEN** 不加载 React、Ink 及其他 flavor 的模块

### Requirement: 恢复延迟
报告 SHALL 按 transcript 大小（1/5/25 MB）给出从 spawn 到首个事件的 p50/p95；本变更不做预热。

#### Scenario: 报告
- **WHEN** 运行度量脚本
- **THEN** 输出三档 p50/p95
