import { cpus } from 'node:os'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import { chromium } from 'playwright'
import { summary } from '../../benchmarks/e2e-baseline/stats'

const requireWeb = createRequire(new URL('../../web/package.json', import.meta.url))
const build = await Bun.build({ target: 'browser', entrypoints: [requireWeb.resolve('diff')], format: 'esm' })
assert(build.success)
const moduleText = await build.outputs[0].text()
function cpuTimes() {
    return cpus().reduce((sum,cpu)=>{
        sum.idle+=cpu.times.idle
        sum.total+=Object.values(cpu.times).reduce((a,b)=>a+b,0)
        return sum
    },{idle:0,total:0})
}
const idleFraction=(a:ReturnType<typeof cpuTimes>,b:ReturnType<typeof cpuTimes>)=>(b.idle-a.idle)/(b.total-a.total)
let preflight=0
for (let i=0;i<30 && preflight<.90;i++) {
    const start=cpuTimes()
    await Bun.sleep(1000)
    preflight=idleFraction(start,cpuTimes())
}
if (preflight<.90) throw Error('Quiet guest unavailable for diff run')
const cpuStart=cpuTimes()
const browser = await chromium.launch({ headless: true, channel: 'chromium', args: ['--no-sandbox'] })
try {
    const page = await browser.newPage()
    await page.route('http://baseline.test/**', route => route.fulfill({ contentType: 'text/html', body: '<!doctype html>' }))
    await page.goto('http://baseline.test/')
    const cdp = await page.context().newCDPSession(page)
    await cdp.send('Tracing.start', { categories: 'devtools.timeline,blink.user_timing', transferMode: 'ReportEvents' })
    let measures = 0
    cdp.on('Tracing.dataCollected', ({ value }) => { measures += value.filter(e => e.name === 'baseline-diff').length })
    const times = await page.evaluate(async moduleText => {
        const url = URL.createObjectURL(new Blob([moduleText], { type: 'text/javascript' }))
        const workerSource = `
            import * as api from '${url}'
            const diffLines = api.diffLines ?? api.default.diffLines
            onmessage = ({ data: [a, b] }) => {
                const start = performance.now()
                const parts = diffLines(a, b)
                const end = performance.now()
                performance.measure('baseline-diff', { start, end })
                postMessage({ ms: end - start, parts: parts.length })
            }`
        const workerUrl = URL.createObjectURL(new Blob([workerSource], { type: 'text/javascript' }))
        const worker = new Worker(workerUrl, { type: 'module' })
        const samples = []
        try {
            for (const workload of [{name:'distributed', lines:2000, stride:10}, {name:'high-edit', lines:2000, stride:2}, {name:'large', lines:10000, stride:13}]) {
                const times = []
                for (let run = 0; run < 23; run++) {
                    const lines = Array.from({ length: workload.lines }, (_, i) => `line ${i}: observed session state`)
                    const b = lines.map((s, i) => i % workload.stride === run % workload.stride ? s + ' updated' : s).join('\n')
                    const result = await new Promise<{ ms: number, parts: number }>((resolve, reject) => {
                        worker.onmessage = e => resolve(e.data)
                        worker.onerror = e => reject(Error(e.message))
                        worker.postMessage([lines.join('\n'), b])
                    })
                    if (result.parts < 200) throw Error('Diff workload produced too few edits')
                    times.push(result.ms)
                }
                samples.push({workload:workload.name,lines:workload.lines,stride:workload.stride,warmup:times.slice(0,3),times:times.slice(3)})
            }
        } finally {
            worker.terminate()
            URL.revokeObjectURL(workerUrl)
            URL.revokeObjectURL(url)
        }
        return samples
    }, moduleText)
    const done = new Promise<void>(resolve => cdp.once('Tracing.tracingComplete', () => resolve()))
    await cdp.send('Tracing.end')
    await done
    assert(measures >= 55)
    for (const trial of times) console.log(JSON.stringify({kind:'worker',preflightIdle:preflight,runIdle:idleFraction(cpuStart,cpuTimes()),...trial, ms:summary(trial.times), traceEvents:measures}))
} finally { await browser.close() }
