import { beforeAll, describe, expect, it } from 'bun:test'
import { Hono } from 'hono'
import type { SyncEngine } from '../../sync/syncEngine'
import type { WebAppEnv } from '../middleware/auth'
import { createConfiguration } from '../../configuration'
import { createCliRoutes } from './cli'
import { createSessionsRoutes } from './sessions'

beforeAll(async () => {
    const config = await createConfiguration()
    config._setCliApiToken('test-token', 'env', false)
}, 30_000)

const SESSIONS = [
    { id: 'abc-111', namespace: 'default' },
    { id: 'abc-222', namespace: 'default' },
    { id: 'xyz-333', namespace: 'default' },
    { id: 'foreign-1', namespace: 'other' },
]

function fakeEngine(calls: Array<[string, string, string, unknown?]>): Partial<SyncEngine> {
    const find = (id: string) => SESSIONS.find((session) => session.id === id)
    return {
        getSession: ((id: string) => find(id)) as never,
        getSessionsByNamespace: ((namespace: string) => SESSIONS.filter((session) => session.namespace === namespace)) as never,
        resolveSessionAccess: ((id: string, namespace: string) => {
            const session = find(id)
            if (!session) return { ok: false, reason: 'not-found' }
            if (session.namespace !== namespace) return { ok: false, reason: 'access-denied' }
            return { ok: true, sessionId: id, session: { ...session, active: true, metadata: null } }
        }) as never,
        hibernateSessionNow: (async (id: string, namespace: string) => {
            calls.push(['hibernate', id, namespace])
            const session = find(id)
            if (!session) return { type: 'refused', sessionId: id, code: 'session_not_found' }
            if (session.namespace !== namespace) return { type: 'refused', sessionId: id, code: 'access_denied' }
            if (id === 'xyz-333') return { type: 'refused', sessionId: id, code: 'mid_turn' }
            return { type: 'hibernated', sessionId: id }
        }) as never,
        markArchivedSessionHibernated: ((id: string, namespace: string, expectedVersion?: number) => {
            calls.push(['mark', id, namespace, expectedVersion])
            if (find(id)?.namespace !== namespace) return { type: 'refused', sessionId: id, code: 'access_denied' }
            if (expectedVersion === 1) return { type: 'refused', sessionId: id, code: 'version_mismatch' }
            return { type: 'hibernated', sessionId: id }
        }) as never,
        listHibernateCandidates: ((namespace: string, opts: unknown) => {
            calls.push(['candidates', namespace, '', opts])
            return [{ sessionId: 'abc-111', idleMs: 1, refusal: null }, { sessionId: 'xyz-333', idleMs: 1, refusal: 'mid_turn' }]
        }) as never,
        listSignalArchivedSessions: ((namespace: string, opts: unknown) => {
            calls.push(['signalled', namespace, '', opts])
            return []
        }) as never,
    }
}

function webApp(engine: Partial<SyncEngine>, namespace = 'default') {
    const app = new Hono<WebAppEnv>()
    app.use('*', async (c, next) => {
        c.set('namespace', namespace)
        await next()
    })
    app.route('/api', createSessionsRoutes(() => engine as SyncEngine))
    return app
}

function cliApp(engine: Partial<SyncEngine>) {
    const app = new Hono()
    app.route('/cli', createCliRoutes(() => engine as SyncEngine))
    return app
}

const post = (body?: unknown, token = 'test-token') => ({
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    ...(body === undefined ? {} : { body: JSON.stringify(body) })
})

describe('POST /api/sessions/:id/hibernate', () => {
    it('hibernates in the caller namespace and reports refusals', async () => {
        const calls: Array<[string, string, string, unknown?]> = []
        const app = webApp(fakeEngine(calls))
        const ok = await app.request('/api/sessions/abc-111/hibernate', { method: 'POST' })
        expect(ok.status).toBe(200)
        expect(await ok.json()).toEqual({ ok: true, sessionId: 'abc-111' })
        const refused = await app.request('/api/sessions/xyz-333/hibernate', { method: 'POST' })
        expect(refused.status).toBe(409)
        expect(await refused.json()).toEqual({ error: 'The agent is mid-turn', code: 'mid_turn' })
        expect(calls).toEqual([['hibernate', 'abc-111', 'default'], ['hibernate', 'xyz-333', 'default']])
    })

    it('denies another namespace before calling the engine', async () => {
        const calls: Array<[string, string, string, unknown?]> = []
        const app = webApp(fakeEngine(calls))
        expect((await app.request('/api/sessions/foreign-1/hibernate', { method: 'POST' })).status).toBe(403)
        expect((await app.request('/api/sessions/missing/hibernate', { method: 'POST' })).status).toBe(404)
        expect(calls).toEqual([])
    })
})

