import { useCallback, useEffect, useMemo, useRef, useState, type Dispatch, type DragEvent as ReactDragEvent, type ReactNode, type SetStateAction } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import type { ApiClient } from '@/api/client'
import { ConfirmDialog } from '@/components/ui/ConfirmDialog'
import { queryKeys } from '@/lib/query-keys'
import { FileIcon } from '@/components/FileIcon'
import { useSessionDirectory } from '@/hooks/queries/useSessionDirectory'
import { useFileMenuTrigger } from '@/hooks/useFileMenuTrigger'
import type { AnchoredMenuPoint } from '@/hooks/useAnchoredMenu'
import { formatDirectoryError } from '@/lib/files-i18n'
import { useTranslation } from '@/lib/use-translation'
import { useToast } from '@/lib/toast-context'
import { downloadBase64File, downloadBlob } from '@/lib/file-download'
import { formatFileMetadata } from '@/lib/file-metadata'
import { sortDirectoryEntries, type DirectorySort } from '@/lib/directory-sort'
import { randomId } from '@/lib/randomId'

function ChevronIcon(props: { className?: string; collapsed: boolean }) {
    return (
        <svg
            xmlns="http://www.w3.org/2000/svg"
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            className={`${props.className ?? ''} transition-transform duration-200 ${props.collapsed ? '' : 'rotate-90'}`}
        >
            <polyline points="9 18 15 12 9 6" />
        </svg>
    )
}

function FolderIcon(props: { className?: string }) {
    return (
        <svg
            xmlns="http://www.w3.org/2000/svg"
            width="22"
            height="22"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.6"
            strokeLinecap="round"
            strokeLinejoin="round"
            className={props.className}
        >
            <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
        </svg>
    )
}

function DownloadIcon(props: { className?: string }) {
    return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={props.className}>
            <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
            <polyline points="7 10 12 15 17 10" />
            <line x1="12" y1="15" x2="12" y2="3" />
        </svg>
    )
}

function PlusCircleIcon(props: { className?: string }) {
    return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={props.className}>
            <circle cx="12" cy="12" r="9" />
            <line x1="12" y1="8" x2="12" y2="16" />
            <line x1="8" y1="12" x2="16" y2="12" />
        </svg>
    )
}

const ADD_TO_COMPOSER_CLASS = 'flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-[var(--app-hint)] transition-colors hover:bg-[var(--app-secondary-bg)] hover:text-[var(--app-link)]'

function AddToComposerButton(props: { name: string; onClick: () => void; className?: string }) {
    const { t } = useTranslation()
    return (
        <button
            type="button"
            onClick={props.onClick}
            className={`${ADD_TO_COMPOSER_CLASS} ${props.className ?? ''}`}
            title={t('file.menu.addToComposer')}
            aria-label={t('files.directories.addToComposerNamed', { name: props.name })}
        >
            <PlusCircleIcon className="h-4 w-4" />
        </button>
    )
}

function FolderPlusIcon(props: { className?: string }) {
    return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={props.className}>
            <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
            <line x1="12" y1="11" x2="12" y2="16" />
            <line x1="9.5" y1="13.5" x2="14.5" y2="13.5" />
        </svg>
    )
}

function UploadIcon(props: { className?: string }) {
    return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={props.className}>
            <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
            <polyline points="17 8 12 3 7 8" />
            <line x1="12" y1="3" x2="12" y2="15" />
        </svg>
    )
}

function TrashIcon(props: { className?: string }) {
    return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={props.className}>
            <polyline points="3 6 5 6 21 6" />
            <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
            <path d="M10 11v6" />
            <path d="M14 11v6" />
            <path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2" />
        </svg>
    )
}

const ROW_ACTION_CLASS = 'flex h-9 w-9 shrink-0 items-center justify-center rounded-full text-[var(--app-hint)] transition-colors hover:bg-[var(--app-secondary-bg)] hover:text-[var(--app-fg)] disabled:cursor-wait disabled:opacity-50'
const ROW_DELETE_CLASS = 'flex h-9 w-9 shrink-0 items-center justify-center rounded-full text-[var(--app-hint)] transition-colors hover:bg-red-500/10 hover:text-red-600 disabled:cursor-wait disabled:opacity-50'

function parentDirectory(path: string): string {
    const index = path.lastIndexOf('/')
    return index === -1 ? '' : path.slice(0, index)
}

/**
 * Where a new entry goes inside a folder: the top of the folder (`null`), or
 * right before/after an existing row. Only affects where in-flight uploads
 * are drawn; the runner listing decides the final order.
 */
type InsertPosition = { anchor: string; side: 'before' | 'after' } | null

type DropTarget = { dir: string; position: InsertPosition }

type PendingUpload = {
    id: string
    dir: string
    name: string
    size: number
    loaded: number
    position: InsertPosition
}

type WorkspaceFileActions = {
    uploadTo: (dir: string, label: string) => void
    uploadFilesTo: (files: File[], dir: string, label: string, position: InsertPosition) => void
    requestDelete: (path: string, kind: 'file' | 'directory') => void
    createFolderIn: (dir: string, existingNames: readonly string[]) => void
    /** Path whose name is being edited inline (rename / freshly created folder). */
    editingPath: string | null
    startRename: (path: string) => void
    commitRename: (path: string, newName: string) => Promise<boolean>
    cancelRename: () => void
    /** Files being uploaded, shown in their target folder until they land. */
    pendingUploads: PendingUpload[]
    /** Insertion marker while files are dragged over the tree. */
    dropTarget: DropTarget | null
    /** Zip the folder on the runner and download it. */
    downloadFolder: (dir: string, label: string) => void
    /** Folder download in progress, with its phase and percent. */
    folderDownload: FolderDownloadProgress | null
}

type FolderDownloadProgress = { dir: string; phase: 'packing' | 'downloading'; percent: number }

