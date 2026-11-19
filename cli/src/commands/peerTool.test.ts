import { afterEach, expect, it, vi } from 'vitest'
import { handlePeerTool } from '@/modules/pingPeer/peerTool'
import { resolveCommand } from './registry'

afterEach(() => vi.unstubAllEnvs())
it('dispatches peer-tool as a non-interactive command', async () => {
    expect((await resolveCommand(['peer-tool', 'list_peers']))?.command.requiresRuntimeAssets).toBe(false)
})
it('refuses peer operations when disabled, including direct subcommand calls', async () => {
    vi.stubEnv('HAPI_DISABLE_PEER_TOOLS', 'YES')
    for (const name of ['list_peers', 'inspect_peer', 'ping_peer', 'spawn_peer']) {
        const result = await handlePeerTool(name, {}, { callerSessionId: 'parent' })
        expect(result.isError).toBe(true)
        expect(result.content[0]?.text).toContain('HAPI_DISABLE_PEER_TOOLS')
    }
})
it('validates arguments before contacting the hub', async () => {
    vi.stubEnv('HAPI_DISABLE_PEER_TOOLS', '')
    const result = await handlePeerTool('ping_peer', { message: 'test', sessionIdPrefix: 'abc', deliveryMode: 'invalid' }, { callerSessionId: 'parent' })
    expect(result.isError).toBe(true)
    expect(result.content[0]?.text).toContain('deliveryMode')
})

it('returns the same receipts and parent identity as the TypeScript MCP handlers', async () => {
    vi.stubEnv('HAPI_DISABLE_PEER_TOOLS', '')
    vi.stubEnv('HAPI_SESSION_ID', 'parent')
    vi.stubEnv('HAPI_SESSION_NAME', 'Caller')
    vi.stubEnv('HAPI_AGENT_SESSION_ID', 'native')
    vi.stubEnv('HAPI_PEER_TOOL_CWD', '/project')
    const peers = await import('@/modules/pingPeer/pingPeer')
    const spawns = await import('@/modules/spawnPeer/spawnPeer')
    const ping = vi.spyOn(peers, 'pingPeer').mockResolvedValue({
        sessionId: 'child', name: 'Worker', localId: 'task', delivery: 'accepted', resumed: true
    })
    const spawn = vi.spyOn(spawns, 'spawnPeer').mockResolvedValue({
        sessionId: 'child', name: 'Worker', localId: 'task', flavor: 'codex'
    })
    const list = vi.spyOn(peers, 'listPeerSessions').mockResolvedValue([])
    try {
        const watch = vi.fn()
        const ctx = { callerSessionId: 'parent', name: 'Caller', agentSessionId: 'native', cwd: '/project', delegateReply: watch }
        expect((await handlePeerTool('spawn_peer', { directory: '.', message: 'hello', waitForReply: true }, ctx)).content[0]?.text)
            .toBe('Spawned child (Worker); its reply will be posted here')
        expect(watch).toHaveBeenCalledWith('child', 'task', expect.objectContaining({ endedPolls: 0 }))
        expect(spawn).toHaveBeenCalledWith(expect.objectContaining({
            cwd: '/project', requireParent: true, parent: { sessionId: 'parent', name: 'Caller', agentSessionId: 'native' }
        }))
        expect((await handlePeerTool('ping_peer', { sessionIdPrefix: 'child', message: 'test' }, ctx)).content[0]?.text)
            .toBe('Accepted for child (resumed) (Worker) localId=task — delivery shows in the recipient chat when its runtime consumes it')
        expect(ping).toHaveBeenCalledWith(expect.objectContaining({ wake: false }))
        expect((await handlePeerTool('list_peers', {}, ctx)).isError).toBe(false)
        expect(list).toHaveBeenCalledWith(expect.objectContaining({ includeInactive: false }))
    } finally {
        vi.restoreAllMocks()
    }
})

it('exchanges JSON on stdin and stdout for delegated calls and reply polls', async () => {
    const { Readable } = await import('node:stream')
    const { peerToolCommand } = await import('./peerTool')
    const auth = await import('@/ui/tokenInit')
    const peers = await import('@/modules/pingPeer/pingPeer')
    const init = vi.spyOn(auth, 'initializeToken').mockResolvedValue()
    const ping = vi.spyOn(peers, 'pingPeer').mockResolvedValue({
        sessionId: 'peer', name: 'Worker', localId: 'task', delivery: 'accepted', resumed: false
    })
    const advance = vi.spyOn(peers, 'advancePeerReplyWatch').mockResolvedValue(undefined)
    const output = vi.spyOn(process.stdout, 'write').mockReturnValue(true)
    const stdin = vi.spyOn(process, 'stdin', 'get')
    async function run(input: string) {
        stdin.mockReturnValue(Readable.from([Buffer.from(input)]) as typeof process.stdin)
        await peerToolCommand.run({ args: [], commandArgs: ['ping_peer'] })
        return JSON.parse(String(output.mock.calls.at(-1)?.[0]))
    }
    try {
        vi.stubEnv('HAPI_SESSION_ID', 'caller')
        vi.stubEnv('HAPI_DISABLE_PEER_TOOLS', '1')
        expect((await run('{}')).content[0].text).toContain('disabled')
        vi.stubEnv('HAPI_DISABLE_PEER_TOOLS', '')
        vi.stubEnv('HAPI_PEER_TOOL_PHASE', 'call')
        expect((await run('bad json')).isError).toBe(true)
        expect((await run('{}')).isError).toBe(true)
        expect(init).not.toHaveBeenCalled()
        const receipt = await run(JSON.stringify({ sessionIdPrefix: 'peer', message: 'hello', waitForReply: true }))
        expect(receipt.isError).toBe(false)
        expect(receipt._meta).toMatchObject({ peerSessionId: 'peer', taskLocalId: 'task', state: { endedPolls: 0 } })
        vi.stubEnv('HAPI_PEER_TOOL_PHASE', 'poll')
        const next = { ...receipt._meta.state, endedPolls: 1 }
        advance.mockResolvedValueOnce(next)
        const poll = await run(JSON.stringify(receipt._meta))
        expect(poll._meta).toEqual({ ...receipt._meta, state: next })
        expect(await run(JSON.stringify(poll._meta))).toEqual({ content: [], isError: false })
        expect(advance).toHaveBeenLastCalledWith({ peerSessionId: 'peer', taskLocalId: 'task', callerSessionId: 'caller' }, next)
        expect(ping).toHaveBeenCalledTimes(1)
    } finally {
        vi.restoreAllMocks()
    }
})
