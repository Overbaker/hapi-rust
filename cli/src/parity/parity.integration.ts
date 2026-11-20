import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { describe, expect, it } from 'vitest'
import { corpus, names, repo, run, testTimeout } from './run'
import { parityRuntime } from './runtime'
import { expectedRustTranscript } from './rustComparison'

const candidate = process.env.HAPI_PARITY_WRAPPER_BIN

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
            const actual = await run(name, runtime.rust, { rust: true })
            expect(actual).toEqual(expectedRustTranscript(name, golden))
        }, testTimeout)
    }
})
