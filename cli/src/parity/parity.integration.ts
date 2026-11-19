import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import { corpus, names, run } from './run'
import { pendingClaudeScenarios } from './rustPending'

const binary = process.env.HAPI_PARITY_RUST_BIN
const mode = process.env.PARITY_RUNTIME ?? (binary ? 'both' : 'ts')
if (!['ts', 'rust', 'both'].includes(mode)) throw Error('Invalid PARITY_RUNTIME')
if (mode !== 'ts' && !binary) throw Error('Rust parity requires HAPI_PARITY_RUST_BIN')

describe.each(names)('%s', name => {
    const golden: Awaited<ReturnType<typeof run>> = JSON.parse(readFileSync(join(corpus, name, 'golden.ts.json'), 'utf8'))
    it('TS wire transcript and web decoding match the golden', async () => {
        expect(await run(name)).toEqual(golden)
    }, 35_000)
    if (!binary && pendingClaudeScenarios.includes(name)) {
        it.todo('Rust Claude implementation pending: ' + name)
        return
    }
    it('Rust wire transcript and web decoding match TS', async () => {
        if (!binary) throw Error('Missing HAPI_PARITY_RUST_BIN for ' + name)
        const { methods, ...actual } = await run(name, binary)
        const { methods: registered, ...expected } = golden
        expect(actual).toEqual(expected)
        expect(Object.keys(methods)).toEqual(Object.keys(registered))
        for (const [connection, ts] of Object.entries(registered)) {
            const rust = methods[connection]
            const required = ts.filter(method => !/:list(?:Agy|Codex|Copilot|Cursor|Grok|Kimi|Opencode|Pi).*Model/.test(method))
            expect(ts).toEqual(expect.arrayContaining(rust))
            expect(rust).toEqual(expect.arrayContaining(required))
        }
    }, 35_000)
})
