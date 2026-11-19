import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { createServer } from 'node:http'
import { resolve } from 'node:path'
const binary = process.argv[2],
    cli = resolve(import.meta.dir, '../../../../../cli')
const messages = [
    {
        jsonrpc: '2.0',
        id: 1,
        method: 'initialize',
        params: {
            protocolVersion: '2025-11-25',
            clientInfo: { name: 'test', version: '1.0.0' },
            capabilities: {}
        }
    },
    { jsonrpc: '2.0', method: 'notifications/initialized' },
    { jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} }
]
async function run(exe, args, packets, env) {
    const p = spawn(exe, args, { cwd: cli, env, stdio: ['pipe', 'pipe', 'pipe'] })
    const done = once(p, 'close')
    let stdout = '',
        stderr = ''
    p.stdout.on('data', (c) => (stdout += c))
    p.stderr.on('data', (c) => (stderr += c))
    p.stdin.end(packets.map((v) => JSON.stringify(v) + '\n').join(''))
    const [code] = await done
    assert.equal(code, 0, stderr)
    return stdout
        .trim()
        .split('\n')
        .map((s) => JSON.parse(s))
        .sort((a, b) => a.id - b.id)
}
const all =
    'change_title,display_image,display_video,display_media,list_peers,ping_peer,inspect_peer,spawn_peer,session_job,skill_lookup'
for (const [disabled, tools] of [
    ['0', undefined],
    ['0', all],
    ['1', all],
    ['0', 'display_media,skill_lookup']
]) {
    const env = {
        ...process.env,
        HAPI_DISABLE_PEER_TOOLS: disabled,
        HAPI_HTTP_MCP_URL: 'http://127.0.0.1:1/'
    }
    const args = tools === undefined ? [] : ['--tools', tools, '--hapi-session-id=stamp']
    const expected = await run(
        'bun',
        [
            '-e',
            `import {runHappyMcpStdioBridge as run} from './src/codex/happyMcpStdioBridge';await run(${JSON.stringify(args)})`
        ],
        messages,
        env
    )
    assert.deepEqual(await run(binary, ['mcp', ...args], messages, env), expected)
}
const calls = []
const http = createServer(async (req, res) => {
    const chunks = []
    for await (const c of req) chunks.push(c)
    const v = JSON.parse(Buffer.concat(chunks).toString())
    calls.push(v.method)
    assert(req.headers.accept.includes('text/event-stream'))
    if (v.method === 'initialize') {
        res.setHeader('mcp-session-id', 'example')
        res.setHeader('content-type', 'application/json')
        res.end(
            JSON.stringify({
                jsonrpc: '2.0',
                id: v.id,
                result: {
                    protocolVersion: '2025-11-25',
                    capabilities: { tools: {} },
                    serverInfo: { name: 'upstream', version: '1' }
                }
            })
        )
        return
    }
    assert.equal(req.headers['mcp-session-id'], 'example')
    assert.equal(req.headers['mcp-protocol-version'], '2025-11-25')
    if (v.method === 'notifications/initialized') {
        res.writeHead(202).end()
        return
    }
    assert.equal(v.params.name, 'display_image')
    res.writeHead(200, { 'content-type': 'text/event-stream' })
    res.write(
        'event: message\r\ndata: ' +
            JSON.stringify({
                jsonrpc: '2.0',
                id: v.id,
                result: { content: [{ type: 'text', text: v.params.arguments.path }] }
            }) +
            '\r\n\r\n'
    )
})
await new Promise((r) => http.listen(0, '127.0.0.1', r))
try {
    const replies = await run(
        binary,
        ['mcp', '--tools', 'display_image', '--url', 'http://127.0.0.1:' + http.address().port + '/'],
        [
            ...messages,
            ...[3, 4].map((id) => ({
                jsonrpc: '2.0',
                id,
                method: 'tools/call',
                params: { name: 'display_image', arguments: { path: '/image-' + id + '.png' } }
            }))
        ],
        process.env
    )
    assert.deepEqual(calls, ['initialize', 'notifications/initialized', 'tools/call', 'tools/call'])
    assert.deepEqual(
        replies.slice(2).map((v) => v.result.content[0].text),
        ['/image-3.png', '/image-4.png']
    )
} finally {
    http.closeAllConnections()
    http.close()
}
