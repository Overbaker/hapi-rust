import { readFile, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { startHappyServer } from '../../../cli/src/claude/utils/startHappyServer'
import type { ApiSessionClient } from '../../../cli/src/api/apiSession'
import { LATEST_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS } from '../../../cli/node_modules/@modelcontextprotocol/sdk/dist/esm/types.js'

async function golden(name: string, value: unknown) {
    const path = resolve(import.meta.dir, name + '.json')
    const text = JSON.stringify(value, null, 2) + '\n'
    if (process.argv.includes('--check')) {
        if (await readFile(path, 'utf8') !== text) throw new Error('Outdated golden: ' + path)
    } else {
        await writeFile(path, text)
    }
}

await golden('mcp_versions', { latest: LATEST_PROTOCOL_VERSION, supported: SUPPORTED_PROTOCOL_VERSIONS })
for (const disabled of [false, true]) {
    process.env.HAPI_DISABLE_PEER_TOOLS = disabled ? '1' : '0'
    const server = await startHappyServer({ updateMetadata() {} } as unknown as ApiSessionClient, {
        skillLookup: { workingDirectory: process.cwd(), flavor: 'claude' },
        enableLinkPr: false,
    })
    try {
        const headers = new Headers({ accept: 'application/json, text/event-stream', 'content-type': 'application/json' })
        async function rpc(method: string, params: unknown) {
            const response = await fetch(server.url, { method: 'POST', headers,
                body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })
            if (!response.ok) throw new Error(await response.text())
            const session = response.headers.get('mcp-session-id')
            if (session) headers.set('mcp-session-id', session)
            const text = await response.text()
            const message = JSON.parse(text.split('\n').find(line => line.startsWith('data:'))?.slice(5) ?? text)
            if (message.error) throw new Error(JSON.stringify(message.error))
            return message.result
        }
        await rpc('initialize', { protocolVersion: LATEST_PROTOCOL_VERSION, capabilities: {}, clientInfo: { name: 'golden', version: '1' } })
        await golden('mcp_tools_' + (disabled ? 'nopeer' : 'all'), await rpc('tools/list', {}))
    } finally {
        server.stop()
    }
}
