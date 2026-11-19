import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'

/** The scripted engine acts as an MCP client just as a thread would during startup. */
export async function openBridge(config: { command: string; args: string[] }) {
    const child = spawn(config.command, config.args, { stdio: ['pipe', 'pipe', 'inherit'] })
    const lines = createInterface({ input: child.stdout })[Symbol.asyncIterator]()
    const send = (v: object) => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', ...v }) + '\n')
    const request = async (id: number, method: string, params: object) => {
        send({ id, method, params })
        for (;;) {
            const { value, done } = await lines.next()
            if (done) throw Error('bridge exited during ' + method)
            const result = JSON.parse(value)
            if (result.id !== id) continue
            if (result.error || result.result.isError) throw Error(JSON.stringify(result))
            return result.result
        }
    }
    await request(1, 'initialize', {
        protocolVersion: '2025-11-25',
        capabilities: {},
        clientInfo: { name: 'fixture', version: '1' }
    })
    send({ method: 'notifications/initialized' })
    await request(2, 'tools/list', {})
    await request(3, 'tools/call', { name: 'change_title', arguments: { title: 'Measurement' } })
    return child.pid
}
