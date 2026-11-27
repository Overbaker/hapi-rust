import type { SpawnSessionResult } from '@/modules/common/rpcTypes';
import type { ChildProcess } from 'node:child_process';

/** A failed startup may still own a live process using its working directory. */
export function cleanupFailedStartupAfterExit(
    child: Pick<ChildProcess, 'exitCode' | 'signalCode'> & { once(event: 'exit', listener: () => void): unknown },
    shouldCleanup: () => boolean,
    cleanup: () => void,
): void {
    const onExit = () => { if (shouldCleanup()) cleanup(); };
    if (child.exitCode !== null || child.signalCode !== null) onExit();
    else child.once('exit', onExit);
}

export function sessionStartupTimeout(agent: string | undefined, configured?: string): number {
    const explicit = Number(configured);
    if (Number.isFinite(explicit) && explicit > 0) return explicit;
    return agent === 'codex' ? 30_000 : 15_000;
}

/** A caller's deadline does not prove that a shared execution failed to start. */
export function waitForRecoverableStartup<T>(options: {
    timeoutMs: number;
    timeoutMessage: string;
    sessionId: (session: T) => string;
    onTimeout?: () => void;
    onLateReady: (session: T) => void;
    onLateFailure?: (errorMessage: string) => void;
}) {
    let phase: 'waiting' | 'unconfirmed' | 'finished' = 'waiting';
    let resolve!: (result: SpawnSessionResult | Promise<SpawnSessionResult>) => void;
    const result = new Promise<SpawnSessionResult>(done => { resolve = done; });
    const timer = setTimeout(() => {
        if (phase !== 'waiting') return;
        phase = 'unconfirmed';
        options.onTimeout?.();
        resolve({ type: 'error', errorMessage: options.timeoutMessage });
    }, options.timeoutMs);
    return {
        result,
        replace(startRetry: () => Promise<SpawnSessionResult> | null): boolean {
            if (phase !== 'waiting') return false;
            const retry = startRetry();
            if (!retry) return false;
            clearTimeout(timer);
            phase = 'finished';
            resolve(retry);
            return true;
        },
        confirm(session: T): void {
            if (phase === 'finished') return;
            const late = phase === 'unconfirmed';
            phase = 'finished'; clearTimeout(timer);
            if (late) options.onLateReady(session);
            resolve({ type: 'success', sessionId: options.sessionId(session) });
        },
        fail(errorMessage: string): void {
            if (phase === 'finished') return;
            const late = phase === 'unconfirmed';
            phase = 'finished'; clearTimeout(timer);
            if (late) options.onLateFailure?.(errorMessage);
            resolve({ type: 'error', errorMessage });
        },
    };
}

export type WrapperFailure = 'exit-before-webhook' | 'process-error-before-webhook'

/** Exit code a Rust wrapper uses to ask the runner for a TypeScript start. */
export const RUST_WRAPPER_FALLBACK_EXIT_CODE = 86

/** One TypeScript retry, only for a Rust exit 86 that came before any webhook or deadline. */
export function wrapperRetryState(runtime: 'rust' | 'ts') {
    let eligible = runtime === 'rust'
    return (event: WrapperFailure | 'webhook' | 'timeout', exitCode: number | null = null): boolean => {
        const retry = eligible && event === 'exit-before-webhook' && exitCode === RUST_WRAPPER_FALLBACK_EXIT_CODE
        eligible = false
        return retry
    }
}

/** Time left of a start's webhook deadline, shared by its TypeScript retry; null when none is left. */
export function remainingStartupBudget(deadline: number, now = Date.now()): number | null {
    const remaining = Math.floor(deadline - now)
    return remaining > 0 ? remaining : null
}

/**
 * The hub row a TypeScript retry continues, or null when it cannot take it
 * over: the webhook and the wrapper's stderr name different rows, or a fresh
 * Codex row has no native thread to reopen (codex/shared/frontend.ts).
 */
export function fallbackSessionId(options: {
    agent: string
    existingSessionId?: string
    webhookSessionId?: string
    learnedSessionId?: string
}): { sessionId?: string } | null {
    if (options.existingSessionId) return { sessionId: options.existingSessionId }
    const { webhookSessionId, learnedSessionId } = options
    if (webhookSessionId && learnedSessionId && webhookSessionId !== learnedSessionId) return null
    const sessionId = webhookSessionId ?? learnedSessionId
    if (sessionId && options.agent === 'codex') return null
    return { sessionId }
}

export const hubUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

/** Reads the hub session id a Rust wrapper prints on stderr before it can exit 86. */
export function rustSessionMarker() {
    const prefix = 'HAPI_RUST_WRAPPER_SESSION_ID=';
    const maxLength = prefix.length + 36 + 1;
    let line = '';
    let overlong = false;
    let sessionId: string | undefined;
    function finishLine() {
        if (!sessionId && !overlong && line.startsWith(prefix)) {
            const id = line.slice(prefix.length).replace(/\r$/, '');
            if (id.length === 36 && hubUuid.test(id)) sessionId = id;
        }
        line = '';
        overlong = false;
    }
    return {
        write(chunk: string) {
            let start = 0;
            while (!sessionId && start < chunk.length) {
                const newline = chunk.indexOf('\n', start);
                const end = newline < 0 ? chunk.length : newline;
                if (line.length + end - start > maxLength) overlong = true;
                if (!overlong) line += chunk.slice(start, end);
                if (newline < 0) break;
                finishLine();
                start = newline + 1;
            }
        },
        finishLine,
        get id() { return sessionId; },
    };
}

/**
 * Kills what is left of an exited detached leader's process group and waits
 * until the group is empty. The caller has seen the leader exit. A group id
 * stays allocated while any member lives, so a live process holding the
 * leader's PID means that PID was reused after the old group emptied: that
 * group is gone and the new process is not signalled.
 */
export async function reapExitedProcessGroup(pgid: number, startupDeadline = Infinity): Promise<boolean> {
    if (process.platform === 'win32') return true;
    const deadline = Math.min(startupDeadline, Date.now() + 5_000);
    const exists = (target: number): boolean => {
        try {
            process.kill(target, 0);
            return true;
        } catch (error) {
            return (error as NodeJS.ErrnoException).code === 'EPERM';
        }
    };
    const groupAlive = () => exists(-pgid);
    if (exists(pgid) || !groupAlive()) return true;
    try { process.kill(-pgid, 'SIGKILL'); } catch {}
    for (let remaining = deadline - Date.now(); remaining > 0; remaining = deadline - Date.now()) {
        await new Promise(resolve => setTimeout(resolve, Math.min(25, remaining)));
        if (!groupAlive()) return true;
    }
    return false;
}
