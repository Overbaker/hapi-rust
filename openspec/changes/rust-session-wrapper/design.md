## Context
The design is a synthesis of three plan analyses: Fable (primary), default claude, and antigravity. The proposal's user decisions are binding. Plan stage confirmed: all six adjudications approved (option 1). Writer: codex gpt-6.2-astra. Reviewers: Fable + antigravity.

## Goals / Non-Goals
- Goals: S1–S8 in the proposal; one PR per agent wave (1a claude, 1b codex, 1c agy).
- Non-Goals: Windows/macOS; flavors other than claude/codex/agy; codex attached-TUI flows (TS `hapi codex` attach still works through the shared registry/gateway); agy legacy stream-json; porting the peer tools to Rust.

## Adjudications (user-approved)
| # | Decision | Rule |
|---|---|---|
| D1 | peer tools are delegated | The Rust MCP server calls `hapi peer-tool <name>` (TS, args JSON on stdin, MCP result JSON on stdout) for ping/spawn/inspect/list_peers. It is spawned on demand and exits afterwards. `HAPI_DISABLE_PEER_TOOLS` is honoured. tools/list output is identical to TS (golden). |
| D2 | Lean dependencies | No axum. hyper 1.x hand-rolls the ≤6 MCP/hook routes. tokio current_thread; blocking FS and ps go through spawn_blocking (max_blocking_threads=4). Hand-written Socket.IO/MCP subsets; rust_socketio, rmcp and reqwest are rejected. |
| D3 | Startup fallback | When the Rust wrapper cannot run before the webhook (agent too old, agy ACP initialize fails, required env missing), it exits **86**. If a hub session was already created, it first writes `HAPI_RUST_WRAPPER_SESSION_ID=<id>` to stderr. The runner treats no-pid, exit-before-webhook (any exit code) and process-error-before-webhook as triggers. It kills the Rust pid and confirms it is dead (marker check) before retrying **once** with TS, using `--existing-session-id <id>` when the id is known. A webhook timeout does not trigger a retry. |
| D4 | SIGTERM time budget | Cleanup completes within 1500 ms (the runner SIGKILLs after 2000 ms). session-end is sent before any flush wait; flush is capped at 1000 ms. Agent tree: SIGTERM, then SIGKILL after at most 2 s within the budget, keeping the total under 1.5 s. |
| D5 | claude permission mode | Same as TS: plan and auto are passed natively; only acceptEdits and bypassPermissions map to default and are emulated inside can_use_tool (claudeRemote.ts:59-61, 335, 411-436). |
| D6 | agy title on resume | conversation_summaries.db is not read. The title comes from session_info_update, session/list or the prompt fallback. Documented as a known behavioural difference, excluded from the harness. |

## Other decisions (no further confirmation needed)

- **A1** Proposal constraint 8 says claude always gets `--permission-mode default`, but TS passes plan/auto natively (cli/src/claude/claudeRemote.ts:59-61, 335). → Follow TS: only acceptEdits and bypassPermissions map to default. Amend the spec delta wording.
- **A2** update-metadata/update-state acks have no timeout and socket.io-client silently drops them on disconnect (socket.js:479-491), so TS can hang the CAS lock forever. → Rust treats a disconnect before the ack as a failed attempt and retries through the same backoff after reconnect. Harness scenario `disconnect-during-cas` asserts convergence; this is the one deliberate deviation.
- **A3** TS strips unknown metadata/agentState keys through zod before writing back. → Rust keeps documents as serde_json::Value and never strips; typed accessors only for fields it reads.
- **A4** JSON key order and `undefined` handling. → Wire contract is semantic JSON equality. Exceptions that must be byte-exact: codex batchKey, projection sha256 fallback, normalizeContent, hook settings file (4-space indent not required).
- **A5** MCP peer tools (ping_peer, spawn_peer, inspect_peer, list_peers) pull in ~2.1k TS LOC (cli/src/modules/pingPeer/pingPeer.ts 1305, spawnPeer.ts 779). → 1a delegates them: add TS subcommand `hapi peer-tool <name>` (args JSON on stdin, MCP result JSON on stdout) that calls the same handler functions extracted from startHappyServer.ts:457-660; Rust execs $HAPI_CLI_EXECUTABLE on demand. Short-lived, reported in the S1 table as non-resident. Native port is a follow-up, not part of 1a-1c.
- **A6** capabilities.terminal:true implies terminal:* support, implemented with Bun.Terminal. → Port in 1a with nix openpty; same limits (4 terminals, HAPI_TERMINAL_IDLE_TIMEOUT_MS, HAPI_TERMINAL_MAX_TERMINALS; cli/src/terminal/TerminalManager.ts:29, 164-165).
- **A7** `switch` RPC would move a TS session to local mode; Rust has no local mode. → Rust registers `switch` as a no-op returning undefined (same as RemoteLauncherBase.ts:270-271) and never emits a 'switch' event.
- **A8** Which pre-webhook failures trigger the TS retry. → no-pid, exit-before-webhook (any code, 86 = planned fallback), process-error-before-webhook. Not webhook timeout. Exactly one retry.
- **A9** metadata.version / happyLibDir come from the TS package and runtime path. → Runner passes HAPI_WRAPPER_CLI_VERSION and HAPI_WRAPPER_RUNTIME_PATH; wrapper exits 86 if either is missing.
- **A10** Hook command in the claude settings file points at `hapi hook-forwarder` (generateHookSettings.ts:114-122), a Bun process per hook. → Rust writes `<own exe> hook-forwarder --flavor claude --port N --token T` (shell-quoted like shellQuote.ts) and generates only `session-hook-<pid>.json` (no local file).
- **A11** Claude metadata probe spawns `claude` with prompt 'hello', maxTurns 1, 10 s (metadataExtractor.ts:83-97); model list probe caches 5 min (claudeModels.ts:22-23). → Keep both as agent spawns (not Bun); same timeouts and cache TTL; in-process cache only.
- **A12** Log file location/format for the Rust wrapper. → <HAPI_HOME>/logs/<ISO-date>-pid-<pid>-wrapper.log, debug lines only when DEBUG is set; nothing on stdout; stderr reserved for fatal startup errors (runner captures a 4000-char tail, run.ts:853-862).
- **A13** Detachment: runner spawns with detached:true (run.ts:873) so the wrapper is already a group leader. → Wrapper asserts getpgrp()==getpid() and calls setsid only if not; agent children stay in the wrapper's group so the runner sweep (-pgid) reaches them.
- **A14** codex bridge command when the wrapper is Rust. → mcp_servers.hapi.command = own exe, args ['mcp','--url',...,'--tools',...,'--hapi-session-id',sid]; orphan matcher commandMatchesHapiMcpBridgeForSession (orphanReap.ts:120-126) already matches this shape.
- **A15** proper-lockfile on-disk protocol was not read from library source. → Task 1b.3 reads node_modules/proper-lockfile/lib/lockfile.js first and adds a TS↔Rust contention test before any other codex work merges.
- **A16** agy fallback after the webhook (AgyAcpStartError → legacy in-process, agyRemoteLauncher.ts:8-13). → Rust runs the version probe, spawns `agy acp` and completes `initialize` before bootstrap; any failure there exits 86. Later failures are session errors, no legacy fallback.
- **A17** agy resume title reads conversation_summaries.db via bun:sqlite (agyNativeTitle.ts:46-57). → Skip in Rust; titles come from session_info_update, session/list and the prompt fallback. Documented behavioural gap, covered by a harness exclusion.
- **A18** inotify replacement for the 350 ms poll and 50 ms tick (agyAcpRemoteLauncher.ts:68, 511-534, 650). → Watch the logs dir; if absent watch the nearest existing ancestor for IN_CREATE and re-arm downward. Settle/park/max-idle (250/1500/10000/600000 ms) are one-shot deadline timers armed only while a wakeup is in progress. On inotify error fall back to a 350 ms timer and log once.
- **A19** agy is not hibernation-eligible on the hub (sessionIdle.ts:207). → No change in 1c; S6 round-trip is tested for claude and codex only.
- **A20** MCP protocol version negotiation is delegated to @modelcontextprotocol/sdk ^1.25.1. → Echo the client's protocolVersion when it is one of the SDK's supported list, else reply with the SDK's latest; task 1a.9.1 copies both values from node_modules at implementation time and pins them in a test.
- **A21** S2 wakeup budget vs. timers. → Idle wrapper may hold exactly two timers: 10 s keepalive and the Engine.IO ping deadline. All others (delay queue, backoff, deadlines) are armed on demand.
- **A22** Other-flavor model RPCs registered by every TS wrapper (registerCommonHandlers.ts:23-33). → Not ported; only claudeModels (1a), codexModels (1b), agyModels (1c).

