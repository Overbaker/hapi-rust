import { parentPort } from 'node:worker_threads'
import { performance } from 'node:perf_hooks'

const port = parentPort!
let timer: ReturnType<typeof setInterval> | undefined
let epoch = 0
const samples: number[] = []
port.on('message', message => {
    if (message.cmd === 'start') {
        epoch++
        samples.length = 0
        timer = setInterval(() => port.postMessage({ping:performance.now(),epoch}), 5)
        port.postMessage({ready:true})
    } else if (message.cmd === 'stop') {
        clearInterval(timer)
        port.postMessage({samples:[...samples]})
    } else if (message.epoch === epoch) {
        samples.push(performance.now()-message.ping)
    }
})
