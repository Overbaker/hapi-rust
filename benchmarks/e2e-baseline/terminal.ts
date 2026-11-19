import assert from 'node:assert/strict'
import { connect } from 'node:net'
import { join } from 'node:path'
import { io, withHub } from './fixture'
import { summary } from './stats'

async function probe(dir:string,cmd:string) {
    const socket=connect(join(dir,'bench-probe.sock'))
    socket.setTimeout(10000,()=>socket.destroy(Error('instrumentation timeout')))
    socket.on('connect',()=>socket.write(cmd))
    let data=''
    for await (const chunk of socket) data+=chunk
    return JSON.parse(data)
}
await withHub(async ({dir,ids,url,token,jwt})=>{
    const sid=ids[0]
    const cli=io(url+'/cli',{auth:{token,clientType:'session-scoped',sessionId:sid},transports:['websocket'],autoConnect:false})
    const web=io(url+'/terminal',{auth:{token:jwt},transports:['websocket'],autoConnect:false})
    const alive=setInterval(()=>cli.emit('session-alive',{sid,time:Date.now(),thinking:false}),3000)
    try {
        for (const socket of [cli,web]) await new Promise<void>((ok,fail)=>socket.once('connect',ok).once('connect_error',fail).connect())
        cli.emit('session-alive',{sid,time:Date.now(),thinking:false})
        await Bun.sleep(200)
        const terminalId=crypto.randomUUID()
        const opened=Promise.withResolvers<void>()
        cli.once('terminal:open',()=>opened.resolve())
        web.once('terminal:error',e=>opened.reject(Error(e.message)))
        web.emit('terminal:create',{sessionId:sid,terminalId,cols:80,rows:24})
        await Promise.race([opened.promise,Bun.sleep(5000).then(()=>{throw Error('terminal open timeout')})])
        await probe(dir,'calibrate')
        const calibration=await probe(dir,'stop')
        const histogramValid=calibration.hubMax>=30
        const workerValid=Math.max(...calibration.workerRTT)>=30
        console.log(JSON.stringify({kind:'probeCalibration',histogramValid,workerValid,hubMax:calibration.hubMax,workerMax:Math.max(...calibration.workerRTT)}))
        let received=0, receivedBytes=0
        web.on('terminal:output',e=>{received++;receivedBytes+=e.data.length})
        const reverse=process.argv.includes('--reverse')
        for (const mode of ['idle',...(reverse?['noop','append']:['append','noop'])]) {
            received=0
            receivedBytes=0
            await probe(dir,mode)
            const start=performance.now()
            const size=64*1024, chunks=3200, interval=8
            if (mode==='idle') await Bun.sleep(1500)
            else {
                for (let i=0;i<chunks;i++) {
                    const data=String(i).padStart(8,'0')+'x'.repeat(size-8)
                    cli.emit('terminal:output',{sessionId:sid,terminalId,data})
                    await Bun.sleep(interval)
                }
                for (let i=0;i<100 && received<chunks;i++) await Bun.sleep(20)
                assert.equal(received,chunks)
                assert.equal(receivedBytes,200*1024*1024)
            }
            const elapsedMs=performance.now()-start
            const r=await probe(dir,'stop')
            assert.equal(r.calls.length,received)
            console.log(JSON.stringify({kind:'terminal',mode,size,interval,received,receivedBytes,elapsedMs,
                calls:r.calls,append:received?summary(r.calls):null,ticks:r.ticks,hubP99:histogramValid?r.hubP99:null,
                workerTicks:r.workerRTT.length,workerRTT:workerValid?summary(r.workerRTT):null}))
        }
    } finally {clearInterval(alive);cli.close();web.close()}
},undefined,[],join(import.meta.dir,'terminal-probe.ts'))
