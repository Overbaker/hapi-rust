## Context
本设计以两路独立分析为输入：claude 负责后端，antigravity 负责集成与 Web。两份分析都附了 file:line。共识点直接采纳；分歧点按代码证据裁决，见下方"裁决"一节。用户已确认：休眠窗口 2 h，可配置；不改用户 MCP 配置；4 项各提一个 PR；Rust 重写放到最后，只做 Claude、Codex、agy。

## Goals / Non-Goals
- Goals：空闲会话 0 进程；恢复后 id 不变；孤儿 wrapper 被跟踪或上报；空闲 wrapper ≤0.3% CPU；wrapper 冷启动更快、常驻内存更少。
- Non-Goals：不做预热；不做共享 MCP 代理；不缩短 10 s keepalive；不改 `ios/`、`android/`；不新增依赖。

## Decisions

### P1 会话休眠（PR #1）
- **配置**：`hub/src/sync/sessionIdle.ts` 新增 `DEFAULT_SESSION_HIBERNATE_MS = 7_200_000` 和 `resolveSessionHibernateMs(env)`。`0` 表示关闭；未设或非法取默认；1..59999 提升为 60000。
- **定时**：不新建 timer。在 `syncEngine.ts` 的 `expireInactive()` 里，紧接 `reconcileKeepaliveIdle()` 之后调用 `void this.hibernateIdleSessions()`，复用现有 5 s tick。同一时刻只跑一轮扫描；每台机器每轮最多休眠 1 个会话。
- **时钟**：用 `getAgentProgressAt(session)`，改为 public。有效进展时间取 `max(getAgentProgressAt, metadata.lifecycleStateSince ?? 0)`，resume/reopen 时还要调用 `recordAgentProgress`，防止刚恢复就再次休眠（antigravity 发现的缺陷）。
- **判定函数**：`shouldHibernate(session, signals, agentProgressAt, now, hibernateMs, runnerCapable)`，规则按下表顺序：

  | 顺序 | 条件 |
  |---|---|
  | 1 | `hibernateMs > 0` |
  | 2 | `active` |
  | 3 | `lifecycleState ∈ {running, idle}` |
  | 4 | `idleReconcileExempt !== true` |
  | 5 | 由 runner 启动 |
  | 6 | 非 `controlledByUser`，非 PTY |
  | 7 | flavor 在白名单 `HIBERNATE_ELIGIBLE_FLAVORS = ['claude','codex','opencode','pi']` 内 |
  | 8 | flavor 专属阻断条件均不成立 |
  | 9 | `resolveAgentResumeId` 非空 |
  | 10 | runner 声明 `capabilities.sessionHibernation` |
  | 11 | `!hasLiveWork(session, signals)` |
  | 12 | `now - progressAt > hibernateMs` |

- **flavor 专属阻断**：
  - codex：attached 运行时、`codexArchivedFork`、`codexResumePending` 时不休眠。shared runtime 按单个 root 停止。
  - opencode：rewind、fork、clear 进行中时不休眠。
  - pi：需要 runner 有 `piExistingSessionResume` 能力，且没有 `piResumeAttempt`。
  - cursor、agy 及其余 flavor：排除。
- **活工作信号 `LiveWorkSignals`**：在现有的 thinking、后台任务、请求之外，新增以下几项：

  | 信号 | 数据来源 |
  |---|---|
  | 运行中的附加 job | `sessionJobs.getPrimaryRunning` |
  | 活跃 team 成员或进行中的 team 任务 | `teamState` |
  | 打开的终端 | 注入 `setTerminalSessionCounter` |
  | 未投递的 queued/scheduled 消息 | `countUndeliveredForSession` |
  | 正在进行的操作 | `historyActionsInFlight`、`resumeTails`、`archiveRequests`、pty/pi 恢复中 |
  | thinking 宽限期 | `hasPendingThinking` |

  "有人在 Web 上查看"不算活工作。
