import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { I18nProvider } from '@/lib/i18n-context'
import { ToastProvider } from '@/lib/toast-context'
import { ToastContainer } from '@/components/ToastContainer'
import { DEFAULT_DIRECTORY_SORT } from '@/lib/directory-sort'
import { DirectoryTree } from '@/components/SessionFiles/DirectoryTree'

const mocks = vi.hoisted(() => ({
    entries: [
        { name: 'README.md', type: 'file' as const, size: 12, modified: 1_784_175_060_000 },
        { name: 'src', type: 'directory' as const },
    ] as Array<{ name: string; type: 'file' | 'directory'; size?: number; modified?: number }>,
}))

vi.mock('@/hooks/queries/useSessionDirectory', () => ({
    useSessionDirectory: () => ({
        entries: mocks.entries,
        error: null,
        isLoading: false,
        refetch: vi.fn(),
    }),
}))

function renderTree(handlers: {
    onOpenFile?: () => void
    onRequestFileMenu?: (path: string, point: { x: number; y: number }) => void
    api?: unknown
    onAddToComposer?: (path: string) => void
    renameRequest?: { path: string; nonce: number } | null
} = {}) {
    const onOpenFile = handlers.onOpenFile ?? vi.fn()
    const onRequestFileMenu = handlers.onRequestFileMenu ?? vi.fn()

    render(
        <QueryClientProvider client={new QueryClient()}>
        <I18nProvider>
            <ToastProvider>
                <DirectoryTree
                    api={(handlers.api ?? {}) as never}
                    sessionId="session-1"
                    rootLabel="project"
                    onOpenFile={onOpenFile}
                    onRequestFileMenu={onRequestFileMenu}
                    onAddToComposer={handlers.onAddToComposer}
                    renameRequest={handlers.renameRequest}
                    sort={DEFAULT_DIRECTORY_SORT}
                />
                <ToastContainer />
            </ToastProvider>
        </I18nProvider>
        </QueryClientProvider>
    )

    return { onOpenFile, onRequestFileMenu }
}

/** The file name button (a directory row's download button also mentions the name in its aria-label). */
function fileRow(): HTMLButtonElement {
    return screen.getByText('README.md').closest('button') as HTMLButtonElement
}

beforeEach(() => {
    vi.clearAllMocks()
    window.sessionStorage.clear()
})

afterEach(() => cleanup())

