import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { corpus, names, codexCorpus, codexNames, run } from './run'

const codex = process.argv[2] === '--codex'
const selected = process.argv.slice(codex ? 3 : 2)
for (const name of selected.length ? selected : codex ? codexNames : names) {
    const transcript = await run(name, undefined, codex ? 'codex' : 'claude')
    writeFileSync(
        join(codex ? codexCorpus : corpus, name, 'golden.ts.json'),
        JSON.stringify(transcript, null, 2) + '\n'
    )
    console.log(name)
}
