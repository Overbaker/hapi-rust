import {describe,expect,it} from 'vitest'
import {readFileSync} from 'node:fs'
import {join} from 'node:path'
import {piCorpus,piNames,piRun} from './piRun'
import {repo,testTimeout} from './run'
import {parityRuntime} from './runtime'
import {expectedPiRust} from './rustKnownGaps'
const runtime = parityRuntime(process.env,repo)
describe.each(piNames)('pi/%s', name => {
    const golden = JSON.parse(readFileSync(join(piCorpus,name,'golden.ts.json'),'utf8'))
    if (runtime.ts) it('matches the recorded TypeScript transcript',async () => {
        expect(await piRun(name)).toEqual(golden)
    },testTimeout)
    if (runtime.rust) it('matches the recorded pi contract',async () => {
        expect(await piRun(name,runtime.rust)).toEqual(process.env.HAPI_PARITY_RAW ? golden : expectedPiRust(golden))
    },testTimeout)
})