describe('DirectoryTree file context menu', () => {
    it('requests the menu on right-click with the entry path and pointer', () => {
        const { onRequestFileMenu } = renderTree()

        fireEvent.contextMenu(fileRow(), { clientX: 321, clientY: 123 })

        expect(onRequestFileMenu).toHaveBeenCalledWith('README.md', { x: 321, y: 123 }, 'file')
    })

    it('requests the menu on touch long-press and does not open the file on release', () => {
        vi.useFakeTimers()
        try {
            const { onOpenFile, onRequestFileMenu } = renderTree()

            fireEvent.touchStart(fileRow(), { touches: [{ clientX: 44, clientY: 55 }] })
            act(() => {
                vi.advanceTimersByTime(600)
            })

            expect(onRequestFileMenu).toHaveBeenCalledWith('README.md', { x: 44, y: 55 }, 'file')
            expect(onOpenFile).not.toHaveBeenCalled()

            fireEvent.touchEnd(fileRow(), { changedTouches: [{ clientX: 44, clientY: 55 }] })
            expect(onOpenFile).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })
})

describe('DirectoryTree upload and delete', () => {
    it('asks before deleting a file and calls the runner delete on confirm', async () => {
        const deleteWorkspacePath = vi.fn(async () => ({ success: true }))
        renderTree({ api: { deleteWorkspacePath } })

        fireEvent.click(screen.getByRole('button', { name: 'Delete README.md' }))
        expect(deleteWorkspacePath).not.toHaveBeenCalled()
        expect(await screen.findByText('Delete file?')).toBeTruthy()

        fireEvent.click(screen.getByRole('button', { name: 'Delete' }))
        await waitFor(() => expect(deleteWorkspacePath).toHaveBeenCalledWith('session-1', 'README.md'))
    })

    it('does not delete when the confirmation is cancelled', async () => {
        const deleteWorkspacePath = vi.fn(async () => ({ success: true }))
        renderTree({ api: { deleteWorkspacePath } })

        fireEvent.click(screen.getByRole('button', { name: 'Delete src' }))
        expect(await screen.findByText('Delete folder?')).toBeTruthy()
        fireEvent.click(screen.getByRole('button', { name: 'Cancel' }))

        await waitFor(() => expect(screen.queryByText('Delete folder?')).toBeNull())
        expect(deleteWorkspacePath).not.toHaveBeenCalled()
    })

    it('offers upload on folders (including the root) but delete only below the root', () => {
        renderTree()
        expect(screen.getByRole('button', { name: 'Upload files to project' })).toBeTruthy()
        expect(screen.getByRole('button', { name: 'Upload files to src' })).toBeTruthy()
        expect(screen.queryByRole('button', { name: 'Delete project' })).toBeNull()
    })

    it('uploads into the clicked folder and asks before replacing an existing file', async () => {
        const uploadWorkspaceFileChunk = vi.fn()
            .mockResolvedValueOnce({ success: false, code: 'exists', error: 'exists' })
            .mockResolvedValueOnce({ success: true, size: 5, path: 'src/a.txt' })
        renderTree({ api: { uploadWorkspaceFileChunk } })

        fireEvent.click(screen.getByRole('button', { name: 'Upload files to src' }))
        const input = screen.getByTestId('workspace-upload-input') as HTMLInputElement
        fireEvent.change(input, { target: { files: [new File(['hello'], 'a.txt')] } })

        expect(await screen.findByText('Replace existing file?')).toBeTruthy()
        expect(uploadWorkspaceFileChunk).toHaveBeenCalledTimes(1)
        expect(uploadWorkspaceFileChunk.mock.calls[0][1]).toMatchObject({ dir: 'src', name: 'a.txt', overwrite: false, final: true })

        fireEvent.click(screen.getByRole('button', { name: 'Replace' }))
        await waitFor(() => expect(uploadWorkspaceFileChunk).toHaveBeenCalledTimes(2))
        expect(uploadWorkspaceFileChunk.mock.calls[1][1]).toMatchObject({ dir: 'src', name: 'a.txt', overwrite: true })
    })
})

describe('DirectoryTree add to composer', () => {
    it('adds files by path and folders with a trailing slash, but not the root', () => {
        const onAddToComposer = vi.fn()
        renderTree({ onAddToComposer })

        fireEvent.click(screen.getByRole('button', { name: 'Add README.md to composer' }))
        fireEvent.click(screen.getByRole('button', { name: 'Add src to composer' }))

        expect(onAddToComposer.mock.calls).toEqual([['README.md'], ['src/']])
        expect(screen.queryByRole('button', { name: 'Add project to composer' })).toBeNull()
    })
})

describe('DirectoryTree upload progress', () => {
    function deferred<T>() {
        let resolve!: (value: T) => void
        const promise = new Promise<T>((r) => { resolve = r })
        return { promise, resolve }
    }

    it('shows the file in its folder with a progress bar, then confirms success', async () => {
        const firstChunk = deferred<{ success: boolean; size?: number }>()
        const uploadWorkspaceFileChunk = vi.fn()
            .mockImplementationOnce(() => firstChunk.promise)
            .mockResolvedValueOnce({ success: true, size: 1_500_000, path: 'src/big.bin' })
        renderTree({ api: { uploadWorkspaceFileChunk } })

        fireEvent.click(screen.getByRole('button', { name: 'Upload files to project' }))
        const input = screen.getByTestId('workspace-upload-input') as HTMLInputElement
        fireEvent.change(input, { target: { files: [new File([new Uint8Array(1_500_000)], 'big.bin')] } })

        const row = await screen.findByTestId('pending-upload-row')
        expect(row.textContent).toContain('big.bin')
        expect(screen.getByRole('progressbar', { name: 'big.bin' }).getAttribute('aria-valuenow')).toBe('0')

        await act(async () => firstChunk.resolve({ success: true, size: 1_048_576 }))
        await waitFor(() => expect(
            Number(screen.queryByRole('progressbar', { name: 'big.bin' })?.getAttribute('aria-valuenow') ?? 100)
        ).toBeGreaterThan(0))

        expect(await screen.findByText('Upload complete')).toBeTruthy()
        expect(screen.getByText('Uploaded big.bin to project')).toBeTruthy()
        expect(screen.queryByTestId('pending-upload-row')).toBeNull()
        expect(uploadWorkspaceFileChunk).toHaveBeenCalledTimes(2)
    })

    it('drops the row and reports the failure when the upload fails', async () => {
        const uploadWorkspaceFileChunk = vi.fn().mockResolvedValue({ success: false, error: 'Disk full' })
        renderTree({ api: { uploadWorkspaceFileChunk } })

        fireEvent.click(screen.getByRole('button', { name: 'Upload files to project' }))
        const input = screen.getByTestId('workspace-upload-input') as HTMLInputElement
        fireEvent.change(input, { target: { files: [new File(['x'], 'bad.txt')] } })

        expect(await screen.findByText('Failed to upload bad.txt')).toBeTruthy()
        expect(screen.getByText('Disk full')).toBeTruthy()
        expect(screen.queryByTestId('pending-upload-row')).toBeNull()
        expect(screen.queryByText('Upload complete')).toBeNull()
    })
})

describe('DirectoryTree drag and drop', () => {
    function dataTransferWith(files: File[]) {
        return {
            types: ['Files'],
            files,
            items: files.map((file) => ({ kind: 'file', getAsFile: () => file, webkitGetAsEntry: () => ({ isDirectory: false }) })),
            dropEffect: 'none',
        }
    }

    function folderRow(name: string): HTMLElement {
        return screen.getByText(name, { selector: 'div' }).closest('[data-tree-row]') as HTMLElement
    }

    it('marks the top of a hovered folder and uploads the drop there, above its sub-folders', async () => {
        let release!: (value: { success: boolean }) => void
        const uploadWorkspaceFileChunk = vi.fn((..._args: unknown[]) => new Promise((resolve) => { release = resolve }))
        renderTree({ api: { uploadWorkspaceFileChunk } })
        const file = new File(['hi'], 'dropped.txt')

        fireEvent.dragOver(folderRow('project'), { dataTransfer: dataTransferWith([file]) })
        expect(screen.getByTestId('drop-marker')).toBeTruthy()

        fireEvent.drop(folderRow('project'), { dataTransfer: dataTransferWith([file]) })
        expect(screen.queryByTestId('drop-marker')).toBeNull()
        const pending = await screen.findByTestId('pending-upload-row')
        // In-flight uploads sit at the very top of the folder, before "src".
        const srcRow = screen.getByText('src', { selector: 'div' })
        expect(pending.compareDocumentPosition(srcRow) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
        await waitFor(() => expect(uploadWorkspaceFileChunk).toHaveBeenCalled())
        expect(uploadWorkspaceFileChunk.mock.calls[0][1]).toMatchObject({ dir: '', name: 'dropped.txt' })
        await act(async () => release({ success: true }))
    })

    it('drops between files at the marked position', async () => {
        const uploadWorkspaceFileChunk = vi.fn((..._args: unknown[]) => new Promise(() => {}))
        renderTree({ api: { uploadWorkspaceFileChunk } })
        const fileRowEl = screen.getByText('README.md').closest('[data-tree-row]') as HTMLElement
        fileRowEl.getBoundingClientRect = () => ({ top: 100, height: 40, bottom: 140, left: 0, right: 200, width: 200, x: 0, y: 100, toJSON: () => ({}) })
        const file = new File(['x'], 'between.txt')

        fireEvent.dragOver(fileRowEl, { dataTransfer: dataTransferWith([file]), clientY: 135 })
        const marker = screen.getByTestId('drop-marker')
        expect(fileRowEl.compareDocumentPosition(marker) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()

        fireEvent.drop(fileRowEl, { dataTransfer: dataTransferWith([file]), clientY: 135 })
        const pending = await screen.findByTestId('pending-upload-row')
        expect(fileRowEl.compareDocumentPosition(pending) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    })
})

describe('DirectoryTree create folder and rename', () => {
    it('creates "New folder" (avoiding taken names) and opens it for renaming', async () => {
        mocks.entries = [
            { name: 'README.md', type: 'file' },
            { name: 'src', type: 'directory' },
            { name: 'New folder', type: 'directory' },
        ]
        const createWorkspaceDirectory = vi.fn(async () => {
            mocks.entries = [...mocks.entries, { name: 'New folder 2', type: 'directory' }]
            return { success: true, path: 'New folder 2' }
        })
        const renameWorkspacePath = vi.fn(async () => ({ success: true, path: 'assets' }))
        try {
            renderTree({ api: { createWorkspaceDirectory, renameWorkspacePath } })

            fireEvent.click(screen.getByRole('button', { name: 'New folder in project' }))
            await waitFor(() => expect(createWorkspaceDirectory).toHaveBeenCalledWith('session-1', '', 'New folder 2'))

            const input = await screen.findByTestId('rename-input') as HTMLInputElement
            expect(input.value).toBe('New folder 2')
            fireEvent.change(input, { target: { value: 'assets' } })
            fireEvent.keyDown(input, { key: 'Enter' })
            await waitFor(() => expect(renameWorkspacePath).toHaveBeenCalledWith('session-1', 'New folder 2', 'assets'))
        } finally {
            mocks.entries = [
                { name: 'README.md', type: 'file', size: 12, modified: 1_784_175_060_000 },
                { name: 'src', type: 'directory' },
            ]
        }
    })

    it('renames a file in place when asked, keeping the extension out of the selection', async () => {
        const renameWorkspacePath = vi.fn(async () => ({ success: true, path: 'NOTES.md' }))
        renderTree({ api: { renameWorkspacePath }, renameRequest: { path: 'README.md', nonce: 1 } })

        const input = await screen.findByTestId('rename-input') as HTMLInputElement
        expect(input.value).toBe('README.md')
        expect([input.selectionStart, input.selectionEnd]).toEqual([0, 6])
        fireEvent.change(input, { target: { value: 'NOTES.md' } })
        fireEvent.keyDown(input, { key: 'Enter' })
        await waitFor(() => expect(renameWorkspacePath).toHaveBeenCalledWith('session-1', 'README.md', 'NOTES.md'))
    })

    it('cancels a rename with Escape without calling the runner', async () => {
        const renameWorkspacePath = vi.fn()
        renderTree({ api: { renameWorkspacePath }, renameRequest: { path: 'README.md', nonce: 1 } })
        const input = await screen.findByTestId('rename-input')
        fireEvent.change(input, { target: { value: 'other.md' } })
        fireEvent.keyDown(input, { key: 'Escape' })
        await waitFor(() => expect(screen.queryByTestId('rename-input')).toBeNull())
        expect(renameWorkspacePath).not.toHaveBeenCalled()
    })

    it('opens the folder context menu with the directory kind', () => {
        const onRequestFileMenu = vi.fn()
        renderTree({ onRequestFileMenu })
        fireEvent.contextMenu(screen.getByText('src', { selector: 'div' }).closest('button')!, { clientX: 5, clientY: 6 })
        expect(onRequestFileMenu).toHaveBeenCalledWith('src', { x: 5, y: 6 }, 'directory')
    })
})

describe('DirectoryTree folder download', () => {
    it('zips the folder on the runner, pulls it in chunks, saves it, then releases', async () => {
        const downloadBlob = vi.spyOn(await import('@/lib/file-download'), 'downloadBlob').mockImplementation(() => {})
        const archiveId = 'b'.repeat(32)
        const api = {
            startWorkspaceArchive: vi.fn(async () => ({ success: true, archiveId, state: 'building', totalBytes: 10, doneBytes: 0, fileName: 'src.zip' })),
            getWorkspaceArchiveStatus: vi.fn(async () => ({ success: true, archiveId, state: 'ready', size: 6 })),
            readWorkspaceArchiveChunk: vi.fn(async (_s: string, _id: string, offset: number) => new Uint8Array(offset === 0 ? [1, 2, 3, 4, 5, 6] : []).buffer),
            releaseWorkspaceArchive: vi.fn(async () => ({ success: true })),
        }
        try {
            renderTree({ api })
            fireEvent.click(screen.getByRole('button', { name: 'Download src as zip' }))
            expect(await screen.findByText(/Packing \d+%/)).toBeTruthy()

            await waitFor(() => expect(downloadBlob).toHaveBeenCalled(), { timeout: 3000 })
            expect(api.startWorkspaceArchive).toHaveBeenCalledWith('session-1', 'src')
            const [name, blob] = downloadBlob.mock.calls[0] as [string, Blob]
            expect(name).toBe('src.zip')
            expect(blob.size).toBe(6)
            await waitFor(() => expect(api.releaseWorkspaceArchive).toHaveBeenCalledWith('session-1', archiveId))
            expect(screen.queryByText(/Packing|Downloading/)).toBeNull()
        } finally {
            downloadBlob.mockRestore()
        }
    })

    it('reports a folder that is too large', async () => {
        const api = {
            startWorkspaceArchive: vi.fn(async () => ({ success: false, code: 'too_large', error: 'Folder is too large to download' })),
            releaseWorkspaceArchive: vi.fn(),
        }
        renderTree({ api })
        fireEvent.click(screen.getByRole('button', { name: 'Download project as zip' }))
        expect(await screen.findByText('Failed to download project')).toBeTruthy()
        expect(screen.getByText('Folder is too large to download')).toBeTruthy()
        expect(api.releaseWorkspaceArchive).not.toHaveBeenCalled()
    })
})