describe('CLI hibernate routes', () => {
    it('requires the CLI token', async () => {
        const app = cliApp(fakeEngine([]))
        expect((await app.request('/cli/sessions/abc-111/hibernate', { method: 'POST' })).status).toBe(401)
        expect((await app.request('/cli/sessions/abc-111/hibernate', post(undefined, 'wrong'))).status).toBe(401)
    })

    it('resolves a unique id prefix within the token namespace', async () => {
        const calls: Array<[string, string, string, unknown?]> = []
        const app = cliApp(fakeEngine(calls))
        const ok = await app.request('/cli/sessions/xyz/hibernate', post())
        expect(ok.status).toBe(409)
        expect(await ok.json()).toMatchObject({ code: 'mid_turn' })
        const ambiguous = await app.request('/cli/sessions/abc/hibernate', post())
        expect(ambiguous.status).toBe(409)
        expect(await ambiguous.json()).toMatchObject({ code: 'ambiguous_prefix' })
        expect((await app.request('/cli/sessions/foreign/hibernate', post())).status).toBe(404)
        expect(calls).toEqual([['hibernate', 'xyz-333', 'default']])
    })

    it('scopes a full foreign id to the token namespace', async () => {
        const calls: Array<[string, string, string, unknown?]> = []
        const app = cliApp(fakeEngine(calls))
        expect((await app.request('/cli/sessions/foreign-1/hibernate', post())).status).toBe(403)
        expect((await app.request('/cli/sessions/foreign-1/hibernate', post(undefined, 'test-token:other'))).status).toBe(200)
        expect((await app.request('/cli/sessions/foreign-1/mark-hibernated', post({}))).status).toBe(403)
    })

    it('marks a signal-archived session hibernated and passes the expected version', async () => {
        const calls: Array<[string, string, string, unknown?]> = []
        const app = cliApp(fakeEngine(calls))
        const ok = await app.request('/cli/sessions/abc-222/mark-hibernated', post({ expectedVersion: 4 }))
        expect(ok.status).toBe(200)
        const stale = await app.request('/cli/sessions/abc-222/mark-hibernated', post({ expectedVersion: 1 }))
        expect(stale.status).toBe(409)
        expect(await stale.json()).toMatchObject({ code: 'version_mismatch' })
        expect((await app.request('/cli/sessions/abc-222/mark-hibernated', post({ expectedVersion: 'x' }))).status).toBe(400)
        expect((await app.request('/cli/sessions/abc-222/mark-hibernated', post())).status).toBe(200)
        expect(calls).toEqual([
            ['mark', 'abc-222', 'default', 4],
            ['mark', 'abc-222', 'default', 1],
            ['mark', 'abc-222', 'default', undefined],
        ])
    })

    it('lists candidates and signal-archived sessions for the token namespace', async () => {
        const calls: Array<[string, string, string, unknown?]> = []
        const app = cliApp(fakeEngine(calls))
        const headers = { authorization: 'Bearer test-token:other' }
        const candidates = await app.request('/cli/hibernate/candidates?machineId=m1&minIdleMinutes=30', { headers })
        expect(candidates.status).toBe(200)
        expect(await candidates.json()).toEqual({ sessions: [
            { sessionId: 'abc-111', idleMs: 1, refusal: null },
            { sessionId: 'xyz-333', idleMs: 1, refusal: 'mid_turn', message: 'The agent is mid-turn' },
        ] })
        expect((await app.request('/cli/hibernate/signal-archived?since=2026-11-20T00:00:00Z', { headers })).status).toBe(200)
        expect((await app.request('/cli/hibernate/signal-archived?since=yesterday', { headers })).status).toBe(400)
        expect(calls).toEqual([
            ['candidates', 'other', '', { machineId: 'm1', minIdleMs: 30 * 60_000 }],
            ['signalled', 'other', '', { machineId: undefined, since: Date.parse('2026-11-20T00:00:00Z') }],
        ])
    })
})
