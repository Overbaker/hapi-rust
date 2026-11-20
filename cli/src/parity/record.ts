import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { corpus, names, run } from './run'

import { codexCorpus, codexNames, run as runCodex } from './codexRun'

process.once('SIGINT', () => process.exit(130))
process.once('SIGTERM', () => process.exit(143))

const codex = process.argv[2] === '--codex'
const selected = process.argv.slice(codex ? 3 : 2)
for (const name of selected.length ? selected : codex ? codexNames : names) {
    const transcript = await (codex ? runCodex : run)(name)
    writeFileSync(join(codex ? codexCorpus : corpus, name, 'golden.ts.json'), JSON.stringify(transcript, null, 2) + '\n')
    console.log(name)
}
