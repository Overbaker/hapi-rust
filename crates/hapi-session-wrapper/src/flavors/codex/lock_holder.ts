import { join } from 'node:path'
import { createInterface } from 'node:readline'
import { withSettingsFileLock } from '../../../../../shared/src/settingsFileLock'
const lines = createInterface({ input: process.stdin })[Symbol.asyncIterator]()
console.log('waiting')
await lines.next()
await withSettingsFileLock(join(process.argv[2], 'ownership'), async () => {
    console.log('held')
    await lines.next()
})
process.exit(0)
