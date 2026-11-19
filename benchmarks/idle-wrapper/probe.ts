import { appendFileSync, existsSync } from 'node:fs'

const seen = new Set<string>()
export function mark(event: string) {
    if (event.startsWith('reactive:') && !existsSync(process.env.IDLE_ANSWERED!)) return
    if (!seen.has(event)) {
        appendFileSync(process.env.IDLE_MARKERS!, event + '\n')
        seen.add(event)
    }
}
export function disabled(name: string) {
    mark(name + ':reached')
    if (process.env.IDLE_DISABLE !== name) return false
    mark(name + ':disabled')
    return true
}