- **停止流程（裁决见下节）**：
  1. 用新鲜信号重新校验一次判定，防止判定与执行之间状态变化。
  2. 以 CAS 写入 `metadata.hibernation = {state:'stopping', since, reason}`。
  3. 非 shared 的 flavor 直接调用 `rpcGateway.stopRunnerSession(machineId, id, {timeoutMs: 90_000})`；shared codex 沿用 KillSession → StopSession。
  4. runner 端在进程树结束后执行 `sweepProcessGroup(rootPid)`：先 SIGTERM，最多等 5 s，再 SIGKILL，直到 `kill(-pgid,0)` 返回 ESRCH。
  5. 抽出 `confirmRunnerStop()` 与 archive 共用，archive 的行为保持不变。
  6. 结果为 `stopped` 或 `already_gone` 时：先 `waitForSessionInactive`，再 `markSessionHibernatedFromHub`。这一步写入 `lifecycleState:'hibernated'`，删除 CLI 写入的 `archivedBy`/`archiveReason`，并设 `touchUpdatedAt:false`。
  7. 其他结果：清除标记，对该会话退避 10 分钟。
  8. 本次 session-end 跳过 `triggerDedupIfNeeded`。
  9. hub 启动时把残留的 `stopping` 标记收敛为 `hibernated`（会话已 inactive 时）或直接清除。
- **数据结构**：`shared/src/sessionLifecycle.ts` 新增 `SESSION_LIFECYCLE_HIBERNATED` 和 `isHibernatedLifecycleState`，不加入 `isLiveLifecycleState`。`MetadataSchema` 新增 `hibernation: {state:'stopping'|'hibernated', since:number, reason:'idle'|'binary-drift'}`，可选字段。
- **恢复**：
  - 入口不变，仍走现有 resume。
  - `POST /api/sessions/:id/messages` 遇到休眠会话时自动 resume。
  - 定时消息到期时会唤醒休眠会话。
  - `ping_peer` 把 hibernated 视为可自动唤醒。
  - 若 `fromHibernation` 且 spawn 返回的 id 与原 id 不同：停掉新进程，返回 `resume_failed`，不调用 `mergeSessions`。
  - archive 路由允许对 hibernated 会话执行。
- **Web**：
  - `SessionList`、`SessionRowSummary` 显示"已休眠"徽标，会话保留在 Active 筛选中，允许快速归档。
  - `StatusBar` 恢复中显示"唤醒中…"。
  - 新增 i18n 键 `session.item.hibernated`、`session.composer.waking`，en、zh-CN、ru 三种语言都要补。
  - `sessionReference` 和 `ProjectGroupRow` 同步处理。

### P2 孤儿与漂移（PR #2）
- 新增 `cli/src/runner/wrapperInventory.ts`。在 `persistResumeProcesses()` 之后执行一次，之后每 10 分钟执行一次（unref）。
- 枚举对象：`--started-by runner` 的 HAPI driver 进程，排除 MCP bridge 和本进程。会话 id 用 `extractHapiSessionIdFromCommand` 从 argv 中提取。
- 分类：已跟踪 / 已接管（要求有启动标记和会话 id，写入 persisted 记录并 `recoverChild`）/ 已上报（无 id 或无启动标记）。永不自动结束进程。
- 漂移检测：仅 Linux，`readlink /proc/<pid>/exe`。以 ` (deleted)` 结尾判为 `deleted`；与当前 `execPath` 的 realpath 不同且为编译版判为 `mismatch`；dev 模式跳过。
- `RunnerStateSchema` 新增 `wrapperInventory {scannedAt, tracked, adopted, reported[≤200]}`。hub 对漂移会话使用 `DRIFT_HIBERNATE_IDLE_MS = 600_000`。

### P3 空闲 CPU（PR #3）
- 新增 `benchmarks/idle-wrapper/run.ts`：同一 commit 分别用 Bun 1.3.11 和 1.4.4 编译，跑 5 个空闲会话，先预热 60 s，再采样 10 分钟。
- 采样指标：`/proc/<pid>/stat` 的 CPU 时间、`/proc/<pid>/schedstat` 的唤醒次数、`smaps_rollup` 的 PSS。
- 对照组：空 `setInterval` 的 Bun 进程；以及关闭 scanner 的 wrapper（`HAPI_DEBUG_DISABLE_SCANNERS=1`，诊断完即删除）。
- 判定：两个 Bun 版本的空进程对照差 ≥0.2% CPU 或 ≥3 次唤醒/s，且 wrapper 的差异与之同步 → 锁定较好的 Bun 版本（`package.json` 和 CI）。否则修 HAPI 侧，候选修法：
  - `BaseSessionScanner` 在 watcher 健康时把轮询放宽到 30 s。
  - `startFileWatcher` 出错重启改为指数退避，上限 30 s。
  - agy 的 reactive poll 在空闲时清除。
- 不改 10 s keepalive。结论不明确时保持 bun@1.4.4。

