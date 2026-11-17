# HAPI Rust + WASM Private Deployment Edition (`Overbaker/hapi-rust`)

This mirror repository adds a native Rust/WASM acceleration stack alongside the optimized TypeScript codebase for zero-jank UI streaming, zero-copy SSE fan-out, and minimal daemon memory overhead.

## Workspace Crates

- `crates/hapi-core`: Block-cached streaming Markdown renderer (`pulldown-cmark`), Myers unified diff engine (`similar`), and hash-indexed `O(1)` message window folder.
- `crates/hapi-wasm`: `wasm32-unknown-unknown` C-ABI WebAssembly binary (`web/public/wasm/hapi_wasm.wasm`) driven from a dedicated Web Worker (`web/src/wasm/hapiWasmClient.ts`) with automatic JS fallback.
- `crates/hapi-hub-rs`: `tokio` + `axum` + `rusqlite` (WAL + `mmap_size` + `zstd`) Hub gateway featuring `O(1)` live tail tracking, incremental `event: delta` streaming, zero-copy `bytes::Bytes` SSE fan-out, and gzip `/cli` & `/api` responses.
- `crates/hapi-runner-rs`: Single-process Tokio multi-session supervisor daemon managing concurrent sessions with `10s` coalesced heartbeats.

## Build & Test

```bash
cargo test --workspace
cargo build --workspace --release
cargo build -p hapi-wasm --target wasm32-unknown-unknown --release
bun benchmarks/rust_vs_ts_bench.ts
```

## One-Command Local Deployment

```bash
chmod +x scripts/deploy-rust.sh
./scripts/deploy-rust.sh
```
