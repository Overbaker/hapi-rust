import assert from 'node:assert/strict'
import { readRuntimes, saveRuntime, runtimeAuthHash } from '../../../../../cli/src/codex/shared/registry'

const records = await readRuntimes({ strict: true })
assert.equal(records.length, 1)
const [rust] = records
assert.equal(rust.id, 'owner')
assert.equal(rust.authHash, runtimeAuthHash())
assert.deepEqual(rust.sessions.sid, { threadId: 'thread', namespace: 'ns', active: true })
await saveRuntime({
    ...rust, id: 'ts', kind: 'attached', serverPid: undefined, serverMarker: undefined,
    command: '', args: [], token: 'secret', attachedAt: 1234,
    nativeEndpoint: 'unix:///private/engine.sock', supersedes: 'old', pendingCreations: ['pending']
})