const ARCHIVE_POLL_MS = 500
const ARCHIVE_CHUNK_BYTES = 4 * 1024 * 1024

type PendingConfirm = {
    title: string
    description: string
    confirmLabel: string
    confirmingLabel: string
    onConfirm: () => Promise<void>
    onCancel?: () => void
}

/** Smaller than the attachment chunk so the per-row progress bar moves smoothly. */
const WORKSPACE_UPLOAD_CHUNK_BYTES = 1024 * 1024

function joinPath(dir: string, name: string): string {
    return dir ? `${dir}/${name}` : name
}

function baseName(path: string): string {
    const index = path.lastIndexOf('/')
    return index === -1 ? path : path.slice(index + 1)
}

/** "New folder", then "New folder 2", "New folder 3", … avoiding existing names. */
export function uniqueFolderName(base: string, existingNames: readonly string[]): string {
    const taken = new Set(existingNames.map((name) => name.toLowerCase()))
    if (!taken.has(base.toLowerCase())) return base
    for (let n = 2; n < 1000; n += 1) {
        const candidate = `${base} ${n}`
        if (!taken.has(candidate.toLowerCase())) return candidate
    }
    return `${base} ${Date.now()}`
}

/**
 * Upload/delete/create/rename against the session workspace on the runner's
 * machine. Picked or dropped files appear in their target folder straight away
 * with a progress bar; each lands as a normal row once the listing refreshes,
 * or disappears on failure. Uploads run one at a time so a name clash prompt
 * is never interleaved. Deletes always ask first.
 */
