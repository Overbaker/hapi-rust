## Context
两路独立分析（claude=后端，antigravity=Web）一致：多数大收益来自架构，不是重写。本设计按 ROI 排序，并给每项测量方法与 kill criterion；未达门槛即放弃，保留 TS。

## Goals / Non-Goals
- Goals：流式字节与 CPU 随流长度由 O(n²) 降为 O(n)；Web 主线程无 >50ms 长任务；短命进程冷启动 ≤10ms；真实栈（含 agent 子进程）内存归因诚实。
- Non-Goals：不重写 4 个 flavor 适配器；不完整替换 TS hub；不改 ios/android；不合并上游 PR；不承诺 10x–100x。

## Decisions（零待定）
| ID | 决策 | 参数 | 落点 |
|---|---|---|---|
| D-A | 增量协议 | `message-delta`：`{sessionId, messageId, baseSeq, offset, text}`；offset 为 UTF-16 码元前缀长度；不连续即回落尾部同步；live 增量不写 SQLite，仅内存 live buffer，终态落一行 | CLI `AcpMessageHandler.ts`、`piMessageAccumulator.ts`；hub `sessionHandlers.ts`、`sseManager.ts`；`shared/src/schemas.ts`；web `useSSE.ts`、`message-window-store.ts` |
| D-B | 消息页拼接 | 存储 JSON 文本直接拼进信封；大小估计用预计算字节长度 | `messageService.ts`、`store/messages.ts` |
| D-C | 终端环形缓冲 | 块数组 + 字节计数，上限沿用 256K | `userTerminalBuffer.ts` |
| D-D | Diff | <500 行且 <16KB 同步 `diffLines`；≥ 阈值 Worker；Worker 内先用 JS，WASM `similar` 仅当 JS p95 ≥12ms；请求序号丢弃过期结果；修 `fallbackComputeDiff` 为 Myers | `DiffView.tsx`、`web/src/wasm` |
| D-E | Markdown | 块前缀 memo + rAF 合并（≈30Hz）；WASM 仅当此后 p95 帧时仍 >12ms；启用 WASM 前 `STREAM_ENGINE` 必须按 streamId 键控并带释放；WASM HTML 必须净化；围栏代码/KaTeX/Mermaid/自定义链接仍走 React | `MarkdownRenderer.tsx`、`markdown-text.tsx`、`crates/hapi-wasm` |
| D-F | 页面水合 | 仅当主线程 parse+normalize p95 ≥25ms 才进 Worker；Worker 复用同一 `normalizeDecryptedMessage`，不动 fixtures | `api/client.ts` |
| D-G | hapi-hook | 新 crate `crates/hapi-hook`，仅 std（无新依赖），经 loopback HTTP/UDS 转发；缺失二进制回落 TS | `cli/src/claude/utils/sessionHookForwarder.ts`、`cli/src/commands/job.ts` |
| D-H | wasm 加载 | `BASE_URL`+内容 hash 文件名、`instantiateStreaming`、进 SW precache | `hapiWasm.worker.ts`、`vite.config.ts`、`sw.ts` |
| D-I | 新 Rust 依赖 | 本阶段**不新增**（socketioxide/jsonwebtoken/portable-pty 不引入，对应 gated 项未获准入） | — |

