// This entry is copied beside stock-bootstrap.ts in the temporary diagnostic build.
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { mark } from './idle-probe'

switch (process.argv[2]) {
    case 'idle-scanner': {
        const { createCodexSessionScanner } = await import('./codex/utils/codexSessionScanner')
        const transcriptPath = join(process.cwd(), 'transcript.jsonl')
        if (process.argv[3] === 'healthy') writeFileSync(transcriptPath, '')
        await createCodexSessionScanner({ transcriptPath, onEvent() {} })
        mark('ready')
        process.stdin.resume()
        await new Promise(() => {})
        break
    }
    case 'idle-network': {
        const { startHappyServer } = await import('./claude/utils/startHappyServer')
        const { io } = await import('socket.io-client')
        await startHappyServer({ updateMetadata() {} } as Parameters<typeof startHappyServer>[0], { enabledTools: ['display_image'] })
        const socket = io(process.env.IDLE_SOCKET_URL!, { transports: ['websocket'], reconnection: false })
        socket.on('connect', () => mark('ready'))
        socket.on('disconnect', () => process.exit(1))
        break
    }
    default:
        await import('./stock-bootstrap')
}