Additions from the claude analysis, also adopted:
- Engine.IO pingInterval/pingTimeout are taken from the open packet, never hard-coded. The watchdog is their sum.
- TLS: rustls-native-certs plus the PEM from NODE_EXTRA_CA_CERTS, no webpki-roots fallback. HTTP(S)_PROXY is ignored, matching TS ws/fetch.
- metadata.version is the same as TS. The runner passes HAPI_WRAPPER_CLI_VERSION; if it is missing the wrapper exits 86. `--version` prints `<cli version> (rust <git sha>)`.
- The MCP tools/list goldens are produced by a TS script (`golden/mcp_tools_{all,nopeer}.json`). A test fails if TS output drifts from them.
- CI grep gate: Rust source must never spawn HAPI_CLI_EXECUTABLE, with the single exception of D1 `peer-tool`.
- No interval timers when idle: only the 10 s keepalive and the Engine.IO ping deadline. All other timers are one-shot and armed on demand.

## Layout
```json
{
 "summary": "Two new workspace members. Add both to /root/hapi-rust/Cargo.toml:3-8; the release profile (lto, codegen-units=1, strip) at Cargo.toml:10-14 is inherited. Rust edition 2021, toolchain 1.100 (installed).",
 "crates/hapi-wrapper-core (lib)": {
  "src/jsjson.rs": "JS-compatible JSON helpers: serde_json with preserve_order; spread/merge that drops `undefined`; JSON.stringify-equal number formatting; deep-equal.",
  "src/config.rs": "Env + ~/.hapi/settings.json resolution: HAPI_API_URL > settings.apiUrl > settings.serverUrl > http://localhost:3006; CLI_API_TOKEN > settings.cliApiToken; HAPI_EXTRA_HEADERS_JSON > settings.extraHeaders; HAPI_HOME; machineId.",
  "src/sio/{eio.rs,packet.rs,client.rs,backoff.rs}": "Hand-rolled Engine.IO 4 + Socket.IO 5 client (WebSocket only).",
  "src/rest.rs": "hyper client: POST /cli/machines, POST /cli/sessions, GET /cli/sessions/:id, GET /cli/sessions/:id/messages, runner webhook POST /session-started.",
  "src/session/{client.rs,incoming_filter.rs,versioned_ack.rs,backfill.rs,echo.rs,bootstrap.rs,metadata.rs}": "ApiSessionClient equivalent (always 'active'; no lazy materialisation, which is terminal-only per cli/src/agent/sessionFactory.ts:306-308).",
  "src/rpc/{manager.rs,handlers/*.rs}": "RpcHandlerManager + common handlers.",
  "src/queue.rs": "MessageQueue2 port.",
  "src/permission/base.rs": "BasePermissionHandler port.",
  "src/mcp/{http.rs,tools.rs,stdio_bridge.rs}": "Streamable-HTTP MCP server, HAPI tools, `mcp` stdio bridge (bridge lands in 1b).",
  "src/hooks/{server.rs,settings.rs,forwarder.rs}": "Hook HTTP server, settings-file generator, `hook-forwarder` subcommand.",
  "src/lifecycle/{signals.rs,proctree.rs,start_marker.rs,runner_lifecycle.rs}": "setsid check, SIGTERM/SIGINT, archive + session-end, tree kill, `ps -o lstart=` marker.",
  "src/terminal.rs": "terminal:* PTY manager (nix openpty), max 4 terminals, idle timeout.",
  "src/watch.rs": "inotify wrapper (used by claude awaitFileExist in 1a, agy in 1c)."
 },
 "crates/hapi-session-wrapper (bin `hapi-session-wrapper`)": {
  "src/main.rs": "Dispatch argv[1]: claude | codex | agy | mcp | hook-forwarder | --version. Current-thread tokio runtime (S1/S2).",
  "src/cli.rs": "Parser byte-compatible with buildCliArgs output (cli/src/runner/run.ts:2358-2460) and cli/src/commands/claude.ts:83-152.",
  "src/flavors/claude/{run.rs,session.rs,query.rs,remote.rs,launcher.rs,permission.rs,sdk_to_log.rs,outgoing_queue.rs,special_commands.rs,system_prompt.rs,compact.rs,title.rs,tool_result_images.rs,metadata_probe.rs}": "PR-1a",
  "src/flavors/codex/{frontend.rs,runtime.rs,root.rs,registry.rs,ownership_lock.rs,gateway.rs,launch.rs,queue.rs,projection.rs,permissions.rs,async_questions.rs,settings.rs,plan.rs,app_server_client.rs,event_converter.rs,permission_adapter.rs,slash_commands.rs,mcp_bridge_config.rs,...}": "PR-1b",
  "src/flavors/agy/{run.rs,launcher.rs,reactive_wakeup.rs,question.rs,tool_cards.rs,swarm_board.rs,title.rs,model_selection.rs,version.rs}": "PR-1c",
  "src/acp/{transport.rs,backend.rs,message_handler.rs,permission.rs,converter.rs}": "PR-1c (agy-only subset of cli/src/agent/backends/acp)."
 },
 "hand_rolled_protocol_subsets": {
  "engine_io_4": "GET ws(s)://host/socket.io/?EIO=4&transport=websocket with extraHeaders on the upgrade request. Text frames only. Packets: '0'{sid,pingInterval,pingTimeout,maxPayload} open, '1' close, '2' ping (server→client) answered with '3', '4' message. Liveness: no ping within pingInterval+pingTimeout ⇒ close with reason 'ping timeout'. No polling, no upgrade probe, no binary.",
  "socket_io_5": "Namespace '/cli'. CONNECT `40/cli,{\"token\":..,\"clientType\":\"session-scoped\",\"sessionId\":..}`; CONNECT ack `40/cli,{sid}`; CONNECT_ERROR `44/cli,{message}`; EVENT `42/cli,<ackId?>[name,...args]`; ACK `43/cli,<id>[...]`; DISCONNECT `41/cli,`. Ack ids: integer counter from 0 per socket. Server-initiated acks (rpc-request, update cancel/retry) answered with `43/cli,<id>[payload]`. No BINARY_EVENT/BINARY_ACK (terminal data is string).",
  "client_semantics": "Send buffer for non-volatile emits while disconnected, flushed in order after CONNECT ack; volatile emits dropped when the transport is not writable (socket.io-client 4.8.3 socket.js:270-281); reconnect backoff = backo2 (min 1000, max 5000, factor 2, jitter 0.5, formula at socket.io-client build/cjs/contrib/backo2.js:29-37), attempts reset on successful connect; connect timeout 20000 ms (manager.js:62).",
  "mcp_streamable_http_subset": "POST / only path-agnostic JSON-RPC: initialize (issue `mcp-session-id` UUID header), notifications/initialized (202), ping, tools/list, tools/call; responses as `application/json` or single-event `text/event-stream` (answer with JSON when the client's Accept allows it, else SSE); GET with session id → open SSE stream kept idle; DELETE → close session; unknown session id → 404 (cli/src/claude/utils/startHappyServer.ts:771-786). Server info {name:'HAPI MCP', version:'1.0.0'} (startHappyServer.ts:184-187). No resources/prompts/sampling.",
  "hook_server": "POST /hook/session-start, /hook/permission-request, /hook/pre-tool-use, /hook/agy-pre-invocation with header x-hapi-hook-token (cli/src/claude/utils/startHookServer.ts:100-322). Remote wrapper only needs session-start semantics; others return the TS no-op bodies."
 }
}
```

## Dependencies
- {"crate": "tokio", "version": "1.54", "features": ["rt", "macros", "net", "time", "sync", "process", "signal", "io-util", "fs"], "note": "current_thread runtime; matches Cargo.lock 1.54.1"}
- {"crate": "tokio-tungstenite", "version": "0.30", "features": ["rustls-tls-native-roots"], "default-features": false, "note": "also used for codex unix-socket WebSocket via client_async over tokio UnixStream"}
- {"crate": "rustls", "version": "0.23", "features": ["ring", "std", "tls12"], "default-features": false, "note": "0.24 is still -dev on crates.io; do not use"}
- {"crate": "rustls-native-certs", "version": "0.8"}
- {"crate": "tokio-rustls", "version": "0.26", "default-features": false, "features": ["ring"]}
- {"crate": "hyper", "version": "1.12", "features": ["http1", "client", "server"]}
- {"crate": "hyper-util", "version": "0.1", "features": ["tokio", "client-legacy", "http1", "server"]}
- {"crate": "hyper-rustls", "version": "0.27", "default-features": false, "features": ["native-tokio", "http1", "ring"], "note": "feature names to be confirmed with `cargo add` at task 1.2; fallback is a 60-line tokio-rustls connector"}
- {"crate": "http-body-util", "version": "0.1"}
- {"crate": "bytes", "version": "1"}
- {"crate": "serde", "version": "1", "features": ["derive"]}
- {"crate": "serde_json", "version": "1", "features": ["preserve_order"]}
- {"crate": "futures-util", "version": "0.3", "default-features": false, "features": ["sink", "std"]}
- {"crate": "nix", "version": "0.32", "features": ["signal", "process", "term", "fs", "inotify", "user", "ioctl"]}
- {"crate": "uuid", "version": "1", "features": ["v4"]}
- {"crate": "sha2", "version": "0.11", "note": "codex authHash / projection hash keys (1b)"}
- {"crate": "base64", "version": "0.22", "note": "uploads, images"}
- {"crate": "percent-encoding", "version": "2"}
- {"crate": "regex-lite", "version": "0.1", "note": "smaller than regex; lookbehind in agy version regex is hand-coded"}
- {"crate": "proptest", "version": "1.12", "dev": true}
- {"crate": "tempfile", "version": "3", "dev": true}
- {"rejected": ["axum (not needed for 6 routes; saves RSS)", "rust_socketio", "rmcp", "reqwest", "rusqlite for agy titles (see ambiguity A17)"]}

## TS→Rust mapping and LOC budget
```json
{
 "note": "LOC = Rust, excluding tests. Totals: 1a ≈ 10,900; 1b ≈ 7,400; 1c ≈ 6,300; overall ≈ 24.6k, i.e. the proposal's upper estimate is the realistic one.",
 "1a_core": [
  {
   "ts": "socket.io-client usage in cli/src/api/apiSession.ts:275-289",
   "rust": "sio/*",
   "loc": 750,
   "tests": "new (no TS unit tests); covered by harness + proptest"
  },
  {
   "ts": "cli/src/configuration.ts, cli/src/ui/{tokenInit,apiUrlInit,extraHeadersInit}.ts, cli/src/api/hubExtraHeaders.ts",
   "rust": "config.rs",
   "loc": 200,
   "tests": "cli/src/configuration.test.ts (extra headers cases)"
  },
  {
   "ts": "cli/src/api/api.ts:70-222, cli/src/runner/controlClient.ts:38-92",
   "rust": "rest.rs",
   "loc": 300,
   "tests": "cli/src/runner/controlClient.test.ts (webhook part)"
  },
  {
   "ts": "cli/src/api/apiSession.ts (63-116, 300-501, 690-1124, 1137-1249, 1323-1712), cli/src/api/versionedUpdate.ts, cli/src/utils/{time,lock}.ts",
   "rust": "session/*",
   "loc": 1150,
   "tests": "cli/src/api/apiSession*.test.ts, cli/src/api/versionedUpdate.test.ts (all non-materialisation cases)"
  },
  {
   "ts": "cli/src/agent/sessionFactory.ts:111-152, 154-218, 245-301, 389-474",
   "rust": "session/bootstrap.rs, session/metadata.rs",
   "loc": 350,
   "tests": "cli/src/agent/sessionFactory*.test.ts"
  },
  {
   "ts": "cli/src/agent/runnerLifecycle.ts, cli/src/claude/registerKillSessionHandler.ts, cli/src/agent/localHandoff.ts, cli/src/utils/process.ts:106-131 + tree kill",
   "rust": "lifecycle/*",
   "loc": 450,
   "tests": "cli/src/agent/runnerLifecycle.test.ts, cli/src/claude/registerKillSessionHandler.test.ts"
  },
  {
   "ts": "cli/src/api/rpc/RpcHandlerManager.ts",
   "rust": "rpc/manager.rs",
   "loc": 120,
   "tests": "RpcHandlerManager.test.ts"
  },
  {
   "ts": "cli/src/modules/common/handlers/{bash,files,directories,ripgrep,difftastic,git,uploads,slashCommands,skills,claudeModels}.ts + modules/common/{pathSecurity,slashCommands,skills,claudeModels,workspaceFileHandlers}.ts",
   "rust": "rpc/handlers/*",
   "loc": 1700,
   "tests": "matching *.test.ts next to each handler; other-flavor model handlers are intentionally not ported (hub sees 'Method not found' like an old wrapper, constraint 11)"
  },
  {
   "ts": "cli/src/utils/MessageQueue2.ts, cli/src/utils/deterministicJson.ts (equality only)",
   "rust": "queue.rs",
   "loc": 700,
   "tests": "cli/src/utils/MessageQueue2*.test.ts"
  },
  {
   "ts": "cli/src/modules/common/permission/BasePermissionHandler.ts",
   "rust": "permission/base.rs",
   "loc": 300,
   "tests": "BasePermissionHandler.test.ts"
  },
  {
   "ts": "cli/src/claude/utils/startHappyServer.ts, modules/common/{generatedImages,inlineMediaSource,displayToolDescriptions}.ts, modules/sessionJob/*, agent/sessionDisplayRename.ts",
   "rust": "mcp/http.rs, mcp/tools.rs",
   "loc": 1300,
   "tests": "startHappyServer*.test.ts, sessionJob tests; peer tools delegated (A5)"
  },
  {
   "ts": "cli/src/claude/utils/startHookServer.ts, sessionHookForwarder.ts, modules/common/hooks/generateHookSettings.ts, modules/common/shellQuote.ts",
   "rust": "hooks/*",
   "loc": 450,
   "tests": "startHookServer.test.ts, sessionHookForwarder.test.ts, generateHookSettings.test.ts"
  },
  {
   "ts": "cli/src/terminal/TerminalManager.ts",
   "rust": "terminal.rs",
   "loc": 350,
   "tests": "cli/src/terminal/TerminalManager.test.ts"
  }
 ],
 "1a_claude": [
  {
   "ts": "cli/src/claude/runClaude.ts, loop.ts, session.ts, agent/sessionBase.ts, claude/{model,effort}.ts, commands/claude.ts:83-152",
   "rust": "flavors/claude/{run,session}.rs, cli.rs",
   "loc": 650,
   "tests": "cli/src/commands/claude.test.ts, claude/session tests, runner/buildCliArgs.test.ts (as parser round-trip)"
  },
  {
   "ts": "cli/src/claude/sdk/{query,stream,utils,types}.ts, claude/utils/mcpConfig.ts",
   "rust": "query.rs",
   "loc": 450,
   "tests": "cli/src/claude/sdk/query.test.ts, utils.test.ts"
  },
  {
   "ts": "cli/src/claude/claudeRemote.ts",
   "rust": "remote.rs",
   "loc": 650,
   "tests": "cli/src/claude/claudeRemote*.test.ts incl. claudeRemote.seam.test.ts"
  },
  {
   "ts": "cli/src/claude/claudeRemoteLauncher.ts, modules/common/remote/RemoteLauncherBase.ts:186-271 (no ink UI)",
   "rust": "launcher.rs",
   "loc": 750,
   "tests": "claudeRemoteLauncher*.test.ts"
  },
  {
   "ts": "cli/src/claude/utils/{permissionHandler,askUserQuestionAnswers,getToolName,getToolDescriptor}.ts, claude/sdk/prompts.ts",
   "rust": "permission.rs",
   "loc": 400,
   "tests": "permissionHandler*.test.ts, askUserQuestionAnswers.test.ts"
  },
  {
   "ts": "cli/src/claude/utils/{sdkToLogConverter,OutgoingMessageQueue,toolResultImages,transcriptEntry}.ts, agent/messageConverter.ts (usage only)",
   "rust": "sdk_to_log.rs, outgoing_queue.rs, tool_result_images.rs",
   "loc": 800,
   "tests": "sdkToLogConverter*.test.ts, OutgoingMessageQueue.test.ts, toolResultImages.test.ts"
  },
  {
   "ts": "cli/src/parsers/specialCommands.ts, utils/attachmentFormatter.ts, claude/utils/{systemPrompt,compactCompletion,compactSummaryLookup,sessionTitleFallback,claudeCheckSession,path}.ts, claude/sdk/metadataExtractor.ts, modules/watcher/awaitFileExist.ts",
   "rust": "special_commands.rs, system_prompt.rs, compact.rs, title.rs, metadata_probe.rs",
   "loc": 600,
   "tests": "specialCommands.test.ts, attachmentFormatter.test.ts, compact*.test.ts, metadataExtractor.test.ts"
  },
  {
   "not_ported": "claudeLocal*.ts, localPermissionBridge.ts, ink UI, resumeIdentity.ts terminal path"
  }
 ],
 "1b_codex": [
  {
   "ts": "cli/src/codex/shared/{frontend,runtime,root}.ts",
   "rust": "frontend.rs, runtime.rs, root.rs",
   "loc": 1500,
   "tests": "shared/{frontend,root}.test.ts; runtime.integration.test.ts via harness"
  },
  {
   "ts": "shared/registry.ts, shared/src/settingsFileLock.ts (proper-lockfile protocol)",
   "rust": "registry.rs, ownership_lock.rs",
   "loc": 350,
   "tests": "shared/registry.test.ts + new TS↔Rust lock interop test"
  },
  {
   "ts": "shared/gateway.ts, websocketLimits.ts",
   "rust": "gateway.rs",
   "loc": 300,
   "tests": "shared/gateway.test.ts"
  },
  {
   "ts": "shared/launch.ts, utils/{codexExecutable,codexCliOverrides,permissionModeConfig,modelCatalog,reasoningEffort}.ts",
   "rust": "launch.rs",
   "loc": 450,
   "tests": "shared/launch.test.ts + each util test"
  },
  {
   "ts": "shared/queue.ts",
   "rust": "queue.rs",
   "loc": 850,
   "tests": "shared/queue.test.ts, queue.batch.test.ts"
  },
  {
   "ts": "shared/{projection,plan,settings,permissions,asyncQuestions}.ts",
   "rust": "projection.rs, plan.rs, settings.rs, permissions.rs, async_questions.rs",
   "loc": 800,
   "tests": "the five matching test files + projection.title.test.ts"
  },
  {
   "ts": "codexAppServerClient.ts (socket half), appServerTypes.ts",
   "rust": "app_server_client.rs",
   "loc": 600,
   "tests": "codexAppServerClient.test.ts (socket cases)"
  },
  {
   "ts": "utils/{appServerEventConverter,appServerPermissionAdapter,codexCompactionSummary,codexGenerationLedger,roundDuration,assistantText}.ts, utils/appServerConfig.ts (buildUserInputFromMessage only)",
   "rust": "event_converter.rs, permission_adapter.rs, ...",
   "loc": 1900,
   "tests": "appServerEventConverter.test.ts (1319 lines), appServerPermissionAdapter.test.ts, others"
  },
  {
   "ts": "utils/{slashCommands,systemPrompt,buildHapiMcpBridge,codexMcpServers}.ts, modules/common/{codexModels,handlers/codexModels}.ts",
   "rust": "slash_commands.rs, mcp_bridge_config.rs",
   "loc": 400,
   "tests": "matching tests"
  },
  {
   "ts": "cli/src/codex/happyMcpStdioBridge.ts",
   "rust": "core mcp/stdio_bridge.rs (`hapi-session-wrapper mcp`)",
   "loc": 250,
   "tests": "happyMcpStdioBridge.test.ts"
  },
  {
   "not_ported": "codexRemoteLauncher.ts (unreachable: cli/src/codex/runCodex.ts:3 re-exports runSharedCodex), codexLocal*, shared/attached*.ts (`hapi attach` stays TS), shared/archivedFork.ts (runner side), happyMcpStdioProxy.ts (Windows)"
  }
 ],
 "1c_agy": [
  {
   "ts": "cli/src/agent/backends/acp/AcpStdioTransport.ts",
   "rust": "acp/transport.rs",
   "loc": 500,
   "tests": "AcpStdioTransport.test.ts (skip agentCliGuard suite)"
  },
  {
   "ts": "AcpSdkBackend.ts (agy branches only)",
   "rust": "acp/backend.rs",
   "loc": 1100,
   "tests": "AcpSdkBackend.{initialize,elicitation}.test.ts + agy cases of AcpSdkBackend.test.ts"
  },
  {
   "ts": "AcpMessageHandler.ts, agent/{messageConverter,utils,internalEventFilter,rateLimitParser,generationClock}.ts",
   "rust": "acp/message_handler.rs, acp/converter.rs",
   "loc": 1100,
   "tests": "AcpMessageHandler.test.ts (delta + generic), messageConverter*.test.ts, small unit tests"
  },
  {
   "ts": "modules/common/permission/AcpPermissionHandler.ts",
   "rust": "acp/permission.rs",
   "loc": 250,
   "tests": "AcpPermissionHandler.test.ts"
  },
  {
   "ts": "cli/src/agy/agyAcpRemoteLauncher.ts",
   "rust": "flavors/agy/launcher.rs",
   "loc": 1700,
   "tests": "agyAcpRemoteLauncher.test.ts (87), both replay tests with their __fixtures__"
  },
  {
   "ts": "cli/src/agy/agyReactiveWakeup.ts",
   "rust": "reactive_wakeup.rs (+ core watch.rs)",
   "loc": 950,
   "tests": "agyReactiveWakeup.test.ts (31)"
  },
  {
   "ts": "agy/{runAgy,loop,session,agyQuestion,agyToolCards,agySwarmBoard,agySessionTitle,agyNativeTitle}.ts, agy/utils/*, legacyStreamJson/agyVersion.ts, agent/{acpSessionTitle,nativeSessionTitle,sessionConfigRpc}.ts, modules/common/agyModels.ts",
   "rust": "run.rs, question.rs, tool_cards.rs, swarm_board.rs, title.rs, model_selection.rs, version.rs",
   "loc": 700,
   "tests": "each matching *.test.ts, transportSelection.test.ts (probe cases)"
  },
  {
   "not_ported": "cli/src/agy/legacyStreamJson/* except agyVersion.ts (TS fallback)"
  }
 ]
}
```

## Behaviours that must stay identical
| Area | Rule | TS source |
|---|---|---|
| auth | Socket.IO namespace `${apiUrl}/cli`, path '/socket.io/', auth {token, clientType:'session-scoped', sessionId}, websocket only, extraHeaders on upgrade. | cli/src/api/apiSession.ts:275-289; hub check hub/src/socket/server.ts:117-126 |
| auth | REST headers: extraHeaders first, then Authorization: Bearer <token>, Content-Type: application/json (base wins on conflict). | cli/src/api/hubExtraHeaders.ts:3-8, cli/src/api/api.ts:38-43 |
| bootstrap | Order: POST /cli/machines → POST /cli/sessions {id?,adopt?,tag,metadata,agentState,model,modelReasoningEffort,effort} (60 s timeout) or GET /cli/sessions/:id for --existing-session-id → open socket → export HAPI_SESSION_ID to children → webhook. adopt:true only when --hapi-session-id is a UUID. | cli/src/agent/sessionFactory.ts:54-64, 245-301, 389-474; cli/src/api/api.ts:88-115 |
| bootstrap | Existing-session refusal messages (hub-archived, attached codex, pending archived fork) and preserved-field list are identical. | cli/src/agent/sessionFactory.ts:154-218, 409-433 |
| metadata | Create-time fields: path, host (HAPI_HOSTNAME//hostname), version, os, machineId, homeDir, happyHomeDir, happyLibDir, happyToolsDir, startedFromRunner, hostPid, hostStartMarker, startedBy, lifecycleState:'running', lifecycleStateSince, flavor, capabilities:{terminal:true}, worktree; plus new wrapperRuntime. | cli/src/agent/sessionFactory.ts:123-151 |
| metadata | hostStartMarker = trimmed stdout of `ps -p <pid> -o lstart=` with env LC_ALL=C TZ=UTC; shape `\w{3}\s+\w{3}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\s+\d{4}`. Rust shells out to ps (10 s timeout); it does not derive it from /proc. | cli/src/utils/process.ts:106-131; consumer regex cli/src/runner/orphanReap.ts:199 |
| webhook | POST http://127.0.0.1:<runner.state.json httpPort>/session-started body {sessionId, metadata}; 10 s timeout (HAPI_RUNNER_HTTP_TIMEOUT); skipped when state file missing or runner pid dead; failures are non-fatal. | cli/src/runner/controlClient.ts:38-92; cli/src/runner/controlServer.ts:41-60; cli/src/agent/sessionFactory.ts:231-243 |
| events_on_connect | On every connect: rpc-register for every handler ({method:'<sessionId>:<name>'}), then session-alive {sid,time,thinking:false,...latestKeepAlive}; on reconnect first GET session (15 s) to reconcile metadata, then backfill, then 'connected'. | cli/src/api/apiSession.ts:300-327; cli/src/api/rpc/RpcHandlerManager.ts:64-69, 89-91 |
| rpc | rpc-request {method, params:<JSON string>} answered by ack with a JSON string; unknown method → '{"error":"Method not found"}'; thrown error → {error: message}. | cli/src/api/apiSession.ts:329-331; cli/src/api/rpc/RpcHandlerManager.ts:42-62 |
| message_payloads | claude log message: {sid, message:{role:'agent',content:{type:'output',data},meta:{sentFrom:'cli'}}, createdAt?}; external user row → {role:'user',content:{type:'text',text},meta:{sentFrom:'cli',isTranscriptEcho?}}; summary rows also update metadata.summary. | cli/src/api/apiSession.ts:1074-1135 |
| message_payloads | sendAgentMessage → content.type 'codex' (AGENT_MESSAGE_PAYLOAD_TYPE) with optional localId followed by messages-consumed; sendSessionEvent → {role:'agent',content:{id:uuid,type:'event',data}}; sendUserMessage likewise. | cli/src/api/apiSession.ts:1137-1249; shared/src/modes.ts:8 |
| event_names | Emitted: message, session-alive, session-ready, session-end, messages-consumed, messages-indeterminate, native-queue-message, messages-steer-state, update-metadata, update-state, rpc-register, terminal:*, ping. Received: update, rpc-request, terminal:open/write/resize/close, agent-terminal:* (ignored: no agent PTY in remote mode). | shared/src/socket.ts:254-313; cli/src/api/apiSession.ts:372-496 |
| update_acks | cancel-queued-message ack {removed,inFlight,indeterminate,consumed}; retry-queued-message ack {removed:false,accepted} or {removed:false,accepted:false,consumed:true}; handler error ack {removed:false,accepted:false,indeterminate:true}. | cli/src/api/apiSession.ts:417-496 |
| ack_timeouts | messages-steer-state 5000 ms; flush ping = remaining budget (default 5000; 1000 on archive); update-metadata / update-state have no timeout. | cli/src/api/apiSession.ts:1384-1397, 1440, 1515, 1643-1690; cli/src/agent/runnerLifecycle.ts:68 |
| keepalive | session-alive volatile every 10 000 ms and on every thinking/mode change with {sid,time,thinking,mode,permissionMode,model,effort,...}; dropped (not buffered) while disconnected; last payload replayed on connect. | cli/src/api/keepAlive.ts:6; cli/src/agent/sessionBase.ts:82-100; cli/src/api/apiSession.ts:1323-1345 |
| cas | One serial lock per document; loop: handler(current) → emitWithAck {sid, expectedVersion, metadata/agentState} → apply ack value+version for both 'success' and 'version-mismatch'; mismatch/error/invalid ⇒ retry forever with delay round(rand × (250 + 15 × min(failures,50))) ms; remote update-session applied only when version > local. | cli/src/api/apiSession.ts:1418-1543, 466-485; cli/src/api/versionedUpdate.ts:23-65; cli/src/utils/time.ts:12-46 |
| hub_archived | metadata lifecycleState 'archived' + archivedBy 'hub' (from update-session, ack, or REST) ⇒ stop reconnecting, skip further metadata writes, exit via cleanup with archiveReason 'User terminated'. | cli/src/api/apiSession.ts:868-898, 1431-1495; cli/src/claude/registerKillSessionHandler.ts:57-61, 123-128 |
| incoming | Dedup by id (LRU 256, refresh on hit) then seq cursor; user messages with meta.sentFrom==='cli' are dropped unless native-queued + concurrentClients + localId; backfill pages of 200 via GET /messages?afterSeq&limit, 15 s timeout; opencodeRewind branches may be omitted (not claude/codex/agy). | cli/src/api/apiSession.ts:63-116, 779-817, 959-1072 |
| queue | MessageQueue2: batches group consecutive same-mode-hash non-isolated items; isolate items run alone; pushIsolateAndClear drops queued items; unshift for plan restart; reservation states (take/beginDispatch/restore/commit/indeterminate) and cancelByLocalId results true/false/'in-flight'/'indeterminate'/'consumed'. | cli/src/utils/MessageQueue2.ts:97-259, 401-583, 785-845; cli/src/claude/runClaude.ts:305-308, 414-498, 515-528 |
| steer_claude | RPC steer-queued-message {localId} → exact error strings and state persistence order (dispatching → steer → messages-consumed{steered:true} / restore 'queued' / messages-indeterminate). | cli/src/claude/claudeRemoteLauncher.ts:158-254 |
| claude_argv | `--output-format stream-json --verbose [--system-prompt] --append-system-prompt <hapi prompt> [--max-turns] [--model] [--effort] --permission-prompt-tool stdio [--resume id] [--fork-session] <claudeArgs> --settings <hook file> [--allowedTools a,b] [--disallowedTools] --add-dir <blobs dir> --permission-mode <native> [--fallback-model] --input-format stream-json --replay-user-messages --mcp-config '{"mcpServers":{"hapi":{"type":"http","url":...}}}'`; env CLAUDE_CODE_ENTRYPOINT=sdk-ts (if unset), DISABLE_AUTOUPDATER=1; executable HAPI_CLAUDE_PATH or `claude` on PATH. | cli/src/claude/sdk/query.ts:404-462; cli/src/claude/claudeRemote.ts:146, 329-356; cli/src/claude/utils/mcpConfig.ts:15-58; cli/src/claude/runClaude.ts:601, 611-616; cli/src/claude/sdk/utils.ts:124-149 |
| claude_permission_mode | Native mode = 'default' for acceptEdits and bypassPermissions (emulated in can_use_tool); plan and auto are passed natively and switched live with control_request set_permission_mode; init-mismatch falls back to default with the exact notice text. | cli/src/claude/claudeRemote.ts:59-61, 405-436, 703-713; cli/src/claude/utils/permissionHandler.ts:251-309 |
| claude_control_protocol | stdin user lines {type:'user',uuid,message:{role:'user',content}}; control_request ids = random base36; can_use_tool answered with control_response success/error; control_cancel_request aborts; subtypes used: interrupt, initialize, get_context_usage, set_permission_mode, apply_flag_settings (effort, 5 s cap). | cli/src/claude/sdk/query.ts:126-140, 172-356; cli/src/claude/claudeRemote.ts:57, 173-176, 440-465 |
| claude_permission_emulation | Order: session allow-lists (Bash literal / `prefix:*`) → bypassPermissions allow (ExitPlanMode → fake reject + PLAN_FAKE_RESTART unshift) → acceptEdits allow for edit tools → resolve tool_use id by name+deep-equal input (retry once after 1000 ms) → agentState.requests[id]={tool,arguments,createdAt}; AskUserQuestion / request_user_input / ExitPlanMode responses and deny text verbatim. | cli/src/claude/utils/permissionHandler.ts:158-246, 251-309, 358-402; cli/src/modules/common/permission/BasePermissionHandler.ts:186-243, 324-340 |
| claude_conversion | SDK→log rows: fresh uuid v4 per row, parentUuid = previous main-chain uuid, sidechains keyed by parent_tool_use_id, isSidechain, userType 'external', cwd, sessionId, version, gitBranch, ISO timestamp; chain reset on new claude session id; replayed user echoes (isReplay with a pushed uuid) are never relayed; manual compact boundary/echo suppressed; tool_result rows get `permissions`; ExitPlanMode fake-reject rewritten to 'Plan approved'. | cli/src/claude/utils/sdkToLogConverter.ts:116-124, 215-218, 250-316, 546, 565-637; cli/src/claude/claudeRemote.ts:252-264, 627-656; cli/src/claude/claudeRemoteLauncher.ts:311-403 |
| claude_ordering | Assistant rows containing tool_use are delayed 250 ms and released early on permission request or tool_result; queue flushed before next prompt, before 'ready', and on teardown; interrupted tool calls get synthetic results. | cli/src/claude/claudeRemoteLauncher.ts:151-153, 378-403, 529, 671, 835-846; cli/src/claude/utils/OutgoingMessageQueue.ts:33-55, 87-99, 149-155 |
| claude_events | Session event strings: 'Aborted by user', 'Process exited unexpectedly: <detail>', the 3-failure drop message, '📦 Compaction started', 'Context was reset', {type:'ready'} only when queue empty; respawn backoff 1000 ms (CLAUDE_REMOTE_RESPAWN_BACKOFF_MS), 3 immediate failures. | cli/src/claude/claudeRemoteLauncher.ts:43-51, 712-714, 729, 794-823; cli/src/claude/claudeRemote.ts:238, 308 |
| claude_metadata | claudeSessionId + claudeSessionIdHistory on session found (after the transcript file exists), tools + slashCommands from the metadata probe, hapiMcpUrl, capabilities.conversationHistory, startingMode 'remote', agentState {controlledByUser:false, startingMode:'remote', steeringActive}. | cli/src/claude/session.ts:72-79; cli/src/claude/runClaude.ts:117-133, 271-280, 300-301; cli/src/claude/utils/startHappyServer.ts:802-806; cli/src/agent/runnerLifecycle.ts:224-242; cli/src/claude/claudeRemote.ts:717-727 |
| claude_rpcs | Registered: common handlers, listSkills, killSession (+ process-scoped prepare method), handoff-local, fork-conversation ({nativeSessionId, forkSession:true}), rewind-conversation (throws), set-session-config ({applied:{permissionMode,model,effort}}), abort, switch, steer-queued-message, permission. | cli/src/claude/runClaude.ts:143-148, 268-297, 562-588; shared/src/rpcMethods.ts:14-101 |
| sigterm | SIGTERM: stop keepalive → metadata {lifecycleState:'archived', lifecycleStateSince, archivedBy:'cli', archiveReason:'Hub restart'} → session-end {sid,time,reason:'terminated'} → flush ≤1000 ms → close → stop MCP/hook servers, delete hook file → kill agent tree → exit 0. SIGINT same with 'User terminated'. Natural end: reason 'completed', archiveReason 'Session completed'. Crash: 'error' / 'Session crashed', exit 1. | cli/src/agent/runnerLifecycle.ts:48-70, 149-209; cli/src/claude/runClaude.ts:254-265, 625-647 |
| kill_session | killSession validates expectedMachineId, expectedPid + expectedProcessStartMarker, prepare lease (monotonic whole-second uptime ms, ≤30 000 ms ahead); reply {success,message,pid,processStartMarker?,preparationLease?,machineId?}; sets archiveReason 'User terminated'. | cli/src/claude/registerKillSessionHandler.ts:63-116; cli/src/utils/time.ts:4-6; shared/src/rpcMethods.ts:1, 99-101 |
| hibernation | No wrapper-side hibernate flag: the runner stops the wrapper (SIGTERM path above) and the hub overwrites archivedBy/archiveReason with lifecycleState 'hibernated'. Resume is a normal `--resume <native id> --existing-session-id <id>` spawn. The group must be empty within the sweep (SIGTERM → 5 s → SIGKILL → 1 s). | openspec/changes/agent-session-orchestration/design.md:50-57; cli/src/utils/process.ts:596-651; cli/src/runner/run.ts:1422-1427; hub/src/sync/sessionIdle.ts:207 (eligible flavors exclude agy) |
| codex_registry | File <HAPI_HOME>/codex-runtimes/<uuid>.json plus copy <codexHome>/hapi-runtime-owners/<uuid>.json; dirs 0700, tmp file 0600 + rename; compact JSON with keys id,pid,marker,serverPid,serverMarker,endpoint,token?,command,args,codexHome,hub,authHash(sha256 hex of token),pendingCreations?,sessions{[sid]:{threadId,namespace,active}}; never deleted; TS reads strictly, so one malformed file blocks stops and resumes. | cli/src/codex/shared/registry.ts:12-36, 67, 77-82, 123-126; cli/src/codex/shared/runtime.ts:47, 80-82, 143-145 |
| codex_lock | Ownership lock = proper-lockfile directory `<codexHome>/hapi-runtime-owners/ownership.hapi.lock` (mkdir, mtime refresh every 10 s, stale after 30 s, 50 retries × 100 ms, rmdir on release); preflight error strings verbatim. | shared/src/settingsFileLock.ts:9-12, 35-53; cli/src/codex/shared/registry.ts:87-121 |
| codex_gateway | /tmp/hapi-cx-XXXXXX (0700) with engine.sock (app-server) and clients.sock (0600); WebSocket over unix socket, Host: localhost, no deflate, 256 MiB max payload; one JSON object per text frame without `jsonrpc`; control methods hapi/stopSession → {stopped:true}, hapi/attach → {threadId}; before/after hook rules, error codes -32600/-32603, close codes 1007/1011, 60 s drain. | cli/src/codex/shared/gateway.ts:8-138; cli/src/codex/shared/runtime.ts:52-55, 268-338, 364-377; cli/src/codex/websocketLimits.ts:4 |
| codex_app_server | Version gate regex /codex-cli (\d+)\.(\d+)\.(\d+)/ and message for <0.154; spawn `<codex> app-server <serverArgs> --listen unix://<dir>/engine.sock`, env CODEX_HOME=realpath, HAPI_SESSION_ID removed; initialize {clientInfo:{name:'hapi',title:'HAPI',version:'1'},capabilities:{experimentalApi:true}} + initialized; capability probe with threadId '__hapi_capability_probe__'; request timeout 20 s (30 s initialize/model/list). | cli/src/codex/shared/launch.ts:35-166; cli/src/codex/shared/runtime.ts:56-58, 345-359; cli/src/codex/codexAppServerClient.ts:349-366, 432-441 |
| codex_stop | Per-session stop: thread/archive → sessions[sid].active=false persisted before replying → after 100 ms close root as archived → POST /session-started with lifecycleState 'archived' → shut the runtime down only when no roots/reservations remain. SIGTERM/SIGINT/SIGHUP stop every root: suspend (≤5 s, queued rows released) → kill app-server → all active=false → roots closed un-archived → remove socket dir. Every new root is reported with capabilities.concurrentClients:true and hostStartMarker. | cli/src/codex/shared/runtime.ts:118-170, 246, 339-341; cli/src/runner/run.ts:494-500, 1442-1475; cli/src/runner/sharedSessionStop.ts:43-90 |
| codex_ids | Message id `codex:<threadId>:<turnId/'thread'>:<itemId ?? call_id ?? sha256(JSON.stringify(event))>:<event.type>` and all derived ids in the projection table; user local id = clientId ?? clientUserMessageId ?? `codex:<threadId>:user:<itemId>`; history fully re-projected on bind/reconnect, so ids must match byte for byte. | cli/src/codex/shared/projection.ts:75-79, 122, 156-159, 177, 186, 232-268, 342-356; cli/src/codex/shared/plan.ts:3-20; cli/src/codex/shared/permissions.ts:24, 64; cli/src/codex/shared/asyncQuestions.ts:18-20, 48, 137, 153 |
| codex_queue_ledger | <HAPI_HOME>/codex-runtimes/queues/<sid>.queue.json v2 schema; batchKey is a persisted JSON string compared by equality (key order = settings spread order then fallbackModel, customSystemPrompt, appendSystemPrompt, allowedTools, disallowedTools); normalizeContent sorts keys and drops empty text_elements. | cli/src/codex/shared/queue.ts:8-30, 50-64, 101-113; cli/src/codex/shared/root.ts:156-163, 283 |
| codex_mcp | Thread config `mcp_servers.hapi` = {command,args:[... 'mcp','--url','http://127.0.0.1:<port>/','--tools',<list>,'--hapi-session-id',<sid>],tools:{approval_mode}}; change_title returns success without writing (projection applies the title). | cli/src/codex/shared/root.ts:215-216, 272-279; cli/src/codex/utils/buildHapiMcpBridge.ts:95-138; cli/src/codex/shared/projection.ts:82-108 |
| agy_acp | Spawn `agy acp` (PATH only; no env override), env + GEMINI_FORCE_FILE_STORAGE=true − SSH_CLIENT/SSH_CONNECTION/SSH_TTY; ndjson JSON-RPC; initialize {protocolVersion:1, clientCapabilities:{fs:{readTextFile:false,writeTextFile:false},terminal:false,elicitation:{form:{}},_meta:{parameterizedModelPicker:true}},clientInfo:{name:'hapi',version}}; session/new/resume {sessionId?,cwd,mcpServers:[]}; set_mode every turn; prompt drain windows 200/1200, 120/2000, 50/250/6000 ms. | cli/src/agy/utils/agyAcpBackend.ts:9-32; cli/src/agent/backends/acp/AcpSdkBackend.ts:172-202, 288-306, 445-528, 802-869; cli/src/agy/agyAcpRemoteLauncher.ts:851-875, 956-979 |
| agy_messages | Text idle snapshot 1000 ms, reasoning snapshot 250 ms, ids per segment, tool-card mapping table, round-summary shape, permission outcome mapping, question form mapping, metadata agySessionId + agyProtocol:'acp' + session-ready. | cli/src/agent/backends/acp/AcpMessageHandler.ts:45-52, 582-639, 759-799, 960-1126; cli/src/agy/agyToolCards.ts:62-218; cli/src/agy/agyAcpRemoteLauncher.ts:115-146, 359, 887-888; cli/src/modules/common/permission/AcpPermissionHandler.ts:27-50; cli/src/agy/agyQuestion.ts:46-158 |