function useWorkspaceFileActions(props: {
    api: ApiClient | null
    sessionId: string
    onExpand: (path: string) => void
    onPathRenamed: (from: string, to: string) => void
}): {
    actions: WorkspaceFileActions
    elements: ReactNode
    setDropTarget: Dispatch<SetStateAction<DropTarget | null>>
} {
    const { t } = useTranslation()
    const toast = useToast()
    const queryClient = useQueryClient()
    const inputRef = useRef<HTMLInputElement>(null)
    const targetRef = useRef<{ dir: string; label: string } | null>(null)
    const queueRef = useRef<Promise<void>>(Promise.resolve())
    const [pendingUploads, setPendingUploads] = useState<PendingUpload[]>([])
    const [dropTarget, setDropTarget] = useState<DropTarget | null>(null)
    const [editingPath, setEditingPath] = useState<string | null>(null)
    const [folderDownload, setFolderDownload] = useState<FolderDownloadProgress | null>(null)
    const [confirm, setConfirm] = useState<PendingConfirm | null>(null)
    const [confirmPending, setConfirmPending] = useState(false)

    const refreshDirectory = useCallback(async (dir: string) => {
        void queryClient.invalidateQueries({ queryKey: queryKeys.gitStatus(props.sessionId) })
        await queryClient.invalidateQueries({ queryKey: queryKeys.sessionDirectory(props.sessionId, dir) })
    }, [props.sessionId, queryClient])

    const updatePending = useCallback((id: string, loaded: number) => {
        setPendingUploads((prev) => prev.map((item) => item.id === id ? { ...item, loaded } : item))
    }, [])
    const removePending = useCallback((id: string) => {
        setPendingUploads((prev) => prev.filter((item) => item.id !== id))
    }, [])

    // Toasts here only inform: clicking one must not navigate to the chat
    // (that would close the files panel), so no session/url target.
    const notify = useCallback((title: string, body: string) => {
        toast.addToast({ title, body, sessionId: '', url: '' })
    }, [toast])

    const askToReplace = useCallback((name: string, dirLabel: string) => new Promise<boolean>((resolve) => {
        let settled = false
        const settle = (value: boolean) => {
            if (settled) return
            settled = true
            resolve(value)
        }
        setConfirm({
            title: t('files.directories.overwrite.title'),
            description: t('files.directories.overwrite.description', { name, dir: dirLabel }),
            confirmLabel: t('files.directories.overwrite.confirm'),
            confirmingLabel: t('files.directories.overwrite.confirming'),
            onConfirm: async () => settle(true),
            onCancel: () => settle(false),
        })
    }), [t])

    const uploadOne = useCallback(async (
        file: File,
        dir: string,
        overwrite: boolean,
        onProgress: (loaded: number) => void,
    ): Promise<'ok' | 'exists'> => {
        const api = props.api
        if (!api) throw new Error('Not connected')
        const uploadId = randomId()
        let offset = 0
        do {
            const end = Math.min(offset + WORKSPACE_UPLOAD_CHUNK_BYTES, file.size)
            const result = await api.uploadWorkspaceFileChunk(props.sessionId, {
                dir,
                name: file.name,
                uploadId,
                offset,
                final: end >= file.size,
                overwrite,
            }, file.slice(offset, end))
            if (!result.success) {
                if (result.code === 'exists') return 'exists'
                if (offset > 0) {
                    void api.abortWorkspaceUpload(props.sessionId, { dir, name: file.name, uploadId }).catch(() => {})
                }
                throw new Error(result.error ?? t('files.directories.upload.error.title'))
            }
            offset = end
            onProgress(offset)
        } while (offset < file.size)
        return 'ok'
    }, [props.api, props.sessionId, t])

    const runUploads = useCallback(async (
        queued: Array<{ file: File; pending: PendingUpload }>,
        dir: string,
        label: string,
    ) => {
        const uploaded: string[] = []
        try {
            for (const { file, pending } of queued) {
                const onProgress = (loaded: number) => updatePending(pending.id, loaded)
                try {
                    let outcome = await uploadOne(file, dir, false, onProgress)
                    if (outcome === 'exists') {
                        if (!await askToReplace(file.name, label)) {
                            removePending(pending.id)
                            continue
                        }
                        outcome = await uploadOne(file, dir, true, onProgress)
                    }
                    // Keep the progress row until the refreshed listing shows
                    // the real file, so the row never blinks out and back.
                    await refreshDirectory(dir).catch(() => {})
                    removePending(pending.id)
                    uploaded.push(file.name)
                } catch (error) {
                    removePending(pending.id)
                    notify(
                        t('files.directories.upload.failedNamed', { name: file.name }),
                        error instanceof Error ? error.message : String(error),
                    )
                }
            }
        } finally {
            setPendingUploads((prev) => prev.filter((item) => !queued.some((q) => q.pending.id === item.id)))
        }

        if (uploaded.length > 0) {
            notify(
                t('files.directories.upload.successTitle'),
                uploaded.length === 1
                    ? t('files.directories.upload.doneNamed', { name: uploaded[0], dir: label })
                    : t('files.directories.upload.done', { count: uploaded.length, dir: label }),
            )
        }
    }, [askToReplace, notify, refreshDirectory, removePending, t, updatePending, uploadOne])

    const uploadFilesTo = useCallback((files: File[], dir: string, label: string, position: InsertPosition) => {
        if (files.length === 0) return
        props.onExpand(dir)
        const queued = files.map((file) => ({
            file,
            pending: { id: randomId(), dir, name: file.name, size: file.size, loaded: 0, position },
        }))
        // Show every file right away; the uploads themselves run in order.
        setPendingUploads((prev) => [...prev, ...queued.map((item) => item.pending)])
        queueRef.current = queueRef.current
            .then(() => runUploads(queued, dir, label))
            .catch(() => {})
    }, [props, runUploads])

    const uploadTo = useCallback((dir: string, label: string) => {
        targetRef.current = { dir, label }
        inputRef.current?.click()
    }, [])

    const requestDelete = useCallback((path: string, kind: 'file' | 'directory') => {
        setConfirm({
            title: t(kind === 'directory' ? 'files.directories.delete.folderTitle' : 'files.directories.delete.fileTitle'),
            description: t(
                kind === 'directory' ? 'files.directories.delete.folderDescription' : 'files.directories.delete.fileDescription',
                { path }
            ),
            confirmLabel: t('files.directories.delete.confirm'),
            confirmingLabel: t('files.directories.delete.confirming'),
            onConfirm: async () => {
                if (!props.api) throw new Error('Not connected')
                const result = await props.api.deleteWorkspacePath(props.sessionId, path)
                if (!result.success) {
                    throw new Error(result.error ?? t('files.directories.delete.error.default'))
                }
                void refreshDirectory(parentDirectory(path))
                if (kind === 'directory') {
                    queryClient.removeQueries({ queryKey: queryKeys.sessionDirectory(props.sessionId, path) })
                }
            },
        })
    }, [props.api, props.sessionId, queryClient, refreshDirectory, t])

    const createFolderIn = useCallback((dir: string, existingNames: readonly string[]) => {
        const api = props.api
        if (!api) return
        void (async () => {
            const taken = [...existingNames]
            for (let attempt = 0; attempt < 3; attempt += 1) {
                const name = uniqueFolderName(t('files.directories.newFolderName'), taken)
                const result = await api.createWorkspaceDirectory(props.sessionId, dir, name).catch((error: unknown) => ({
                    success: false as const,
                    error: error instanceof Error ? error.message : String(error),
                    code: undefined,
                    path: undefined,
                }))
                if (result.success) {
                    props.onExpand(dir)
                    await refreshDirectory(dir).catch(() => {})
                    setEditingPath(result.path ?? joinPath(dir, name))
                    return
                }
                if (result.code !== 'exists') {
                    notify(t('files.directories.newFolder.error'), result.error ?? '')
                    return
                }
                taken.push(name)
            }
            notify(t('files.directories.newFolder.error'), '')
        })()
    }, [notify, props, refreshDirectory, t])

    const downloadFolder = useCallback((dir: string, label: string) => {
        const api = props.api
        if (!api || folderDownload) return
        setFolderDownload({ dir, phase: 'packing', percent: 0 })
        void (async () => {
            let archiveId: string | undefined
            try {
                const started = await api.startWorkspaceArchive(props.sessionId, dir)
                if (!started.success || !started.archiveId) {
                    throw new Error(started.error ?? t('files.directories.downloadFolder.error'))
                }
                archiveId = started.archiveId
                const fileName = started.fileName ?? `${label}.zip`
                let status = started
                while (status.state !== 'ready') {
                    if (!status.success || status.state === 'error') {
                        throw new Error(status.error ?? t('files.directories.downloadFolder.error'))
                    }
                    const total = status.totalBytes ?? 0
                    setFolderDownload({
                        dir,
                        phase: 'packing',
                        percent: total > 0 ? Math.min(99, Math.floor(((status.doneBytes ?? 0) / total) * 100)) : 0,
                    })
                    await new Promise((resolve) => setTimeout(resolve, ARCHIVE_POLL_MS))
                    status = await api.getWorkspaceArchiveStatus(props.sessionId, archiveId)
                }
                const size = status.size ?? 0
                const parts: ArrayBuffer[] = []
                let offset = 0
                setFolderDownload({ dir, phase: 'downloading', percent: 0 })
                while (offset < size) {
                    const chunk = await api.readWorkspaceArchiveChunk(props.sessionId, archiveId, offset, ARCHIVE_CHUNK_BYTES)
                    if (chunk.byteLength === 0) throw new Error(t('files.directories.downloadFolder.error'))
                    parts.push(chunk)
                    offset += chunk.byteLength
                    setFolderDownload({ dir, phase: 'downloading', percent: Math.floor((offset / size) * 100) })
                }
                downloadBlob(fileName, new Blob(parts, { type: 'application/zip' }))
            } catch (error) {
                notify(
                    t('files.directories.downloadFolder.failedNamed', { name: label }),
                    error instanceof Error ? error.message : String(error),
                )
            } finally {
                if (archiveId) void api.releaseWorkspaceArchive(props.sessionId, archiveId).catch(() => {})
                setFolderDownload(null)
            }
        })()
    }, [folderDownload, notify, props.api, props.sessionId, t])

    const startRename = useCallback((path: string) => setEditingPath(path), [])
    const cancelRename = useCallback(() => setEditingPath(null), [])

    const commitRename = useCallback(async (path: string, newName: string): Promise<boolean> => {
        const name = newName.trim()
        if (!name || name === baseName(path)) {
            setEditingPath(null)
            return true
        }
        if (!props.api) return false
        try {
            const result = await props.api.renameWorkspacePath(props.sessionId, path, name)
            if (!result.success) {
                notify(t('files.directories.rename.error'), result.error ?? '')
                return false
            }
            const parent = parentDirectory(path)
            const renamed = result.path ?? joinPath(parent, name)
            props.onPathRenamed(path, renamed)
            queryClient.removeQueries({ queryKey: queryKeys.sessionDirectory(props.sessionId, path) })
            setEditingPath(null)
            await refreshDirectory(parent).catch(() => {})
            return true
        } catch (error) {
            notify(t('files.directories.rename.error'), error instanceof Error ? error.message : String(error))
            return false
        }
    }, [notify, props, queryClient, refreshDirectory, t])

    const closeConfirm = useCallback(() => {
        confirm?.onCancel?.()
        setConfirm(null)
    }, [confirm])

    const elements = (
        <>
            <input
                ref={inputRef}
                type="file"
                multiple
                className="hidden"
                data-testid="workspace-upload-input"
                onChange={(event) => {
                    const files = Array.from(event.target.files ?? [])
                    event.target.value = ''
                    const target = targetRef.current
                    if (target) uploadFilesTo(files, target.dir, target.label, null)
                }}
            />
            <ConfirmDialog
                isOpen={confirm !== null}
                onClose={closeConfirm}
                title={confirm?.title ?? ''}
                description={confirm?.description ?? ''}
                confirmLabel={confirm?.confirmLabel ?? ''}
                confirmingLabel={confirm?.confirmingLabel ?? ''}
                onConfirm={async () => {
                    if (!confirm) return
                    setConfirmPending(true)
                    try {
                        // The replace prompt settles once, so the close that
                        // follows a confirm cannot also report "cancelled".
                        await confirm.onConfirm()
                    } finally {
                        setConfirmPending(false)
                    }
                }}
                isPending={confirmPending}
                destructive
            />
        </>
    )

    return {
        actions: {
            uploadTo,
            uploadFilesTo,
            requestDelete,
            createFolderIn,
            editingPath,
            startRename,
            commitRename,
            cancelRename,
            pendingUploads,
            dropTarget,
            downloadFolder,
            folderDownload,
        },
        elements,
        setDropTarget,
    }
}

