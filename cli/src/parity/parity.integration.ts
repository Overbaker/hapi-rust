import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import { corpus, names, repo, run, testTimeout } from './run'
import { pendingClaudeScenarios } from './rustPending'
import { parityRuntime } from './runtime'

const runtime = parityRuntime(process.env, repo)

describe.each(names)('%s', name => {
    const golden: Awaited<ReturnType<typeof run>> = JSON.parse(readFileSync(join(corpus, name, 'golden.ts.json'), 'utf8'))
    if (runtime.ts) it('TS wire transcript and web decoding match the golden', async () => {
        expect(await run(name)).toEqual(golden)
    }, testTimeout)
    const leg = runtime.rustLeg(pendingClaudeScenarios.includes(name))
    if (leg === 'off') return
    if (leg === 'pending') {
        it.todo('Rust Claude implementation pending: ' + name)
        return
    }
    it('Rust wire transcript and web decoding match TS', async () => {
        const { methods, ...actual } = await run(name, runtime.binary!)
        const { methods: registered, ...expected } = golden
        expect(actual).toEqual(expected)
        expect(Object.keys(methods)).toEqual(Object.keys(registered))
        for (const [connection, ts] of Object.entries(registered)) {
            const rust = methods[connection]
            const required = ts.filter(method => !/:list(?:Agy|Codex|Copilot|Cursor|Grok|Kimi|Opencode|Pi).*Model/.test(method))
            expect(ts).toEqual(expect.arrayContaining(rust))
            expect(rust).toEqual(expect.arrayContaining(required))
        }
    }, testTimeout)
})
