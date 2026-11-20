import { existsSync, readFileSync, rmSync, renameSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { SyncEngine } from '../../../../hub/src/sync/syncEngine.ts'

const request = join(process.env.HAPI_HOME, 'test-idle-session')
const original = SyncEngine.prototype.hibernateIdleSessions
SyncEngine.prototype.hibernateIdleSessions = async function (...args) {
    if (!existsSync(request)) return original.apply(this, args)
    const id = readFileSync(request, 'utf8')
    const now = Date.now() + SyncEngine.HIBERNATE_STARTUP_GRACE_MS + 60_001
    const status = await this.hibernateSession(id, { now, hibernateMs: 60_000 })
    writeFileSync(request + '.tmp', JSON.stringify({ id, status }))
    renameSync(request + '.tmp', request + '.result')
    rmSync(request, { force: true })
    return status === 'hibernated' ? [id] : []
}

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