function PendingUploadRow(props: { upload: PendingUpload; indent: number }) {
    const { t } = useTranslation()
    const percent = props.upload.size === 0
        ? (props.upload.loaded > 0 ? 100 : 0)
        : Math.min(100, Math.round((props.upload.loaded / props.upload.size) * 100))
    return (
        <div
            className="relative flex w-full items-center gap-3 overflow-hidden px-3 py-2 text-left"
            style={{ paddingLeft: props.indent }}
            data-testid="pending-upload-row"
            aria-busy="true"
        >
            <div
                className="pointer-events-none absolute inset-y-0 left-0 bg-[var(--app-link)] opacity-15 transition-[width] duration-300 ease-out"
                style={{ width: `${percent}%` }}
                role="progressbar"
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={percent}
                aria-label={props.upload.name}
            />
            <span className="relative h-7 w-7 shrink-0" />
            <span className="relative h-4 w-4" />
            <span className="relative opacity-60">
                <FileIcon fileName={props.upload.name} size={22} />
            </span>
            <div className="relative min-w-0 flex-1">
                <div className="truncate font-medium text-[var(--app-hint)]">{props.upload.name}</div>
                <div className="truncate text-xs text-[var(--app-link)]">
                    {t('files.directories.uploadingPercent', { percent })}
                </div>
            </div>
        </div>
    )
}

/** Insertion line shown while files are dragged over the tree. */
function DropMarker(props: { indent: number }) {
    return (
        <div className="pointer-events-none relative h-0" data-testid="drop-marker" aria-hidden="true">
            <div
                className="absolute right-3 top-[-1px] h-0.5 rounded-full bg-[var(--app-link)]"
                style={{ left: props.indent }}
            />
            <div
                className="absolute top-[-4px] h-2 w-2 rounded-full border-2 border-[var(--app-link)] bg-[var(--app-bg)]"
                style={{ left: props.indent - 4 }}
            />
        </div>
    )
}

/** Inline name editor for rename / a freshly created folder. */
function RenameInput(props: {
    initialName: string
    selectStem: boolean
    onCommit: (name: string) => Promise<boolean>
    onCancel: () => void
}) {
    const { t } = useTranslation()
    const inputRef = useRef<HTMLInputElement>(null)
    const [value, setValue] = useState(props.initialName)
    const [busy, setBusy] = useState(false)
    const doneRef = useRef(false)

    useEffect(() => {
        const input = inputRef.current
        if (!input) return
        input.focus()
        const dot = props.initialName.lastIndexOf('.')
        input.setSelectionRange(0, props.selectStem && dot > 0 ? dot : props.initialName.length)
    }, [props.initialName, props.selectStem])

    const commit = async () => {
        if (doneRef.current || busy) return
        setBusy(true)
        const ok = await props.onCommit(value)
        setBusy(false)
        if (ok) {
            doneRef.current = true
        } else {
            inputRef.current?.focus()
        }
    }

    return (
        <input
            ref={inputRef}
            value={value}
            disabled={busy}
            onChange={(event) => setValue(event.target.value)}
            onClick={(event) => event.stopPropagation()}
            onKeyDown={(event) => {
                event.stopPropagation()
                if (event.key === 'Enter') {
                    event.preventDefault()
                    void commit()
                } else if (event.key === 'Escape') {
                    event.preventDefault()
                    doneRef.current = true
                    props.onCancel()
                }
            }}
            onBlur={() => void commit()}
            aria-label={t('files.directories.rename.label')}
            data-testid="rename-input"
            className="h-7 w-full min-w-0 rounded border border-[var(--app-link)] bg-[var(--app-bg)] px-1.5 text-sm font-medium text-[var(--app-fg)] outline-none"
            autoCapitalize="none"
            autoCorrect="off"
            spellCheck={false}
        />
    )
}

