import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { describe, expect, it } from 'vitest'
import { corpus, names, repo, run, testTimeout } from './run'
import { parityRuntime } from './runtime'

const candidate = process.env.HAPI_PARITY_WRAPPER_BIN

// The Rust wrapper stamps its runtime into session metadata; the TS golden runs without a runner stamp.
function withoutRustStamp(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(withoutRustStamp)
    if (!value || typeof value !== 'object') return value
    return Object.fromEntries(Object.entries(value)
        .filter(([key, v]) => !(key === 'wrapperRuntime' && v === 'rust'))
        .map(([key, v]) => [key, withoutRustStamp(v)]))
}
const runtime = parityRuntime(process.env, repo)

describe.each(names)('%s', name => {
    const golden = JSON.parse(readFileSync(join(corpus, name, 'golden.ts.json'), 'utf8'))
    if (runtime.ts) {
        it('matches the wrapper wire transcript and web decoding', async () => {
            expect(await run(name)).toEqual(golden)
        }, testTimeout)
    }
    if (candidate) {
        it('matches the same golden with the supplied wrapper', async () => {
            expect(await run(name, resolve(repo, candidate))).toEqual(golden)
        }, testTimeout)
    }
    if (runtime.rust) {
        it('Rust wire transcript and web decoding match TS', async () => {
            const { methods, ...actual } = withoutRustStamp(await run(name, runtime.rust, { rust: true })) as typeof golden
            const { methods: registered, ...expected } = golden as { methods: Record<string, string[]> }
            expect(actual).toEqual(expected)
            expect(Object.keys(methods)).toEqual(Object.keys(registered))
            for (const [connection, ts] of Object.entries(registered)) {
                const rust = methods[connection]
                // TS registers model listers for every agent; the Rust Claude wrapper registers only its own.
                const required = ts.filter(method => !/:list(?:Agy|Codex|Copilot|Cursor|Grok|Kimi|Opencode|Pi).*Model/.test(method))
                expect(ts).toEqual(expect.arrayContaining(rust))
                expect(rust).toEqual(expect.arrayContaining(required))
            }
        }, testTimeout)
    }
})
