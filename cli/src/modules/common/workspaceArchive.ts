import { createReadStream, createWriteStream, type WriteStream } from 'node:fs'
import { randomBytes } from 'node:crypto'
import { lstat, mkdir, open, readdir, rm, stat } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createDeflateRaw, crc32 } from 'node:zlib'

/**
 * Folder downloads for the files panel. The runner zips a workspace folder in
 * the background into a temp file; the browser polls progress and then pulls
 * the zip in chunks, so neither the RPC nor the HTTP request has to stay open
 * for the whole compression.
 *
 * The writer emits plain (non-zip64) archives: UTF-8 names, deflate with data
 * descriptors, so content is capped below 4 GiB and 65 535 entries.
 */

export const MAX_ARCHIVE_INPUT_BYTES = 2 * 1024 * 1024 * 1024
export const MAX_ARCHIVE_ENTRIES = 60_000
export const MAX_ARCHIVE_READ_BYTES = 4 * 1024 * 1024
const ARCHIVE_TTL_MS = 60 * 60 * 1000

type ArchiveEntry = { name: string; path: string; kind: 'file' | 'directory'; size: number; mtime: Date }

export class ArchiveTooLargeError extends Error {}

/** Walk `root` without following symlinks; returns zip entries named relative to `prefix`. */
export async function collectArchiveEntries(root: string, prefix: string): Promise<{ entries: ArchiveEntry[]; totalBytes: number }> {
    const entries: ArchiveEntry[] = []
    let totalBytes = 0
    const rootStats = await lstat(root)
    entries.push({ name: `${prefix}/`, path: root, kind: 'directory', size: 0, mtime: rootStats.mtime })

    const walk = async (dir: string, zipDir: string): Promise<void> => {
        const children = await readdir(dir, { withFileTypes: true })
        children.sort((a, b) => a.name.localeCompare(b.name))
        for (const child of children) {
            const path = join(dir, child.name)
            const name = `${zipDir}/${child.name}`
            // Symlinks and special files are skipped: following them could
            // pull in content from outside the workspace.
            if (child.isDirectory()) {
                const stats = await lstat(path)
                entries.push({ name: `${name}/`, path, kind: 'directory', size: 0, mtime: stats.mtime })
                if (entries.length > MAX_ARCHIVE_ENTRIES) throw new ArchiveTooLargeError('too many entries')
                await walk(path, name)
            } else if (child.isFile()) {
                const stats = await lstat(path)
                totalBytes += stats.size
                entries.push({ name, path, kind: 'file', size: stats.size, mtime: stats.mtime })
                if (entries.length > MAX_ARCHIVE_ENTRIES) throw new ArchiveTooLargeError('too many entries')
                if (totalBytes > MAX_ARCHIVE_INPUT_BYTES) throw new ArchiveTooLargeError('too large')
            }
        }
    }
    await walk(root, prefix)
    return { entries, totalBytes }
}

function dosDateTime(date: Date): { time: number; date: number } {
    const year = Math.max(1980, date.getFullYear())
    return {
        time: (date.getHours() << 11) | (date.getMinutes() << 5) | Math.floor(date.getSeconds() / 2),
        date: ((year - 1980) << 9) | ((date.getMonth() + 1) << 5) | date.getDate(),
    }
}

function write(stream: WriteStream, chunk: Buffer): Promise<void> {
    return new Promise((resolve, reject) => {
        stream.write(chunk, (error) => error ? reject(error) : resolve())
    })
}