function DirectorySkeleton(props: { depth: number; rows?: number }) {
    const rows = props.rows ?? 4
    const indent = 12 + props.depth * 14

    return (
        <div className="animate-pulse">
            {Array.from({ length: rows }).map((_, index) => (
                <div
                    key={`dir-skel-${props.depth}-${index}`}
                    className="flex items-center gap-3 px-3 py-2"
                    style={{ paddingLeft: indent }}
                >
                    <div className="h-5 w-5 rounded bg-[var(--app-subtle-bg)]" />
                    <div className="h-3 w-40 rounded bg-[var(--app-subtle-bg)]" />
                </div>
            ))}
        </div>
    )
}

function DirectoryErrorRow(props: { depth: number; message: string }) {
    const indent = 12 + props.depth * 14
    return (
        <div
            className="px-3 py-2 text-xs text-[var(--app-hint)] bg-amber-500/10"
            style={{ paddingLeft: indent }}
        >
            {props.message}
        </div>
    )
}

export type FileMenuRequestHandler = (path: string, point: AnchoredMenuPoint, kind?: 'file' | 'directory') => void

function DirectoryFileRow(props: {
    fileName: string
    parentPath: string
    metadata: string | null
    indent: number
    isDownloading: boolean
    downloadDisabled: boolean
    editing?: { onCommit: (name: string) => Promise<boolean>; onCancel: () => void }
    onOpen: () => void
    onDownload: () => void
    onDelete?: () => void
    onAddToComposer?: () => void
    onOpenMenu?: (point: AnchoredMenuPoint) => void
}) {
    const { t } = useTranslation()
    const rowHandlers = useFileMenuTrigger({
        onOpen: props.onOpen,
        onOpenMenu: props.onOpenMenu,
    })

    return (
        <div
            className="flex w-full items-center gap-3 px-3 py-2 text-left hover:bg-[var(--app-subtle-bg)] transition-colors"
            style={{ paddingLeft: props.indent }}
            data-tree-row="file"
            data-parent={props.parentPath}
            data-name={props.fileName}
        >
            {props.onAddToComposer ? (
                <AddToComposerButton name={props.fileName} onClick={props.onAddToComposer} />
            ) : null}
            <span className="h-4 w-4" />
            <FileIcon fileName={props.fileName} size={22} />
            {props.editing ? (
                <div className="min-w-0 flex-1">
                    <RenameInput
                        initialName={props.fileName}
                        selectStem
                        onCommit={props.editing.onCommit}
                        onCancel={props.editing.onCancel}
                    />
                </div>
            ) : (
                <button
                    type="button"
                    {...rowHandlers}
                    className="min-w-0 flex-1 text-left"
                >
                    <div className="truncate font-medium">{props.fileName}</div>
                    {props.metadata ? <div className="truncate text-xs text-[var(--app-hint)]">{props.metadata}</div> : null}
                </button>
            )}
            <button
                type="button"
                onClick={props.onDownload}
                disabled={props.downloadDisabled}
                className={ROW_ACTION_CLASS}
                title={t('files.directories.download')}
                aria-label={t('files.directories.downloadNamed', { name: props.fileName })}
            >
                <DownloadIcon className={`h-4 w-4 ${props.isDownloading ? 'animate-pulse' : ''}`} />
            </button>
            {props.onDelete ? (
                <button
                    type="button"
                    onClick={props.onDelete}
                    className={ROW_DELETE_CLASS}
                    title={t('files.directories.delete')}
                    aria-label={t('files.directories.deleteNamed', { name: props.fileName })}
                >
                    <TrashIcon className="h-4 w-4" />
                </button>
            ) : null}
        </div>
    )
}

function FolderRowLabel(props: {
    label: string
    isExpanded: boolean
    onToggle: () => void
    onOpenMenu?: (point: AnchoredMenuPoint) => void
}) {
    const rowHandlers = useFileMenuTrigger({ onOpen: props.onToggle, onOpenMenu: props.onOpenMenu })
    return (
        <button
            type="button"
            {...rowHandlers}
            className="flex min-w-0 flex-1 items-center gap-3 py-2 pr-2 text-left"
            aria-expanded={props.isExpanded}
        >
            <ChevronIcon collapsed={!props.isExpanded} className="shrink-0 text-[var(--app-hint)]" />
            <FolderIcon className="shrink-0 text-[var(--app-link)]" />
            <div className="min-w-0 flex-1">
                <div className="truncate font-medium">{props.label}</div>
            </div>
        </button>
    )
}

