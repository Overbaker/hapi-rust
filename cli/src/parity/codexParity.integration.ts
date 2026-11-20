import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import { run, codexCorpus, codexNames, repo, testTimeout } from './codexRun'
import { parityRuntime } from './runtime'

const runtime = parityRuntime(process.env, repo)

describe.each(codexNames)('codex/%s', name => {
    const expected: Awaited<ReturnType<typeof run>> = JSON.parse(readFileSync(join(codexCorpus,name,'golden.ts.json'),'utf8'))
    if(runtime.ts)it('TS wire transcript and web decoding match the golden', async()=>{
        expect(await run(name)).toEqual(expected)
    },testTimeout)
    if(runtime.binary)it('Rust wire transcript and web decoding match TS',async()=>{
        const {methods,...actual}=await run(name,runtime.binary!)
        const {methods:registered,...golden}=expected
        expect(actual).toEqual(golden)
        expect(Object.keys(methods)).toEqual(Object.keys(registered))
        for(const [connection,ts] of Object.entries(registered)){
            const required=ts.filter(m=>!/:list(?:Agy|Claude|Copilot|Cursor|Grok|Kimi|Opencode|Pi).*Model/.test(m))
            expect(ts).toEqual(expect.arrayContaining(methods[connection]))
            expect(methods[connection]).toEqual(expect.arrayContaining(required))
        }
    },testTimeout)
})
