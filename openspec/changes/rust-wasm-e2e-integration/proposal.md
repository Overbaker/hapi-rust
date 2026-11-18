## Why

`deploy/rust` 已有 Rust/WASM 资产（hapi-core/wasm、hub-rs、runner-rs）与微基准，但三者均未接入真实链路：`HapiWasmWorkerClient` 在 `web/src` 内零引用；hub-rs 仅 4 路由、无鉴权、无 Socket.IO（TS hub 约 174 路由 + `/cli` `/terminal` 命名空间）；runner-rs 只是不 spawn 进程的探针。现有 477x/66x/79x/18x 数字是微基准，不是端到端结果。本变更把它们接成可托管真实会话、可复现实测的整条链路。

## What Changes

**A. Web WASM 接入（antigravity 写 `web/src/components|hooks`，codex/claude 写 `crates/hapi-wasm` 与协议）**
- 流式 Markdown：仅对"正在流式的纯文本/段落块"走 Worker 中的 WASM；围栏代码（Shiki/CodeHeader）、KaTeX、Mermaid、FilePath/SessionPath 锚点、UriConfirmDialog 仍由 React 组件处理；流结束后回落完整 React 管线。WASM HTML 必须经净化后再注入（XSS 边界）。
- `hapi-wasm` 的 `STREAM_ENGINE` 改为按 streamId 键控并带显式释放；`HapiWasmWorkerClient` 补齐 5 个 worker 方法并加请求序号防过期结果覆盖。
- DiffView：小 diff（阈值待定，初定 <500 行/16KB）保持同步 `diffLines`；大 diff 交 Worker，输出需满足 DiffView 现有全文件双行号语义；修复 `fallbackComputeDiff` 的逐行下标比较。
- `message-window-store` 保持同步（`useSyncExternalStore` + `shared/fixtures/**` 一致性）；WASM `foldWindow` 仅用于大页批量合并（≥200 条），逐帧 SSE 路径保持 JS O(1)。
- SSE：`useSSE` 增加 `delta` 事件（新增 SyncEvent 变体 `message-delta` 并进入 `shared/src/schemas.ts`，或 `addEventListener('delta')`，以 feature flag 控制）；hub-rs 帧必须带 `type` 判别符、`connection-changed` 握手与 JSON heartbeat。
- Worker 的 wasm 加载使用 `BASE_URL`/打包资源 URL + `instantiateStreaming`，wasm 带内容 hash 并进入 SW precache（避免新 JS 配旧 wasm）。
- 全程 feature flag，默认在 Worker/WASM 失败、404、MIME 错误时回落 JS，既有 web vitest 全部零回归。

**B. Hub/Runner 实连桥接（codex/claude）**
- hub-rs 与 `~/.hapi/hapi.db`（`user_version=36`，19 表）的兼容：读取需支持 TS `contentCodec`（<256 字符 TEXT、其余无头 zstd BLOB）；`user_version!=36` 一律拒绝打开；禁止写入 Rust 私有 `0x00/0x01` 前缀 BLOB。
- 鉴权对齐：`CLI_API_TOKEN[:namespace]` 常数时间比较、`POST /api/auth` HS256 JWT `{uid,ns}`、`/cli` 与 `/api` 401；鉴权必须先于任何 spawn/PTY 路由落地。
- 以"缺口清单"驱动范围（对照 `hub/src/web/routes/*.ts` 与 `shared/src/socket.ts` 打勾），范围外路由由 TS hub 提供，不追求"全量对等"。
- runner-rs 由探针升级为真实 supervisor：向 hub 注册 machine、处理 `rpc-request` spawn、监管真实 `hapi claude|codex|pi|opencode` 子进程。
- `scripts/deploy-rust.sh` 补齐：启动 runner、托管 `web/dist`、可选使用真实 `~/.hapi/hapi.db`。

**C. 端到端实测报告**
- 新增 Playwright(headless Chromium，`--no-sandbox`) 度量脚本 + 扩展 `benchmarks/rust_vs_ts_bench.ts`：LCP（500 消息会话）、100 chunk 流式 p50/p95/p99 帧时与长帧数、2k 消息滚动帧时、10+ 会话 hub+runner+子进程总 RSS 与浏览器 JSHeapUsedSize、TTFB 与 prompt→首 token、SSE 字节数（full vs delta）。
- 三组基线：上游 `tiann/hapi@37446bcf`、TS+#2476/#2477/#2479/#2480、Rust；输出 JSON+Markdown，含硬件与 commit 元数据；一条命令复现。

## Capabilities

### New Capabilities
- `stream-delta-protocol`: CLI→hub→Web 流式文本增量（`message-delta`），取代 250ms 全量快照
- `hub-read-path-efficiency`: 消息页 JSON 拼接免解析、终端环形缓冲
- `web-offmain-rendering`: Diff/Markdown 尾块/页面水合移出主线程，大 diff 走 Worker(+WASM)，带回落
- `native-hook-forwarder`: 短命子进程（Claude hook、`hapi job`）的 Rust 原生转发器
- `perf-measurement-ledger`: 先测量后移植的方法学、kill criterion、真实栈 PSS 与浏览器度量、报告

### Modified Capabilities
- 无

## 第一性原理重定向（/osm:plan 阶段，Boss 指令）
Boss 明确：重写不是目的，只在 Rust/WASM 能带来可测量巨大收益处使用。两路独立分析一致认为多数巨大收益来自**架构**（delta 增量、免解析拼接、Worker 化、memoize），不需要 Rust；Rust 只在下列处成立：短命进程冷启动（hapi-hook，~25x）、大 diff WASM（门槛 ≥500 行/16KB）、以及经测量后才决定的 hub 只读 sidecar / CLI wrapper 内存。上文 A/B 中的"hub-rs 完整接管、runner-rs 真实 supervisor"降级为 **gated**（Phase 3，须过 kill criterion），D1–D4 作废，由 design.md 的 ROI 表与 Phase 0 实测取代。

