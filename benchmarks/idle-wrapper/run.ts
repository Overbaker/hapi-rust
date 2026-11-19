import { spawn, execFileSync, type ChildProcess } from 'node:child_process'
import { createServer } from 'node:http'
import { createRequire } from 'node:module'
import { readdirSync, statSync, closeSync, copyFileSync, unlinkSync, existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, readlinkSync, rmSync, symlinkSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import { randomUUID } from 'node:crypto'
import { header, interval, row, snapshot } from './metrics'

if (process.argv.length <= 3) {
    const output = resolve(process.argv[2] ?? 'idle-wrapper-results')
    execFileSync('bash', [join(import.meta.dir, 'suite.sh'), output], {
        cwd: resolve(import.meta.dir, '../..'), stdio: 'inherit'
    })
    process.exit(0)
}
const [version, group, outputArg] = process.argv.slice(2)
const groupNames = ['bare','network','claude','codex','agy','agy-poll','agy-off','scan','scan-off','retry','retry-off','historic','orphan','auth-rejected','kimi-local']
if (!groupNames.includes(group) || !['1.3.11','1.4.4'].includes(version) || !outputArg) {
    throw Error('Usage: bun run.ts <1.3.11|1.4.4> <' + groupNames.join('|') + '> <output directory>')
}
const root = resolve(import.meta.dir, '../..')
const out = resolve(outputArg, version + '-' + group)
mkdirSync(out, { recursive: true, mode: 0o700 })
const scratch = mkdtempSync(join(tmpdir(), 'hapi-idle-'))
const localPids: number[] = []
const children = new Set<ChildProcess>()
function cleanup() {
    for (const pid of localPids) { try { process.kill(-pid, 'SIGKILL') } catch {} }
    for (const child of children) {
        if (!child.pid) continue
        try { process.kill(-child.pid, 'SIGKILL') } catch (e) {
            if ((e as NodeJS.ErrnoException).code !== 'ESRCH') throw e
        }
    }
    children.clear()
    rmSync(scratch, { recursive: true, force: true })
}
process.on('exit', cleanup)
process.on('SIGTERM', () => process.exit(143))
process.on('SIGINT', () => process.exit(130))
function start(exe: string, args: string[], env: NodeJS.ProcessEnv, cwd: string, label: string) {
    const fd = openSync(join(out, label + '.log'), 'w', 0o600)
    try {
        const p = spawn(exe, args, { detached: true, stdio: ['ignore',fd,fd], cwd, env })
        children.add(p)
        return p
    } finally { closeSync(fd) }
}
async function until(label: string, test: () => Promise<boolean> | boolean) {
    const deadline = performance.now() + 90_000
    do {
        for (const p of children) if (p.exitCode !== null || p.signalCode) throw Error(`PID ${p.pid} exited waiting for ${label}; see ${out}`)
        if (await test()) return
        await Bun.sleep(250)
    } while (performance.now() < deadline)
    throw Error('Timed out waiting for ' + label + '; see ' + out)
}

const benchBuild = process.env.IDLE_BUILD!
const bun = process.env[version === '1.3.11' ? 'BUN_A' : 'BUN_B']!
const exe = (kind: string, v = version) => join(benchBuild, v, kind, 'bun-linux-x64-baseline/hapi')
const historic = group === 'historic'
const stock = historic ? process.env.IDLE_HISTORIC! : exe('stock')
const diag = exe('diagnostic')
const embedded = execFileSync(stock, ['--version'], { env: { ...process.env, BUN_BE_BUN: '1' }, encoding: 'utf8' }).trim()
if (embedded !== version) throw Error('Unexpected embedded version ' + embedded)
const disconnected = ['orphan','auth-rejected'].includes(group)
const local = group === 'kimi-local'
const flavor = historic || disconnected ? 'claude' : group.split('-')[0]
const wrapper = ['claude','codex','agy','kimi'].includes(flavor)
const candidate = flavor === 'agy' && group !== 'agy' ? 'reactive' : ['scan','retry'].includes(flavor) ? flavor : ''
const bins = join(scratch, 'bin')
mkdirSync(bins)
for (const name of ['agy','claude']) symlinkSync(join(import.meta.dir, 'agent.mjs'), join(bins,name))
const env: NodeJS.ProcessEnv = { PATH: bins + ':' + process.env.PATH, HOME: scratch, HAPI_HOME: scratch,
    HAPI_DISABLE_VERSION_HANDOFF: '1', HAPI_SESSION_HIBERNATE_MS: '0', HAPI_CLAUDE_PATH: join(bins,'claude'),
    CLI_API_TOKEN: randomUUID(), TELEGRAM_NOTIFICATION: 'false', SERVERCHAN_NOTIFICATION: 'false',
    HAPI_LISTEN_HOST: '127.0.0.1', IDLE_FLAVOR: flavor }
const auth: Record<string,string> = { 'Content-Type': 'application/json' }
async function api(path: string, body?: unknown) {
    const r = await fetch(env.HAPI_API_URL + path, { method: body ? 'POST' : 'GET', headers: auth,
        signal: AbortSignal.timeout(5000), body: body ? JSON.stringify(body) : undefined })
    if (!r.ok) throw Error(`${path}: ${r.status} ${await r.text()}`)
    return r.json()
}
if (wrapper) {
    const socket = Bun.listen({ hostname: '127.0.0.1', port: 0, socket: { data() {} } })
    env.HAPI_LISTEN_PORT = String(socket.port)
    env.HAPI_API_URL = env.HAPI_PUBLIC_URL = 'http://127.0.0.1:' + socket.port
    socket.stop()
    const service = historic ? stock : exe('stock', '1.4.4')
    if (historic && process.env.IDLE_HISTORIC_HUB) {
        const hub = resolve(process.env.IDLE_HISTORIC_HUB)
        start(bun, ['--preload', createRequire(join(root, 'hub/package.json')).resolve('reflect-metadata'),
            join(hub, 'src/index.ts')], env, hub, 'hub')
    } else {
        start(service, ['hub'], env, scratch, 'hub')
    }
    await until('hub health', async () => !!(await fetch(env.HAPI_API_URL + '/health').catch(() => null))?.ok)
    auth.Authorization = 'Bearer ' + (await api('/api/auth', { accessToken: env.CLI_API_TOKEN })).token
    start(service, ['runner','start-sync'], env, scratch, 'runner')
    await until('runner', () => existsSync(join(scratch,'runner.state.json')))
}
if (group === 'network') {
    const { Server } = createRequire(join(root, 'hub/package.json'))('socket.io')
    const http = createServer()
    new Server(http, { transports: ['websocket'] })
    await new Promise<void>(resolve => http.listen(0,'127.0.0.1',resolve))
    const addr = http.address()
    if (!addr || typeof addr === 'string') throw Error('No Socket.IO port')
    env.IDLE_SOCKET_URL = 'http://127.0.0.1:' + addr.port
}

const services = [...children]
const rejected = new Set<string>()
if (local) {
    const kimi = process.env.IDLE_KIMI ?? Bun.which('kimi')
    if (!kimi) throw Error('kimi-local requires a Kimi executable on PATH or in IDLE_KIMI')
    symlinkSync(resolve(kimi), join(bins, 'kimi'))
}
const prompt = 'idle-benchmark-prompt'
const answer = 'idle-benchmark-answer'
const sessions: { pid: number, cwd: string, id?: string, agentPid?: number, answered: string, markers: string }[] = []
const historicWrapper = join(scratch, "old/hapi")
if (historic) { mkdirSync(join(scratch, "old")); copyFileSync(stock, historicWrapper) }
const responses: string[] = []
if (flavor === 'codex') {
    symlinkSync(process.env.IDLE_CODEX!, join(bins, 'codex'))
}
for (let i = 0; i < 5; i++) {
    const cwd = join(scratch, 'work' + i)
    mkdirSync(cwd)
    const answered = join(cwd, 'answered')
    const markers = join(cwd, 'markers')
    const childEnv = { ...env, IDLE_ANSWERED: answered, IDLE_MARKERS: markers,
        IDLE_FORCE_REACTIVE: candidate === 'reactive' ? '1' : '0', IDLE_DISABLE: group.endsWith('-off') ? candidate : '', CODEX_HOME: join(cwd, '.codex') }
    if (local) {
        const home = join(cwd, '.kimi-code')
        mkdirSync(home)
        Object.assign(childEnv, { KIMI_CODE_HOME: home })
        const http = Bun.serve({ hostname: '127.0.0.1', port: 0, async fetch(request) {
            if (!new URL(request.url).pathname.endsWith('/chat/completions')) return new Response(null, { status: 404 })
            const requestBody = await request.text()
            if (requestBody.includes(prompt)) responses.push(cwd)
            const fields = { id: 'fixture', object: 'chat.completion.chunk', model: 'fixture', created: 1 }
            const frames = [{ delta: { role: 'assistant', content: answer }, finish_reason: null },
                { delta: {}, finish_reason: 'stop' }].map(choice => 'data: ' + JSON.stringify({ ...fields, choices: [{ index: 0, ...choice }] }))
            return new Response([...frames, 'data: [DONE]', ''].join('\n\n'), { headers: { 'Content-Type': 'text/event-stream' } })
        } })
        writeFileSync(join(home,'config.toml'), `default_model = "fixture"
[providers.fixture]
type = "openai"
base_url = "${http.url}v1"
api_key = "local-fixture"
[models.fixture]
provider = "fixture"
model = "fixture"
max_context_size = 32000
`)
    }
    if (flavor === 'codex') {
        const http = Bun.serve({ hostname: '127.0.0.1', port: 0, async fetch(request) {
            if (!new URL(request.url).pathname.endsWith('/responses')) return new Response(null,{status:404})
            if ((await request.text()).includes(prompt)) responses.push(cwd)
            const id = randomUUID()
            const events = [{ type: 'response.created', response: { id } },
                { type: 'response.output_item.done', item: { type: 'message', id: randomUUID(), role: 'assistant', content: [{ type: 'output_text', text: answer }] } },
                { type: 'response.completed', response: { id, usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 } } }]
            return new Response(events.map(e => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join(''), { headers: { 'Content-Type':'text/event-stream' } })
        } })
        mkdirSync(childEnv.CODEX_HOME)
        writeFileSync(join(childEnv.CODEX_HOME,'config.toml'), `model = "mock-model"
model_provider = "fixture"
approval_policy = "never"
sandbox_mode = "read-only"
check_for_update_on_startup = false
[model_providers.fixture]
name = "Loopback"
base_url = "${http.url}v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
[analytics]
enabled = false
[feedback]
enabled = false
[projects."${cwd}"]
trust_level = "trusted"
`)
    }
    const binary = historic ? historicWrapper : group === 'bare' || (wrapper && !candidate) ? stock : diag
    if (group === 'bare') Object.assign(childEnv, { BUN_BE_BUN: '1' })
    const args = local ? ['kimi','--started-by','terminal','--hapi-starting-mode','local'] : group === 'bare' ? ['-e','setInterval(()=>{},10000)'] : wrapper ? [flavor,'--started-by','runner']
        : group === 'network' ? ['idle-network'] : ['idle-scanner', flavor === 'scan' ? 'healthy' : 'missing']
    let pid: number
    if (local) {
        const terminal = start('python3', [join(import.meta.dir, 'local-pty.py'), binary, ...args], { ...childEnv, TERM:'xterm-256color' }, cwd, 'pid'+i)
        await until('PTY wrapper', () => existsSync(join(cwd,'wrapper.pid')))
        pid = Number(readFileSync(join(cwd,'wrapper.pid'),'utf8'))
        if (!Number.isSafeInteger(pid) || pid <= 1) throw Error('Invalid PTY wrapper PID')
        const info = readFileSync(`/proc/${pid}/stat`, 'utf8')
        if (Number(info.slice(info.lastIndexOf(')') + 2).split(' ')[1]) !== terminal.pid) {
            throw Error('Unexpected PTY wrapper parent')
        }
        localPids.push(pid)
    } else {
        pid = start(binary,args,childEnv,cwd,'pid'+i).pid!
    }
    sessions.push({ pid, cwd, answered, markers })
}
type Active = { id: string, metadata?: { path: string }, thinking: boolean }
async function active(): Promise<Active[]> { return (await api('/api/sessions?active=true')).sessions }
if (wrapper) {
    try {
        await until('five active sessions', async () => {
            const list = await active()
            for (const s of sessions) s.id = list.find(r => r.metadata?.path === s.cwd)?.id
            return sessions.every(s => s.id)
        })
    } catch (error) {
        if (flavor !== 'codex' || version !== '1.3.11') throw error
        const reason = 'Wrong url scheme for WebSocket ws+unix:'
        const logs = sessions.map((_, i) => join(out, 'pid' + i + '.log'))
        for (let i = 0; i < 40 && !logs.every(p => readFileSync(p,'utf8').includes(reason)); i++) await Bun.sleep(250)
        if (!logs.every(p => readFileSync(p,'utf8').includes(reason))) throw error
        writeFileSync(join(out,'unsupported.json'), JSON.stringify({ group, version, reason, launches: logs.length }))
        writeFileSync(join(out,'table.md'), `| ${group} | ${version} | N/A: ws+unix rejected on all five launches | — | — | — | — | — | — |\n`)
        console.log(reason + ' (5/5), no idle samples')
        cleanup()
        process.exit(0)
    }
    if (local) {
        await until('local Kimi wire watches', () => sessions.every(s => {
            const root = join(s.cwd,'.kimi-code/sessions')
            if (!existsSync(root)) return false
            const dirs = readdirSync(root).filter(d => d.startsWith('wd_'+s.cwd.split('/').at(-1)+'_'))
            for (const d of dirs) for (const id of readdirSync(join(root,d))) {
                const wire = join(root,d,id,'agents/main/wire.jsonl')
                if (!existsSync(wire) || !responses.includes(s.cwd) || !readFileSync(wire,'utf8').includes(answer)) continue
                const inode = statSync(wire,{bigint:true}).ino.toString(16)
                const fdinfo = '/proc/'+s.pid+'/fdinfo'
                if (readdirSync(fdinfo).some(fd => {
                    try { return readFileSync(join(fdinfo,fd),'utf8').includes(' ino:'+inode+' ') } catch { return false }
                })) return true
            }
            return false
        }))
    } else {
        await Promise.all(sessions.map(s => api('/api/sessions/' + s.id + '/messages', { text: prompt, localId: randomUUID() })))
        await until('five completed post-prompt turns', async () => {
            const list = await active()
            for (const s of sessions) {
                if (!list.some(r => r.id === s.id && !r.thinking)) return false
                if (flavor === 'codex' ? !responses.includes(s.cwd) : !existsSync(s.answered)) return false
                if (!JSON.stringify(await api('/api/sessions/'+s.id+'/messages')).includes(answer)) return false
            }
            return true
        })
        for (const s of sessions) {
            if (flavor !== 'codex') s.agentPid = Number(readFileSync(s.answered, 'utf8'))
            else {
                const kids = readFileSync(`/proc/${s.pid}/task/${s.pid}/children`, 'utf8').trim().split(/\s+/).map(Number)
                s.agentPid = kids.find(pid => {
                    try { return readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0').includes('app-server') } catch { return false }
                })
            }
            if (!s.agentPid) throw Error('Missing agent child for ' + s.pid)
        }
    }
}
function evidence(s: typeof sessions[number]) {
    return existsSync(s.markers) ? readFileSync(s.markers,'utf8').trim().split('\n') : []
}
if (candidate || group === 'network') {
    await until('construction evidence', () => sessions.every(s => {
        const events = evidence(s)
        if (!candidate) return events.includes('ready')
        return events.includes(candidate+':reached') && events.includes(candidate+(group.endsWith('-off') ? ':disabled' : ':armed'))
    }))
}
if (disconnected) {
    await Promise.all(services.map(child => new Promise<void>((resolve) => {
        child.once('exit', () => { children.delete(child); resolve() })
        process.kill(-child.pid!, 'SIGKILL')
    })))
    if (group === 'auth-rejected') {
        const { Server } = createRequire(join(root, 'hub/package.json'))('socket.io')
        const http = createServer()
        const io = new Server(http, { transports: ['websocket'] })
        io.of('/cli').use((socket: {handshake:{auth:{sessionId:string}}}, next: (error: Error) => void) => {
            rejected.add(socket.handshake.auth.sessionId)
            next(new Error('Unauthorized'))
        })
        await new Promise<void>(resolve => http.listen(Number(env.HAPI_LISTEN_PORT),'127.0.0.1',resolve))
        await until('five rejected session credentials', () => sessions.every(s => rejected.has(s.id!)))
    }
}
async function verify() {
    const list = wrapper && !disconnected ? await active() : []
    for (const s of sessions) {
        if (wrapper && !disconnected && !list.some(r => r.id === s.id && !r.thinking)) throw Error('Session disappeared or thinking')
        if (s.agentPid) {
            const stat = readFileSync(`/proc/${s.agentPid}/stat`,'utf8')
            const [state, parent] = stat.slice(stat.lastIndexOf(')') + 2).split(' ')
            if (state === 'Z' || Number(parent) !== s.pid) throw Error('Agent is not a live wrapper child')
        }
        if (group.endsWith('-off') && evidence(s).includes(candidate+':armed')) throw Error('Disabled timer was armed')
    }
}
if (historic) unlinkSync(historicWrapper)
await verify()
console.log(`${version} ${group}: five ready`)
await Bun.sleep(60_000)
await verify()
let previous = sessions.map(s => snapshot(s.pid))
const startAt = performance.now()
const windows = []
for (let minute=1; minute<=10; minute++) {
    await Bun.sleep(Math.max(0, startAt + minute*60_000 - performance.now()))
    const next = sessions.map(s => snapshot(s.pid))
    windows.push(...next.map((value,i) => interval(previous[i], value)))
    previous = next
    await verify()
}
const result = { version, group, embedded, windows, rejectedSessions: rejected.size,
    nproc:Number(execFileSync('nproc',{encoding:'utf8'})), loadavg: readFileSync('/proc/loadavg','utf8').trim(),
    concurrentGroups: process.env.IDLE_GROUPS ?? group, sessions: sessions.map(s => ({
    pid:s.pid, sessionId:s.id, agentPid:s.agentPid, markers:evidence(s), exe:readlinkSync('/proc/'+s.pid+'/exe') })) }
writeFileSync(join(out,'samples.json'), JSON.stringify(result,null,2))
const line = row(group,version,windows)
writeFileSync(join(out,'table.md'), [header,'|---|---|---:|---:|---:|---:|---:|---:|---:|',line].join('\n')+'\n')
console.log(line)
if (wrapper && !candidate) {
    const trace = spawn(process.env.STRACE ?? 'strace', ['-qq','-f','-c','-o',join(out,'strace.txt'),'-p',String(sessions[0].pid)], { stdio:'inherit' })
    let requestedStop = false
    const timer = setTimeout(() => { requestedStop = trace.kill('SIGINT') }, 30_000)
    try {
        const status = await new Promise<{ code: number | null, signal: NodeJS.Signals | null }>((resolve, reject) => {
            trace.once('error', reject)
            trace.once('exit', (code, signal) => resolve({ code, signal }))
        })
        const interrupted = status.code === 130 || status.signal === 'SIGINT'
        const summary = readFileSync(join(out, 'strace.txt'), 'utf8')
        if (!requestedStop || (status.code !== 0 && !interrupted) || !/\btotal\s*$/m.test(summary)) {
            throw Error('Incomplete 30 s trace: ' + JSON.stringify(status))
        }
    } finally {
        clearTimeout(timer)
    }
}
cleanup()
process.exit(0)