/** Write `entries` as a zip to `outPath`; `onProgress` receives input bytes consumed so far. */
export async function writeZip(
    entries: readonly ArchiveEntry[],
    outPath: string,
    onProgress?: (bytes: number) => void,
): Promise<number> {
    const out = createWriteStream(outPath)
    const central: Buffer[] = []
    let offset = 0
    let consumed = 0
    try {
        for (const entry of entries) {
            const name = Buffer.from(entry.name, 'utf8')
            const { time, date } = dosDateTime(entry.mtime)
            const isDir = entry.kind === 'directory'
            const flags = isDir ? 0x0800 : 0x0808 // UTF-8 names (+ data descriptor for files)
            const method = isDir ? 0 : 8
            const localOffset = offset

            const local = Buffer.alloc(30)
            local.writeUInt32LE(0x04034b50, 0)
            local.writeUInt16LE(20, 4)
            local.writeUInt16LE(flags, 6)
            local.writeUInt16LE(method, 8)
            local.writeUInt16LE(time, 10)
            local.writeUInt16LE(date, 12)
            // crc / sizes are 0 here; files carry them in the data descriptor.
            local.writeUInt16LE(name.length, 26)
            await write(out, Buffer.concat([local, name]))
            offset += local.length + name.length

            let crc = 0
            let compressed = 0
            let size = 0
            if (!isDir) {
                const deflate = createDeflateRaw({ level: 6 })
                const source = createReadStream(entry.path)
                source.on('data', (chunk: Buffer | string) => {
                    const buffer = typeof chunk === 'string' ? Buffer.from(chunk) : chunk
                    crc = crc32(buffer, crc)
                    size += buffer.length
                    consumed += buffer.length
                    onProgress?.(consumed)
                })
                source.on('error', (error) => deflate.destroy(error))
                source.pipe(deflate)
                for await (const chunk of deflate) {
                    compressed += (chunk as Buffer).length
                    await write(out, chunk as Buffer)
                }
                offset += compressed

                const descriptor = Buffer.alloc(16)
                descriptor.writeUInt32LE(0x08074b50, 0)
                descriptor.writeUInt32LE(crc >>> 0, 4)
                descriptor.writeUInt32LE(compressed, 8)
                descriptor.writeUInt32LE(size, 12)
                await write(out, descriptor)
                offset += descriptor.length
            }

            const header = Buffer.alloc(46)
            header.writeUInt32LE(0x02014b50, 0)
            header.writeUInt16LE((3 << 8) | 20, 4) // made by Unix, spec 2.0
            header.writeUInt16LE(20, 6)
            header.writeUInt16LE(flags, 8)
            header.writeUInt16LE(method, 10)
            header.writeUInt16LE(time, 12)
            header.writeUInt16LE(date, 14)
            header.writeUInt32LE(crc >>> 0, 16)
            header.writeUInt32LE(compressed, 20)
            header.writeUInt32LE(size, 24)
            header.writeUInt16LE(name.length, 28)
            const mode = isDir ? 0o40755 : 0o100644
            header.writeUInt32LE(((mode << 16) | (isDir ? 0x10 : 0)) >>> 0, 38)
            header.writeUInt32LE(localOffset, 42)
            central.push(header, name)
        }

        const directory = Buffer.concat(central)
        await write(out, directory)
        const end = Buffer.alloc(22)
        end.writeUInt32LE(0x06054b50, 0)
        end.writeUInt16LE(entries.length, 8)
        end.writeUInt16LE(entries.length, 10)
        end.writeUInt32LE(directory.length, 12)
        end.writeUInt32LE(offset, 16)
        await write(out, end)
        offset += directory.length + end.length
    } finally {
        await new Promise<void>((resolve) => out.end(() => resolve()))
    }
    return offset
}

type ArchiveJob = {
    id: string
    cwd: string
    file: string
    state: 'building' | 'ready' | 'error'
    error?: string
    totalBytes: number
    doneBytes: number
    size: number
    createdAt: number
}

export type ArchiveStatus = {
    success: boolean
    error?: string
    code?: string
    archiveId?: string
    state?: ArchiveJob['state']
    totalBytes?: number
    doneBytes?: number
    size?: number
}

const ARCHIVE_ID_PATTERN = /^[a-f0-9]{32}$/

/** In-memory registry of archive jobs for this runner process. */
export class WorkspaceArchiveManager {
    private readonly jobs = new Map<string, ArchiveJob>()
    private readonly dir = join(tmpdir(), 'hapi-workspace-archives')