function DirectoryNode(props: {
    api: ApiClient | null
    sessionId: string
    path: string
    label: string
    depth: number
    onOpenFile: (path: string) => void
    onRequestFileMenu?: FileMenuRequestHandler
    expanded: Set<string>
    onToggle: (path: string) => void
    sort: DirectorySort
    actions?: WorkspaceFileActions
    onAddToComposer?: (path: string) => void
}) {
    const { t, locale } = useTranslation()
    const toast = useToast()
    const [downloadingPath, setDownloadingPath] = useState<string | null>(null)
    const isExpanded = props.expanded.has(props.path)
    const { entries, error, isLoading } = useSessionDirectory(props.api, props.sessionId, props.path, {
        enabled: isExpanded
    })

    const sortedEntries = useMemo(
        () => sortDirectoryEntries(entries, props.sort, locale),
        [entries, locale, props.sort],
    )
    const directories = useMemo(() => sortedEntries.filter((entry) => entry.type === 'directory'), [sortedEntries])
    const files = useMemo(() => sortedEntries.filter((entry) => entry.type === 'file'), [sortedEntries])
    const pendingHere = useMemo(
        () => (props.actions?.pendingUploads ?? []).filter((upload) => upload.dir === props.path),
        [props.actions?.pendingUploads, props.path],
    )
    // A replacing upload stands in for the existing row until it lands.
    const visibleFiles = useMemo(() => {
        if (pendingHere.length === 0) return files
        const pendingNames = new Set(pendingHere.map((upload) => upload.name))
        return files.filter((entry) => !pendingNames.has(entry.name))
    }, [files, pendingHere])
    const childDepth = props.depth + 1

    const indent = 12 + props.depth * 14
    const childIndent = 12 + childDepth * 14

    const dropTarget = props.actions?.dropTarget ?? null
    const dropHere = dropTarget !== null && dropTarget.dir === props.path ? dropTarget.position : undefined
    const isDropFolder = dropTarget !== null && dropTarget.dir === props.path && dropTarget.position === null
    const editingPath = props.actions?.editingPath ?? null
    const folderDownloadHere = props.actions?.folderDownload?.dir === props.path ? props.actions.folderDownload : null

    const handleDownload = async (filePath: string, fileName: string) => {
        if (!props.api || downloadingPath) return
        setDownloadingPath(filePath)
        try {
            const result = await props.api.readSessionFile(props.sessionId, filePath)
            if (!result.success || result.content === undefined) {
                throw new Error(result.error ?? t('files.directories.download.error.default'))
            }
            downloadBase64File(fileName, result.content)
        } catch (error) {
            toast.addToast({
                title: t('files.directories.download.error.title'),
                body: error instanceof Error ? error.message : t('files.directories.download.error.default'),
                sessionId: props.sessionId,
                url: `/sessions/${props.sessionId}/files?tab=directories`,
            })
        } finally {
            setDownloadingPath(null)
        }
    }

    // Uploads and the drop marker anchored at a given row/side.
    const renderAnchored = (anchor: string | null, side: 'before' | 'after') => {
        const matches = (position: InsertPosition) => anchor === null
            ? position === null
            : position !== null && position.anchor === anchor && position.side === side
        return (
            <>
                {side === 'after' && dropHere !== undefined && matches(dropHere) ? <DropMarker indent={childIndent} /> : null}
                {pendingHere.filter((upload) => matches(upload.position)).map((upload) => (
                    <PendingUploadRow key={upload.id} upload={upload} indent={childIndent} />
                ))}
                {side === 'before' && dropHere !== undefined && matches(dropHere) ? <DropMarker indent={childIndent} /> : null}
            </>
        )
    }
    const childNames = useMemo(
        () => new Set([...directories, ...visibleFiles].map((entry) => entry.name)),
        [directories, visibleFiles],
    )
    // Uploads anchored to a row that has since disappeared fall back to the top.
    const orphanPending = pendingHere.filter((upload) => upload.position !== null && !childNames.has(upload.position.anchor))
    const isRoot = props.depth === 0

    return (
        <div data-tree-dir={props.path}>
            <div
                className={`flex w-full items-center gap-1 pr-3 transition-colors ${isDropFolder ? 'bg-[var(--app-link)]/10 ring-1 ring-inset ring-[var(--app-link)]/40' : 'hover:bg-[var(--app-subtle-bg)]'}`}
                style={{ paddingLeft: indent }}
                data-tree-row="folder"
                data-path={props.path}
            >
                {props.onAddToComposer ? (
                    !isRoot ? (
                        <AddToComposerButton
                            name={props.label}
                            onClick={() => props.onAddToComposer!(`${props.path}/`)}
                            className="mr-2"
                        />
                    ) : (
                        // Root: keep the column aligned; the workspace itself is not a reference.
                        <span className="mr-2 h-7 w-7 shrink-0" />
                    )
                ) : null}
                {props.actions && editingPath === props.path && !isRoot ? (
                    <div className="flex min-w-0 flex-1 items-center gap-3 py-1.5 pr-2">
                        <ChevronIcon collapsed={!isExpanded} className="shrink-0 text-[var(--app-hint)]" />
                        <FolderIcon className="shrink-0 text-[var(--app-link)]" />
                        <RenameInput
                            initialName={props.label}
                            selectStem={false}
                            onCommit={(name) => props.actions!.commitRename(props.path, name)}
                            onCancel={props.actions.cancelRename}
                        />
                    </div>
                ) : (
                    <FolderRowLabel
                        label={props.label}
                        isExpanded={isExpanded}
                        onToggle={() => props.onToggle(props.path)}
                        onOpenMenu={props.onRequestFileMenu && !isRoot
                            ? (point) => props.onRequestFileMenu!(props.path, point, 'directory')
                            : undefined}
                    />
                )}
                {props.actions ? (
                    <>
                        <button
                            type="button"
                            onClick={() => props.actions!.createFolderIn(props.path, [...childNames])}
                            className={ROW_ACTION_CLASS}
                            title={t('files.directories.newFolder')}
                            aria-label={t('files.directories.newFolderIn', { name: props.label })}
                        >
                            <FolderPlusIcon className="h-4 w-4" />
                        </button>
                        {folderDownloadHere ? (
                            <span className="shrink-0 whitespace-nowrap text-xs tabular-nums text-[var(--app-link)]" role="status">
                                {t(folderDownloadHere.phase === 'packing'
                                    ? 'files.directories.downloadFolder.packing'
                                    : 'files.directories.downloadFolder.downloading', { percent: folderDownloadHere.percent })}
                            </span>
                        ) : null}
                        <button
                            type="button"
                            onClick={() => props.actions!.uploadTo(props.path, props.label)}
                            className={ROW_ACTION_CLASS}
                            title={t('files.directories.upload')}
                            aria-label={t('files.directories.uploadNamed', { name: props.label })}
                        >
                            <UploadIcon className={`h-4 w-4 ${pendingHere.length > 0 ? 'animate-pulse' : ''}`} />
                        </button>
                        <button
                            type="button"
                            onClick={() => props.actions!.downloadFolder(props.path, props.label)}
                            disabled={props.actions.folderDownload !== null}
                            className={ROW_ACTION_CLASS}
                            title={t('files.directories.downloadFolder')}
                            aria-label={t('files.directories.downloadFolderNamed', { name: props.label })}
                        >
                            <DownloadIcon className={`h-4 w-4 ${folderDownloadHere ? 'animate-pulse' : ''}`} />
                        </button>
                        {!isRoot ? (
                            <button
                                type="button"
                                onClick={() => props.actions!.requestDelete(props.path, 'directory')}
                                className={ROW_DELETE_CLASS}
                                title={t('files.directories.delete')}
                                aria-label={t('files.directories.deleteNamed', { name: props.label })}
                            >
                                <TrashIcon className="h-4 w-4" />
                            </button>
                        ) : (
                            <span className="h-9 w-9 shrink-0" />
                        )}
                    </>
                ) : null}
            </div>

            {!isExpanded && isDropFolder ? <DropMarker indent={childIndent} /> : null}

            {isExpanded ? (
                isLoading ? (
                    <>
                        {renderAnchored(null, 'before')}
                        <DirectorySkeleton depth={childDepth} />
                    </>
                ) : error ? (
                    <DirectoryErrorRow depth={childDepth} message={formatDirectoryError(error, t)} />
                ) : (
                    <div>
                        {/* Top of the folder: the drop marker, then in-flight uploads. */}
                        {isDropFolder ? <DropMarker indent={childIndent} /> : null}
                        {pendingHere.filter((upload) => upload.position === null).map((upload) => (
                            <PendingUploadRow key={upload.id} upload={upload} indent={childIndent} />
                        ))}
                        {orphanPending.map((upload) => (
                            <PendingUploadRow key={upload.id} upload={upload} indent={childIndent} />
                        ))}

                        {directories.map((entry) => {
                            const childPath = joinPath(props.path, entry.name)
                            return (
                                <DirectoryNode
                                    key={childPath}
                                    api={props.api}
                                    sessionId={props.sessionId}
                                    path={childPath}
                                    label={entry.name}
                                    depth={childDepth}
                                    onOpenFile={props.onOpenFile}
                                    onRequestFileMenu={props.onRequestFileMenu}
                                    expanded={props.expanded}
                                    onToggle={props.onToggle}
                                    sort={props.sort}
                                    actions={props.actions}
                                    onAddToComposer={props.onAddToComposer}
                                />
                            )
                        })}

                        {visibleFiles.map((entry) => {
                            const filePath = joinPath(props.path, entry.name)
                            const metadata = formatFileMetadata(entry.size, entry.modified, locale)
                            const isDownloading = downloadingPath === filePath
                            return (
                                <div key={filePath}>
                                    {renderAnchored(entry.name, 'before')}
                                    <DirectoryFileRow
                                        fileName={entry.name}
                                        parentPath={props.path}
                                        metadata={metadata}
                                        indent={childIndent}
                                        isDownloading={isDownloading}
                                        downloadDisabled={Boolean(downloadingPath)}
                                        editing={props.actions && editingPath === filePath
                                            ? {
                                                onCommit: (name) => props.actions!.commitRename(filePath, name),
                                                onCancel: props.actions.cancelRename,
                                            }
                                            : undefined}
                                        onOpen={() => props.onOpenFile(filePath)}
                                        onDownload={() => void handleDownload(filePath, entry.name)}
                                        onDelete={props.actions
                                            ? () => props.actions!.requestDelete(filePath, 'file')
                                            : undefined}
                                        onAddToComposer={props.onAddToComposer
                                            ? () => props.onAddToComposer!(filePath)
                                            : undefined}
                                        onOpenMenu={props.onRequestFileMenu
                                            ? (point) => props.onRequestFileMenu!(filePath, point, 'file')
                                            : undefined}
                                    />
                                    {renderAnchored(entry.name, 'after')}
                                </div>
                            )
                        })}

                        {directories.length === 0 && files.length === 0 && pendingHere.length === 0 ? (
                            <div
                                className="px-3 py-2 text-sm text-[var(--app-hint)]"
                                style={{ paddingLeft: childIndent }}
                            >
                                {t('files.directories.empty')}
                            </div>
                        ) : null}
                    </div>
                )
            ) : null}
        </div>
    )
}

