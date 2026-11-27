import { useEffect, useState, type RefObject } from 'react'

/**
 * Returns true while the user is dragging files over the browser window — or,
 * with `scopeRef`, only while the drag is over that element (so a drop zone's
 * highlight does not light up while files are dragged over a sibling drop
 * target such as the files panel). Always suppresses the browser's default
 * file-open behaviour for drags that land outside an explicit drop zone.
 */
export function useDragOver(scopeRef?: RefObject<HTMLElement | null>): boolean {
    const [isDraggingFiles, setIsDraggingFiles] = useState(false)

    useEffect(() => {
        const isInScope = (target: EventTarget | null) => {
            if (!scopeRef) return true
            const scope = scopeRef.current
            return Boolean(scope && target instanceof Node && scope.contains(target))
        }

        // `dragenter` fires for every element the pointer moves into, so its
        // target tracks where the drag currently is.
        const onDragEnter = (e: DragEvent) => {
            if (e.dataTransfer?.types.includes('Files')) {
                setIsDraggingFiles(isInScope(e.target))
            }
        }

        // Only clear when the drag leaves the browser window entirely
        // (relatedTarget === null means the pointer moved outside the document)
        const onDragLeave = (e: DragEvent) => {
            if (e.relatedTarget === null) {
                setIsDraggingFiles(false)
            }
        }

        const clearDrag = () => setIsDraggingFiles(false)

        // Prevent the browser from opening/navigating to a file dropped outside
        // an explicit drop zone (e.g. the sidebar). This must run on BOTH
        // `dragover` and `drop`: preventing only `dragover` still lets the
        // browser perform its default file-open action on the `drop` event.
        const preventFileDefault = (e: DragEvent) => {
            if (e.dataTransfer?.types.includes('Files')) {
                e.preventDefault()
            }
        }

        const onDrop = (e: DragEvent) => {
            preventFileDefault(e)
            clearDrag()
        }

        document.addEventListener('dragenter', onDragEnter)
        document.addEventListener('dragleave', onDragLeave)
        document.addEventListener('dragend', clearDrag)
        document.addEventListener('drop', onDrop)
        document.addEventListener('dragover', preventFileDefault)

        return () => {
            document.removeEventListener('dragenter', onDragEnter)
            document.removeEventListener('dragleave', onDragLeave)
            document.removeEventListener('dragend', clearDrag)
            document.removeEventListener('drop', onDrop)
            document.removeEventListener('dragover', preventFileDefault)
        }
    }, [scopeRef])

    return isDraggingFiles
}