    /** Start zipping `folder` (already resolved inside `cwd`) as `<name>/...`. */
    async start(cwd: string, folder: string, name: string): Promise<ArchiveStatus> {
        await this.sweep()
        let collected: Awaited<ReturnType<typeof collectArchiveEntries>>
        try {
            collected = await collectArchiveEntries(folder, name)
        } catch (error) {
            if (error instanceof ArchiveTooLargeError) {
                return {
                    success: false,
                    code: 'too_large',
                    error: `Folder is too large to download (limit ${MAX_ARCHIVE_INPUT_BYTES / 1024 ** 3} GB / ${MAX_ARCHIVE_ENTRIES} items)`,
                }
            }
            throw error
        }
        await mkdir(this.dir, { recursive: true, mode: 0o700 })
        const id = randomBytes(16).toString('hex')
        const job: ArchiveJob = {
            id,
            cwd,
            file: join(this.dir, `${id}.zip`),
            state: 'building',
            totalBytes: collected.totalBytes,
            doneBytes: 0,
            size: 0,
            createdAt: Date.now(),
        }
        this.jobs.set(id, job)
        void writeZip(collected.entries, job.file, (bytes) => { job.doneBytes = bytes })
            .then((size) => {
                job.size = size
                job.doneBytes = job.totalBytes
                job.state = 'ready'
            })
            .catch(async (error: unknown) => {
                job.state = 'error'
                job.error = error instanceof Error ? error.message : String(error)
                await rm(job.file, { force: true }).catch(() => {})
            })
        return this.status(cwd, id)
    }

    status(cwd: string, id: unknown): ArchiveStatus {
        const job = this.find(cwd, id)
        if (!job) return { success: false, code: 'not_found', error: 'Download expired or not found' }
        return {
            success: job.state !== 'error',
            ...(job.error ? { error: job.error } : {}),
            archiveId: job.id,
            state: job.state,
            totalBytes: job.totalBytes,
            doneBytes: job.doneBytes,
            ...(job.state === 'ready' ? { size: job.size } : {}),
        }
    }

    async read(cwd: string, id: unknown, offset: unknown, length: unknown): Promise<{ success: boolean; error?: string; content?: string }> {
        const job = this.find(cwd, id)
        if (!job || job.state !== 'ready') return { success: false, error: 'Download is not ready' }
        const start = typeof offset === 'number' && Number.isSafeInteger(offset) && offset >= 0 ? offset : -1
        const want = typeof length === 'number' && Number.isSafeInteger(length) && length > 0
            ? Math.min(length, MAX_ARCHIVE_READ_BYTES)
            : MAX_ARCHIVE_READ_BYTES
        if (start < 0 || start > job.size) return { success: false, error: 'Invalid offset' }
        const handle = await open(job.file, 'r')
        try {
            const buffer = Buffer.alloc(Math.min(want, job.size - start))
            const { bytesRead } = await handle.read(buffer, 0, buffer.length, start)
            return { success: true, content: buffer.subarray(0, bytesRead).toString('base64') }
        } finally {
            await handle.close()
        }
    }

    async release(cwd: string, id: unknown): Promise<{ success: boolean }> {
        const job = this.find(cwd, id)
        if (job) {
            this.jobs.delete(job.id)
            await rm(job.file, { force: true }).catch(() => {})
        }
        return { success: true }
    }

    private find(cwd: string, id: unknown): ArchiveJob | undefined {
        if (typeof id !== 'string' || !ARCHIVE_ID_PATTERN.test(id)) return undefined
        const job = this.jobs.get(id)
        // An archive is only readable through the session workspace it came from.
        return job && job.cwd === cwd ? job : undefined
    }

    /** Drop archives nobody fetched within the TTL, including leftovers from a previous runner. */
    private async sweep(): Promise<void> {
        const now = Date.now()
        for (const job of [...this.jobs.values()]) {
            if (now - job.createdAt > ARCHIVE_TTL_MS && job.state !== 'building') {
                this.jobs.delete(job.id)
                await rm(job.file, { force: true }).catch(() => {})
            }
        }
        const files = await readdir(this.dir).catch(() => [] as string[])
        for (const file of files) {
            if (this.jobs.has(file.replace(/\.zip$/, ''))) continue
            const path = join(this.dir, file)
            const stats = await stat(path).catch(() => null)
            if (stats && now - stats.mtimeMs > ARCHIVE_TTL_MS) {
                await rm(path, { force: true }).catch(() => {})
            }
        }
    }
}