const STORAGE_KEY_PREFIX = 'hapi-dir-expanded-'

function readExpanded(sessionId: string): Set<string> {
    try {
        const raw = sessionStorage.getItem(STORAGE_KEY_PREFIX + sessionId)
        if (raw) {
            const parsed = JSON.parse(raw)
            if (Array.isArray(parsed)) return new Set(parsed as string[])
        }
    } catch {
        // ignore
    }
    return new Set([''])
}

function writeExpanded(sessionId: string, expanded: Set<string>) {
    try {
        sessionStorage.setItem(STORAGE_KEY_PREFIX + sessionId, JSON.stringify([...expanded]))
    } catch {
        // ignore
    }
}

function hasDraggedFiles(event: ReactDragEvent): boolean {
    return Array.from(event.dataTransfer?.types ?? []).includes('Files')
}

/** Folder + insertion point under the pointer while files are dragged over the tree. */
function dropTargetFromEvent(event: ReactDragEvent): DropTarget {
    const element = event.target instanceof Element ? event.target : null
    const row = element?.closest<HTMLElement>('[data-tree-row]')
    if (row?.dataset.treeRow === 'folder') {
        return { dir: row.dataset.path ?? '', position: null }
    }
    if (row?.dataset.treeRow === 'file') {
        const rect = row.getBoundingClientRect()
        const side = event.clientY < rect.top + rect.height / 2 ? 'before' : 'after'
        return { dir: row.dataset.parent ?? '', position: { anchor: row.dataset.name ?? '', side } }
    }
    const container = element?.closest<HTMLElement>('[data-tree-dir]')
    return { dir: container?.dataset.treeDir ?? '', position: null }
}

