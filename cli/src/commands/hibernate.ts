import axios, { type AxiosInstance } from 'axios'
import chalk from 'chalk'
import { readSettings } from '@/persistence'
import { initializeToken } from '@/ui/tokenInit'
import { buildHubRequestHeaders } from '@/api/hubExtraHeaders'
import { resolveAccessToken, resolveApiUrl } from '@/modules/pingPeer/pingPeer'
import type { CommandDefinition } from './types'

export type ParsedHibernateArgs =
    | { kind: 'help' }
    | { kind: 'one'; sessionIdPrefix: string }
    | { kind: 'all-idle'; minIdleMinutes?: number }
    | { kind: 'from-archived'; sessionIdPrefix: string }
    | { kind: 'all-signalled'; since?: string }

export class HibernateArgsError extends Error {}

export function hibernateHelpText(): string {
    return `
${chalk.bold('hapi hibernate')} - Put runner sessions to sleep on demand

${chalk.bold('Usage:')}
  hapi hibernate <session-id-or-prefix>
  hapi hibernate --all-idle [--min-idle-minutes N]
  hapi hibernate --from-archived <session-id-or-prefix>
  hapi hibernate --all-signalled [--since <ISO time>]

${chalk.bold('Notes:')}
  A hibernated session stays in the session list and wakes on the next
  message, exactly like automatic idle hibernation. The hub refuses (with a
  reason) sessions that are mid-turn, uploading, waiting on a peer reply,
  without a replayable runner launch, etc. Works even when automatic
  hibernation is off (HAPI_SESSION_HIBERNATE_MS=0).

  --all-idle         every eligible session of this namespace on this machine
  --min-idle-minutes only sessions with no agent progress for N minutes
  --from-archived    turn a session archived by a wrapper signal (SIGTERM)
                     back into a hibernated one, if its runner can wake it
  --all-signalled    the same for every signal-archived session on this machine
  --since            only sessions archived at or after this ISO time
`
}

export function parseHibernateArgs(args: string[]): ParsedHibernateArgs {
    type Mode = 'one' | 'all-idle' | 'from-archived' | 'all-signalled'
    let mode = null as Mode | null
    let prefix: string | undefined
    let minIdleMinutes: number | undefined
    let since: string | undefined
    const setMode = (next: Mode) => {
        if (mode && mode !== 'one') throw new HibernateArgsError(`--${next} cannot be combined with --${mode}`)
        mode = next
    }
    for (let i = 0; i < args.length; i += 1) {
        const arg = args[i]!
        if (arg === '--help' || arg === '-h') return { kind: 'help' }
        if (arg === '--all-idle') { setMode('all-idle'); continue }
        if (arg === '--all-signalled') { setMode('all-signalled'); continue }
        if (arg === '--from-archived') { setMode('from-archived'); continue }
        if (arg === '--min-idle-minutes' || arg === '--since') {
            const value = args[i + 1]
            if (value === undefined || value.startsWith('--')) throw new HibernateArgsError(`${arg} needs a value`)
            i += 1
            if (arg === '--since') {
                if (Number.isNaN(Date.parse(value))) throw new HibernateArgsError(`--since: invalid ISO time '${value}'`)
                since = new Date(Date.parse(value)).toISOString()
            } else {
                const minutes = Number(value)
                if (!Number.isFinite(minutes) || minutes < 0) throw new HibernateArgsError(`--min-idle-minutes: invalid number '${value}'`)
                minIdleMinutes = minutes
            }
            continue
        }
        if (arg.startsWith('-')) throw new HibernateArgsError(`unexpected flag: ${arg}`)
        if (prefix !== undefined) throw new HibernateArgsError(`unexpected arg: ${arg}`)
        prefix = arg.trim()
    }
    const effective: Mode = (mode as Mode | null) ?? 'one'
    if (minIdleMinutes !== undefined && effective !== 'all-idle') throw new HibernateArgsError('--min-idle-minutes requires --all-idle')
    if (since !== undefined && effective !== 'all-signalled') throw new HibernateArgsError('--since requires --all-signalled')
    if (effective === 'all-idle' || effective === 'all-signalled') {
        if (prefix !== undefined) throw new HibernateArgsError(`--${effective} does not take a session id`)
        return effective === 'all-idle' ? { kind: 'all-idle', minIdleMinutes } : { kind: 'all-signalled', since }
    }
    if (!prefix) throw new HibernateArgsError('a session id or prefix is required')
    return { kind: effective, sessionIdPrefix: prefix }
}

export type HibernateOutcome = { sessionId: string; ok: boolean; code?: string; message?: string }

type HibernateClientOptions = { apiUrl: string; accessToken: string; http?: AxiosInstance }

function cliHeaders(accessToken: string): Record<string, string> {
    return buildHubRequestHeaders({ Authorization: `Bearer ${accessToken}`, 'Content-Type': 'application/json' })
}

