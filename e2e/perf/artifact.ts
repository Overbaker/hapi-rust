import { gzipSync } from 'node:zlib'
import { mkdirSync, writeFileSync } from 'node:fs'
import { basename, join } from 'node:path'
import { randomUUID } from 'node:crypto'

export function artifact(type: string, data: unknown) {
    const root = 'benchmarks/e2e-baseline/results'
    mkdirSync(root, { recursive: true })
    const id = basename(process.env.BASELINE_RUN_ID ?? 'standalone')
    const file = `${id}-${randomUUID()}.${type}.json.gz`
    writeFileSync(join(root, file), gzipSync(JSON.stringify(data)))
    return file
}
