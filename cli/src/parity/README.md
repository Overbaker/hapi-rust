# Claude remote wrapper wire contract

Run `bun run test:parity` from the repository root on Linux. Each scenario launches the CLI from this checkout against a loopback Socket.IO `/cli` hub, REST endpoints, and a fake runner control server. The wrapper launches the scripted Claude executable through `HAPI_CLAUDE_PATH`. The golden captures wire events and acknowledgements, agent invocations and stdin, and the messages decoded by the web client.

## Add or update a scenario

1. Copy `corpus/claude/plain-turn/` to a new scenario directory. Edit `scenario.json` to set wrapper arguments and script the hub actions. Edit `agent.jsonl` to match stdin and emit Claude stream events, one script step per line.
2. Give each hub action an `until` fence for the events and trailing acknowledgements it triggers. Use `release` and an agent `awaitMarker` to synchronize interleaved actions. Before disconnect or shutdown, use `settledAlive` to wait for the expected heartbeat on that connection.
3. Run `bun run parity:record <scenario-name>` and inspect the golden diff. Run the command again to check that the recording is identical, then run `bun run test:parity`. Omit the scenario name to re-record the full corpus.

The normalizer validates process IDs, paths, ports, and timestamps before replacing variable values. It preserves event order within each step and channel and compares the first and last volatile heartbeat on each connection.

Set `HAPI_PARITY_WRAPPER_BIN` to an executable to run a second wrapper against the same goldens. Relative paths resolve from the repository root. The executable receives the scenario's CLI arguments and environment. The supplied wrapper must match the CLI version in `cli/package.json` because the fake runner advertises that version during startup.

## Rust wrapper leg

Set `HAPI_PARITY_RUST_BIN` to a `hapi-session-wrapper` build to compare the Rust Claude wrapper with the same goldens: `HAPI_PARITY_RUST_BIN=target/release/hapi-session-wrapper bun run test:parity`. With the binary supplied the default runs both legs; set `PARITY_RUNTIME=ts|rust|both` to select legs explicitly. The Rust leg receives `HAPI_WRAPPER_CLI_VERSION` and `HAPI_WRAPPER_RUNTIME_PATH` as the runner would set them, and accepts a registered RPC method list that omits the other agents' model listers.

### Runtime identity

The Rust wrapper stamps session metadata with wrapperRuntime=rust and exposes
the Claude model lister. The TS wrapper registers the other agents' model
listers too. The Rust comparison checks the exact registration set on every
connection, including reconnect.

## Linux shared Codex

Record the eleven TypeScript transcripts with `bun run parity:record --codex`.
Run `PARITY_RUNTIME=ts bun run test:parity codexParity` to check them independently.
The Rust leg compares the entire transcript with the named Codex entries in
`rustKnownGaps.ts`. Set `HAPI_PARITY_RAW=1` to compare directly with the TS golden.

Shared Codex roots report upload and peer-watch activity. Their TS ready call takes
no native ID, so the heartbeat carries no native-resume proof. Their kill RPC
ends one root and acknowledges success. Process-generation kill methods belong
to the process shutdown handler. Registering one on a shared root could stop
a sibling's engine. The lifecycle cases exercise stopping a root with a live
sibling and the hub's idle sweep skipping a shared session.

## Linux agy

Record the agy transcripts with `bun run parity:record --agy [scenario]` and check them
with `PARITY_RUNTIME=ts bun run test:parity agyParity`. `fakes/agy.ts` stands in for
`agy` on PATH: `--version` answers 1.5.3 and `agy acp` speaks ndjson JSON-RPC driven by
the scenario's `agent.jsonl` (see the header of the fake for the step DSL). Methods the
script never awaits (`initialize`, `session/new`, `session/resume`, `session/list`,
`session/set_mode`, `session/set_model`) are answered automatically.

The agy harness scrubs `duration_ms`, `generation_ms` and the `session-ready` time, and
gives request ids used as agent-state keys (`<call>#<uuid>`) ordinal `<key:N>` tokens.
Scenarios that answer a permission or a question fence on the pending tool card first,
so the turn's output is attributed to the same step on both legs.

The Rust leg compares with the named agy entries in `rustKnownGaps.ts`: the runtime
stamp, the model listers of other agents, the process-generation kill method, and the
shared Rust shutdown handler's kill acknowledgement (machine id, `User terminated`).
