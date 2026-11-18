## ADDED Requirements

### Requirement: 流式文本以增量传输
CLI 对同一条 live 消息（reasoning / assistant 文本）的连续更新 MUST 以相对上一帧的增量发送；hub MUST 向支持增量的 Web 客户端广播 `message-delta` SyncEvent，并 MUST 在 `shared/src/schemas.ts` 中定义该变体。

#### Scenario: 增量折叠等价
- **WHEN** 客户端先收到 `message-received`（基准行）再按序收到 N 个 `message-delta`
- **THEN** 折叠结果文本等于对应的最终全量快照文本，逐字节相等

#### Scenario: 丢帧或中途加入
- **WHEN** 客户端缺失任一 delta（序号不连续）或在流中途连接
- **THEN** 客户端 MUST 通过尾部同步取得全量基准后继续，且不显示错误文本

#### Scenario: 旧客户端
- **WHEN** 客户端未声明增量能力
- **THEN** 仍收到全量 `message-received`，行为与现状一致

### Requirement: 增量带宽判据
在 ≥2 KB 的真实流式回合上，`message-delta` 路径的 SSE 传输字节（gzip 后）MUST 比全量快照路径至少少 2 倍；否则本能力 MUST 被回退并保留全量路径。

#### Scenario: 带宽不达标
- **WHEN** 真实回合 gzip 后字节降幅 <2x
- **THEN** 回退到全量快照路径

### Requirement: 持久化语义不变
live 增量 MUST NOT 为每个 chunk 写入新 SQLite 行；最终落库内容与 seq 单调性 MUST 与改动前一致，keyset 游标（beforeSeq/afterSeq）不受影响。

#### Scenario: 落库
- **WHEN** 一个 live 流产生 100 个 delta
- **THEN** 仅终态写入一行，seq 单调，keyset 游标行为不变
