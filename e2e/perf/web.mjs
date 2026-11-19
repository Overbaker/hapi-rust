import { createRequire } from 'node:module'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const root = resolve('web')
const requireWeb = createRequire(resolve(root, 'package.json'))
const { build, preview } = await import(pathToFileURL(requireWeb.resolve('vite')))
const outDir = process.argv[2]
const config = { root, configFile: resolve(root, 'vite.config.ts'), build: { outDir, sourcemap: 'hidden', rollupOptions: { preserveEntrySignatures: 'exports-only', input: { index: resolve(root,'index.html'), probe: 'virtual:baseline-probe' }, output: { entryFileNames: chunk => chunk.name === 'probe' ? 'probe.js' : 'assets/[name]-[hash].js' } } }, logLevel: 'error' }
await build({ ...config, plugins: [{
    name: 'baseline-normalize-timing',
    enforce: 'pre',
    transform(source,id) {
        if (id.endsWith('/components/DiffView.tsx')) {
            return source.replace("import { diffLines } from 'diff'", `import { diffLines as originalDiffLines } from 'diff'
function diffLines(a, b) {
    const start = performance.now()
    const parts = originalDiffLines(a,b)
    globalThis.__diffTimes?.push(performance.now()-start)
    return parts
}`)
        }
        if (!id.endsWith('/chat/normalize.ts')) return
        return source.replace('export function normalizeDecryptedMessage','function normalizeOriginal') + `
export function normalizeDecryptedMessage(message) {
    const h = globalThis.__cold
    if (!h?.ids?.has(message.id)) return normalizeOriginal(message)
    const t = performance.now()
    try { return normalizeOriginal(message) }
    finally { h.normalize += performance.now()-t; h.seen.add(message.id) }
}`
    },
    resolveId(id) { if (id === 'virtual:baseline-probe') return '\0' + id },
    load(id) {
        if (id !== '\0virtual:baseline-probe') return
        return `
import { DiffView } from '/src/components/DiffView.tsx'
import { I18nProvider } from '/src/lib/i18n-context.tsx'
import { createElement } from 'react'
import { createRoot } from 'react-dom/client'
import { flushSync } from 'react-dom'
export function mountDiff(a,b) {
    const container = document.createElement('div')
    document.body.appendChild(container)
    const root = createRoot(container)
    globalThis.__diffTimes=[]
    const start = performance.now()
    flushSync(() => root.render(createElement(I18nProvider, null, createElement(DiffView, {oldString:a,newString:b}))))
    const commit = performance.now()
    const height = container.offsetHeight
    return {diffMs:globalThis.__diffTimes,mountMs:commit-start, forcedLayoutMs:performance.now()-commit, height, nodes:container.querySelectorAll('*').length}
}`
    }
}] })
const server = await preview({ ...config, preview: { host: '127.0.0.1', port: 0 } })
console.log('BASELINE_WEB=' + server.resolvedUrls.local[0])
