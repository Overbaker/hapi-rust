import assert from 'node:assert/strict'
import { chromium, type Page, type Browser } from 'playwright'
import { replay, withHub, type Seed } from '../../benchmarks/e2e-baseline/fixture'
import { trace } from './trace'
import { profile } from './profile'

const repeat = Number(process.env.STREAM_REPEAT || 1)
const emit = (record: object) => console.log(JSON.stringify({ ...record, repeat }))
const flags = ['--disable-gpu', '--run-all-compositor-stages-before-draw']
const selected = new Set((process.env.BROWSER_CASES || 'navigation,scroll,mount,stream').split(','))

async function fresh(browser: Browser, rate: number, cold = false) {
    const page = await browser.newPage({ viewport: { width:1440, height:1000 }, serviceWorkers:'block' })
    const cdp = await page.context().newCDPSession(page)
    await cdp.send('Emulation.setCPUThrottlingRate', {rate})
    await page.addInitScript(({cold,watchPaint}) => {
        const w = window as any
        if (watchPaint) new PerformanceObserver(list => {
            for (const entry of list.getEntries() as LargestContentfulPaint[]) {
                w.__lcp = {ms:entry.startTime, size:entry.size, tag:entry.element?.tagName ?? null,
                    inThread:entry.element?.closest('.happy-thread-messages') != null,
                    mountedAtPaint:document.querySelector('.happy-thread-messages')?.childElementCount ?? 0}
            }
        }).observe({type:'largest-contentful-paint',buffered:true})
        if (cold) {
            const original = Response.prototype.json
            Response.prototype.json = async function () {
                if (!this.url.includes('/messages?')) return original.call(this)
                const text = await this.text()
                const t = performance.now()
                const parsed = JSON.parse(text)
                const parse = performance.now()-t
                if (parsed.messages?.length === 200 && !w.__cold)
                    w.__cold = { parse, responseBytes:new TextEncoder().encode(text).length, responseChars: text.length, normalize:0,
                        ids:new Set(parsed.messages.map((m: {id:string})=>m.id)), seen:new Set() }
                return parsed
            }
        }
    },{cold,watchPaint:selected.has('navigation') || selected.has('lcp')})
    if (cold) await page.route('**/messages?*', async route => {
        const url=new URL(route.request().url())
        if (!url.searchParams.has('beforeAt') && !url.searchParams.has('afterAt')) url.searchParams.set('limit','200')
        await route.continue({url:url.href})
    })
    return page
}

const tail = (p: Page,n:number) => p.getByText(`Baseline ${n} message ${n}`,{exact:true}).last()
const mounted = (p: Page) => p.locator('.happy-thread-messages').last().evaluate(el=>el.childElementCount)
async function nav(page: Page, address:string,n:number) {
    await page.goto(address,{waitUntil:'domcontentloaded'})
    await tail(page,n).waitFor({timeout:60000})
}

async function stableRows(page:Page, count:number) {
    return page.evaluate(async expected => {
        const size=()=>document.querySelector('.happy-thread-messages')?.childElementCount
        return new Promise<boolean>(resolve=>{
            let frame=0, consecutive=0
            const timeout=setTimeout(()=>{cancelAnimationFrame(frame);resolve(false)},8000)
            const tick=()=>{
                consecutive=size()===expected ? consecutive+1 : 0
                if (consecutive>=6) {clearTimeout(timeout);resolve(true)}
                else frame=requestAnimationFrame(tick)
            }
            frame=requestAnimationFrame(tick)
        })
    },count)
}

async function warmCompositor(page: Page, blank?: string) {
    if (blank) await page.goto(blank)
    await page.bringToFront()
    await page.mouse.move(25,25)
    return await page.evaluate(() => new Promise<boolean>(resolve => {
        let previous=0, good=0, raf=0
        const timeout=setTimeout(()=>{cancelAnimationFrame(raf);resolve(false)}, 90000)
        function frame(t: number) {
            good = t-previous<50 ? good+1 : 0
            previous=t
            if (good>=12) { clearTimeout(timeout); resolve(true) }
            else raf=requestAnimationFrame(frame)
        }
        raf=requestAnimationFrame(frame)
    }))
}

