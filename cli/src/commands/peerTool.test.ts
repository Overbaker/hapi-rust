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
        const result = await handlePeerTool(name, {})
        expect(result.isError).toBe(true)
        expect(result.content[0]?.text).toContain('HAPI_DISABLE_PEER_TOOLS')
    }
})
it('validates arguments before contacting the hub', async () => {
    vi.stubEnv('HAPI_DISABLE_PEER_TOOLS', '')
    const result = await handlePeerTool('ping_peer', { message: 'test', sessionIdPrefix: 'abc', deliveryMode: 'invalid' })
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
        expect((await handlePeerTool('spawn_peer', { directory: '.', message: 'hello', waitForReply: true }, watch)).content[0]?.text)
            .toBe('Spawned child (Worker); its reply will be posted here')
        expect(watch).toHaveBeenCalledWith('child', 'task', expect.objectContaining({ endedPolls: 0 }))
        expect(spawn).toHaveBeenCalledWith(expect.objectContaining({
            cwd: '/project', requireParent: true, parent: { sessionId: 'parent', name: 'Caller', agentSessionId: 'native' }
        }))
        expect((await handlePeerTool('ping_peer', { sessionIdPrefix: 'child', message: 'test' })).content[0]?.text)
            .toBe('Accepted for child (resumed) (Worker) localId=task — delivery shows in the recipient chat when its runtime consumes it')
        expect(ping).toHaveBeenCalledWith(expect.objectContaining({ wake: false }))
        expect((await handlePeerTool('list_peers', {})).isError).toBe(false)
        expect(list).toHaveBeenCalledWith(expect.objectContaining({ includeInactive: false }))
    } finally {
        vi.restoreAllMocks()
    }
})
