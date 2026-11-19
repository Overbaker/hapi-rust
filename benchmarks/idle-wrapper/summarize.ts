import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { join } from 'node:path'
import { header, row, type interval } from './metrics'

type Sample = ReturnType<typeof interval>
type Trial = { group: string, version: string, embedded: string, windows: Sample[],
    sessions: { pid: number, agentPid?: number, markers: string[], exe: string }[] }
const base = process.argv[2]
const runs: Trial[] = []
const unavailable: string[] = []
for (const dir of readdirSync(base)) {
    const path = join(base, dir, 'samples.json')
    if (!existsSync(path)) {
        if (existsSync(join(base, dir, 'unsupported.json'))) unavailable.push(readFileSync(join(base, dir, 'table.md'),'utf8').trim())
        continue
    }
    const r: Trial = JSON.parse(readFileSync(path,'utf8'))
    const { sessions, windows } = r
    if (sessions.length !== 5 || r.embedded !== r.version || windows.length !== 50 ||
        sessions.some(s => windows.filter(w => w.pid === s.pid).length !== 10) ||
        windows.some(w => Math.abs(w.seconds - 60) > 2 || w.byThread.some(t => t.slices < 0))) {
        throw Error('Invalid sample coverage: ' + dir)
    }
    if (r.group === 'historic' && !sessions.every(s => s.exe.endsWith(' (deleted)'))) throw Error('Missing deleted-exe evidence')
    runs.push(r)
}
console.log(header + '\n|---|---|---:|---:|---:|---:|---:|---:|---:|')
for (const r of runs.sort((a,b) => a.group.localeCompare(b.group) || a.version.localeCompare(b.version))) {
    console.log(row(r.group, r.version, r.windows))
}
for (const line of unavailable) console.log(line)
console.log('\n| Workload | Bun | Top thread names (slices/s, summed by name) | Top syscalls (calls in 30 s) |\n|---|---|---|---|')
for (const r of runs.filter(r => ['claude','codex','agy','historic'].includes(r.group))) {
    const totals = new Map<string,number>()
    for (const w of r.windows) for (const t of w.byThread) {
        totals.set(t.name, (totals.get(t.name) ?? 0) + t.slices / w.seconds / r.windows.length)
    }
    const top = [...totals].sort((a,b) => b[1]-a[1]).slice(0,4).map(([name,n]) => name + ' ' + n.toFixed(2))
    const trace = readFileSync(join(base,r.version+'-'+r.group,'strace.txt'),'utf8')
    if (!trace.includes('total')) throw Error('Incomplete syscall trace')
    const calls = trace.trim().split('\n').map(line => line.trim().split(/\s+/))
        .filter(f => /^\d+\.\d+$/.test(f[0]) && f.at(-1) !== 'total')
        .sort((a,b) => Number(b[3])-Number(a[3])).slice(0,5).map(f => f.at(-1) + ' ' + f[3])
    console.log('| ' + [r.group,r.version,top.join(', '),calls.join(', ')].join(' | ') + ' |')
}
