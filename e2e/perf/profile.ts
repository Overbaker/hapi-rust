import { artifact } from './artifact'
import { readFileSync } from 'node:fs'
import { basename, join } from 'node:path'
import { createRequire } from 'node:module'
import type { Page } from 'playwright'

const requirePlugin = createRequire(createRequire(new URL('../../web/package.json', import.meta.url)).resolve('@vitejs/plugin-react'))
const requireBabel = createRequire(requirePlugin.resolve('@babel/core'))
const requireMap = createRequire(requireBabel.resolve('@babel/generator'))
const { TraceMap, originalPositionFor } = requireMap('@jridgewell/trace-mapping')

export async function profile(page: Page, dist: string) {
    const cdp = await page.context().newCDPSession(page)
    await cdp.send('Profiler.enable')
    await cdp.send('Profiler.start')
    return async (window: { bounds:number[], layoutIntervals:number[][] }) => {
        const { profile } = await cdp.send('Profiler.stop')
        await cdp.detach()
        const nodes = new Map(profile.nodes.map(n => [n.id,n]))
        const parents = new Map<number,number>()
        const categories = new Map<number,string>()
        const markdownSubtree = new Set<number>()
        const maps = new Map<string,any>()
        const names = new Map<number,string>()
        for (const n of profile.nodes) {
            for (const child of n.children ?? []) parents.set(child,n.id)
            const f=n.callFrame
            let source='', name=f.functionName
            if (f.url.includes('/assets/') && f.lineNumber >= 0) {
                const file=basename(f.url)
                if (!maps.has(file)) maps.set(file,new TraceMap(JSON.parse(readFileSync(join(dist,'assets',file+'.map'),'utf8'))))
                const pos=originalPositionFor(maps.get(file),{line:f.lineNumber+1,column:Math.max(0,f.columnNumber)})
                source=pos.source ?? ''
                name=pos.name ?? name
            }
            names.set(n.id, (source.split('/').slice(-3).join('/') || f.functionName) + ' ' + name)
            if (/micromark|mdast|remark-|unified\/|markdown-it/.test(source)) categories.set(n.id,'markdown')
            if (categories.get(n.id) === 'markdown'
                || /react-markdown|hast-util-|@shikijs|shiki|markdown-renderer|markdown-text|MarkdownRenderer/i.test(source)
                || /MarkdownRenderer|MarkdownText|MarkdownRoot|MarkdownContent|SyntaxHighlighter/.test(name+' '+f.functionName)) {
                markdownSubtree.add(n.id)
            }
            if (/react-dom/.test(source) && /renderRoot|performUnitOfWork|beginWork|completeWork/.test(name+' '+f.functionName)) categories.set(n.id,'reactRender')
            if (/react-dom/.test(source) && /commit/i.test(name+' '+f.functionName)) categories.set(n.id,'reactCommit')
        }
        const ms = { markdown:0, reactRender:0, reactCommit:0, layout:0, other:0, idle:0 }
        let ts = profile.startTime, layoutIndex = 0
        const hot = new Map<string,number>()
        for (const [i,id] of (profile.samples ?? []).entries()) {
            const duration = profile.timeDeltas?.[i] ?? 0
            ts += duration
            if (ts < window.bounds[0] || ts > window.bounds[1]) continue
            let bucket: keyof typeof ms = 'other'
            if (nodes.get(id)?.callFrame.functionName === '(idle)') bucket='idle'
            for (let n: number|undefined=id;n !== undefined;n=parents.get(n)) {
                if (categories.has(n)) {bucket=categories.get(n) as keyof typeof ms;break}
            }
            while (window.layoutIntervals[layoutIndex]?.[1] < ts) layoutIndex++
            if (window.layoutIntervals[layoutIndex]?.[0] <= ts) bucket='layout'
            const delta=duration/1000
            ms[bucket]+=delta
            if (bucket !== 'idle') hot.set(names.get(id)!, (hot.get(names.get(id)!) ?? 0)+delta)
        }
        const profileFile = artifact('cpu', { startTime:profile.startTime, endTime:profile.endTime,
            nodes:[...nodes.values()].map(n=>({id:n.id, frame:names.get(n.id), category:categories.get(n.id), markdownSubtree:markdownSubtree.has(n.id), children:n.children})),
            samples:profile.samples,timeDeltas:profile.timeDeltas })
        return { profileFile,sampledMs:ms, hot:[...hot].sort((a,b)=>b[1]-a[1]).slice(0,8) }
    }
}
