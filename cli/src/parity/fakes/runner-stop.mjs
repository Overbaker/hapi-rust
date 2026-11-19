import '../../test/fixtures/hibernate-clock.mjs'
import { existsSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { SyncEngine } from '../../../../hub/src/sync/syncEngine.ts'

const file = join(process.env.HAPI_HOME, 'test-raw-stop')
const sweep = SyncEngine.prototype.hibernateIdleSessions
SyncEngine.prototype.hibernateIdleSessions = async function (...args) {
    if (!existsSync(file)) return sweep.apply(this, args)
    renameSync(file, file + '.running')
    const id = readFileSync(file + '.running', 'utf8')
    const m = this.getSession(id)?.metadata
    if (!m?.hostStartMarker || !m.hostPid || !m.machineId)
        throw Error('missing verified test wrapper identity')
    const status = await this.rpcGateway.stopRunnerSession(m.machineId, 'PID-' + m.hostPid, {
        processStartMarker: m.hostStartMarker
    })
    writeFileSync(file + '.tmp', JSON.stringify({ pid: m.hostPid, status }))
    renameSync(file + '.tmp', file + '.result')
    return []
}
