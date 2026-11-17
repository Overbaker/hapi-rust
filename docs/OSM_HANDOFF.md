# OSM Development Handoff Brief (India VPS Claude Lead)

## 1. Handoff Context & Goal
This repository (`Overbaker/hapi-rust`, deployable branch `deploy/rust`) is the dedicated high-performance Rust + WASM edition and comparison mirror forked from `tiann/hapi` (`37446bcf`).
Continuing development now shifts to **Claude on `india-vps`** acting as the **OpenSlime (`/osm:*`) Lead**, driving end-to-end Rust + WASM integration across the live Web/Hub/Runner stack (`/osm:research` -> `/osm:plan` -> `/osm:impl` -> `/osm:review`).

Note on mobile native apps: `ios/` and `android/` are intentionally out of scope for now per Boss direction.

---

## 2. Upstream PRs Already Submitted (Do Not Merge Upstream)
Four strictly non-overlapping TypeScript optimization PRs (plus `#2473`) are open against `tiann/hapi` (`base: main`):

| PR | Branch | Scope | Status at Handoff |
| --- | --- | --- | --- |
| [#2473](https://github.com/tiann/hapi/pull/2473) | `fix/static-gzip-and-auth-recovery` | Static asset compression & auth resilience (5 files) | All CI + Codex PR Review passed |
| [#2476](https://github.com/tiann/hapi/pull/2476) | `perf/web-runner-hub-memory-network` | Web cold bundle split, SW precache `133 -> 37`, KaTeX `.woff2` only `60 -> 20`, lazy Voice/Terminal mount, session guard | Final review fixes (`RealtimeSession.ts` attempt guard + hashed `.woff2` SW runtime route) pushed |
| [#2477](https://github.com/tiann/hapi/pull/2477) | `perf/cli-http-gzip-and-heartbeat` | `/cli` gzip (`56,480 B -> 3,681 B`, `15.34x`) & shared `@/api/keepAlive` `10 s` heartbeat | All CI + Codex PR Review passed |
| [#2479](https://github.com/tiann/hapi/pull/2479) | `perf/web-message-window-lru` | `MAX_RETAINED_INACTIVE_WINDOWS = 12` LRU eviction, `nextGenerationFloor`, limit check before JSON parse | All CI + Codex PR Review passed |
| [#2480](https://github.com/tiann/hapi/pull/2480) | `perf/hub-streaming-sse-and-sqlite` | Single SSE `JSON.stringify` fan-out, 60 s idle alive coalescing, keyset cursor pagination, live reasoning FTS skip, replenishing `O(1)` live tail cache | All CI + Codex PR Review passed |

---

## 3. Rust Mirror Architecture (`Overbaker/hapi-rust`)
Three modular PRs stand cleanly on `main`:
- **PR #1** (`rust/wasm-chat-engine`, https://github.com/Overbaker/hapi-rust/pull/1): `crates/hapi-core` + `crates/hapi-wasm` (`411 KB` stripped `wasm32-unknown-unknown` binary) + `web/src/wasm/` Worker & JS fallback bridge (`HapiWasmWorkerClient`).
- **PR #2** (`rust/hub-gateway`, https://github.com/Overbaker/hapi-rust/pull/2): `crates/hapi-hub-rs` (`3.8 MB` release binary): `tokio` + `axum` + `rusqlite` (`bundled`, WAL + mmap + zstd) + `O(1)` tail tracking + zero-copy `bytes::Bytes` SSE fan-out + `event: delta` text streaming + gzip responses.
- **PR #3** (`rust/runner-daemon-and-bench`, https://github.com/Overbaker/hapi-rust/pull/3): `crates/hapi-runner-rs` (`581 KB` release binary): single-process Tokio multi-session supervisor + 10 s coalesced heartbeats, `scripts/deploy-rust.sh`, `README.rust.md`, and `benchmarks/rust_vs_ts_bench.ts`.

Unified deployment branch `deploy/rust` merges all four TS optimization branches (`#2476/#2477/#2479/#2480`) + PR #1/#2/#3 + `openspec/config.yaml`.

### Verified Baseline Benchmarks (`benchmarks/rust_vs_ts_bench.ts`)
- **10-session idle runner memory (`VmRSS`)**: `1,091.10 MB` (10x Bun `apiSession.ts`) -> `2.29 MB` (`hapi-runner-rs`, **`477.47x` less RAM**)
- **10-session cold boot**: `1,540.75 ms` -> `23.18 ms` OS spawn / `0.06 ms` internal init (**`66.48x` faster**)
- **21 KB streaming Markdown (50 chunks)**: `1,202.96 ms` JS (`unified`+`remark-parse`+`remark-gfm`) -> `15.08 ms` `hapi_wasm.wasm` (**`79.78x` faster**, **`0 ms`** main-thread blocking in Worker)
- **Hub 100-chunk turn + 10-SSE fan-out (500-msg session)**: `102.05 ms` TS Baseline (`3,353,760 B` wire) -> `51.63 ms` TS-Optimized (`#2480`) -> `5.64 ms` `hapi-hub-rs --bench` (**`18.09x` faster** vs baseline) & `591,720 B` wire (**`5.67x` less bandwidth** via `delta` frames)
- **`/cli` 200-message sync gzip**: `56,480 B -> 3,681 B` (`15.34x` in TS `#2477` test payload) and `45,833 B -> 8,308 B` (`5.52x` in `hapi-hub-rs` loopback test)

---

## 4. Next-Phase OSM Roadmap for `india-vps` Claude Lead
Run `/osm:research` first in `/root/hapi-rust` (`deploy/rust`) to establish concrete OpenSpec constraints before planning and implementation:

1. **Web WASM Production Integration (`web/src/components/MarkdownRenderer*`, Diff ToolCard, `web/src/lib/message-window-store.ts`, `web/src/hooks/useSSE.ts`)**
   - Wire `HapiWasmWorkerClient` cleanly into live streaming Markdown blocks, large file diffs, and `event: delta` incremental SSE patch application, keeping feature-flagged/graceful JS fallback when WASM or Workers are disabled.
2. **End-to-End `hapi-hub-rs` & `hapi-runner-rs` Live Protocol Bridge**
   - Extend `crates/hapi-hub-rs` and `crates/hapi-runner-rs` with full HAPI CLI/Web auth, Socket.IO/WebSocket or HTTP+SSE bidirectional framing, session spawn/pty multiplex, and SQLite schema compatibility with `~/.hapi/hapi.db` so `scripts/deploy-rust.sh` can serve real sessions end-to-end.
3. **Production Benchmarking & Comparison Ledger**
   - Measure real Playwright / headless Chromium UI frame timings (streaming turn + fast scroll) and end-to-end `hapi-runner-rs` <-> `hapi-hub-rs` <-> Web latency against Baseline TS.

---

## 5. `india-vps` Operational Notes
- **GitHub identity**: `gh auth status` on `india-vps` is logged in as `AIR-HAN`. Before pushing branches back to GitHub from `india-vps`, either add `AIR-HAN` as collaborator on `Overbaker/hapi-rust` or fork via `gh repo fork Overbaker/hapi-rust --remote`. Do **not** overwrite upstream `tiann/hapi` PR branches unless explicitly instructed.
- **Tooling paths**: `cargo`, `rustc`, `rustup`, `bun`, `node`, `pnpm`, `claude`, `codex`, `agy`, `openspec`, `hapi`, `~/.claude/bin/osm` are installed and symlinked under `/usr/local/bin` on `india-vps`.
- **Clean tree rule**: `/osm:impl` requires `git status --porcelain` to be clean before dispatching worktree writers. Always verify clean git state or commit specs first.
