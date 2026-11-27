import { afterEach, describe, expect, it } from 'bun:test'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { Hono } from 'hono'
import { updateSettings, getSettingsFile } from '../config/settings'
import type { WebAppEnv } from './middleware/auth'
import { createOwnerSettingsAccessMiddleware, hasOwnerSettingsAccess } from './ownerSettingsAccess'

const directories: string[] = []
afterEach(async () => {
    await Promise.all(directories.splice(0).map((dir) => rm(dir, { recursive: true, force: true })))
})

async function probe(namespace: string, share?: boolean): Promise<boolean> {
    const dataDir = await mkdtemp(join(tmpdir(), 'hapi-owner-access-'))
    directories.push(dataDir)
    if (share !== undefined) {
        await updateSettings(getSettingsFile(dataDir), (current) => ({
            settings: { ...current, shareOwnerSettingsWithNamespaces: share },
            result: null
        }))
    }
    const app = new Hono<WebAppEnv>()
    app.use('*', async (c, next) => {
        c.set('namespace', namespace)
        await next()
    })
    app.use('*', createOwnerSettingsAccessMiddleware(dataDir))
    app.get('/probe', (c) => c.json({ access: hasOwnerSettingsAccess(c) }))
    const body = await (await app.request('/probe')).json() as { access: boolean }
    return body.access
}

describe('owner settings access', () => {
    it('is shared with other namespaces by default', async () => {
        expect(await probe('tenant')).toBe(true)
    })

    it('stays with the owner when sharing is off', async () => {
        expect(await probe('tenant', false)).toBe(false)
        expect(await probe('default', false)).toBe(true)
    })
})
