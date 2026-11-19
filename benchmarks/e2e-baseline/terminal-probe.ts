import { mock } from 'bun:test'
import { createServer } from 'node:net'
import { join } from 'node:path'
import { Worker } from 'node:worker_threads'
import { monitorEventLoopDelay, performance } from 'node:perf_hooks'
import * as buffer from '../../hub/src/socket/userTerminalBuffer'

const append = buffer.appendUserTerminalOutput
let mode = 'idle'
let calls: number[] = []
mock.module(import.meta.resolve('../../hub/src/socket/userTerminalBuffer'), () => ({
    ...buffer,
    appendUserTerminalOutput(session: string, terminal: string, text: string) {
        const before = performance.now()
        if (mode !== 'noop') append(session, terminal, text)
        calls.push(performance.now()-before)
    }
}))
const worker = new Worker(new URL('./lag-worker.ts',import.meta.url))
const histogram = monitorEventLoopDelay({resolution:1})
let workerReply: ((value:any)=>void) | undefined
worker.on('message', message => {
    if ('ping' in message) worker.postMessage(message)
    else workerReply?.(message)
})
function workerCommand(cmd:string) {
    return new Promise<any>(resolve=>{workerReply=resolve; worker.postMessage({cmd})})
}
async function command(cmd:string) {
    if (cmd === 'stop') {
        histogram.disable()
        const {samples}=await workerCommand('stop')
        const count=Number(histogram.count)
        return {calls, workerRTT:samples, ticks:count,
            hubP99:count ? histogram.percentile(99)/1e6 : null, hubMax:count ? histogram.max/1e6 : null}
    }
    mode=cmd
    calls=[]
    histogram.reset()
    histogram.enable()
    await workerCommand('start')
    if (cmd === 'calibrate') {
        await Bun.sleep(20)
        const until=performance.now()+45
        while (performance.now()<until) {}
        await Bun.sleep(20)
    }
    return {ready:true}
}
createServer(socket=>{
    socket.once('data',async data=>{
        try {socket.end(JSON.stringify(await command(data.toString().trim())))}
        catch (error) {socket.destroy(error as Error)}
    })
}).listen(join(process.env.HAPI_HOME!,'bench-probe.sock'))