async function post(options: HibernateClientOptions, path: string, body: unknown = {}): Promise<{ status: number; data: any }> {
    const http = options.http ?? axios
    const response = await http.post(`${options.apiUrl}/cli${path}`, body, {
        headers: cliHeaders(options.accessToken), timeout: 120_000, validateStatus: () => true
    })
    return { status: response.status, data: response.data }
}

async function get(options: HibernateClientOptions, path: string, params: Record<string, string>): Promise<any> {
    const http = options.http ?? axios
    const response = await http.get(`${options.apiUrl}/cli${path}`, {
        headers: cliHeaders(options.accessToken), params, timeout: 30_000, validateStatus: () => true
    })
    if (response.status < 200 || response.status >= 300) {
        throw new Error(typeof response.data?.error === 'string' ? response.data.error : `HTTP ${response.status}`)
    }
    return response.data
}

function toOutcome(sessionId: string, response: { status: number; data: any }): HibernateOutcome {
    if (response.status >= 200 && response.status < 300 && response.data?.ok === true) {
        return { sessionId: response.data.sessionId ?? sessionId, ok: true }
    }
    return {
        sessionId: response.data?.sessionId ?? sessionId,
        ok: false,
        code: typeof response.data?.code === 'string' ? response.data.code : `http_${response.status}`,
        message: typeof response.data?.error === 'string' ? response.data.error : `HTTP ${response.status}`
    }
}

export async function runHibernate(parsed: Exclude<ParsedHibernateArgs, { kind: 'help' }>, options: HibernateClientOptions & { machineId?: string }): Promise<HibernateOutcome[]> {
    if (parsed.kind === 'one' || parsed.kind === 'from-archived') {
        const path = parsed.kind === 'one' ? 'hibernate' : 'mark-hibernated'
        return [toOutcome(parsed.sessionIdPrefix, await post(options, `/sessions/${encodeURIComponent(parsed.sessionIdPrefix)}/${path}`))]
    }
    if (!options.machineId) throw new Error('This machine has no machine id yet; start the runner once (hapi runner start)')
    const params: Record<string, string> = { machineId: options.machineId }
    if (parsed.kind === 'all-idle') {
        if (parsed.minIdleMinutes !== undefined) params.minIdleMinutes = String(parsed.minIdleMinutes)
        const data = await get(options, '/hibernate/candidates', params)
        const outcomes: HibernateOutcome[] = []
        for (const candidate of (data?.sessions ?? []) as Array<{ sessionId: string; refusal: string | null; message?: string }>) {
            if (candidate.refusal) {
                outcomes.push({ sessionId: candidate.sessionId, ok: false, code: candidate.refusal, message: candidate.message ?? candidate.refusal })
                continue
            }
            outcomes.push(toOutcome(candidate.sessionId, await post(options, `/sessions/${encodeURIComponent(candidate.sessionId)}/hibernate`)))
        }
        return outcomes
    }
    if (parsed.since !== undefined) params.since = parsed.since
    const data = await get(options, '/hibernate/signal-archived', params)
    const outcomes: HibernateOutcome[] = []
    for (const row of (data?.sessions ?? []) as Array<{ sessionId: string }>) {
        outcomes.push(toOutcome(row.sessionId, await post(options, `/sessions/${encodeURIComponent(row.sessionId)}/mark-hibernated`)))
    }
    return outcomes
}

export function formatHibernateOutcome(outcome: HibernateOutcome): string {
    return outcome.ok
        ? `${outcome.sessionId} hibernated`
        : `${outcome.sessionId} refused: ${outcome.message ?? outcome.code} (${outcome.code})`
}

export const hibernateCommand: CommandDefinition = {
    name: 'hibernate',
    requiresRuntimeAssets: false,
    run: async ({ commandArgs }) => {
        try {
            const parsed = parseHibernateArgs(commandArgs)
            if (parsed.kind === 'help') {
                console.log(hibernateHelpText())
                return
            }
            await initializeToken()
            const settings = await readSettings()
            const outcomes = await runHibernate(parsed, {
                apiUrl: resolveApiUrl(), accessToken: resolveAccessToken(), machineId: settings.machineId
            })
            if (outcomes.length === 0) console.log('hapi hibernate: no matching sessions')
            for (const outcome of outcomes) {
                console.log(outcome.ok ? chalk.green(formatHibernateOutcome(outcome)) : chalk.yellow(formatHibernateOutcome(outcome)))
            }
            if ((parsed.kind === 'one' || parsed.kind === 'from-archived') && outcomes.some((outcome) => !outcome.ok)) process.exit(1)
        } catch (error) {
            if (error instanceof HibernateArgsError) console.error(hibernateHelpText())
            console.error(chalk.red('hapi hibernate:'), error instanceof Error ? error.message : 'Unknown error')
            process.exit(1)
        }
    }
}
