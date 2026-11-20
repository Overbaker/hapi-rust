import { describe, expect, it } from 'vitest'
import { formatHibernateOutcome, HibernateArgsError, parseHibernateArgs, runHibernate } from './hibernate'

describe('parseHibernateArgs', () => {
    it('parses each mode', () => {
        expect(parseHibernateArgs(['7d55ed21'])).toEqual({ kind: 'one', sessionIdPrefix: '7d55ed21' })
        expect(parseHibernateArgs(['--all-idle'])).toEqual({ kind: 'all-idle', minIdleMinutes: undefined })
        expect(parseHibernateArgs(['--all-idle', '--min-idle-minutes', '30'])).toEqual({ kind: 'all-idle', minIdleMinutes: 30 })
        expect(parseHibernateArgs(['--from-archived', 'abc'])).toEqual({ kind: 'from-archived', sessionIdPrefix: 'abc' })
        expect(parseHibernateArgs(['abc', '--from-archived'])).toEqual({ kind: 'from-archived', sessionIdPrefix: 'abc' })
        expect(parseHibernateArgs(['--all-signalled'])).toEqual({ kind: 'all-signalled', since: undefined })
        expect(parseHibernateArgs(['--all-signalled', '--since', '2026-11-20T08:00:00+08:00']))
            .toEqual({ kind: 'all-signalled', since: '2026-11-20T00:00:00.000Z' })
        expect(parseHibernateArgs(['--help'])).toEqual({ kind: 'help' })
        expect(parseHibernateArgs(['abc', '-h'])).toEqual({ kind: 'help' })
    })

    it.each([
        [[]],
        [['--from-archived']],
        [['a', 'b']],
        [['--yolo']],
        [['--all-idle', 'abc']],
        [['--all-idle', '--all-signalled']],
        [['--all-idle', '--from-archived', 'abc']],
        [['--all-idle', '--min-idle-minutes']],
        [['--all-idle', '--min-idle-minutes', '-5']],
        [['--all-idle', '--min-idle-minutes', 'soon']],
        [['abc', '--min-idle-minutes', '5']],
        [['--all-signalled', '--since', 'not-a-date']],
        [['--all-idle', '--since', '2026-11-20T00:00:00Z']],
    ])('rejects %j', (args) => {
        expect(() => parseHibernateArgs(args)).toThrow(HibernateArgsError)
    })
})

describe('runHibernate', () => {
    function fakeHttp(responses: Record<string, { status: number; data: unknown }>) {
        const requests: Array<{ method: string; url: string; params?: unknown; auth?: string }> = []
        const respond = (method: string, url: string, config: any) => {
            requests.push({ method, url, params: config?.params, auth: config?.headers?.Authorization })
            return responses[`${method} ${url}`] ?? { status: 404, data: { error: 'nope' } }
        }
        const http = {
            post: async (url: string, _body: unknown, config: unknown) => respond('POST', url, config),
            get: async (url: string, config: unknown) => respond('GET', url, config),
        }
        return { http: http as never, requests }
    }

    it('posts one session with the CLI token and reports refusals', async () => {
        const { http, requests } = fakeHttp({
            'POST http://hub/cli/sessions/abc/hibernate': { status: 409, data: { error: 'The agent is mid-turn', code: 'mid_turn' } },
        })
        const outcomes = await runHibernate({ kind: 'one', sessionIdPrefix: 'abc' }, { apiUrl: 'http://hub', accessToken: 'tok:ns', http })
        expect(outcomes).toEqual([{ sessionId: 'abc', ok: false, code: 'mid_turn', message: 'The agent is mid-turn' }])
        expect(requests[0]!.auth).toBe('Bearer tok:ns')
        expect(formatHibernateOutcome(outcomes[0]!)).toBe('abc refused: The agent is mid-turn (mid_turn)')
    })

    it('hibernates every eligible candidate on this machine and prints refusals for the rest', async () => {
        const { http, requests } = fakeHttp({
            'GET http://hub/cli/hibernate/candidates': { status: 200, data: { sessions: [
                { sessionId: 's1', refusal: null },
                { sessionId: 's2', refusal: 'mid_turn', message: 'The agent is mid-turn' },
            ] } },
            'POST http://hub/cli/sessions/s1/hibernate': { status: 200, data: { ok: true, sessionId: 's1' } },
        })
        const outcomes = await runHibernate({ kind: 'all-idle', minIdleMinutes: 15 },
            { apiUrl: 'http://hub', accessToken: 't', machineId: 'm1', http })
        expect(outcomes.map(formatHibernateOutcome)).toEqual(['s1 hibernated', 's2 refused: The agent is mid-turn (mid_turn)'])
        expect(requests[0]!.params).toEqual({ machineId: 'm1', minIdleMinutes: '15' })
        expect(requests).toHaveLength(2)
    })

    it('converts every signal-archived session since a time', async () => {
        const { http, requests } = fakeHttp({
            'GET http://hub/cli/hibernate/signal-archived': { status: 200, data: { sessions: [{ sessionId: 's9' }] } },
            'POST http://hub/cli/sessions/s9/mark-hibernated': { status: 200, data: { ok: true, sessionId: 's9' } },
        })
        const outcomes = await runHibernate({ kind: 'all-signalled', since: '2026-11-20T00:00:00.000Z' },
            { apiUrl: 'http://hub', accessToken: 't', machineId: 'm1', http })
        expect(outcomes).toEqual([{ sessionId: 's9', ok: true }])
        expect(requests[0]!.params).toEqual({ machineId: 'm1', since: '2026-11-20T00:00:00.000Z' })
    })

    it('needs a machine id for the bulk modes', async () => {
        const { http } = fakeHttp({})
        await expect(runHibernate({ kind: 'all-idle' }, { apiUrl: 'http://hub', accessToken: 't', http })).rejects.toThrow(/machine id/)
    })
})
