import { randomUUID } from 'node:crypto'
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { configuration } from '@/configuration'
import { hashObject } from '@/utils/deterministicJson'
import { filterCatalogAffectingClaudeArgs } from './sdk/metadataExtractor'
import { getClaudeCliVersion } from './sdk/utils'

export interface CatalogLaunch {
    cwd: string
    model?: string | null
    args?: readonly string[]
}

type CatalogLists = { tools?: string[], slashCommands?: string[] }

function stampPath(sessionId: string): string {
    return join(configuration.happyHomeDir, 'claude-catalog', hashObject(sessionId) + '.json')
}

function fingerprint(launch: CatalogLaunch, lists: CatalogLists): string {
    return hashObject([launch.cwd, launch.model ?? null, launch.args ?? [], lists.tools, lists.slashCommands])
}

export function recordCatalog(sessionId: string, launch: CatalogLaunch, lists: CatalogLists, version?: string): void {
    const file = stampPath(sessionId)
    const tmp = file + '.' + randomUUID()
    try {
        if (!version || !lists.tools?.length || !lists.slashCommands?.length) {
            rmSync(file, { force: true })
            return
        }
        mkdirSync(dirname(file), { recursive: true, mode: 0o700 })
        const stamp = { fingerprint: fingerprint(launch, lists), version, time: Date.now() }
        writeFileSync(tmp, JSON.stringify(stamp), { mode: 0o600 })
        renameSync(tmp, file)
    } catch {
        // Cache I/O is best effort.
    } finally {
        try { rmSync(tmp, { force: true }) } catch {}
    }
}

export async function canReuseCatalog(sessionId: string, launch: CatalogLaunch, metadata?: (CatalogLists & { path: string }) | null): Promise<boolean> {
    if (metadata?.path !== launch.cwd || !metadata.tools?.length || !metadata.slashCommands?.length
        || filterCatalogAffectingClaudeArgs(launch.args).length) return false
    try {
        const stamp = JSON.parse(readFileSync(stampPath(sessionId), 'utf8'))
        const age = Date.now() - stamp.time
        if (typeof stamp.time !== 'number' || age < 0 || age >= 24 * 60 * 60 * 1000
            || stamp.fingerprint !== fingerprint(launch, metadata)) return false
        const version = await getClaudeCliVersion(launch.cwd)
        return Boolean(version && version === stamp.version)
    } catch {
        return false
    }
}
