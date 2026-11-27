import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { execFileSync } from 'node:child_process'
import { mkdir, mkdtemp, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { RPC_METHODS } from '@hapi/protocol/rpcMethods'
import { RpcHandlerManager } from '@/api/rpc/RpcHandlerManager'
import { MachinePathPolicy } from '@/api/machinePathPolicy'
import { registerWorkspaceFileHandlers } from './workspaceFileHandlers'
import { collectArchiveEntries, writeZip } from './workspaceArchive'

async function call(rpc: RpcHandlerManager, params: unknown): Promise<Record<string, unknown>> {
    const response = await rpc.handleRequest({ method: `machine-test:${RPC_METHODS.WorkspaceArchive}`, params: JSON.stringify(params) })
    return JSON.parse(response) as Record<string, unknown>
}

function pythonZipListing(zipPath: string): { names: string[]; contents: Record<string, string> } {
    const script = [
        'import json,sys,zipfile',
        'z=zipfile.ZipFile(sys.argv[1])',
        'assert z.testzip() is None',
        'print(json.dumps({"names":z.namelist(),"contents":{n:(z.read(n).decode("utf8") if z.getinfo(n).file_size < 1024 else "#%d" % z.getinfo(n).file_size) for n in z.namelist() if not n.endswith("/")}}))',
    ].join('\n')
    return JSON.parse(execFileSync('python3', ['-I', '-c', script, zipPath], { encoding: 'utf8' }))
}

describe('workspace folder archive', () => {
    let base: string
    let root: string

    beforeEach(async () => {
        base = await mkdtemp(join(tmpdir(), 'hapi-archive-'))
        root = join(base, 'workspace')
        await mkdir(join(root, 'docs', 'empty'), { recursive: true })
        await mkdir(join(base, 'outside'))
        await writeFile(join(root, 'docs', 'a.txt'), 'alpha')
        await writeFile(join(root, 'docs', '中文.md'), '# 你好')
        await writeFile(join(root, 'docs', 'big.bin'), Buffer.alloc(3 * 1024 * 1024, 7))
        await writeFile(join(base, 'outside', 'secret.txt'), 'keep out')
        await symlink(join(base, 'outside'), join(root, 'docs', 'link-out'))
    })

    afterEach(async () => {
        await rm(base, { recursive: true, force: true })
    })

    it('writes a valid zip with nested folders, unicode names, and no symlinked content', async () => {
        const { entries, totalBytes } = await collectArchiveEntries(join(root, 'docs'), 'docs')
        expect(totalBytes).toBe(5 + Buffer.byteLength('# 你好') + 3 * 1024 * 1024)
        const zipPath = join(base, 'out.zip')
        await writeZip(entries, zipPath)

        const listing = pythonZipListing(zipPath)
        expect(listing.names.sort()).toEqual(['docs/', 'docs/a.txt', 'docs/big.bin', 'docs/empty/', 'docs/中文.md'])
        expect(listing.contents['docs/a.txt']).toBe('alpha')
        expect(listing.contents['docs/中文.md']).toBe('# 你好')
        expect(listing.contents['docs/big.bin']).toBe(`#${3 * 1024 * 1024}`)
        execFileSync('unzip', ['-tqq', zipPath])
    })

    it('zips through the runner RPC: start, poll, read in chunks, release', async () => {
        const rpc = new RpcHandlerManager({ scopePrefix: 'machine-test' })
        registerWorkspaceFileHandlers(rpc, new MachinePathPolicy({ workspaceRoots: [root] }))

        const started = await call(rpc, { action: 'start', cwd: root, path: 'docs' })
        expect(started).toMatchObject({ success: true, fileName: 'docs.zip' })
        const archiveId = started.archiveId as string

        let status = started
        for (let i = 0; i < 200 && status.state !== 'ready'; i += 1) {
            await new Promise((resolve) => setTimeout(resolve, 20))
            status = await call(rpc, { action: 'status', cwd: root, archiveId })
        }
        expect(status).toMatchObject({ success: true, state: 'ready' })

        // Another workspace cannot read this archive.
        expect(await call(rpc, { action: 'read', cwd: join(root, 'docs'), archiveId, offset: 0 })).toMatchObject({ success: false })

        const size = status.size as number
        const parts: Buffer[] = []
        for (let offset = 0; offset < size;) {
            const chunk = await call(rpc, { action: 'read', cwd: root, archiveId, offset, length: 64 * 1024 })
            const bytes = Buffer.from(chunk.content as string, 'base64')
            expect(bytes.length).toBeGreaterThan(0)
            parts.push(bytes)
            offset += bytes.length
        }
        const zipPath = join(base, 'downloaded.zip')
        await writeFile(zipPath, Buffer.concat(parts))
        expect(pythonZipListing(zipPath).names).toContain('docs/a.txt')

        expect(await call(rpc, { action: 'release', cwd: root, archiveId })).toEqual({ success: true })
        expect(await call(rpc, { action: 'status', cwd: root, archiveId })).toMatchObject({ success: false, code: 'not_found' })
    })

    it('zips the workspace root and refuses folders outside it', async () => {
        const rpc = new RpcHandlerManager({ scopePrefix: 'machine-test' })
        registerWorkspaceFileHandlers(rpc, new MachinePathPolicy({ workspaceRoots: [root] }))
        const rootStart = await call(rpc, { action: 'start', cwd: root, path: '' })
        expect(rootStart, JSON.stringify(rootStart)).toMatchObject({ success: true, fileName: 'workspace.zip' })
        expect((await call(rpc, { action: 'start', cwd: root, path: '../outside' })).success).toBe(false)
        expect((await call(rpc, { action: 'start', cwd: root, path: 'docs/a.txt' })).success).toBe(false)
    })
})
