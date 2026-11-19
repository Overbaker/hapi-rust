## Why

Each HAPI session wrapper (`hapi <flavor> --started-by runner`) is a full Bun runtime. Measured on india-vps:
- Average PSS per wrapper is about 147 MB. An empty Bun process is 10 MB; loading the claude code graph alone takes it to 98 MB.
- A single wrapper idles at 0.5–2% CPU.
- On top of the wrapper, each session also spawns short-lived or resident Bun child processes: codex `hapi mcp` bridges and claude `hapi hook-forwarder`.

The user has decided to rewrite the wrapper in Rust anyway, in waves, while keeping the TS wrapper as a fallback. Target: per-wrapper resident memory of a few MB, near-zero idle CPU, and wire protocol and user-visible behaviour identical to TS.

Gain caveat: once P1 (idle hibernation) is live, only sessions that are active or inside their hibernation window keep a wrapper. The net gain from the rewrite is therefore roughly "number of resident wrappers × ~130 MB". Each wave must report measured numbers.

## What Changes

New Rust workspace members:
- `crates/hapi-wrapper-core`: shared core.
  - Socket.IO v4 / Engine.IO 4 client: WebSocket only, JSON parser.
  - Bootstrap REST calls and the runner webhook.
  - Session client with the same semantics as `apiSession.ts`: CAS metadata/state, acks, volatile keepalive, reconnect + backfill.
  - MessageQueue2 semantics and permission base class.
  - Common RPC handlers.
  - Streamable-HTTP MCP and hook HTTP servers.
  - `mcp` stdio bridge and `hook-forwarder` subcommands, so no hidden Bun processes remain.
  - Lifecycle: setsid, SIGTERM → session-end → kill the agent tree, verified process-group cleanup.
- A single binary `hapi-session-wrapper` with three flavor modules:
  - **claude**: stream-json SDK query, permission-mode emulation (same as TS: plan/auto native, acceptEdits/bypass emulated), ExitPlanMode / AskUserQuestion, SDK → HAPI message conversion (uuid chain unchanged).
  - **codex**: fully compatible with the TS shared runtime. Same app-server, gateway socket, `~/.hapi/codex-runtimes/*.json` registry and ownership-lock format; interoperates with the TS runner stop path (sibling-session protection). `/new` and fork behave as today. Requires codex ≥ 0.154.
  - **agy**: ACP path only (agy ≥ 1.5.0); transcript watching by inotify, no fixed-interval polling. Legacy stream-json is not ported; it falls back to TS.

Runner (TS) changes:
- Rust wrapper chosen per flavor via `HAPI_RUST_WRAPPER_FLAVORS` (e.g. `claude,codex,agy`; empty or `none` means all TS, an instant rollback).
- Binary resolution: `HAPI_RUST_WRAPPER_BIN`, then the same directory as `hapi`, then `target/release` in dev mode.
- Automatic fallback to TS when: the binary is missing, the platform is not Linux, the agent version is too old, or the wrapper fails before the webhook (one retry with TS).
- `metadata.wrapperRuntime: 'rust'|'ts'` is recorded.
- Wrapper inventory, orphan reaping and drift detection recognise both `hapi` and `hapi-session-wrapper`, each compared against its own binary's dev+ino. No change to the argv contract (`buildCliArgs`).

Delivered in waves, each with its own PR:
- **1a**: shared core + claude (20 of 23 resident wrappers in the production sample are claude).
- **1b**: codex.
- **1c**: agy.

## Capabilities

### New Capabilities
- `rust-session-wrapper`: wire and lifecycle contract of the Rust wrapper, behavioural equivalence with the TS wrapper, per-flavor switch and fallback.
- `wrapper-parity-harness`: record/replay differential tests (fake agent stdio plus a fake or real hub `/cli`), golden decoding, socket transcript diffing.

### Modified Capabilities
- None. `session-process-hygiene` and the hibernation specs are not yet archived into `openspec/specs/`; their changes are covered by this change's spec deltas.

## Impact

- New `crates/hapi-wrapper-core` and the binary `hapi-session-wrapper`.
- Changes to `cli/src/runner/{run.ts, wrapperInventory.ts, orphanReap.ts}` and `cli/src/utils/spawnHappyCLI.ts`.
- `shared/src/schemas.ts`: `metadata.wrapperRuntime`.
- `scripts/deploy-rust.sh` builds the binary.
- New Rust dependencies, confirmed in plan: tokio, tokio-tungstenite + rustls (native roots), serde/serde_json, hyper or axum (MCP/hook HTTP), nix (setsid/signals/inotify), with the Socket.IO protocol subset hand-written. Avoid immature crates such as rust_socketio and rmcp.
- Out of scope: `ios/`, `android/`, Windows, macOS, and flavors other than claude/codex/agy.

## Constraint set

