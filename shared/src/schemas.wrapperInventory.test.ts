import { describe, expect, test } from 'bun:test'
import { MetadataSchema, RunnerStateSchema } from './schemas'

const base = { status: 'running', capabilities: { sessionHibernation: true } }

describe('RunnerStateSchema.wrapperInventory', () => {
    test('slices an oversized reported list to 200 entries instead of rejecting runnerState', () => {
        const reported = Array.from({ length: 250 }, (_, i) => ({ pid: i + 1, reason: 'no_session_id' }))
        const parsed = RunnerStateSchema.parse({
            ...base,
            wrapperInventory: { scannedAt: 1, enumerated: 250, tracked: 0, adopted: 0, reportedTotal: 250, reported },
        })
        expect(parsed.wrapperInventory?.reported).toHaveLength(200)
        expect(parsed.wrapperInventory?.reported[0]).toEqual({ pid: 1, reason: 'no_session_id' })
        expect(parsed.wrapperInventory?.reportedTotal).toBe(250)
        expect(parsed.status).toBe('running')
    })

    test('keeps drift adoptOutcome and new reasons', () => {
        const parsed = RunnerStateSchema.parse({
            ...base,
            wrapperInventory: {
                scannedAt: 1, tracked: 0, adopted: 0,
                reported: [
                    { pid: 1, sessionId: 's', drift: 'deleted', reason: 'drift', adoptOutcome: 'foreign_runner' },
                    { pid: 2, sessionId: 't', reason: 'foreign_runner' },
                ],
            },
        })
        expect(parsed.wrapperInventory?.reported).toEqual([
            { pid: 1, sessionId: 's', drift: 'deleted', reason: 'drift', adoptOutcome: 'foreign_runner' },
            { pid: 2, sessionId: 't', reason: 'foreign_runner' },
        ])
    })

    test.each([
        ['wrong field types', { scannedAt: 'later', tracked: 0, adopted: 0, reported: [] }],
        ['missing reported', { scannedAt: 1, tracked: 0, adopted: 0 }],
        ['bad drift value', { scannedAt: 1, tracked: 0, adopted: 0, reported: [{ pid: 1, drift: 'gone', reason: 'drift' }] }],
        ['not an object', 'inventory'],
    ])('coerces a malformed wrapperInventory (%s) to undefined and keeps the rest', (_label, wrapperInventory) => {
        const result = RunnerStateSchema.safeParse({ ...base, wrapperInventory })
        expect(result.success).toBe(true)
        expect(result.data?.wrapperInventory).toBeUndefined()
        expect(result.data?.status).toBe('running')
        expect(result.data?.capabilities).toEqual({ sessionHibernation: true })
    })
})

test('session metadata accepts either wrapper runtime and older sessions without a runtime', () => {
    const metadata = { host: 'host', path: '/project' }
    for (const wrapperRuntime of ['rust', 'ts', undefined]) {
        expect(MetadataSchema.parse({ ...metadata, wrapperRuntime }).wrapperRuntime).toBe(wrapperRuntime)
    }
    expect(MetadataSchema.safeParse({ ...metadata, wrapperRuntime: 'bun' }).success).toBe(false)
})

test('last spawn error preserves validated Rust fallback details only when present', () => {
    const lastSpawnError = { message: 'spawn failed', at: 1 }
    const parse = (error: object) => RunnerStateSchema.parse({ ...base, lastSpawnError: error }).lastSpawnError
    expect(parse(lastSpawnError)).toEqual(lastSpawnError)
    const both = { ...lastSpawnError, wrapperFallback: { reason: 'exit-before-webhook', exitCode: 86 } }
    expect(parse(both)).toEqual(both)
    expect(() => parse({ ...both, wrapperFallback: { reason: 'timeout', exitCode: null } })).toThrow()
})
