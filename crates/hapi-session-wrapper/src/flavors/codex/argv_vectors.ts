import { mock } from 'bun:test'
import { resolve } from 'node:path'

let options: Record<string, unknown>
const stub = (file: string, exports: object) => mock.module(resolve('src', file + '.ts'), () => exports)
stub('codex/runCodex', { runCodex: async (value: Record<string, unknown>) => { options = value } })
stub('ui/tokenInit', { initializeToken: async () => {} })
stub('ui/auth', { authAndSetupMachineIfNeeded: async () => {} })
stub('utils/autoStartServer', { maybeAutoStartServer: async () => {} })
stub('codex/shared/launch', { resolveSharedCodex: () => ({}) })
const { codexCommand } = await import('../../../../../cli/src/commands/codex')
const exit = Symbol()
process.exit = () => { throw exit }
let error: string
console.error = (_prefix, message) => { error = message }
const results = []
for (const commandArgs of JSON.parse(await Bun.stdin.text())) {
    try {
        await codexCommand.run({ commandArgs } as Parameters<typeof codexCommand.run>[0])
        results.push({ flavor: 'codex', ...options })
    } catch (e) {
        if (e !== exit) throw e
        results.push(error)
    }
}
console.log(JSON.stringify(results))