**Hard constraints**
1. Agent CLIs are not modified. Only argv, env, `--mcp-config` and `--settings` are under our control. The user's MCP config is not modified.
2. Wire protocol identical: Socket.IO v4 over WebSocket only, default JSON parser, auth `{token, clientType:'session-scoped', sessionId}`, `HAPI_EXTRA_HEADERS_JSON`, ack timeouts, versioned CAS metadata/state, update acks `{removed,inFlight,indeterminate,accepted,consumed}`.
3. argv byte-compatible with `buildCliArgs`; webhook metadata `hostPid` / `hostStartMarker` formatted exactly like `getProcessStartMarker` (`ps -o lstart=` with `LC_ALL=C TZ=UTC`).
4. Must outlive runner restarts (detached, own process group). On SIGTERM it must finish session-end and kill the agent tree within the runner's stop window (hibernation 90 s; group sweep SIGTERM → 5 s → SIGKILL).
5. Hibernation compatible: claude and codex expose the same resume-id metadata and exit cleanly on StopSession. Cross-implementation resume, TS → Rust and Rust → TS, must keep the transcript and session id intact.
6. The codex registry, gateway and lock formats are exactly the TS ones. Any deviation breaks sibling-session protection.
7. Message ids and projection ids are stable: claude uuid chain, codex projection. Switching implementation mid-session must not duplicate or lose messages.
8. claude remote permission mode matches TS: plan and auto are passed through natively (`nativePermissionMode`, `claudeRemote.ts:59-61`), and only acceptEdits and bypassPermissions are mapped to `default` and emulated inside can_use_tool. Correction (plan stage): the earlier wording "always default" was a mistake.
9. Every emitted message decodes through web `normalizeDecryptedMessage` exactly like the TS output. Unknown types are allowlist-filtered.
10. No hidden Bun processes: the MCP bridge, hook-forwarder and model probe are handled by Rust subcommands or caches.
11. Mixed TS and Rust wrappers coexist on one machine. A Rust wrapper missing a new RPC degrades the same way an old TS wrapper does.

**Soft constraints**: TS strict, 4-space indentation; tasks use `- [ ] X.Y`; Rust style follows the existing crates (release lto, strip); each Rust module ports the corresponding TS unit tests.

## Success criteria

| # | Criterion |
|---|---|
| S1 | Idle Rust wrapper PSS ≤ 15 MB (claude/agy). codex is reported per wrapper and per hosted session, and the per-session total includes all HAPI helper processes. Today: 147 MB. |
| S2 | Idle wrapper 60 s mean ≤ 0.1% CPU and ≤ 1 wakeup/s, excluding the 10 s keepalive and the Engine.IO ping. |
| S3 | Recorded scenario corpus (plain turn, tool + approve/deny, AskUserQuestion, plan mode, steer, queue cancel, interrupt, model/effort switch, attachment, compact, fork, kill, SIGTERM, disconnect/reconnect + backfill): normalised Rust and TS socket transcripts are identical. |
| S4 | Golden decoding: all Rust output passes web decoding identically to TS. Existing `shared/fixtures` show no drift. |
| S5 | The runner treats Rust wrappers as tracked or adopted, never as drift. StopSession returns `stopped` with 0 surviving processes in the group. |
| S6 | Hibernation round trip, Rust → TS and TS → Rust: session id unchanged, transcript intact, no merge. |
| S7 | Fallback: missing binary, old agent version, non-Linux or pre-webhook failure all spawn TS transparently. `HAPI_RUST_WRAPPER_FLAVORS=` rolls back without a hub restart. |
| S8 | spawn → webhook p50 not slower than TS. The existing CLI integration tests and the web live e2e pass with the Rust wrapper enabled. |

## Risks

- Large scope: the two estimates differ (~10k vs ~27–32k lines of Rust including tests). Plan sizes and budgets each wave separately.
- Dual maintenance against upstream churn: over the last 90 days codex/modules/claude/agy had 164/117/106/51 commits. Without a conformance corpus, Rust silently falls behind. Ongoing alignment cost is a long-term tax.
- Socket.IO semantic differences (volatile drops, ack timeouts, reconnect ordering) can produce zombie connections or lost acks; requires the transcript-diff harness.
- Diminishing returns after P1/P4. Mitigation: each wave reports measured PSS/CPU, and the gain is written honestly into the PR.
- Crate maturity: hand-write the subset of the Socket.IO/MCP protocols we use.

## User decisions (2026-11-18)

- Proceed directly, delivered in waves (1a claude → 1b codex → 1c agy), each wave its own PR with measured numbers.
- codex: fully compatible with the TS shared runtime.
- Standalone binary `hapi-session-wrapper`; the runner recognises both binaries.
- Wave 1 is Linux only, with a per-flavor switch and automatic fallback.
- Target repository is Overbaker/hapi-rust, branch + PR, base `deploy/rust`.