function sameDropTarget(a: DropTarget | null, b: DropTarget | null): boolean {
    if (a === null || b === null) return a === b
    if (a.dir !== b.dir) return false
    if (a.position === null || b.position === null) return a.position === b.position
    return a.position.anchor === b.position.anchor && a.position.side === b.position.side
}

/** Hovering a collapsed folder this long while dragging opens it. */
const DRAG_EXPAND_DELAY_MS = 700

export function DirectoryTree(props: {
    api: ApiClient | null
    sessionId: string
    rootLabel: string
    onOpenFile: (path: string) => void
    onRequestFileMenu?: FileMenuRequestHandler
    sort: DirectorySort
    /** Show upload/delete/create/rename actions (default true). */
    allowFileManagement?: boolean
    /** Insert a workspace-relative path (folders end in `/`) into the composer. */
    onAddToComposer?: (path: string) => void
    /** Ask the tree to start renaming this path inline (e.g. from the context menu). */
    renameRequest?: { path: string; nonce: number } | null
}) {
    const [expanded, setExpanded] = useState<Set<string>>(() => readExpanded(props.sessionId))

    useEffect(() => {
        writeExpanded(props.sessionId, expanded)
    }, [props.sessionId, expanded])

    const handleToggle = useCallback((path: string) => {
        setExpanded((prev) => {
            const next = new Set(prev)
            if (next.has(path)) {
                next.delete(path)
            } else {
                next.add(path)
            }
            return next
        })
    }, [])

    const expand = useCallback((path: string) => {
        setExpanded((prev) => {
            if (prev.has(path)) return prev
            const next = new Set(prev)
            next.add(path)
            return next
        })
    }, [])

    // Keep a renamed folder (and anything open beneath it) expanded.
    const handlePathRenamed = useCallback((from: string, to: string) => {
        setExpanded((prev) => {
            let changed = false
            const next = new Set<string>()
            for (const path of prev) {
                if (path === from || path.startsWith(`${from}/`)) {
                    next.add(to + path.slice(from.length))
                    changed = true
                } else {
                    next.add(path)
                }
            }
            return changed ? next : prev
        })
    }, [])

    const fileActions = useWorkspaceFileActions({
        api: props.api,
        sessionId: props.sessionId,
        onExpand: expand,
        onPathRenamed: handlePathRenamed,
    })
    const actions = props.allowFileManagement === false ? undefined : fileActions.actions
    const { setDropTarget } = fileActions
    const { startRename } = fileActions.actions

    useEffect(() => {
        if (props.renameRequest) startRename(props.renameRequest.path)
    }, [props.renameRequest, startRename])

    const containerRef = useRef<HTMLDivElement>(null)
    const expandTimerRef = useRef<{ path: string; timer: ReturnType<typeof setTimeout> } | null>(null)
    const clearExpandTimer = useCallback(() => {
        if (expandTimerRef.current) clearTimeout(expandTimerRef.current.timer)
        expandTimerRef.current = null
    }, [])
    useEffect(() => clearExpandTimer, [clearExpandTimer])

    const labelFor = useCallback((dir: string) => dir ? baseName(dir) : props.rootLabel, [props.rootLabel])

    const dragHandlers = actions ? {
        onDragOver: (event: ReactDragEvent<HTMLDivElement>) => {
            if (!hasDraggedFiles(event)) return
            event.preventDefault()
            event.dataTransfer.dropEffect = 'copy'
            const target = dropTargetFromEvent(event)
            setDropTarget((prev) => sameDropTarget(prev, target) ? prev : target)
            const folder = target.position === null ? target.dir : null
            if (folder !== null && !expanded.has(folder)) {
                if (expandTimerRef.current?.path !== folder) {
                    clearExpandTimer()
                    expandTimerRef.current = {
                        path: folder,
                        timer: setTimeout(() => {
                            expand(folder)
                            expandTimerRef.current = null
                        }, DRAG_EXPAND_DELAY_MS),
                    }
                }
            } else {
                clearExpandTimer()
            }
        },
        onDragLeave: (event: ReactDragEvent<HTMLDivElement>) => {
            const next = event.relatedTarget
            if (next instanceof Node && containerRef.current?.contains(next)) return
            clearExpandTimer()
            setDropTarget(null)
        },
        onDrop: (event: ReactDragEvent<HTMLDivElement>) => {
            if (!hasDraggedFiles(event)) return
            event.preventDefault()
            clearExpandTimer()
            const target = dropTargetFromEvent(event)
            setDropTarget(null)
            const items = Array.from(event.dataTransfer.items ?? [])
            const files = items.length > 0
                ? items
                    .filter((item) => item.kind === 'file' && !item.webkitGetAsEntry?.()?.isDirectory)
                    .map((item) => item.getAsFile())
                    .filter((file): file is File => file !== null)
                : Array.from(event.dataTransfer.files ?? [])
            actions.uploadFilesTo(files, target.dir, labelFor(target.dir), target.position)
        },
    } : {}

    return (
        <div ref={containerRef} className="border-t border-[var(--app-divider)]" {...dragHandlers}>
            <DirectoryNode
                api={props.api}
                sessionId={props.sessionId}
                path=""
                label={props.rootLabel}
                depth={0}
                onOpenFile={props.onOpenFile}
                onRequestFileMenu={props.onRequestFileMenu}
                expanded={expanded}
                onToggle={handleToggle}
                sort={props.sort}
                actions={actions}
                onAddToComposer={props.onAddToComposer}
            />
            {fileActions.elements}
        </div>
    )
}
