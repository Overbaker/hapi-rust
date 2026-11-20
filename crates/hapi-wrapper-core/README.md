# hapi-wrapper-core

This library provides the foundations for an optional native HAPI session wrapper.
It implements JavaScript-compatible JSON helpers, resolves CLI configuration from
the environment and settings file, and connects to the hub’s `/cli` namespace
over WebSocket using Engine.IO 4 and Socket.IO 5. The TypeScript product does
not use this crate yet.

It also provides the pieces the session layer of `cli/src/api/apiSession.ts`
builds on:

- `rest`: the hub REST client (machine registration, session create, fetch and
  message backfill) and the best-effort runner `session-started` webhook.
- `lifecycle::start_marker`: the `ps lstart` marker that identifies a live
  process across PID reuse.
- `rpc::manager`: session-scoped RPC handler registration and dispatch.
- `rpc::handlers`: workspace path checks and the file RPCs of
  `cli/src/modules/common/handlers/files.ts` and `directories.ts`
  (`readFile`, `readGeneratedImage`, `writeFile`, `listDirectory`, `statFiles`,
  `getDirectoryTree`), plus the in-memory generated media store.
- `rpc::handlers::uploads`: the upload RPCs of
  `cli/src/modules/common/handlers/uploads.ts` (`uploadFile`, `deleteUpload`,
  and chunked `uploadInit`, `uploadChunk`, `uploadFinalize`, `uploadAbort`),
  the per-session upload directory under `<tmp>/hapi-blobs`, and the file
  identity checks used for attachments.
- `schemas`: the session metadata checks of `shared/src/schemas.ts`, including
  the full OpenCode rewind record.

Some tests compare results with the TypeScript implementation. They run `bun`
from `cli/`, and start a Socket.IO hub fixture from `hub/node_modules`. Run
`bun install` first.

The PEM files in `tests/fixtures/` contain a self-signed test CA and a localhost
server certificate and private key. They are used only by the TLS tests.

The file RPCs check both the lexical path and its canonical target against the
working directory, so `listDirectory` and `writeFile` refuse a symlink that
leads outside it (TypeScript checks only the lexical path for these two).
Tree and listing traverse the canonical path, so `getDirectoryTree` reports
canonical paths. The handlers are built on Unix only.
`deleteUpload` rejects a path that contains `..` or whose parent resolves
outside the session upload directory (TypeScript checks the resolved path
string only). An `uploadAbort` that arrives while `uploadFinalize` is running
makes the finalize fail with `upload_conflict` and removes the file
(TypeScript lets the finalize complete).
Directory entries follow English `localeCompare` order for printable ASCII;
other characters sort after ASCII by code point.

Socket.IO JSON decoding replaces each unpaired UTF-16 surrogate with U+FFFD.
Valid surrogate pairs retain their character, and re-encoding preserves the
normalized string.
