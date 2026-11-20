import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import { run, codexCorpus, codexNames, repo, testTimeout } from './codexRun'
import { codexRustKnownGaps } from './rustKnownGaps'
import { parityRuntime } from './runtime'

const runtime = parityRuntime(process.env, repo)
type Transcript = Awaited<ReturnType<typeof run>>

function expectedRust(name: string, golden: Transcript): Transcript {
    const expected = structuredClone(golden)
    const allowed = new Set(codexRustKnownGaps(name).map(gap => gap.path))
    for (const [connection, methods] of Object.entries(expected.methods)) {
        expected.methods[connection] = methods.filter(method => {
            const match = /^<uuid:\d+>:(\w[\w-]*)$/.exec(method)
            return !match || !allowed.has('methods[*].' + match[1])
        })
    }
    function metadata(value: Record<string, any>) {
        if (allowed.has('session-metadata.wrapperRuntime')) {
            expect(value.wrapperRuntime).toBeUndefined()
            value.wrapperRuntime = 'rust'
        }
        const history = value.capabilities?.conversationHistory
        if (history && allowed.has('session-metadata.capabilities.conversationHistory.forkPreview')) {
            expect(history.forkPreview).toBe(true)
            delete history.forkPreview
        }
    }
    for (const rows of Object.values(expected.channels)) {
        for (const row of rows as {event: string; args: any[]; ack?: any[]}[]) {
            if (row.event === 'POST /cli/sessions') metadata(row.args[0].body.metadata)
            if (row.event === 'POST /session-started') metadata(row.args[0].metadata)
            if (row.event === 'update-metadata') {
                metadata(row.args[0].metadata)
                if (row.ack?.[0]?.metadata) metadata(row.ack[0].metadata)
            }
        }
    }
    return expected
}

describe.each(codexNames)('codex/%s', name => {
    const golden: Transcript = JSON.parse(readFileSync(join(codexCorpus, name, 'golden.ts.json'), 'utf8'))
    if (runtime.ts) it('TS wire transcript and web decoding match the golden', async () => {
        expect(await run(name)).toEqual(golden)
    }, testTimeout)
    if (runtime.rust) it('Rust wire transcript and web decoding match the named contract', async () => {
        const actual = await run(name, runtime.rust!)
        expect(actual).toEqual(process.env.HAPI_PARITY_RAW === '1' ? golden : expectedRust(name, golden))
    }, testTimeout)
})
