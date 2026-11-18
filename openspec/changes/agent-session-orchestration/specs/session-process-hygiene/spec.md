## ADDED Requirements

### Requirement: 存活 wrapper 盘点
runner SHALL 在启动后以及每 10 分钟枚举由 runner 启动的 wrapper，并把每个 wrapper 归入"已跟踪"、"已接管"、"已上报"三类之一；三类计数之和 MUST 等于枚举数。

#### Scenario: 重启后接管
- **WHEN** runner 重启后发现带 HAPI 会话 id 和进程启动标记的未跟踪 wrapper
- **THEN** 把它接管为已跟踪，此后 `stopSession(id)` 能停掉它

#### Scenario: 无法识别
- **WHEN** wrapper 的 argv 中没有会话 id，或缺少启动标记
- **THEN** 只上报，不自动结束进程

### Requirement: 二进制漂移
在 Linux 上，wrapper 的 `/proc/<pid>/exe` 指向已删除文件或与当前安装二进制不一致时，MUST 上报为 `deleted` 或 `mismatch`；hub 在休眠开启时 SHALL 把漂移会话的休眠窗口缩短为 10 分钟，其余休眠规则不变。

#### Scenario: 漂移升级
- **WHEN** 漂移会话空闲超过 10 分钟且无活工作
- **THEN** 会话休眠，下次恢复使用当前二进制
