import { execFileSync } from 'node:child_process'
import { readFileSync, readdirSync } from 'node:fs'

const hz = Number(execFileSync('getconf', ['CLK_TCK'], { encoding: 'utf8' }))
function stat(path: string) {
    const line = readFileSync(path, 'utf8')
    return line.slice(line.lastIndexOf(')') + 2).split(' ')
}

export function snapshot(pid: number) {
    const base = `/proc/${pid}`
    const main = stat(base + '/stat')
    const threads: Record<string, { name: string, slices: number }> = {}
    for (const tid of readdirSync(base + '/task')) {
        const task = base + '/task/' + tid
        try {
            const generation = stat(task + '/stat')[19]
            const name = readFileSync(task + '/comm', 'utf8').trim()
            const slices = Number(readFileSync(task + '/schedstat', 'utf8').trim().split(/\s+/)[2])
            if (!Number.isFinite(slices)) throw Error('Invalid schedstat: ' + task)
            threads[tid + '/' + generation] = { name, slices }
        } catch (error) {
            if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
        }
    }
    const pss = /^Pss:\s+(\d+) kB$/m.exec(readFileSync(base + '/smaps_rollup', 'utf8'))
    if (!pss || main[0] === 'Z' || !Object.keys(threads).length) throw Error('Missing process ' + pid)
    return { pid, generation: main[19], ticks: Number(main[11]) + Number(main[12]), threads, pss: Number(pss[1]), time: performance.now() }
}

export function interval(before: ReturnType<typeof snapshot>, after: ReturnType<typeof snapshot>) {
    if (before.generation !== after.generation) throw Error('PID reused')
    const seconds = (after.time - before.time) / 1000
    const byThread = Object.entries(after.threads).map(([tid, t]) => ({
        tid, name: t.name, slices: t.slices - (before.threads[tid]?.slices ?? 0)
    }))
    return { pid: after.pid, seconds, cpu: 100 * (after.ticks - before.ticks) / hz / seconds,
        rate: byThread.reduce((sum, t) => sum + t.slices, 0) / seconds, threads: byThread.length,
        lost: Object.keys(before.threads).filter(tid => !after.threads[tid]).length, byThread, pss: after.pss / 1024 }
}

type Window = ReturnType<typeof interval>
const mean = (v: number[]) => v.reduce((a,b) => a+b, 0) / v.length
export const header = '| Workload | Bun | CPU % mean ± SD | Max CPU % | Slices/s mean ± SD | Max slices/s | PSS MiB mean ± SD | Threads min–max | Lost TIDs |'
export function row(group: string, version: string, windows: Window[]) {
    const pids = [...new Set(windows.map(w => w.pid))]
    function summary(key: 'cpu' | 'rate' | 'pss') {
        const values = pids.map(pid => mean(windows.filter(w => w.pid === pid).map(w => w[key])))
        const avg = mean(values)
        const sd = Math.sqrt(values.reduce((sum, v) => sum + (v-avg)**2, 0) / (values.length - 1))
        return avg.toFixed(3) + ' ± ' + sd.toFixed(3)
    }
    return '| ' + [group, version, summary('cpu'), Math.max(...windows.map(w => w.cpu)).toFixed(3),
        summary('rate'), Math.max(...windows.map(w => w.rate)).toFixed(3), summary('pss'),
        Math.min(...windows.map(w => w.threads)) + '–' + Math.max(...windows.map(w => w.threads)),
        windows.reduce((n,w) => n+w.lost,0)].join(' | ') + ' |'
}
