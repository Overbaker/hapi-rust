`bun run test:parity` compares the TS wrapper with the corpus and reports the pending Rust scenarios. With `HAPI_PARITY_RUST_BIN` supplied, the default runs both legs. Set `PARITY_RUNTIME=ts|rust|both` to select legs explicitly. The Rust leg checks that its binary exists and is executable. Relative paths start at the repository root.

Scenarios in `rustPending.ts` appear as named Vitest todos on the Rust leg. Set `PARITY_RUN_PENDING=1` to exercise them against a candidate binary and treat any divergence as a failure. Removing a scenario from the list makes its Rust comparison mandatory.

Each step waits for all its event fences, including the trailing metadata and state acknowledgements. Socket.IO can discard intermediate volatile heartbeats. Before shutdown or a disconnect, compare the expected settled state against the latest heartbeat on the current connection. A matching sample is reusable. The golden records each connection’s first and last payload, and the harness validates every heartbeat timestamp.

The Codex corpus uses a PATH shim serving WebSockets on Codex's private Unix socket.
Record it from the source TS CLI with `cd cli && bun src/parity/record.ts --codex`.
Each scenario observes the same REST, Socket.IO, runner webhook and web-decoded channels as Claude.
The launch log checks the engine argv and cwd, the native home and the absence of an inherited root identity.
The engine waits for scripted projection fences before continuing a turn. During bind the hub
holds the first binding metadata ACK until the initial native queue snapshot, making the stale-snapshot
branch reproducible. The test driver supplies both subjects with the same stepwise wall clock
using a test-only Linux preload built with `cc`. Turn durations remain in the comparison.
Multiple roots have independent MCP ports and lifecycle clocks. Dynamic permission-map keys
use the same UUID mapping as their references in values.
