import { artifact } from './artifact'
import assert from 'node:assert/strict'
import type { Page } from 'playwright'
import { summary } from '../../benchmarks/e2e-baseline/stats'

type Event = { name: string, pid: number, tid: number, ts: number, dur?: number, args?: any }
type Interval = [number, number]

function merged(events: Event[]): Interval[] {
    const ranges: Interval[] = []
    for (const event of events.sort((a, b) => a.ts - b.ts)) {
        const end = event.ts + (event.dur ?? 0)
        const last = ranges.at(-1)
        if (last && event.ts <= last[1]) last[1] = Math.max(last[1], end)
        else ranges.push([event.ts, end])
    }
    return ranges
}

function time(ranges: Interval[], a: number, b: number) {
    let total = 0
    for (const [start, end] of ranges) {
        if (start >= b) break
        total += Math.max(0, Math.min(b, end) - Math.max(a, start))
    }
    return total / 1000
}

export async function trace(page: Page, action: () => Promise<void>) {
    const cdp = await page.context().newCDPSession(page)
    const events: Event[] = []
    cdp.on('Tracing.dataCollected', ({ value }) => events.push(...value as Event[]))
    await cdp.send('Tracing.start', { categories: 'devtools.timeline,blink.user_timing,toplevel,disabled-by-default-devtools.timeline.frame', transferMode: 'ReportEvents' })
    try {
        await page.evaluate(() => {
            performance.mark('bench-start')
            let raf = requestAnimationFrame(function tick() {
                performance.mark('bench-frame')
                raf = requestAnimationFrame(tick)
            })
            Object.assign(window, { stopBenchFrames: () => cancelAnimationFrame(raf) })
        })
        await action()
        await page.evaluate(() => {
            performance.mark('bench-end')
            ;(window as any).stopBenchFrames()
        })
    } finally {
        const done = new Promise<void>(r => cdp.once('Tracing.tracingComplete', () => r()))
        await cdp.send('Tracing.end')
        await done
        await cdp.detach()
    }
    const first = events.find(e => e.name === 'bench-start')!
    const main = events.filter(e => e.pid === first.pid && e.tid === first.tid)
    const end = main.find(e => e.name === 'bench-end')!.ts
    const tasks = merged(main.filter(e => /^(RunTask|ThreadControllerImpl::RunTask)$/.test(e.name) && e.dur))
    assert(tasks.length, 'Missing renderer main-thread tasks')
    const layout = merged(main.filter(e => ['Layout', 'UpdateLayoutTree'].includes(e.name)))
    const frames = main.filter(e => e.name === 'bench-frame').map(e => e.ts).sort((a,b) => a-b)
    const chunks = main.filter(e => e.name === 'bench-chunk').map(e => e.ts).sort((a,b) => a-b)
    const windows = chunks.length ? chunks : Array.from({length: Math.ceil((end-first.ts)/250000)}, (_,i) => first.ts+i*250000)
    const updateWork = windows.map((a,i) => time(tasks,a,windows[i+1] ?? end))
    const frameWork = frames.slice(1).map((b,i) => time(tasks, frames[i], b))
    let framedWindows = 0
    const activeFrames = windows.map((a,i) => {
        const b = windows[i+1] ?? end
        const cuts = [a, ...frames.filter(t => t>a && t<b), b]
        if (cuts.length>2) framedWindows++
        return Math.max(...cuts.slice(1).map((t,j) => time(tasks,cuts[j],t)))
    })
    const longTasks = tasks.filter(([a,b]) => a>=first.ts && a<end && b-a>50000).length
    const dropped = events.filter(e => e.pid === first.pid && e.name === 'DroppedFrame' && e.ts>=first.ts && e.ts<end)
    const traceFile = artifact('trace', { units:'microseconds', mainPid:first.pid, mainTid:first.tid,
        start:first.ts,end,frames,chunks,tasks:tasks.filter(([a,b])=>a<=end && b>=first.ts),
        layout:layout.filter(([a,b])=>a<=end && b>=first.ts), dropped:dropped.map(e=>e.ts) })
    return { traceFile, bounds:[first.ts,end], layoutIntervals:layout.filter(([a,b])=>a<end && b>first.ts), updates: chunks.length, framedWindows, observedFrames: frames.length, updateWork, peakFramePerUpdate: activeFrames,
        workMs: summary(updateWork), peakFrameMs: framedWindows>=windows.length*.8 ? summary(activeFrames) : null, frameWorkMs: frameWork.length ? summary(frameWork) : null,
        longTasks, droppedFrameEvents: dropped.length, layoutMs: time(layout,first.ts,end),
        busyMs: time(tasks,first.ts,end), elapsedMs:(end-first.ts)/1000 }
}
