# hapi-wrapper-core

This library provides the foundations for an optional native HAPI session wrapper.
It implements JavaScript-compatible JSON helpers, resolves CLI configuration from
the environment and settings file, and connects to the hub’s `/cli` namespace
over WebSocket using Engine.IO 4 and Socket.IO 5. The TypeScript product does
not use this crate yet.

On top of the socket it provides the session layer of `cli/src/api/apiSession.ts`:

- `rest`: the hub REST client (machine registration, session create, fetch and
  message backfill) and the best-effort runner `session-started` webhook.
- `session::bootstrap` and `session::metadata`: create or reopen a session and
  build its metadata. Runner-started sessions record the `ps lstart` marker from
  `lifecycle::start_marker`.
- `session::versioned_ack`: queued compare-and-swap updates for metadata and
  agent state, retried with jittered backoff.
- `session::client`: RPC registration and `session-alive` on every connect,
  `update` handling, the hub-archived latch, the stoppable 10 s keepalive,
  incoming dedup with backfill after reconnect, the emit API, prompt-echo
  bookkeeping, and flush and close.
- `rpc::manager`: session-scoped RPC handler registration and dispatch.
- `schemas`: the session metadata checks of `shared/src/schemas.ts`, including
  the full OpenCode rewind record.

Some tests compare results with the TypeScript implementation. They run `bun`
and the CLI's `vitest` from `cli/`, and start a Socket.IO hub fixture from
`hub/node_modules`. Run `bun install` first.

The PEM files in `tests/fixtures/` contain a self-signed test CA and a localhost
server certificate and private key. They are used only by the TLS tests.

Socket.IO JSON decoding replaces each unpaired UTF-16 surrogate with U+FFFD.
Valid surrogate pairs retain their character, and re-encoding preserves the
normalized string.
