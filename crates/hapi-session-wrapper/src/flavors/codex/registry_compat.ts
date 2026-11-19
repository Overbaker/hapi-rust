import assert from 'node:assert/strict'
import { readRuntimes, saveRuntime, runtimeAuthHash } from '../../../../../cli/src/codex/shared/registry'

const records = await readRuntimes({ strict: true })
assert.equal(records.length, 2)
const rust = records.find(r => r.id === 'owner')!
const attached = records.find(r => r.id === 'rust-attached')!
assert.deepEqual(attached, {
    ...Object.fromEntries(Object.entries(rust).filter(([k]) => !['serverPid', 'serverMarker'].includes(k))),
    id: 'rust-attached', kind: 'attached', nativeEndpoint: 'unix:///attached.sock', attachedAt: 1234,
    supersedes: 'old', token: 'attached-token', command: '', args: [], pendingCreations: ['reservation']
})
assert.equal(rust.id, 'owner')
assert.equal(rust.authHash, runtimeAuthHash())
assert.deepEqual(rust.sessions.sid, { threadId: 'thread', namespace: 'ns', active: true })
await saveRuntime({
    ...rust, id: 'ts', kind: 'attached', serverPid: undefined, serverMarker: undefined,
    command: '', args: [], token: 'secret', attachedAt: 1234,
    nativeEndpoint: 'unix:///private/engine.sock', supersedes: 'old', pendingCreations: ['pending']
})
