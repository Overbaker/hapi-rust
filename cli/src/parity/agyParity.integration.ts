import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import { agyCorpus, agyNames, repo, runAgy, testTimeout } from './agyRun'
import { expectedAgyRustTranscript } from './rustKnownGaps'
import { parityRuntime } from './runtime'

const runtime = parityRuntime(process.env, repo)

describe.each(agyNames)('agy/%s', name => {
    const golden = JSON.parse(readFileSync(join(agyCorpus, name, 'golden.ts.json'), 'utf8'))
    if (runtime.ts) it('TS wire transcript and web decoding match the golden', async () => {
        const actual = await runAgy(name)
        if (process.env.PARITY_DUMP_DIR) writeFileSync(join(process.env.PARITY_DUMP_DIR, name + '.ts.json'), JSON.stringify(actual, null, 2) + '\n')
        expect(actual).toEqual(golden)
    }, testTimeout)
    if (runtime.rust) it('Rust wire transcript and web decoding match the named contract', async () => {
        const actual = await runAgy(name, runtime.rust!, { rust: true })
        if (process.env.PARITY_DUMP_DIR) writeFileSync(join(process.env.PARITY_DUMP_DIR, name + '.rust.json'), JSON.stringify(actual, null, 2) + '\n')
        expect(actual).toEqual(process.env.HAPI_PARITY_RAW === '1' ? golden : expectedAgyRustTranscript(name, golden))
    }, testTimeout)
})
