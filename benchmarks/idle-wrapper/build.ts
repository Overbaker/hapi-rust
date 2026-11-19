import { execFileSync } from 'node:child_process'
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, renameSync, symlinkSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = join(import.meta.dir, '../..')
const directory = resolve(process.argv[2])
const target = 'bun-linux-x64-baseline'
const staged = join(directory, 'source')
mkdirSync(join(staged, 'cli'), { recursive: true })
for (const name of ['node_modules', 'web', 'package.json', 'tsconfig.base.json']) {
    symlinkSync(join(root, name), join(staged, name))
}
// The release builder follows relative asset imports from the staged source tree.
for (const dir of ['shared', 'shared/tools', 'shared/tools/tunwg']) {
    mkdirSync(join(staged, dir))
    for (const name of readdirSync(join(root, dir))) {
        if ((dir === 'shared' && name === 'tools') || (dir === 'shared/tools' && name === 'tunwg') ||
            (dir === 'shared/tools/tunwg' && name === 'tunwg-x64-linux')) continue
        symlinkSync(join(root, dir, name), join(staged, dir, name))
    }
}
symlinkSync(process.env.IDLE_TUNWG ?? join(root, 'shared/tools/tunwg/tunwg-x64-linux'),
    join(staged, 'shared/tools/tunwg/tunwg-x64-linux'))
for (const name of ['tools', 'package.json', 'tsconfig.json', 'bunfig.toml']) {
    if (existsSync(join(root, 'cli', name))) symlinkSync(join(root, 'cli', name), join(staged, 'cli', name))
}
cpSync(join(root, 'cli/src'), join(staged, 'cli/src'), { recursive: true })

const cliModules = join(staged, 'cli/node_modules')
mkdirSync(cliModules)
for (const name of readdirSync(join(root, 'cli/node_modules'))) {
    const path = name === 'hapi-hub' ? join(staged, 'hub') : join(root, 'cli/node_modules', name)
    symlinkSync(path, join(cliModules, name))
}
mkdirSync(join(staged, 'hub'))
for (const name of ['node_modules', 'package.json', 'tsconfig.json']) {
    symlinkSync(join(root, 'hub', name), join(staged, 'hub', name))
}
cpSync(join(root, 'hub/src'), join(staged, 'hub/src'), { recursive: true })
const scripts = join(staged, 'cli/scripts')
mkdirSync(scripts)
cpSync(join(root, 'cli/scripts/build-executable.ts'), join(scripts, 'build-executable.ts'))

function compile(kind: string) {
    const script = join(scripts, 'build-executable.ts')
    for (const [key, version] of [['BUN_A', '1.3.11'], ['BUN_B', '1.4.4']]) {
        const bun = process.env[key]!
        const outdir = join(directory, version, kind)
        execFileSync(bun, [script, '--target', target, '--outdir', outdir], { stdio: 'inherit' })
        const exe = join(outdir, target, 'hapi')
        const embedded = execFileSync(exe, ['--version'], {
            encoding: 'utf8', env: { ...process.env, BUN_BE_BUN: '1' }
        }).trim()
        if (embedded !== version) throw Error(`Embedded Bun ${embedded}, expected ${version}`)
    }
}
if (process.env.IDLE_BASE_PATCH) {
    execFileSync('git', ['apply', '--reverse', process.env.IDLE_BASE_PATCH], { cwd: staged })
}
compile('stock')
if (process.env.IDLE_BASE_PATCH) process.exit(0)

const src = join(staged, 'cli/src')
renameSync(join(src, 'bootstrap.ts'), join(src, 'stock-bootstrap.ts'))
cpSync(join(import.meta.dir, 'wrapper.ts'), join(src, 'bootstrap.ts'))
cpSync(join(import.meta.dir, 'probe.ts'), join(src, 'idle-probe.ts'))

function instrument(file: string, site: string, code: string) {
    const path = join(src, file)
    const source = readFileSync(path, 'utf8')
    if (source.split(site).length !== 2) throw Error(`Diagnostic site changed in ${file}`)
    writeFileSync(path, "import { mark, disabled } from '@/idle-probe';\n" + source.replace(site, code))
}
const scanner = `this.intervalId = setInterval(() => {
            if (healthy) for (const watcher of this.watchers.values()) watcher.verifyIdentity();
            this.sync.invalidate();
        }, intervalMs);`
instrument('modules/common/session/BaseSessionScanner.ts', scanner,
    `if (!disabled('scan')) { ${scanner} mark('scan:armed') }`)
const retry = `retry = setTimeout(() => {
            retry = undefined;
            attach();
        }, backoffMs);`
instrument('modules/watcher/startFileWatcher.ts', retry,
    `if (disabled('retry')) return; ${retry} mark('retry:armed');`)
const agyFile = 'agy/agyAcpRemoteLauncher.ts'
const idleGate = 'armPoll(watchRef.current ? 30_000 : this.reactivePollMs);'
instrument(agyFile, idleGate,
    "armPoll(watchRef.current && process.env.IDLE_FORCE_REACTIVE !== '1' ? 30_000 : this.reactivePollMs);")
const agyPath = join(src, agyFile)
const agy = 'timer = setInterval(checkWakeup, intervalMs);'
const launcher = readFileSync(agyPath, 'utf8')
if (launcher.split(agy).length !== 2) throw Error('Reactive timer site changed')
writeFileSync(agyPath, launcher.replace(agy, `if (disabled('reactive')) return; ${agy} mark('reactive:armed');`))
compile('diagnostic')