const scenarios=(process.env.STREAM_SCENARIOS || 'baseline,idle,clean-tail,default-flags,cpu4,assistant,observer,rows5,rows51,rows100').split(',')
const streamTrials=selected.has('stream') ? scenarios.flatMap(scenario=>(scenario==='idle'?[0]:scenario.startsWith('rows')?[6]:repeat%2?[6,30]:[30,6]).map(kib=>{
    const sweep=scenario.startsWith('rows')
    const expected=sweep?Number(scenario.slice(4)):51
    const seed:Seed={count:sweep?expected:500,kind:sweep?'sweep':scenario==='clean-tail'?'clean':'standard'}
    return {scenario,kib,expected,seed}
})) : []

await withHub(async ({ ids,url,token,dir })=>{
    const dist=dir+'/web'
    const server=Bun.spawn(['node','e2e/perf/web.mjs',dist],{env:{...process.env,VITE_HUB_PROXY:url},stdout:'pipe',stderr:'inherit'})
    try {
        let output=''
        for await (const data of server.stdout) {
            output+=new TextDecoder().decode(data)
            if (/BASELINE_WEB=(http:\/\/[^\s]+)\n/.test(output)) break
        }
        const base=output.match(/BASELINE_WEB=(http:\/\/[^\s]+)/)?.[1]
        assert(base,'Web build failed')
        const address=(i:number)=>base+'sessions/'+ids[i]+'?token='+token
        await Bun.write(dist+'/probe.html','<!doctype html><body><p>Compositor warmup</p></body>')
        const launch=(software:boolean)=>chromium.launch({headless:true,channel:'chromium',args:['--no-sandbox',...(software?flags:[])]})
        const browser=await launch(true)
        let defaultBrowser: Browser | undefined
        try {
            if (selected.has('navigation') || selected.has('lcp')) for (const rate of [1,4]) {
                let accepted=0, rejected=0
                while (accepted<20 && rejected<4) {
                    const page=await fresh(browser,rate)
                    try {
                        const warm=await warmCompositor(page,base+'probe.html')
                        if (!warm) {
                            emit({kind:'invalid',metric:'lcp',rate,reason:'fresh-context compositor did not warm'})
                            rejected++
                            continue
                        }
                        await nav(page,address(0),500)
                        const stable=await stableRows(page,51)
                        const paint=await page.evaluate(()=>(window as any).__lcp)
                        if (stable && paint?.ms>0) {
                            emit({kind:'lcp',rate,i:accepted++,...paint,mounted:51})
                        } else {
                            rejected++
                            emit({kind:'invalid',metric:'lcp',rate,paint:paint ?? null,mounted:await mounted(page),reason:'end-state paint not verified'})
                        }
                    } finally {await page.context().close()}
                }
                if (!selected.has('navigation')) continue
                for (const [idx,seed] of [[0,'standard'],[3,'heavy']] as const) for (let i=0;i<20;i++) {
                    const page=await fresh(browser,rate,true)
                    try {
                        await nav(page,address(idx),500)
                        await page.waitForFunction(()=>(window as any).__cold?.seen.size===200,undefined,{polling:50,timeout:20000})
                        emit({kind:'cold',rate,i,seed,mounted:await mounted(page),...await page.evaluate(()=>{
                            const h=(window as any).__cold
                            return {rows:h.seen.size,parse:h.parse,normalize:h.normalize,ms:h.parse+h.normalize,responseBytes:h.responseBytes}
                        })})
                    } finally {await page.context().close()}
                }
            }
            if (selected.has('mount')) for (const rate of [1,4]) for (let i=0;i<10;i++) {
                const page=await fresh(browser,rate)
                try {
                    await page.goto(base+'probe.html')
                    await page.evaluate(async()=>{
                        const html=await (await fetch('/index.html')).text()
                        const links=new DOMParser().parseFromString(html,'text/html').querySelectorAll('link[rel=stylesheet]')
                        if (!links.length) throw Error('Production CSS missing from build')
                        await Promise.all(Array.from(links, link=>new Promise<void>((ok,fail)=>{
                            const sheet=document.createElement('link')
                            sheet.rel='stylesheet'
                            sheet.href=link.getAttribute('href')!
                            sheet.onload=()=>ok()
                            sheet.onerror=()=>fail(Error('DiffView stylesheet load failed'))
                            document.head.appendChild(sheet)
                        })))
                        await document.fonts.ready
                        const probeUrl=new URL('/probe.js',location.href).href
                        ;(window as any).probe=await import(probeUrl)
                    })
                    let mount
                    const metrics=await trace(page,async()=>{
                        mount=await page.evaluate(()=>{
                            const a=Array.from({length:2000},(_,i)=>`export const field${i} = ${i}`)
                            return (window as any).probe.mountDiff(a.join('\n'),a.map((s,i)=>i%4?s:s+' + 1').join('\n'))
                        })
                        await page.waitForTimeout(200)
                    })
                    emit({kind:'mount',rate,mount,...metrics})
                } finally {await page.context().close()}
            }
            if (selected.has('scroll')) {
                const page=await fresh(browser,1)
                try {
                    const warm=await warmCompositor(page,base+'probe.html')
                    await nav(page,address(1),2000)
                    const active=await warmCompositor(page)
                    if (!warm || !active) throw Error('scroll compositor did not warm')
                    const viewport=page.locator('.app-scroll-y').filter({has:tail(page,2000)}).last()
                    const older:number[]=[]
                    page.on('response',res=>{
                        if (res.status()===200 && res.url().includes('/messages?') && res.url().includes('beforeSeq=')) older.push(Date.now())
                    })
                    await page.waitForTimeout(500)
                    await viewport.evaluate(el=>{
                        const w=window as any
                        w.distance=0
                        w.scrollEvents=0
                        w.upward=0
                        let last=el.scrollTop
                        el.addEventListener('scroll',()=>{
                            const dy=el.scrollTop-last
                            w.distance+=Math.abs(dy)
                            w.upward+=Math.max(0,-dy)
                            w.scrollEvents++
                            last=el.scrollTop
                        })
                    })
                    const box=await viewport.boundingBox()
                    assert(box)
                    await page.mouse.move(box.x+box.width/2,box.y+box.height/2)
                    const initial=await mounted(page)
                    const startCount=older.length
                    let iterations=0
                    const metrics=await trace(page,async()=>{
                        const start=performance.now()
                        for (let i=0;i<150;i++) {
                            iterations++
                            await page.mouse.wheel(0,-1200)
                            await page.waitForTimeout(Math.max(0,start+iterations*25-performance.now()))
                            if (older.length>startCount && i>=30) break
                        }
                        await page.waitForTimeout(500)
                    })
                    const [distance,upward,scrollEvents]=await page.evaluate(()=>{
                        const w=window as any
                        return [w.distance,w.upward,w.scrollEvents] as const
                    })
                    const requests=older.length-startCount
                    assert(upward>1000 && scrollEvents>=3 && requests>0,`scroll distance=${distance} older=${requests}`)
                    const fps=1000*metrics.observedFrames/metrics.elapsedMs
                    const expectedMs=iterations*25+500
                    if (fps<30 || metrics.elapsedMs>expectedMs*1.5 || metrics.elapsedMs<expectedMs*.8) {
                        emit({kind:'invalid', metric:'scroll',reason:'frame production or wheel cadence stalled',fps,iterations,expectedMs,elapsedMs:metrics.elapsedMs,distance,requests,traceFile:metrics.traceFile})
                        throw Error('Scroll smoothness not measurable here')
                    }
                    emit({kind:'scroll',fps,iterations,expectedMs,distance,upward,scrollEvents,requests,initial,mounted:await mounted(page),...metrics})
                } finally {await page.context().close()}
            }
            for (const [index,{scenario,kib,expected,seed}] of streamTrials.entries()) {
                const target=scenario==='default-flags'?(defaultBrowser ??= await launch(false)):browser
                const page=await fresh(target,scenario==='cpu4'?4:1)
                try {
                    await page.addInitScript(controlsInit)
                    await page.route('**/messages?*',route=>{
                        const requestUrl=new URL(route.request().url())
                        if (!requestUrl.searchParams.has('beforeAt') && !requestUrl.searchParams.has('afterAt')) requestUrl.searchParams.set('limit','200')
                        return route.continue({url:requestUrl.href})
                    })
                    const idx=4+index
                    await nav(page,address(idx),seed.count)
                    await page.waitForFunction(()=>(window as any).__connected,undefined,{polling:100})
                    const ready=async()=>await warmCompositor(page) && await stableRows(page,expected)
                    if (!await ready() && !await ready()) {
                        emit({kind:'invalid',metric:'stream',scenario,kib,expected,actual:await mounted(page),reason:'unmatched window or stalled compositor'})
                        continue
                    }
                    const before=await mounted(page)
                    await page.evaluate(()=>{const w=window as any; w.__updates=0; w.__rowCounts=[]})
                    const deliver=async()=>{
                        if (!kib) return page.waitForTimeout(25000)
                        await replay(url,token,ids[idx],kib,250,undefined,scenario==='assistant'?'message':'reasoning')
                        await page.waitForFunction(()=>(window as any).__updates>=100,undefined,{polling:50,timeout:60000})
                        await page.waitForTimeout(250)
                    }
                    if (scenario==='observer') {
                        await page.evaluate(()=>{
                            const w=window as any
                            w.__observerOnly=true
                            const durations:number[]=[]
                            const observer=new PerformanceObserver(list=>durations.push(...list.getEntries().map(e=>e.duration)))
                            observer.observe({type:'longtask'})
                            w.__stopObserver=()=>{
                                durations.push(...observer.takeRecords().map(e=>e.duration))
                                observer.disconnect()
                                return durations
                            }
                        })
                        await deliver()
                        emit({kind:'observer',kib,before,after:await mounted(page),durations:await page.evaluate(()=>(window as any).__stopObserver())})
                        continue
                    }
                    const finish=await profile(page,dist)
                    const metrics=await trace(page,deliver)
                    const cpu=await finish(metrics)
                    assert.equal(metrics.updates,kib?100:0)
                    const counts=await page.evaluate(()=>(window as any).__rowCounts as number[])
                    const low=Math.min(before,...counts), high=Math.max(before,...counts)
                    if (low<expected-1 || high>expected+1) {
                        emit({kind:'invalid',metric:'stream',scenario,kib,expected,actual:low,high,reason:'mounted window changed during replay',traceFile:metrics.traceFile,profileFile:cpu.profileFile})
                        continue
                    }
                    const {layoutIntervals,...work}=metrics
                    emit({kind:'stream',scenario,kib,before,low,high,mounted:await mounted(page),...work,...cpu})
                } finally {await page.context().close()}
            }
        } finally {await defaultBrowser?.close();await browser.close()}
    } finally {server.kill();await server.exited}
},undefined,streamTrials.map(t=>t.seed))

function controlsInit() {
    const w=window as any
    const Native=window.EventSource
    window.EventSource=class extends Native {
        constructor(url:string|URL,init?:EventSourceInit) {
            super(url,init)
            if (!new URL(url,location.href).searchParams.has('sessionId')) return
            this.addEventListener('open',()=>{w.__connected=true})
            this.addEventListener('message',e=>{
                if (e.data.includes('"type":"message-received"') && (e.data.includes('"type":"reasoning"') || e.data.includes('"streamSnapshot":true'))) {
                    if (!w.__observerOnly) performance.mark('bench-chunk')
                    w.__rowCounts?.push(document.querySelector('.happy-thread-messages')?.childElementCount ?? 0)
                    w.__updates++
                }
            })
        }
    }
}
