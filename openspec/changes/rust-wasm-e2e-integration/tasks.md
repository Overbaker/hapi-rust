## 0. Phase 0 测量与基线（先行，门控后续所有项）
- [ ] 0.1 在 india-vps 创建两个基线工作树：上游 `37446bcf` 与 TS-optimized（deploy/rust 的 #2476/#2477/#2479/#2480），均可 build/test
- [ ] 0.2 `cargo build --workspace --release` 与 wasm 构建通过并记录产物大小（已验证 release 构建通过，需纳入脚本）
- [ ] 0.3 种子数据脚本：500/2000 消息会话（含 64KB 截断工具结果、TEXT 与 zstd 混合）与一条真实 ACP/Claude 流式轨迹回放器
- [ ] 0.4 Playwright(headless Chromium, `--no-sandbox`) 度量脚本：LCP、100 chunk 帧时 p50/p95/p99 与 >50ms 长任务数、2k 消息滚动帧时、会话切换主线程 parse 耗时
- [ ] 0.5 hub 侧度量：`bun --cpu-prof` 消息页分解（decode/parse/stringify）、SSE 字节（gzip 前后）、`yes` 洪泛事件循环延迟、50 SSE+10 流会话负载
- [ ] 0.6 栈内存度量：10 真实会话的 runner/wrapper/agent 子进程/hub 的 `smaps_rollup` PSS 与占比
- [ ] 0.7 hapi-hook 冷启基线：hyperfine 200 次 TS hook 与 `sh`+`curl` 对照
- [ ] 0.8 产出 `baseline.md` 并据 design.md 门槛逐项填写 go/no-go（后续 Phase 仅实施 go 项）

## 1. Phase 1 架构优化（TS，无 Rust）
- [ ] 1.1 `shared/src/schemas.ts` 新增 `message-delta` SyncEvent 与能力声明；跑 `bun run gen:fixtures` 并包含生成变更
- [ ] 1.2 CLI 增量发送（`AcpMessageHandler.ts`、`piMessageAccumulator.ts`）与 hub 内存 live buffer、`sessionHandlers.ts`/`sseManager.ts` 广播与旧客户端全量回落
- [ ] 1.3 Web `useSSE.ts`/`message-window-store.ts` 折叠 delta，缺口回落尾部同步；PBT：折叠等价/幂等/乱序
- [ ] 1.4 hub 消息页拼接免解析（D-B）与等价 PBT
- [ ] 1.5 终端环形缓冲（D-C）与洪泛测试
- [ ] 1.6 Markdown 尾块 memo + rAF 合并（D-E，不含 WASM），切分等价 PBT 与净化测试
- [ ] 1.7 DiffView 预览切片、hunk 折叠与单次 memo；修 `fallbackComputeDiff` 为 Myers；大 diff 请求序号防过期
- [ ] 1.8 页面水合 Worker 化（仅 0.8 判定 go 时）

## 2. Phase 2 Rust/WASM（仅 go 项）
- [ ] 2.1 `crates/hapi-hook`：仅 std，loopback/UDS 转发，字节与退出码等价测试，缺失回落 TS（仅 0.8 go）
- [ ] 2.2 `hapi-wasm` `STREAM_ENGINE` 按 streamId 键控并带释放；`HapiWasmWorkerClient` 补齐 5 个方法
- [ ] 2.3 wasm 加载：`BASE_URL`+hash 文件名、`instantiateStreaming`、SW precache、MIME 校验
- [ ] 2.4 大 Diff 走 Worker（先 JS，仅 JS p95≥12ms 时切 WASM `similar`），feature flag 与回落测试
- [ ] 2.5 Markdown WASM 仅当 1.6 后 p95>12ms：净化后注入 + 与 remark 快照对比

## 3. Phase 3 Gated（仅测量越过门槛时）
- [ ] 3.1 G1 wrapper 内存：先做惰性导入/compile；再按 0.6 判定是否 Rust 化，否则写"不实施"结论
- [ ] 3.2 G2 hub-rs 只读 sidecar：仅 0.5 负载门槛触发；`user_version!=36` 拒绝、只读打开、跨运行时 codec PBT
- [ ] 3.3 G3 runner-rs：占比 <5% 则记录不实施

## 4. Phase 4 报告与部署
- [ ] 4.1 `benchmarks/` 一条命令输出 JSON+MD（三组基线、硬件、commit、PSS 含子进程、微基准与端到端分列）
- [ ] 4.2 `scripts/deploy-rust.sh` 按实际启用的组件更新；`bun typecheck && bun run test`、`cargo test --workspace` 全绿
- [ ] 4.3 以 Overbaker 身份推送分支到 origin 并提 PR，更新 deploy/rust
