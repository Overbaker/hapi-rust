## ADDED Requirements

### Requirement: 空闲会话休眠
hub SHALL 在会话只有 keepalive、无 agent 进展持续超过 `HAPI_SESSION_HIBERNATE_MS`（默认 7200000；`0` 关闭；未设或非法取默认；1..59999 提升为 60000）后，停止该会话的 wrapper、agent 及其进程组内全部子进程，并把会话标记为 `lifecycleState: 'hibernated'`。

#### Scenario: 休眠后无残留进程
- **WHEN** 一个符合条件的 claude 会话空闲超过窗口
- **THEN** 该会话进程组内进程数为 0，`lifecycleState` 为 `hibernated`，`metadata.hibernation.state` 为 `hibernated`，会话列表顺序不变

#### Scenario: 关闭开关
- **WHEN** `HAPI_SESSION_HIBERNATE_MS=0`
- **THEN** 不发出任何停止 RPC，行为与改动前一致

### Requirement: 活工作永不休眠
存在以下任一情况的会话 MUST NOT 休眠：thinking 或 thinking 宽限期内；后台任务 >0；待审批/待输入请求；运行中的附加 job；活跃子 agent 成员或进行中的 team 任务；打开的终端；本地控制（`controlledByUser`）；未投递的排队或定时消息；进行中的 resume/archive/历史操作；`idleReconcileExempt=true`。

#### Scenario: 随机活工作
- **WHEN** 任意会话至少满足一项活工作条件，且空闲时长任意
- **THEN** 不休眠

### Requirement: 按 flavor 准入
系统 MUST 仅允许 `claude`、`codex`、`opencode`、`pi` 且由 runner 启动、非 PTY、存在 resume 令牌、所在 runner 声明 `sessionHibernation` 能力的会话休眠；codex attached 运行时、opencode rewind/fork/clear 进行中、pi 无原生 resume 能力时排除；cursor、agy 及其余 flavor 排除。

#### Scenario: 不合格 flavor
- **WHEN** 会话 flavor 为 cursor 或 agy
- **THEN** 永不休眠

### Requirement: 停止必须确认
仅当 runner 停止结果为 `stopped` 或 `already_gone` 时才可标记休眠；`still_alive`、`unknown`、异常或超时 MUST 清除中间标记、保持原状态，并对该会话退避 10 分钟。

#### Scenario: 停止未确认
- **WHEN** 停止 RPC 返回 `still_alive`
- **THEN** 会话不标记休眠，10 分钟内不再尝试

### Requirement: 恢复保持会话 id
休眠会话 SHALL 经现有 resume 路径恢复：Web 发送、`POST /api/sessions/:id/messages`、到期的定时消息、`ping_peer`（不需要 wake）都会自动唤醒。恢复后 HAPI 会话 id MUST 不变；若 spawn 返回不同 id，MUST 停掉新进程并返回 `resume_failed`，且 MUST NOT 合并会话。

#### Scenario: 恢复同 id
- **WHEN** 向休眠会话发送消息
- **THEN** 会话以相同 id 恢复，transcript 完整，消息被投递

#### Scenario: 恢复后不立即再休眠
- **WHEN** 会话刚恢复且尚无新消息
- **THEN** 至少再过一个完整窗口才可能再次休眠

### Requirement: 状态可见
Web 会话列表 SHALL 把休眠会话显示为"已休眠"（不同于离线/已归档），保留在 Active 视图，允许快速归档；正在恢复时显示"唤醒中"。

#### Scenario: 列表展示
- **WHEN** 会话处于 `hibernated`
- **THEN** 显示已休眠标识，并出现在 Active 筛选中
