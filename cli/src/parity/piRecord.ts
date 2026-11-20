import { piRun, piCorpus, piNames } from './piRun'
import {writeFileSync} from 'node:fs'
import {join} from 'node:path'
for (const name of process.argv.slice(2).length ? process.argv.slice(2) : piNames) {
    writeFileSync(join(piCorpus,name,'golden.ts.json'), JSON.stringify(await piRun(name),null,2)+'\n')
}
