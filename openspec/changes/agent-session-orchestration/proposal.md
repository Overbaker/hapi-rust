## Why

在 india-vps 的生产 HAPI（0.36.x）上实测，HAPI 管理多个 agent 对话时，内存和 CPU 的大头来自"会话永不回收"，而不在单个进程的实现语言：

| 项 | 实测 |
|---|---|
| 会话 wrapper（23 个） | PSS 3.38 GB，均值 147 MB |
| claude agent（20 个） | PSS 3.89 GB，均值 194 MB |
| 每会话用户级 MCP（`npm exec fast-context-mcp` + `node`，各 20 个） | 0.84 GB，约 42 MB/会话 |
| 进程存活时长 | 最长约 12 天，多数会话空闲 |
| 空闲 wrapper CPU | 两次采样：均值 0.5% 到 2%，单个最高 3.5% |
| 孤儿进程 | 运行器重启后，wrapper 被 PID 1 收养；23 个 wrapper 仍跑在已删除的旧二进制上 |

现有机制只给会话打 `idle` 标签（12 h，`hub/src/sync/sessionIdle.ts:21`），不会停进程。resume 链路已端到端可用（`hub/src/sync/syncEngine.ts:4743`，Web 发送前会自动 resume）。所以"休眠空闲会话 + 按需恢复"是收益最大、改动最小的方向。

## What Changes

- **会话休眠**：会话连续 `HAPI_SESSION_HIBERNATE_MS` 只有 keepalive、没有 agent 进展（默认 2 h，`0` 关闭）后，hub 停掉该会话的 wrapper、agent 子进程和 MCP 子进程；下一条消息经现有 resume 路径恢复。有活工作的会话永不休眠：后台任务、待审批、子 agent、附加 job、thinking。复用 `hasLiveWork`。
- **按 flavor 准入**：只对能无损 resume 的 flavor 启用（claude、codex、opencode、pi 先行）。agy legacy 和 cursor 在实测证明可安全恢复前排除。
- **恢复延迟实测**：按 transcript 大小（1/5/25 MB）测 spawn 到首个事件的 p50/p95，其中包含用户 MCP 的 `npx --prefer-online` 启动。只有 p95 > 5 s 时才考虑预热。
- **空闲 CPU 回归定位**：用同一 HAPI commit 做 A/B（Bun 1.3.11 vs 1.4.4，空闲 10 分钟）。先定位是 Bun 运行时问题还是 HAPI 代码问题，再修。
- **孤儿与版本漂移**：运行器重启后，对每个存活 wrapper 要么接管跟踪，要么上报。跑在已删除或不匹配二进制上的 wrapper，可以经休眠+resume 升级。
- **不做**：不改写用户的 MCP 配置，也不做共享 MCP 代理（用户已确认）；不把多会话合进一个 wrapper；不用 Rust 重写 wrapper；不缩短 10 s keepalive。

## Capabilities

### New Capabilities
- `session-hibernation`：空闲判定、休眠、恢复的契约，以及按 flavor 的准入
- `session-process-hygiene`：孤儿 wrapper 接管/上报，二进制版本漂移处理
- `session-resource-metrics`：会话级 PSS/CPU、恢复延迟、空闲唤醒频率的测量与报告

### Modified Capabilities
- 无（`openspec/specs/` 为空）

## Impact

- hub：`sync/sessionIdle.ts`、`sync/sessionCache.ts`、`sync/syncEngine.ts`（resume、停止 RPC）。
- cli：`runner/run.ts`（spawn、停止、孤儿处理）、`runner/orphanReap.ts`，以及各 flavor launcher 的停止语义。
- web：会话列表要区分 hibernated 和 offline，并显示恢复中状态。
- 用户 MCP 配置不改。无新依赖。

## 约束集

**硬约束**
1. 不能修改 agent CLI 本身，只能控制 argv、env 和 `--mcp-config`/`--settings` 文件。
2. 不改用户 `~/.claude.json` 的 MCP；`--strict-mcp-config` 最多做成机器级 opt-in。
3. 有活工作的会话不得休眠；停止前必须确认进程确已退出（不是 still_alive/unknown）。
4. 恢复后 HAPI 会话 id 不得变化。peer 引用、`ping_peer`、`/sessions/<id>` 链接都依赖它；若现有 resume 会改 id 或合并会话，必须先修。
5. 不能无损 resume 的 flavor 一律不休眠。
6. wrapper 被设计为 detached，能活过运行器重启（`run.ts:870`）；休眠和清理必须兼容缺少新 RPC 的旧版 wrapper。

**软约束**：沿用 hub 现有的 env 配置风格（`HAPI_SESSION_IDLE_TIMEOUT_MS`）；tasks 用 `- [ ] X.Y` 格式。

## 成功判据

| # | 判据 |
|---|---|
| S1 | 会话空闲超过窗口后，该会话 0 个进程（wrapper、agent、MCP 子进程）；`HAPI_SESSION_HIBERNATE_MS=0` 时行为与现状一致 |
| S2 | 休眠后发消息能自动恢复：transcript 完整、会话 id 不变、回复正常 |
| S3 | 有后台任务、待审批、子 agent 或附加 job 的会话，在任意时长下都不休眠（PBT：随机生成活工作状态） |
| S4 | 本机 20 个 claude 会话场景，休眠后 HAPI 栈 PSS 的降幅按实测报告；按估算，15 个空闲约释放 5.7 GB |
| S5 | 空闲 wrapper 60 s 均值 ≤0.3% CPU，唤醒 ≤5 次/s（若 A/B 证明可修） |
| S6 | 运行器重启后，每个存活 wrapper 都处于"被跟踪"或"已上报"状态 |
| S7 | 恢复延迟 p50/p95 按 transcript 大小写进报告 |

## 风险

- 误休眠正在工作的会话会丢工作，靠 `hasLiveWork` 加停止确认来缓解。
- 恢复首条消息要多付冷启动延迟，还依赖 npm registry（用户 MCP 用了 `--prefer-online`）。
- 空闲 CPU 的根因尚未确认；先 A/B，避免修错方向。
- 第二路模型（antigravity）两次超时，Web 侧交互（休眠状态展示、恢复中提示）只来自 Claude 自己的核查，覆盖不足。

## 与 `rust-wasm-e2e-integration` 的关系

本变更先行，且不依赖前者。前者 G1（wrapper 内存 Rust 化）的门槛要在休眠上线后重新测。休眠回收后，常驻 wrapper 的数量和占比都会大幅下降，G1 很可能不再成立。