## Impact

- 代码：`crates/hapi-{wasm,hub-rs,runner-rs}`、`web/src/{wasm,components,hooks,lib}`、`web/vite.config.ts`、`web/src/sw.ts`、`shared/src/schemas.ts`、`scripts/deploy-rust.sh`、`benchmarks/`、`e2e/`。不触碰 `ios/`、`android/`。
- 新增依赖（需 plan 阶段确认）：`socketioxide`、`jsonwebtoken`、`tower-http`、`subtle`、`portable-pty` 或 `tokio::process`。
- 跨界顺序风险：`SyncEvent` 变更触及 shared 与 web，必须最先落地；`fixtures` 受 CI 漂移检查，改动后需 `bun run gen:fixtures`。

## 约束集（供 /osm:plan 使用）

**硬约束**
1. 任何情况下不得向 `~/.hapi/hapi.db` 写入 TS hub 不可读的内容；`user_version!=36` 拒绝启动。
2. 无鉴权不得暴露 spawn/PTY；默认仅绑定 loopback。
3. 浏览器 `EventSource.onmessage` 收不到命名事件，hub-rs 必须发默认帧或 web 必须 `addEventListener`。
4. CLI 经 Socket.IO `/cli`（仅 websocket）发消息，不走 HTTP POST；真实 CLI 要连 hub-rs 必须有 Socket.IO v4 服务端。
5. `message-window-store` 必须同步并保持 `shared/fixtures/**` 无漂移。
6. WASM 单例流状态必须按 streamId 隔离；WASM HTML 必须净化。
7. 上游 tiann/hapi 的 PR 不合并；`ios/`、`android/` 不动。

**软约束**：4 空格缩进、TS strict、`@/*` 别名；tasks 用 `- [ ] X.Y` 格式；Rust/协议→codex/claude，web 组件→antigravity。

**默认假设（未向 Boss 确认，plan 阶段须复核）**
- D1 hub 策略：Rust hub 采用"渐进接管"——先承接读热路径（`/api/events` SSE、消息分页、静态资源），其余 `/api`、`/cli` 反代到 TS hub；TS hub 仍是 `hapi.db` 的唯一写入者。备选"完整替换"（约 174 路由 + ~30 socket 事件，规模约 10x）。
- D2 runner 策略：runner-rs 监管现有 TS `hapi` CLI 子进程，不在 Rust 重写 4 个 flavor 适配器（约 15k+ LOC）。
- D3 DB 共享：默认使用独立 v36 兼容 DB，对真实 `~/.hapi/hapi.db` 仅只读打开，直至兼容测试通过。
- D4 基线：同时报告上游与 TS-optimized 两组。

## 成功判据（可验证）

| # | 判据 | 验证 |
|---|---|---|
| S1 | WASM 关闭/Worker 不可用/wasm 404 时，现有 web vitest 全绿 | `cd web && bunx vitest run` |
| S2 | 100 chunk 流式 p95 帧时 ≤16.7 ms，且主线程长任务（>50ms）为 0 | Playwright trace |
| S3 | hub-rs 帧全部通过 `SyncEventSchema`；无 delta 能力的客户端仍收到 `message-received` | shared 契约测试 |
| S4 | 未改动的 `cli/src/api/apiSession.ts` 对 hub-rs 完成 connect→session 创建→`message`→`update-metadata` ack | 协议一致性测试，与 TS hub 结果一致 |
| S5 | 错误/缺失 token → 401；Socket 握手失败 | 单测 |
| S6 | hub-rs 写出的 DB 由 TS hub `bun test` 交叉读取一致 | hub 交叉读测试 |
| S7 | `deploy-rust.sh` 起栈后，Playwright 登录→spawn 真实（或 stub flavor）会话→发 prompt→渲染流式回复与工具 diff | e2e |
| S8 | 一条命令产出 JSON+MD 报告，含 6 类指标与三组基线、硬件、commit | `bun benchmarks/...` |

数值门槛（如 LCP、RSS 倍数）在 plan 阶段用首轮实测基线填定，不预先许诺 10x–100x。

## 风险

- 工作量：hub-rs 对 TS hub 约 172/174 路由、0/~30 socket 事件缺失，低估是首要进度风险。
- 过度宣称：477x 比较的是"仅 import apiSession 的 Bun 进程"与"不做事的 Rust 任务"；真实 runner 内存由子 agent 进程主导。报告必须同时给出含子进程的总 RSS。
- hub-rs 直连真实库会写入 BLOB、跳过 `delivery_state` 与内容检索表，破坏 TS hub；直播 reasoning 的 DELETE+INSERT 会扰动 seq 与 keyset 游标。
- 两 hub 同写 WAL 库导致 seq 竞争（内存尾 seq 失效）。
- WASM Markdown 与 remark/rehype 渲染分叉；需快照对比。
- 环境：`target/release/hapi-*` 未构建；基线仓库 `../hapi`、`../hapi-perf-hub-streaming-sse` 在本机不存在；root 下 headless Chromium 需 `--no-sandbox`。

## 阻塞与待 Boss 确认

1. D1–D4 默认假设是否认可（尤其 D1 渐进接管 vs 完整替换，决定工期量级）。
2. 新增 Rust 依赖清单（socketioxide 等）。
3. 在 india-vps 上准备两个 TS 基线工作树（上游 37446bcf、TS-optimized）。