### P4 惰性导入与 bytecode（PR #4）
- 先测量：每个 flavor 的冷启动（spawn 到 webhook）和常驻 PSS，作为基线。
- 惰性导入：
  - `commands/claude.ts` 里的 `./resume`（引入了 React/Ink）改为动态导入。
  - `RemoteLauncherBase`、`claudeRemoteLauncher` 里的 Ink/React 仅在有 TTY 时动态导入。
  - 各 `loop.ts` 的 LocalLauncher 仅在 local 模式下导入。
  - `runClaude` 对 `--existing-session-id` 跳过 SDK 元数据的子进程探测。
  - 新增守卫测试：加载 claude 命令时不得引入 codex/opencode/pi/cursor/hub/web 模块。
- bytecode：`build-executable.ts` 加 `--bytecode` 开关。`bootstrap.ts` 去掉顶层 await，改为静态导入 `preloadProdEnv`，再导入 `index`。如果编译失败，加 `--format=cjs` 再试；仍失败就放弃 bytecode。
- 保留门槛：任一指标改善 ≥10%，且另一项劣化 ≤5%。

## 裁决（分歧点）
| 点 | claude | antigravity | 采用 | 理由 |
|---|---|---|---|---|
| 停止顺序 | 直接 StopSession 加进程组清扫 | 先 KillSession 再 Stop | claude | KillSession 会让 wrapper 先退出，子进程被 PID 1 收养后逃过进程树清理，违反 S1 |
| CLI 写 archived 的竞争 | 结束后删除 archivedBy | 预先打标并保留 | 两者合并 | 先写 `stopping` 标记，结束后覆盖为 hibernated 并清除 archivedBy |
| 并发 | 每机每轮 1 个 | 每 tick 最多 3 个 | 每机 1 个 | 避免同时冲击 runner |
| 漂移窗口 | 10 min | 15 min（新增 env） | 10 min 常量 | 不新增配置项 |
| messages 路由自动 resume | 未提 | 是 | 采纳 | Telegram 和原生客户端发消息时不会预先 resume |
| 恢复后立刻再休眠 | 未提 | 发现该缺陷 | 采纳 | 进展时间要计入 `lifecycleStateSince` |

## PBT（INVARIANT → 证伪策略）
不新增依赖。用种子化 PRNG（mulberry32）生成随机样本，每个属性跑 500 次，失败时打印种子。
- INVARIANT 活工作不休眠：`hasLiveWork` 为真时，`shouldHibernate` 恒为假。随机组合所有活工作字段，`agentProgressAt=0`，`now=1e13`。
- INVARIANT 关闭开关：`hibernateMs<=0` 时恒为假，且 RPC 调用次数为 0。对 `'0'`、`'-1'`、`' 0 '`、`'abc'` 等随机字符串做 resolve。
- INVARIANT 时间单调：若 `shouldHibernate(now1)` 为真，则对任意 `now2≥now1` 也为真。
- INVARIANT flavor 白名单：白名单外的 flavor 恒不休眠。用已知 flavor 加随机模糊字符串。
- INVARIANT 恢复保 id：spawn 结果随机取相同 id、不同 id、错误、永不 active；结果要么是原 id，要么是 error，且 `mergeSessions` 从未被调用。
- INVARIANT 停止确认：标记 hibernated 当且仅当停止结果 ∈ {stopped, already_gone}。随机 4 种状态、throw、超时。
- INVARIANT 互斥：随机交错执行 hibernate、resume、archive；同一 id 最多一个在执行，终态 ∈ {active running, inactive hibernated, archived}。
- INVARIANT 空闲标签收敛器不改 hibernated：用随机时钟跑 `reconcileKeepaliveIdle`，hibernated 会话不受影响。
- INVARIANT 盘点划分：tracked + adopted + reported = 枚举数；adopted 一定同时有启动标记和会话 id。
- INVARIANT argv 提取：对 runner argv，`extractHapiSessionIdFromCommand(cmd)===id` 当且仅当 `commandMatchesRunnerSpawnedSession(cmd,id)`。用随机 token 顺序、`=` 与空格两种写法、`--resume <uuid>` 诱饵。

## Risks / Trade-offs
- 用 setsid 脱离进程组的 MCP 孙进程会逃过清扫。上线前要在 india-vps 上用 `ps -o pid,pgid,ppid,cmd` 验证，盘点会上报残留。
- 恢复首条消息要付冷启动延迟，并依赖 npm registry；只测量，不做预热。
- shared codex 运行时要等所有 root 都休眠后才释放内存。
- 老版本 runner 没有 `sessionHibernation` 能力，其会话永不休眠；hub 在日志中给出计数。
- P3 锁定旧 Bun 版本可能丢掉新版本的修复，切换前在锁定版本上跑全量测试。
