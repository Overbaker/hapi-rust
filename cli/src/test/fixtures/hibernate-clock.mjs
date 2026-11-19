import { existsSync, readFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'
import { SyncEngine } from '../../../../hub/src/sync/syncEngine.ts'

const request = join(process.env.HAPI_HOME, 'test-idle-session')
const original = SyncEngine.prototype.hibernateIdleSessions
SyncEngine.prototype.hibernateIdleSessions = async function (...args) {
    if (!existsSync(request)) return original.apply(this, args)
    const id = readFileSync(request, 'utf8')
    const now = Date.now() + SyncEngine.HIBERNATE_STARTUP_GRACE_MS + 60_001
    if (await this.hibernateSession(id, { now, hibernateMs: 60_000 }) !== 'hibernated') return []
    rmSync(request, { force: true })
    return [id]
}
