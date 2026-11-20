import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { corpus, names, run } from './run'

import { codexCorpus, codexNames, run as runCodex } from './codexRun'
import { agyCorpus, agyNames, runAgy } from './agyRun'

process.once('SIGINT', () => process.exit(130))
process.once('SIGTERM', () => process.exit(143))

const codex = process.argv[2] === '--codex'
const agy = process.argv[2] === '--agy'
const selected = process.argv.slice(codex || agy ? 3 : 2)
for (const name of selected.length ? selected : codex ? codexNames : agy ? agyNames : names) {
    const transcript = await (codex ? runCodex : agy ? runAgy : run)(name)
    writeFileSync(join(codex ? codexCorpus : agy ? agyCorpus : corpus, name, 'golden.ts.json'), JSON.stringify(transcript, null, 2) + '\n')
    console.log(name)
}