## ROI 排序与 kill criterion
| 排序 | 项 | 预期 | 先测什么 | Kill |
|---|---|---|---|---|
| 1 | D-A 增量 | 100 快照 6KB 流：~300KB→~6KB（压缩前），gzip 后收益较小 | 回放真实 ACP/Claude 轨迹过真实 SSEManager+gzip | gzip 后字节降 <2x 且 hub CPU 降 <30% |
| 2 | D-B 拼接 | hub CPU/峰值堆 −40–70%（500 消息页） | `bun --cpu-prof` 种子库 500/2000 条 | parse+stringify <30% 处理时间，或 p95 <50ms |
| 3 | D-D 大 Diff | 大 diff 主线程 0 长任务；挂载/布局 −85%（预览切片+hunk 折叠） | Chromium trace | JS Worker p95 <12ms 则不上 WASM |
| 4 | D-E Markdown | 流式主线程 JS −80–90% | 100 chunk 帧时 | memo 后 p95 ≤12ms 则不上 WASM |
| 5 | D-G hapi-hook | 冷启 ~65ms→~2ms；RSS 60→2MB | hyperfine 200 次 | `sh`+`curl`/常驻端点 ≤10ms 则放弃 |
| 6 | D-C 终端 | 洪泛下事件循环延迟→~0 | `yes` 洪泛 + setInterval 漂移 | p99 <20ms 则放弃 |
| 7 | D-F 水合 | 切会话 −20–70ms 卡顿 | 主线程 parse 计时 | <25ms p95 则放弃 |
| G1 | wrapper 内存 Rust 化 | 10 会话省 ~0.4–0.8GB（仅当 wrapper PSS 50–90MB） | `/proc/<pid>/smaps_rollup` PSS | wrapper PSS 之和 <栈总 25%，或惰性导入使其降 ≥40% |
| G2 | hub-rs 只读 sidecar | C1/C2 后预期 <2x | 50 SSE + 10 流会话压测 | 3x 峰值下 p99 <20ms 且 CPU <25% 单核 |
| G3 | runner-rs 真实 supervisor | ~40–80MB 一次性，无延迟收益 | runner PSS 占比 | <5% 则放弃（预期） |

**不值得移植**：zstd 编解码（同为 libzstd）、SQLite 访问（已原生）、FTS5、SSE 序列化与 gzip（已单次序列化+原生 zlib）、Socket.IO 帧、JSONL/ACP 解析（速率 KB/s）、`message-window-store` 逐帧折叠（JS 已 O(1)，WASM 只多一次边界拷贝）、把大页 JSON.parse 移入 WASM（结构化克隆回主线程成本相当）。Rust 无法改善：agent 子进程内存/启动、LLM 首 token、RTT、DOM 布局。

## 现有 Rust 资产处置
`hapi-hub-rs`、`hapi-runner-rs` 保留为微基准与 G2/G3 的候选，不作为真实会话路径；其 DB 写入格式（`0x00/0x01` 前缀 BLOB）禁止指向真实 `hapi.db`（硬门：`user_version!=36` 拒绝，且只读）。

## Risks / Trade-offs
- 增量协议改动横跨 CLI/hub/shared/web，须先落 shared schema；fixtures 变更需 `bun run gen:fixtures`。
- Rust 路径收益可能被基线反驳——这正是 kill criterion 的作用。
- 基线工作树 `../hapi`、`../hapi-perf-hub-streaming-sse` 本机缺失，Phase 0 先建。

## PBT 属性（INVARIANT / PROPERTY）
- INVARIANT delta 折叠：对任意文本与任意切分，fold(base, deltas)==最终文本；幂等（重复 delta 不改结果）；乱序/缺口触发回落而非错文本。策略：fast-check 生成文本含 emoji/代理对/CRLF，随机丢/重/乱序帧。
- INVARIANT 拼接等价：对任意消息集合，拼接响应 JSON 深度相等于 parse→stringify 响应。策略：随机嵌套 JSON、截断、TEXT/zstd 混合。
- INVARIANT 终端环：任意 chunk 序列后，缓冲内容==全部输入拼接的末 N 字节；长度 ≤ N。
- INVARIANT diff：equal+del 重建 A，equal+add 重建 B；行号单调；结果与 JS `diffLines` 在行集合上等价。过期响应不覆盖新结果。
- INVARIANT markdown：任意累积切分终态 DOM == 全量渲染 DOM；净化后无 script/iframe/object/embed/on* 属性。
- INVARIANT 跨运行时 codec（若 G2 启用）：TS 写、Rust 读与反之往返相等；`user_version!=36` 一律拒绝。
- BOUND hook：同一输入下 TS 与 Rust 转发器的输出字节、退出码相同。
