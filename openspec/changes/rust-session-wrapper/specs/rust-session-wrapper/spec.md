## ADDED Requirements

### Requirement: Rust wrapper selected per flavor
The runner SHALL read `HAPI_RUST_WRAPPER_FLAVORS` (comma list; unset, empty or `none` means TS everywhere) on every spawn. It MUST start `hapi-session-wrapper` only when the platform is Linux, the flavor is in {claude, codex, agy} and in the list, startingMode is not `local`, and the binary resolves and is executable. Otherwise it MUST start the TS wrapper. The argv MUST be byte-identical to `buildCliArgs`. The session MUST record `metadata.wrapperRuntime` (`rust` or `ts`).

#### Scenario: instant rollback
- **WHEN** `HAPI_RUST_WRAPPER_FLAVORS` is set to empty and the runner restarts
- **THEN** new sessions use the TS wrapper, with no hub restart

#### Scenario: binary missing
- **WHEN** the flavor is enabled but the binary cannot be resolved
- **THEN** a TS wrapper is started and the reason is logged at debug level

### Requirement: Startup fallback
When the Rust wrapper cannot run before the webhook, it MUST exit with code 86. If it already created a hub session, it MUST first write `HAPI_RUST_WRAPPER_SESSION_ID=<id>` to stderr. The runner MUST confirm the Rust process is dead before retrying exactly once with TS, using `--existing-session-id <id>` when the id is known. A webhook timeout MUST NOT trigger the retry.

#### Scenario: agent version too old
- **WHEN** codex < 0.154 or agy < 1.5.0
- **THEN** the Rust wrapper exits 86 before any network I/O, a TS wrapper is started, and the hub ends up with exactly one session

#### Scenario: failure after session creation
- **WHEN** the Rust wrapper fails after `POST /cli/sessions` but before the webhook
- **THEN** the TS retry reuses the same session id, and no orphan or duplicate session appears

### Requirement: Wire protocol and behaviour identical to TS
The Rust wrapper MUST match TS on the hub-visible wire: Socket.IO v4 over WebSocket only with the default JSON parser, the auth payload, REST order and headers, event names and payload shapes, ack shapes and timeouts, the 10 s volatile keepalive, CAS retry rules, incoming dedup, and the hub-archived stop latch. JSON MUST be semantically equal. Fields required by the design (such as codex batchKey and the projection hash) MUST be byte-equal.

#### Scenario: transcript diff
- **WHEN** the same recorded scenario runs against both the TS and the Rust wrapper
- **THEN** the normalised hub socket transcripts are identical

### Requirement: Lifecycle and process hygiene
The Rust wrapper MUST stay its own process-group leader so that it outlives runner restarts. On SIGTERM it MUST send session-end within 1500 ms, kill the agent process tree and exit. The webhook metadata `hostPid` / `hostStartMarker` MUST match the format of `ps -o lstart=` (LC_ALL=C TZ=UTC). The runner's wrapper inventory, orphan reaping and drift detection MUST recognise both binaries, each compared against its own dev+ino.

#### Scenario: stop
- **WHEN** the runner runs StopSession on a Rust-wrapped session
- **THEN** the result is `stopped` and 0 processes remain in the process group

#### Scenario: inventory
- **WHEN** the runner restarts and scans
- **THEN** each Rust wrapper is tracked or adopted, and is never reported as drift

### Requirement: Hibernation and cross-implementation resume
claude and codex Rust wrappers MUST expose the same resume-id metadata as TS. Resuming a hibernated session in either direction, TS → Rust or Rust → TS, MUST keep the session id unchanged and the transcript intact, with no duplicate messages and no merge.

#### Scenario: round trip
- **WHEN** a session is hibernated under the TS wrapper and then resumed under the Rust wrapper
- **THEN** it keeps the same session id with no duplicate or lost messages

### Requirement: No hidden Bun processes
The Rust wrapper MUST serve the codex MCP bridge and the claude hook-forwarder through its own subcommands. The only allowed TS exception is the on-demand `hapi peer-tool`, which MUST exit when done.

#### Scenario: idle session process tree
- **WHEN** a Rust-wrapped session is idle
- **THEN** no Bun/node process belonging to HAPI exists in its process tree

### Requirement: Resource targets
An idle claude or agy Rust wrapper MUST have PSS ≤ 15 MB. Idle 60 s mean CPU MUST be ≤ 0.1% with ≤ 1 wakeup/s, excluding the keepalive and the Engine.IO ping. spawn → webhook p50 MUST NOT be slower than TS. Each wave's PR MUST include an india-vps measurement table.

#### Scenario: measurement
- **WHEN** a Rust wrapper has been idle on india-vps for 10 minutes
- **THEN** smaps_rollup and schedstat readings meet the targets above