## Parity harness
```json
{
 "location": "cli/src/parity/ (TS, driven by vitest integration config cli/vitest.integration.config.ts); corpus in cli/src/parity/corpus/<flavor>/<scenario>/; Rust-side decode tests in crates/hapi-wrapper-core/tests/.",
 "fake_agents": {
  "claude": "cli/src/parity/fakes/fake-claude.ts with `#!/usr/bin/env bun`, selected via HAPI_CLAUDE_PATH (cli/src/claude/sdk/utils.ts:130). Plays agent.jsonl: each step is {expectStdin?:<json matcher>, emit:[stdout lines], delayMs?, exit?}. Records received argv/env/stdin to $PARITY_OUT/agent.log.json. Writes <claude projects dir>/<session>.jsonl so awaitFileExist passes.",
  "codex": "No path override exists (cli/src/codex/utils/codexExecutable.ts:88-103): a `codex` shim directory is prepended to PATH. Shim answers `--version` with `codex-cli 0.154.0` and serves a scripted app-server on the `--listen unix://` socket.",
  "agy": "`agy` shim on PATH: `--version` → `1.5.3`, `acp` → scripted ndjson; reuses cli/src/agent/backends/acp/__fixtures__/agy-steered-turn.json and agy-question-turn.json; writes brain transcript files for wakeup scenarios."
 },
 "fake_hub": {
  "tier_A_scripted": "cli/src/parity/fakeHub.ts: node http server + `socket.io` 4.8.3 (same package the hub uses, hub/package.json:32) on namespace /cli plus the four REST routes. It does NOT reuse hub/src/socket/handlers/cli (those need the store and sync engine); it implements versioned CAS acks from shared/src/socket.ts types and a script DSL: sendUpdate, rpc(method, params), dropConnection(ms), forceVersionMismatch(n), delayAck(ms). Everything inbound (event name, args, whether an ack was requested, ack payload returned, connection ordinal) is appended to transcript.json. A fake runner control server records the webhook.",
  "tier_B_real_hub": "Existing integration environment (cli/src/test/integrationEnv.ts, runner.integration.test.ts) re-run with HAPI_RUST_WRAPPER_FLAVORS set, for S5/S6/S8; no transcript diff, only outcome assertions (stopped, 0 survivors, same session id after TS→Rust→TS resume)."
 },
 "corpus_format": {
  "scenario.json": "{flavor, argv (buildCliArgs output), env, hubScript:[steps], agentScript:'agent.jsonl', timeoutMs}",
  "golden.ts.json": "Normalised transcript recorded from the TS wrapper; committed. Regenerate with `bun run parity:record`.",
  "scenarios_1a": [
   "plain-turn",
   "tool-approve",
   "tool-deny",
   "ask-user-question",
   "plan-mode-approve",
   "steer",
   "queue-cancel",
   "interrupt",
   "model-effort-switch",
   "attachment",
   "compact",
   "fork",
   "kill-session",
   "sigterm",
   "disconnect-reconnect-backfill",
   "cas-version-mismatch",
   "hub-archived",
   "agent-crash-respawn"
  ]
 },
 "normalisation": [
  "Parse every payload and re-serialise with recursively sorted keys (key order is not part of the contract).",
  "UUIDs → `<uuid:N>` numbered by first appearance over the whole transcript (preserves the parentUuid chain shape).",
  "Epoch-ms fields (time, createdAt, lifecycleStateSince, completedAt, permissions.date, summary.updatedAt) and ISO `timestamp` → `<ts>`; assert monotonic non-decreasing before replacing.",
  "hostPid / pid → `<pid>`; hostStartMarker / processStartMarker → `<marker>` after asserting the lstart regex and equality with a harness-side `ps` probe.",
  "Port in hapiMcpUrl → `<port>`; tmp paths, HAPI_HOME, cwd → placeholders; control_request request_id → `<req:N>`.",
  "metadata.wrapperRuntime removed; metadata.version/happyLibDir/happyToolsDir asserted equal to runner-provided values then replaced.",
  "session-alive: drop an emission whose payload (minus time) equals the previous one; keep changes.",
  "rpc-register: compared as a sorted set per connection; Rust set must be a subset of TS and contain every method listed under identical_behaviours.claude_rpcs.",
  "Ordering: compare per-channel sequences (message / update-metadata / update-state / messages-* / session-ready,end / others) plus position relative to hub-script step markers; cross-channel interleaving inside one step is not compared.",
  "Engine.IO ping/pong and CONNECT packets excluded; CONNECT auth compared separately."
 ],
 "golden_decoding": "cli/src/parity/decode.test.ts feeds every `message` payload from both transcripts through web/src/chat/normalize.ts:8 (normalizeDecryptedMessage) and asserts deep equality of the normalised output; `bun run fixtures:check` (workflow .github/workflows/fixtures.yml) must stay green.",
 "run": {
  "local": "`cargo build --release -p hapi-session-wrapper && cd cli && bun run test:parity` (new script: `vitest run --config vitest.integration.config.ts src/parity`). `PARITY_RUNTIME=ts|rust|both` (default both).",
  "ci": "New job `parity` in .github/workflows/test.yml next to `integration` (lines 30-37): setup-bun, dtolnay/rust-toolchain stable, cargo build, cargo test -p hapi-wrapper-core -p hapi-session-wrapper, bun run test:parity. TS-vs-golden failure means upstream drift: regenerate the corpus and port the change."
 }
}
```

## Runner integration
```json
{
 "spawn_selection": [
  "New cli/src/runner/wrapperRuntime.ts: `resolveWrapperRuntime(agent, env, platform)` → {runtime:'rust', bin} | {runtime:'ts', reason}. Reads HAPI_RUST_WRAPPER_FLAVORS on every spawn (comma list, trimmed, lower-case; '' or 'none' ⇒ ts) so rollback needs only a runner env change/restart, never a hub restart.",
  "Rust only when: platform === 'linux', agent ∈ {claude,codex,agy} ∩ flavors, startingMode !== 'local', binary resolvable and executable.",
  "Binary resolution order: HAPI_RUST_WRAPPER_BIN (absolute, exists) → dirname(realpath(resolveHappyCliExecutable()))/hapi-session-wrapper (cli/src/utils/spawnHappyCLI.ts:173-190) → <repo>/target/release/hapi-session-wrapper when !isBunCompiled().",
  "New `spawnSessionWrapper(runtime, args, options)` in cli/src/utils/spawnHappyCLI.ts: rust path = spawn(bin, args, options) with no nofile shell wrap (lines 101-132 are Bun-specific) and env additions HAPI_CLI_EXECUTABLE=<hapi>, HAPI_WRAPPER_CLI_VERSION=<package version>, HAPI_WRAPPER_RUNTIME_PATH=<runtimePath()>. Call site: cli/src/runner/run.ts:871-879. argv from buildCliArgs is passed unchanged."
 ],
 "fallback": [
  "Missing binary / non-Linux / flavor not enabled ⇒ TS silently, debug log with reason.",
  "Agent too old is decided by the wrapper, not the runner: it exits with code 86 before the webhook (codex <0.154, agy <1.5.0 or ACP initialize failure, claude never).",
  "In run.ts spawn flow (lines 846-1140): if runtime==='rust' and the failure is no-pid, 'exit-before-webhook' or 'process-error-before-webhook' (lines 894-915, 1018, 1029), do not resolve the error; respawn once with runtime 'ts', same args/env/cwd, fresh webhook timer. Record `wrapperFallback:{reason, exitCode}` in the debug log and in reportSpawnOutcomeToHub details only if the TS attempt also fails.",
  "Webhook timeout (line 1137) is not retried: the existing kill + error path stays.",
  "codex recoverable startup (lines 1039-1068): the retry happens inside the error awaiter before recoverableStartup.fail is called."
 ],
 "metadata_wrapperRuntime": "shared/src/schemas.ts after line 181: `wrapperRuntime: z.enum(['rust','ts']).optional()`. TS sets 'ts' in buildSessionMetadata (cli/src/agent/sessionFactory.ts:123-151); Rust sets 'rust'. Not added to pickExistingSessionMetadata (lines 154-218) so each generation restamps it.",
 "inventory_and_drift": [
  "cli/src/runner/wrapperInventory.ts:56 HAPI_BINARY_BASENAMES += 'hapi-session-wrapper'; hapiDriverEntryIndex (76-90) and extractFlavorFromArgv (93-97) then work unchanged because argv is identical.",
  "isMcpBridgeCommand (52-54) already excludes `hapi-session-wrapper mcp --url`; add an explicit exclusion for `hook-forwarder`.",
  "cli/src/runner/orphanReap.ts:46-56 isHapiDriverCliCommand: add name === 'hapi-session-wrapper' (the /\\bhapi\\b/ regex already matches the path, keep it).",
  "detectBinaryDrift (198-211): choose the reference binary per wrapper: basename(readlink /proc/pid/exe minus ' (deleted)') === 'hapi-session-wrapper' ⇒ compare dev+ino with stat(realpath(resolved rust bin)) and skip the isBunCompiled / 'src/index.ts' gates; otherwise current behaviour. Add `currentRustExecutable` to DriftProbeDeps (162-169). Unresolvable rust bin ⇒ null (no drift).",
  "run.ts adopt (1226-1246) stores flavor only; add `wrapperRuntime` to the persisted resume record for diagnostics.",
  "Hub side needs no change: drifted wrappers reach hibernation through the existing 'binary-drift' window (hub/src/sync/sessionIdle.ts:200-204)."
 ],
 "deploy": "scripts/deploy-rust.sh step 2 (line 15-16): add `-p hapi-session-wrapper`; then `install -m 0755 target/release/hapi-session-wrapper \"$DEST.tmp\" && mv -f \"$DEST.tmp\" \"$DEST\"` where DEST=${HAPI_RUST_WRAPPER_BIN:-$(dirname \"$(readlink -f \"$(command -v hapi)\")\")/hapi-session-wrapper}. Rename gives a new inode, so running wrappers report 'mismatch' rather than being overwritten in place.",
 "tests": "cli/src/runner/{wrapperInventory,orphanReap,buildCliArgs,run.recoveredStop}.test.ts extended; new wrapperRuntime.test.ts; shared/src/schemas.wrapperInventory.test.ts extended for wrapperRuntime."
}
```

## PBT (INVARIANT → falsification)
- INVARIANT Socket.IO packet encode/decode round-trips for any namespace, ack id and JSON args; decoder never panics on arbitrary bytes. → proptest: arbitrary Value trees + random ack ids; fuzz strings with stray digits/commas after the type char; cross-check 200 cases against socket.io-parser via a bun helper in the harness.
- INVARIANT Backoff delay ∈ [0, 5000], and for attempt n without jitter equals min(1000·2ⁿ, 5000); reset after connect. → proptest over attempt counts and injected rand ∈ [0,1); compare with backo2.js formula.
- INVARIANT Send buffer: non-volatile emits issued while disconnected arrive exactly once, in order, after reconnect; volatile ones never arrive. → proptest state machine: random interleaving of emit/volatile/disconnect/connect against an in-memory server model.
- INVARIANT CAS convergence: for any sequence of local handlers and remote version bumps, the final hub document equals handlers applied in order to the hub's value at each success, and local version == hub version. → proptest: model hub with random forced mismatches, errors, dropped acks; assert no lost or doubled handler application. Same generator in TS (fast-check) against ApiSessionClient with a fake socket, excluding dropped-ack cases (A2).
- INVARIANT IncomingMessageFilter: a message id is accepted at most once while within LRU capacity; id-less messages are accepted iff seq > cursor; cursor is monotone except rewind. → proptest random streams with duplicates/re-emits every k; differential against the TS class through a JSON test vector file generated by vitest.
- INVARIANT MessageQueue2: every pushed localId ends in exactly one of {delivered in a batch, cancelled, consumed, indeterminate}; batches never mix mode hashes; isolated items are alone; order within a mode is FIFO. → proptest op sequences (push, pushIsolateAndClear, unshift, take, restore, commit, cancel, collect); differential vectors from TS.
- INVARIANT Permission state: a request id is in at most one of agentState.requests / completedRequests; completed entries are never overwritten unless status was 'canceled'. → proptest random add/response/abort/reset/unknown-id sequences; check after each CAS apply.
- INVARIANT claude uuid chain: main-chain rows form a single linked list (each parentUuid is the previous main-chain uuid or null after reset); sidechain rows only link within the same parent_tool_use_id. → proptest random SDK message streams with nested Task tool uses and session resets.
- INVARIANT Argv parser accepts every buildCliArgs output and reproduces (flavor, resume id, session ids, model, effort, permission mode, passthrough args). → TS fast-check generates SpawnSessionOptions → buildCliArgs → `hapi-session-wrapper --parse-only` JSON → compare with the TS parser (commands/claude.ts) output.
- INVARIANT Start marker read by Rust equals getProcessStartMarker(pid) for the same live pid. → Integration: spawn 50 children across a second boundary; compare both implementations.
- INVARIANT wrapperInventory: every `--started-by runner` process of either binary lands in exactly one bucket; a Rust wrapper is never reported as drift when its exe dev+ino equals the resolved Rust binary, regardless of the hapi binary's identity. → fast-check over process snapshots mixing both basenames, deleted links and identities (extends wrapperInventory.test.ts).
- INVARIANT Runner fallback: for any failure point before the webhook exactly one TS spawn follows; after a webhook no retry happens; total spawns ≤ 2. → fast-check over event orderings (exit/error/webhook/timeout) on the extracted retry state machine.
- INVARIANT (1b) Registry files written by Rust parse under the TS zod schema and vice versa; `active` is false for every session after a clean shutdown. → proptest record generator → file → TS strict reader in a bun helper; kill-at-random-point test for shutdown.
- INVARIANT (1b) Projection ids are a pure function of (threadId, turnId, item) and identical in TS and Rust; replaying history twice emits nothing new. → Differential vectors: TS dumps ids for the converter fixture corpus; Rust must match all, including the sha256 fallback cases.
- INVARIANT (1b) Queue ledger: every localId is in exactly one state; consumed is only reached via a matching userMessage; a crash after any journal write is recoverable without double delivery. → proptest op sequences with injected RPC outcomes (ok / definite error / indeterminate) and crash-restart points.
- INVARIANT (1c) ndjson transport: arbitrary chunking of stdout yields the same message sequence; responses are matched to requests by id exactly once. → proptest: split a valid stream at random byte offsets, interleave notifications.
- INVARIANT (1c) Reactive wakeup inspector returns the same state for the same (steps, now) irrespective of how the file was read (full vs incremental). → proptest: append steps in random chunk sizes, with truncation/replacement to exercise cache invalidation.

## Risks / Trade-offs
- Scope: ~24.6k Rust LOC excluding tests across three waves, with 1a alone ≈ 10.9k because the shared core includes the common RPC handlers, MCP tools and PTY terminal.
- The TS wrapper is a moving target (164/117/106/51 commits in 90 days); the parity job turns upstream drift into a failing check, which is the intent but adds a porting tax to every wrapper-touching PR.
- Socket.IO client is hand-written: buffer/ack/reconnect ordering bugs show up as lost acks or zombie sessions. Mitigated only by the node-server integration test and the reconnect scenarios.
- A2 is a deliberate behavioural difference from TS (retry after a dropped CAS ack); if the hub applied the write before the disconnect, the retry sees a version mismatch and re-applies the handler, which is safe only because handlers are pure functions of the current document.
- Codex: a single malformed registry file or a lock-protocol mismatch blocks runner stops and cold resumes for every codex session on the machine (TS strict read, cli/src/codex/shared/registry.ts:67). 1b must not ship before tasks 2.2 and 2.3 pass.
- Codex projection ids include a sha256 over JS-ordered JSON; any key-order difference duplicates history after a TS↔Rust switch.
- Peer tools still launch a Bun process on demand (A5), so 'no hidden Bun processes' holds for resident helpers only until the follow-up port.
- Start marker depends on procps `ps` output; a busybox ps or a locale-forcing wrapper breaks adoption for both runtimes equally, but the Rust wrapper adds a second place to keep in sync.
- agy without interval polling relies on inotify delivery on the brain directory; network or overlay filesystems that drop events fall back to the 350 ms timer and lose the S2 target.
- Expected gain shrinks once hibernation is live: only resident wrappers benefit, so each PR must report measured numbers rather than the 147 MB → 15 MB headline.
- Not verified in this pass: proper-lockfile internals, MCP SDK version negotiation list, hyper-rustls feature names, per-item mapping in appServerEventConverter.ts:977-1282, and tool handler bodies in startHappyServer.ts:520-712. Each has an explicit task that reads the source first.
