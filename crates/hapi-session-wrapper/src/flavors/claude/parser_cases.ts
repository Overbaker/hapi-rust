import { readFileSync } from 'node:fs'
const source = (name: string) => readFileSync('../../cli/src/' + name, 'utf8')
const builder = source('runner/run.ts').split('export function buildCliArgs(')[1].split('\n}')[0]
const parser = source('commands/claude.ts').split('const args = [...commandArgs]')[1].split('await initializeToken()')[0]
const hubUuid = new Function('return ' + source('runner/sessionStartup.ts').split('export const hubUuid = ')[1].split('\n')[0].replace(/;$/, ''))()
const modes = ['default', 'plan', 'auto', 'acceptEdits', 'bypassPermissions']
const code = new Bun.Transpiler({ loader: 'ts' }).transformSync(
    'function buildCliArgs(' + builder + '\n}\nfunction parse(args) {' + parser + '\nreturn options}\nreturn {buildCliArgs,parse}')
const Flavors = { claude: { allowsPermissionMode: (mode: string) => modes.includes(mode) } }
const {buildCliArgs, parse} = new Function('CLAUDE_PERMISSION_MODES','PERMISSION_MODES','Flavors','hubUuid','z',code)(
    modes,modes,Flavors,hubUuid,{enum: () => ({parse: (v: string) => v})})
const cases = []
for (let mask = 0; mask < 512; mask++) {
    const opts = {
        startingMode: mask & 1 ? 'local' : undefined,
        model: mask & 2 ? 'model Ω' : undefined,
        effort: mask & 4 ? 'high' : undefined,
        permissionMode: [...modes, undefined][mask % 6],
        resumeSessionId: mask & 8 ? 'agent-id' : undefined,
        forkSession: !!(mask & 16),
        existingSessionId: mask & 32 ? 'existing' : undefined,
        reservedSessionId: mask & 64 ? 'reserved' : undefined,
        sessionId: mask & 128 ? 'local-id' : undefined,
    }
    const argv = buildCliArgs('claude', opts, !!(mask & 256))
    if (mask % 5 === 0) argv.push('--model=two','--unknown','a b','--','--model','literal')
    cases.push([argv, {flavor:'claude', resumeSessionId:opts.resumeSessionId, ...parse(argv.slice(1))}])
}
console.log(JSON.stringify(cases))
