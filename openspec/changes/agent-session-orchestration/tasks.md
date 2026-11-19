## 1. PR #0 规格文档
- [ ] 1.1 提交 `openspec/changes/agent-session-orchestration` 与 `openspec/changes/rust-wasm-e2e-integration` 到独立分支并开 PR（base `deploy/rust`）

## 2. PR #1 会话休眠
- [x] 2.1 shared：`SESSION_LIFECYCLE_HIBERNATED`、`isHibernatedLifecycleState`，`MetadataSchema.hibernation`，`RunnerState.capabilities.sessionHibernation`
- [x] 2.2 hub `sessionIdle.ts`：`resolveSessionHibernateMs`、`LiveWorkSignals`、导出 `hasLiveWork(session, signals?)`、`shouldHibernate`、`HIBERNATE_ELIGIBLE_FLAVORS`；单测与 PBT（活工作、关闭开关、单调、flavor）
- [x] 2.3 hub 信号接线：`getAgentProgressAt` 公开并计入 `lifecycleStateSince`、`hasPendingThinking`、`countUndeliveredForSession`、`setTerminalSessionCounter`、附加 job 与 teamState 检查
- [x] 2.4 hub `SyncEngine.hibernateSession`/`hibernateIdleSessions`：stopping 标记、抽出 `confirmRunnerStop`（archive 行为不变）、退避、跳过 dedup、`markSessionHibernatedFromHub`、启动时收敛；`syncEngineHibernate.test.ts`（含互斥与停止确认 PBT）
- [x] 2.5 hub 恢复：`fromHibernation` id 不同则拒绝合并；`POST messages` 自动 resume；定时消息到期唤醒；archive 路由允许 hibernated；resume 时 `recordAgentProgress`；`syncEngineResumeHibernated.test.ts`
- [x] 2.6 cli runner：`sweepProcessGroup`（进程组 SIGTERM→5s→SIGKILL，shared codex 有活跃 root 时跳过）、声明 `sessionHibernation` 能力；`run.stopSession` 测试
- [x] 2.7 cli `pingPeer`：hibernated 目标无需 wake 即可自动恢复；测试
- [x] 2.8 web：已休眠徽标、Active 筛选、快速归档、唤醒中状态、`sessionReference`/`ProjectGroupRow`、三语 i18n；组件测试
- [x] 2.9 验证：`bun typecheck && bun run test`；自审 diff；提交并开 PR

## 3. PR #2 孤儿与漂移
- [ ] 3.1 `extractHapiSessionIdFromCommand` 与测试（含 argv PBT）
- [ ] 3.2 `wrapperInventory.ts`：枚举、分类、接管、漂移检测、10 分钟周期；测试（含划分 PBT）
- [ ] 3.3 `RunnerStateSchema.wrapperInventory` 与上报；hub 对漂移会话使用 10 分钟窗口；测试
- [ ] 3.4 验证并开 PR

## 4. PR #3 空闲 CPU
- [x] 4.1 `benchmarks/idle-wrapper/run.ts` A/B 脚本，包含三个对照组
- [x] 4.2 在 aedas-me WSL2 跑受控 A/B，并在 india-vps 对三个现有进程做只读采样，把结论写入 `openspec/changes/agent-session-orchestration/reports/idle-cpu.md`
- [ ] 4.3 按判定规则二选一：锁定 Bun 版本，或修 scanner/watcher/agy 轮询；附测试；agy/S5 仍未达标，保持开放
- [ ] 4.4 验证并开 PR

## 5. PR #4 惰性导入与 bytecode
- [ ] 5.1 基线：各 flavor 的冷启动与 PSS
- [ ] 5.2 惰性导入（resume/Ink/LocalLauncher/SDK 探测）与 import 图守卫测试
- [ ] 5.3 `--bytecode` 开关与 bootstrap 去除顶层 await；构建验证
- [ ] 5.4 复测并按门槛保留或回退，把报告写入 `reports/startup.md`
- [ ] 5.5 验证并开 PR

## 6. 收尾
- [ ] 6.1 `/osm:review` 交叉审查四个 PR，处理 Critical 问题
- [ ] 6.2 恢复延迟测量（1/5/25 MB transcript 的 p50/p95），写入 `reports/resume-latency.md`
